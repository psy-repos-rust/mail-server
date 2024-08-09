/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs Ltd <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

use std::{
    sync::{atomic::Ordering, Arc, LazyLock},
    thread::{park, Builder, JoinHandle},
    time::SystemTime,
};

use ahash::AHashMap;
use parking_lot::Mutex;

use crate::{
    bitset::{AtomicBitset, USIZE_BITS},
    channel::{Receiver, CHANNEL_FLAGS, CHANNEL_UPDATE_MARKER},
    subscriber::{Interests, Subscriber},
    DeliveryEvent, Event, EventDetails, EventType, Level, NetworkEvent, TOTAL_EVENT_COUNT,
};

pub(crate) type GlobalInterests =
    AtomicBitset<{ (TOTAL_EVENT_COUNT + USIZE_BITS - 1) / USIZE_BITS }>;

pub(crate) static TRACE_INTERESTS: GlobalInterests = GlobalInterests::new();
pub(crate) type CollectorThread = JoinHandle<()>;
pub(crate) static ACTIVE_SUBSCRIBERS: Mutex<Vec<String>> = Mutex::new(Vec::new());
pub(crate) static COLLECTOR_UPDATES: Mutex<Vec<Update>> = Mutex::new(Vec::new());

pub(crate) const EVENT_TYPES: [EventType; TOTAL_EVENT_COUNT] = EventType::variants();

#[allow(clippy::enum_variant_names)]
pub(crate) enum Update {
    RegisterReceiver {
        receiver: Receiver,
    },
    RegisterSubscriber {
        subscriber: Subscriber,
    },
    UnregisterSubscriber {
        id: String,
    },
    UpdateSubscriber {
        id: String,
        interests: Interests,
        lossy: bool,
    },
    UpdateLevels {
        levels: AHashMap<EventType, Level>,
    },
    Shutdown,
}

pub struct Collector {
    receivers: Vec<Receiver>,
    subscribers: Vec<Subscriber>,
    levels: [Level; TOTAL_EVENT_COUNT],
    active_spans: AHashMap<u64, Arc<Event<EventDetails>>>,
}

const EV_CONN_START: usize = EventType::Network(NetworkEvent::ConnectionStart).id();
const EV_CONN_END: usize = EventType::Network(NetworkEvent::ConnectionEnd).id();
const EV_ATTEMPT_START: usize = EventType::Delivery(DeliveryEvent::AttemptStart).id();
const EV_ATTEMPT_END: usize = EventType::Delivery(DeliveryEvent::AttemptEnd).id();

const STALE_SPAN_CHECK_WATERMARK: usize = 8000;
const SPAN_MAX_HOLD: u64 = 86400;

pub(crate) static COLLECTOR_THREAD: LazyLock<Arc<CollectorThread>> = LazyLock::new(|| {
    Arc::new(
        Builder::new()
            .name("stalwart-collector".to_string())
            .spawn(move || {
                Collector::default().collect();
            })
            .expect("Failed to start event collector"),
    )
});

impl Collector {
    fn collect(&mut self) {
        let mut do_continue = true;

        // Update
        self.update();

        while do_continue {
            match CHANNEL_FLAGS.swap(0, Ordering::Relaxed) {
                0 => {
                    park();
                }
                CHANNEL_UPDATE_MARKER..=u64::MAX => {
                    do_continue = self.update();
                }
                _ => {}
            }

            // Collect all events
            let mut closed_rxs = Vec::new();
            for (rx_idx, rx) in self.receivers.iter_mut().enumerate() {
                let timestamp = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs());

                loop {
                    match rx.try_recv() {
                        Ok(Some(event)) => {
                            // Build event
                            let event_id = event.inner.id();
                            let mut event = Event {
                                inner: EventDetails {
                                    level: self.levels[event_id],
                                    typ: event.inner,
                                    timestamp,
                                    span: None,
                                },
                                keys: event.keys,
                            };

                            // Track spans
                            let event = match event_id {
                                EV_CONN_START | EV_ATTEMPT_START => {
                                    let event = Arc::new(event);
                                    self.active_spans.insert(
                                        event.span_id().unwrap_or_else(|| {
                                            panic!("Missing span ID: {event:?}")
                                        }),
                                        event.clone(),
                                    );
                                    if self.active_spans.len() > STALE_SPAN_CHECK_WATERMARK {
                                        self.active_spans.retain(|_, span| {
                                            timestamp.saturating_sub(span.inner.timestamp)
                                                < SPAN_MAX_HOLD
                                        });
                                    }
                                    event
                                }
                                EV_CONN_END | EV_ATTEMPT_END => {
                                    if let Some(span) = self
                                        .active_spans
                                        .remove(&event.span_id().expect("Missing span ID"))
                                    {
                                        event.inner.span = Some(span.clone());
                                    } else {
                                        #[cfg(debug_assertions)]
                                        {
                                            if event.span_id().unwrap() != 0 {
                                                panic!("Unregistered span ID: {event:?}");
                                            }
                                        }
                                    }
                                    Arc::new(event)
                                }
                                _ => {
                                    if let Some(span_id) = event.span_id() {
                                        if let Some(span) = self.active_spans.get(&span_id) {
                                            event.inner.span = Some(span.clone());
                                        } else {
                                            #[cfg(debug_assertions)]
                                            {
                                                if span_id != 0 {
                                                    panic!("Unregistered span ID: {event:?}");
                                                }
                                            }
                                        }
                                    }

                                    Arc::new(event)
                                }
                            };

                            // Send to subscribers
                            for subscriber in self.subscribers.iter_mut() {
                                subscriber.push_event(event_id, event.clone());
                            }
                        }
                        Ok(None) => {
                            break;
                        }
                        Err(_) => {
                            closed_rxs.push(rx_idx); // Channel is closed, remove.
                            break;
                        }
                    }
                }
            }

            if do_continue {
                // Remove closed receivers (should be rare in Tokio)
                if !closed_rxs.is_empty() {
                    let mut receivers = Vec::with_capacity(self.receivers.len() - closed_rxs.len());
                    for (rx_idx, rx) in self.receivers.drain(..).enumerate() {
                        if !closed_rxs.contains(&rx_idx) {
                            receivers.push(rx);
                        }
                    }
                    self.receivers = receivers;
                }

                // Send batched events
                if !self.subscribers.is_empty() {
                    self.subscribers
                        .retain_mut(|subscriber| subscriber.send_batch().is_ok());
                }
            }
        }

