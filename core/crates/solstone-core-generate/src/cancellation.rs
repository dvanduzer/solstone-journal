// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

pub struct Cancellation<'a> {
    check: Option<&'a (dyn Fn() -> bool + Send + Sync)>,
}

impl<'a> Cancellation<'a> {
    pub const fn never() -> Cancellation<'static> {
        Cancellation { check: None }
    }

    pub fn from_fn(check: &'a (dyn Fn() -> bool + Send + Sync)) -> Self {
        Cancellation { check: Some(check) }
    }

    pub fn is_cancelled(&self) -> bool {
        self.check.is_some_and(|f| f())
    }
}
