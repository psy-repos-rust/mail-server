/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs Ltd <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

use ahash::AHashMap;
use common::{core::BuildServer, ipc::ReportingEvent, Inner, Server, KV_LOCK_QUEUE_REPORT};

use std::{
    future::Future,
    sync::Arc,
    time::{Duration, SystemTime},
};
use store::{
    write::{now, QueueClass, ReportEvent, ValueClass},
    Deserialize, IterateParams, Store, ValueKey,
};
use tokio::sync::mpsc;

use crate::queue::spool::LOCK_EXPIRY;

use super::{dmarc::DmarcReporting, tls::TlsReporting, ReportLock};

pub const REPORT_REFRESH: Duration = Duration::from_secs(86400);

impl SpawnReport for mpsc::Receiver<ReportingEvent> {
    fn spawn(mut self, inner: Arc<Inner>) {
        tokio::spawn(async move {
            let mut next_wake_up;

            loop {
                // Read events
                let now = now();
                let events = next_report_event(inner.shared_core.load().storage.data.clone()).await;
                next_wake_up = events
                    .last()
                    .and_then(|e| match e {
                        QueueClass::DmarcReportHeader(e) | QueueClass::TlsReportHeader(e)
                            if e.due > now =>
                        {
                            Duration::from_secs(e.due - now).into()
                        }
                        _ => None,
                    })
                    .unwrap_or(REPORT_REFRESH);

                let server = inner.build_server();
                let server_ = server.clone();
                tokio::spawn(async move {
                    let mut tls_reports = AHashMap::new();
                    for report_event in events {
                        match report_event {
                            QueueClass::DmarcReportHeader(event) if event.due <= now => {
                                let lock_name = event.dmarc_lock();
                                if server.try_lock_report(&lock_name).await {
                                    server.send_dmarc_aggregate_report(event).await;
                                    server.unlock_report(&lock_name).await;
                                }
                            }
                            QueueClass::TlsReportHeader(event) if event.due <= now => {
                                tls_reports
                                    .entry(event.domain.clone())
                                    .or_insert_with(Vec::new)
                                    .push(event);
                            }
                            _ => (),
                        }
                    }

                    for (_, tls_report) in tls_reports {
                        let lock_name = tls_report.first().unwrap().tls_lock();
                        if server.try_lock_report(&lock_name).await {
                            server.send_tls_aggregate_report(tls_report).await;
                            server.unlock_report(&lock_name).await;
                        }
                    }
                });

                match tokio::time::timeout(next_wake_up, self.recv()).await {
                    Ok(Some(event)) => match event {
                        ReportingEvent::Dmarc(event) => {
                            server_.schedule_dmarc(event).await;
                        }
                        ReportingEvent::Tls(event) => {
                            server_.schedule_tls(event).await;
                        }
                        ReportingEvent::Stop => break,
                    },
                    Ok(None) => break,
                    Err(_) => {}
                }
            }
        });
    }
}

async fn next_report_event(store: Store) -> Vec<QueueClass> {
    let now = now();
    let from_key = ValueKey::from(ValueClass::Queue(QueueClass::DmarcReportHeader(
        ReportEvent {
            due: 0,
            policy_hash: 0,
            seq_id: 0,
            domain: String::new(),
        },
    )));
    let to_key = ValueKey::from(ValueClass::Queue(QueueClass::TlsReportHeader(
        ReportEvent {
            due: now + REPORT_REFRESH.as_secs(),
            policy_hash: 0,
            seq_id: 0,
            domain: String::new(),
        },
    )));

    let mut events = Vec::new();
    let result = store
        .iterate(
            IterateParams::new(from_key, to_key).ascending().no_values(),
            |key, _| {
                let event = ReportEvent::deserialize(key)?;
                let do_continue = event.due <= now;
                events.push(if *key.last().unwrap() == 0 {
                    QueueClass::DmarcReportHeader(event)
                } else {
                    QueueClass::TlsReportHeader(event)
                });
                Ok(do_continue)
            },
        )
        .await;

    if let Err(err) = result {
        trc::error!(err
            .caused_by(trc::location!())
            .details("Failed to read from store"));
    }

    events
}

pub trait LockReport: Sync + Send {
    fn try_lock_report(&self, lock: &[u8]) -> impl Future<Output = bool> + Send;

    fn unlock_report(&self, lock: &[u8]) -> impl Future<Output = ()> + Send;
}

impl LockReport for Server {
    async fn try_lock_report(&self, key: &[u8]) -> bool {
        match self
            .in_memory_store()
            .try_lock(KV_LOCK_QUEUE_REPORT, key, LOCK_EXPIRY)
            .await
        {
            Ok(result) => {
                if !result {
                    trc::event!(
                        OutgoingReport(trc::OutgoingReportEvent::Locked),
                        Expires = trc::Value::Timestamp(LOCK_EXPIRY),
                        Key = key
                    );
                }
                result
            }
            Err(err) => {
                trc::error!(err
                    .details("Failed to lock report.")
                    .caused_by(trc::location!()));
                false
            }
        }
    }

    async fn unlock_report(&self, key: &[u8]) {
        if let Err(err) = self
            .in_memory_store()
            .remove_lock(KV_LOCK_QUEUE_REPORT, key)
            .await
        {
            trc::error!(err
                .details("Failed to unlock event.")
                .caused_by(trc::location!()));
        }
    }
}

pub trait ToTimestamp {
    fn to_timestamp(&self) -> u64;
}

impl ToTimestamp for Duration {
    fn to_timestamp(&self) -> u64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
            + self.as_secs()
    }
}

pub trait SpawnReport {
    fn spawn(self, core: Arc<Inner>);
}