        // Send remaining events
        for mut subscriber in self.subscribers.drain(..) {
            let _ = subscriber.send_batch();
        }
    }

    fn update(&mut self) -> bool {
        for update in COLLECTOR_UPDATES.lock().drain(..) {
            match update {
                Update::RegisterReceiver { receiver } => {
                    self.receivers.push(receiver);
                }
                Update::RegisterSubscriber { subscriber } => {
                    ACTIVE_SUBSCRIBERS.lock().push(subscriber.id.clone());
                    self.subscribers.push(subscriber);
                }
                Update::UnregisterSubscriber { id } => {
                    ACTIVE_SUBSCRIBERS.lock().retain(|s| s != &id);
                    self.subscribers.retain(|s| s.id != id);
                }
                Update::UpdateSubscriber {
                    id,
                    interests,
                    lossy,
                } => {
                    for subscriber in self.subscribers.iter_mut() {
                        if subscriber.id == id {
                            subscriber.interests = interests;
                            subscriber.lossy = lossy;
                            break;
                        }
                    }
                }
                Update::UpdateLevels { levels } => {
                    for event in EVENT_TYPES.iter() {
                        let event_id = event.id();
                        if let Some(level) = levels.get(event) {
                            self.levels[event_id] = *level;
                        } else {
                            self.levels[event_id] = event.level();
                        }
                    }
                }
                Update::Shutdown => return false,
            }
        }

        true
    }

    pub fn set_interests(mut interests: Interests) {
        if !interests.is_empty() {
            for event_type in [
                EventType::Network(NetworkEvent::ConnectionStart),
                EventType::Network(NetworkEvent::ConnectionEnd),
                EventType::Delivery(DeliveryEvent::AttemptStart),
                EventType::Delivery(DeliveryEvent::AttemptEnd),
            ] {
                interests.set(event_type);
            }
        }

        TRACE_INTERESTS.update(interests);
    }

    pub fn union_interests(interests: Interests) {
        TRACE_INTERESTS.union(interests);
    }

    #[inline(always)]
    pub fn has_interest(event: impl Into<usize>) -> bool {
        TRACE_INTERESTS.get(event)
    }

    pub fn get_subscribers() -> Vec<String> {
        ACTIVE_SUBSCRIBERS.lock().clone()
    }

    pub fn update_custom_levels(levels: AHashMap<EventType, Level>) {
        COLLECTOR_UPDATES
            .lock()
            .push(Update::UpdateLevels { levels });
    }

    pub fn update_subscriber(id: String, interests: Interests, lossy: bool) {
        COLLECTOR_UPDATES.lock().push(Update::UpdateSubscriber {
            id,
            interests,
            lossy,
        });
    }

    pub fn remove_subscriber(id: String) {
        COLLECTOR_UPDATES
            .lock()
            .push(Update::UnregisterSubscriber { id });
    }

    pub fn shutdown() {
        COLLECTOR_UPDATES.lock().push(Update::Shutdown);
        Collector::reload();
    }

    pub fn is_enabled() -> bool {
        !TRACE_INTERESTS.is_empty()
    }

    pub fn reload() {
        CHANNEL_FLAGS.fetch_or(CHANNEL_UPDATE_MARKER, Ordering::Relaxed);
        COLLECTOR_THREAD.thread().unpark();
    }
}

impl Default for Collector {
    fn default() -> Self {
        let mut c = Collector {
            subscribers: Vec::new(),
            levels: [Level::Disable; TOTAL_EVENT_COUNT],
            active_spans: AHashMap::new(),
            receivers: Vec::new(),
        };

        for event in EVENT_TYPES.iter() {
            let event_id = event.id();
            c.levels[event_id] = event.level();
        }

        c
    }
}
