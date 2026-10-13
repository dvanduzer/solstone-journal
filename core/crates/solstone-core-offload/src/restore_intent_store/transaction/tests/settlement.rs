// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use super::*;

#[test]
fn actual_publication_and_no_effect_move_receipts_after_confirmed_cleanup() {
    for case in 0..3 {
        let f = Fixture::new(false);
        if case != 0 {
            fs::write(
                f.raw(),
                if case == 1 {
                    b"abc".as_slice()
                } else {
                    b"owner bytes".as_slice()
                },
            )
            .unwrap();
        }
        let allowed = Cell::new(true);
        let staged = f.staged(&allowed);
        let carrier = Arc::clone(&staged.owner.carrier);
        let mut detached = staged
            .commit(2, options())
            .retire_intent(options())
            .into_owned();
        assert!(detached.completed(), "{:?}", detached.facts().hold);
        assert!(
            detached
                .qualify_settlement(
                    &carrier,
                    &f.witness,
                    detached.disposition(),
                    detached.effects()
                )
                .is_ok()
        );
        assert!(detached.facts().settlement_cleanup.is_none());
        assert!(detached.cleanup_settlement().is_ok());
        assert!(detached.revalidate_settlement(&carrier).is_ok());
        let settled = detached.into_settled();
        let facts = settled.facts();
        let cleanup = facts.settlement_cleanup.unwrap();
        assert_eq!(cleanup.cleanup, Cleanup::Complete);
        #[cfg(unix)]
        assert!(cleanup.parent_sync.unwrap().is_ok());
        #[cfg(windows)]
        assert_eq!(
            cleanup.parent_sync.unwrap().unwrap_err().kind(),
            std::io::ErrorKind::Unsupported
        );
        assert!(!facts.retained_media);
        assert!(!facts.store_exclusion_retained);
        assert!(
            facts
                .retirement
                .as_ref()
                .unwrap()
                .removal
                .unwrap()
                .failures
                .is_empty()
        );
        assert_eq!(
            facts.publication.unwrap().effect,
            match case {
                0 => PublicationEffect::Published,
                1 => PublicationEffect::AlreadyPresent,
                _ => PublicationEffect::Conflict,
            }
        );
        assert_eq!(
            fs::read(f.raw()).unwrap(),
            if case == 2 {
                b"owner bytes".as_slice()
            } else {
                b"abc".as_slice()
            }
        );
    }
}

#[test]
fn settlement_parent_sync_failure_and_pending_intent_retain_original_owners() {
    let f = Fixture::new(false);
    let allowed = Cell::new(true);
    let staged = f.staged(&allowed);
    let carrier = Arc::clone(&staged.owner.carrier);
    let mut detached = staged
        .commit(2, options())
        .retire_intent(options())
        .into_owned();
    assert!(
        detached
            .qualify_settlement(
                &carrier,
                &f.witness,
                detached.disposition(),
                detached.effects()
            )
            .is_ok()
    );
    let (result, hit) = run_with_media_hook(
        MediaPrimitive::ParentSync,
        || Err(std::io::Error::from_raw_os_error(13)),
        || detached.cleanup_settlement(),
    );
    assert!(hit);
    assert_eq!(
        result.err().unwrap().reason,
        "settlement_cleanup_unconfirmed"
    );
    let cleanup = detached.facts().settlement_cleanup.unwrap();
    assert_eq!(cleanup.cleanup, Cleanup::Complete);
    assert_eq!(
        cleanup
            .parent_sync
            .unwrap()
            .as_ref()
            .unwrap_err()
            .raw_os_error(),
        Some(13)
    );
    assert!(detached.facts().retained_media);
    assert!(carrier.revalidate().is_ok());
    assert!(detached.cleanup_settlement().is_err());

    let pending = Fixture::new(true);
    let staged = pending.staged(&allowed);
    let carrier = Arc::clone(&staged.owner.carrier);
    let detached = staged
        .commit(2, options())
        .retire_intent(options())
        .into_owned();
    assert!(!detached.completed());
    assert!(
        detached
            .qualify_settlement(
                &carrier,
                &pending.witness,
                detached.disposition(),
                detached.effects()
            )
            .is_err()
    );
    assert!(detached.facts().settlement_cleanup.is_none());
    assert!(detached.facts().store_exclusion_retained);
    assert!(detached.facts().retained_media);
}
