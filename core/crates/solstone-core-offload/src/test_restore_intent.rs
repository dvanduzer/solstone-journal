// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Private DATA fixtures captured from the predecessor owning encoder.

#![allow(clippy::unwrap_used)]

use solstone_core_retention::Register;
use solstone_core_retention::restore_intent::RestoreIntent;

pub(crate) fn fixture(with_mark: bool) -> RestoreIntent {
    let bytes: &[u8] = if with_mark {
        include_bytes!("../../solstone-core-retention/src/restore_intent/fixtures/with-mark.json")
    } else {
        include_bytes!(
            "../../solstone-core-retention/src/restore_intent/fixtures/without-mark.json"
        )
    };
    RestoreIntent::decode(Ok(bytes)).unwrap()
}

pub fn register(intent: &RestoreIntent) -> Register {
    let mut register = Register::empty();
    if let Some(mark) = intent.retirement.expected() {
        register.version = 2;
        register.marks.insert(mark.id.clone(), mark.clone());
    }
    register
}
