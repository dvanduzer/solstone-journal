// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FreezePoint {
    Before,
    MarkerSampled,
    After,
}

struct Barrier {
    point: FreezePoint,
    ordinal: usize,
    callback: Box<dyn FnOnce()>,
}

struct TraceState {
    attempted_before: usize,
    attempted_marker_sampled: usize,
    attempted_after: usize,
    barriers: Vec<Barrier>,
    barriers_fired: usize,
}

thread_local! {
    static TRACE: std::cell::RefCell<Option<TraceState>> = const {
        std::cell::RefCell::new(None)
    };
}

pub(crate) fn checkpoint(point: FreezePoint) {
    let callback = TRACE.with(|cell| {
        let mut borrowed = cell.borrow_mut();
        let state = borrowed.as_mut()?;
        let ordinal = match point {
            FreezePoint::Before => {
                state.attempted_before += 1;
                state.attempted_before
            }
            FreezePoint::MarkerSampled => {
                state.attempted_marker_sampled += 1;
                state.attempted_marker_sampled
            }
            FreezePoint::After => {
                state.attempted_after += 1;
                state.attempted_after
            }
        };
        if let Some(pos) = state
            .barriers
            .iter()
            .position(|b| b.point == point && b.ordinal == ordinal)
        {
            state.barriers_fired += 1;
            Some(state.barriers.remove(pos).callback)
        } else {
            None
        }
    });
    if let Some(cb) = callback {
        cb();
    }
}

#[allow(dead_code)]
pub(crate) fn run_with_source_freeze_barrier<T>(
    point: FreezePoint,
    ordinal: usize,
    callback: impl FnOnce() + 'static,
    op: impl FnOnce() -> T,
) -> (T, bool) {
    TRACE.with(|cell| {
        assert!(
            cell.borrow().is_none(),
            "freeze barrier trace already active"
        );
        *cell.borrow_mut() = Some(TraceState {
            attempted_before: 0,
            attempted_marker_sampled: 0,
            attempted_after: 0,
            barriers: vec![Barrier {
                point,
                ordinal,
                callback: Box::new(callback),
            }],
            barriers_fired: 0,
        });
    });
    let result = op();
    let fired = TRACE.with(|cell| {
        let state = cell.borrow_mut().take().expect("trace active");
        state.barriers_fired > 0
    });
    (result, fired)
}
