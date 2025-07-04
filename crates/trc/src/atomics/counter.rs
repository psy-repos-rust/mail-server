/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

use std::sync::atomic::{AtomicU64, Ordering};

pub struct AtomicCounter {
    id: &'static str,
    description: &'static str,
    unit: &'static str,
    value: AtomicU64,
}

impl AtomicCounter {
    pub const fn new(id: &'static str, description: &'static str, unit: &'static str) -> Self {
        Self {
            id,
            description,
            unit,
            value: AtomicU64::new(0),
        }
    }

    #[inline(always)]
    pub fn increment(&self) {
        self.value.fetch_add(1, Ordering::Relaxed);
    }

    #[inline(always)]
    pub fn increment_by(&self, value: u64) {
        self.value.fetch_add(value, Ordering::Relaxed);
    }

    #[inline(always)]
    pub fn decrement(&self) {
        self.value.fetch_sub(1, Ordering::Relaxed);
    }

    #[inline(always)]
    pub fn decrement_by(&self, value: u64) {
        self.value.fetch_sub(value, Ordering::Relaxed);
    }

    #[inline(always)]
    pub fn get(&self) -> u64 {
        self.value.load(Ordering::Relaxed)
    }

    pub fn id(&self) -> &'static str {
        self.id
    }

    pub fn description(&self) -> &'static str {
        self.description
    }

    pub fn unit(&self) -> &'static str {
        self.unit
    }

    pub fn is_active(&self) -> bool {
        self.value.load(Ordering::Relaxed) > 0
    }
}
