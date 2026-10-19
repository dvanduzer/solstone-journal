// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Local half of the iOS after-clear restart handoff. Native custody is proved
//! separately by the actual locked destination; descriptive records cannot mint it.

use solstone_core_backup::teardown_configuration::TeardownConfiguration;
use solstone_core_backup::teardown_recovery::retained::Store;
use solstone_core_journal_config_write::{BoundConfigPublication, DirectoryFlush};
use solstone_core_journal_io::media::RootTeardownCarrier;
use solstone_core_journal_io::media::teardown_preparation::TeardownPreparation;

pub(super) fn confirm_local_custody(
    store: &Store,
    preparation: &TeardownPreparation,
    carrier: &RootTeardownCarrier,
    configuration: &TeardownConfiguration,
    publication: &BoundConfigPublication,
) -> Result<(), ()> {
    store.revalidate().map_err(|_| ())?;
    let saved = store.record();
    if !saved.remote_settled
        || saved.remote_facts.is_none()
        || saved.binding_removed
        || saved.native_retired
        || saved.carrier_retired
        || publication.directory_flush != DirectoryFlush::Synced
        || saved.preparation != *preparation.observation()
        || saved.clear.as_ref()
            != Some(
                &configuration
                    .published_clear_observation(publication)
                    .map_err(|_| ())?,
            )
        || saved.carrier != carrier.recovery_observation().map_err(|_| ())?
    {
        return Err(());
    }
    preparation.revalidate().map_err(|_| ())?;
    preparation.published_directory().map_err(|_| ())?;
    if preparation.marker().map_err(|_| ())?.is_some() {
        return Err(());
    }
    store.confirm_carrier_ready().map_err(|_| ())?;
    store.revalidate().map_err(|_| ())
}

#[cfg(all(test, feature = "full-tests"))]
mod tests;
