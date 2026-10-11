// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Original local guards across the consuming native carrier boundary.

use super::*;

/// Keeps the actual local transition and both guards while the owning coordinator
/// settles and retires its carrier. It supplies no remote/cooperation authority.
pub struct TeardownFinishAdmission {
    lease: Arc<TeardownLease>,
    configuration: TeardownConfiguration,
    binding: BindingFinalization,
    publication: Arc<BoundConfigPublication>,
}

impl BoundTeardownAdmission {
    /// Release only the operational carrier share. The returned guards must stay
    /// alive across retirement and its final checks. Arc uniqueness is not proof
    /// of settlement or permission to retire a reservation.
    pub fn into_finish(
        self,
        reader: &mut TeardownReader<'_>,
    ) -> Result<(TeardownFinishAdmission, Arc<RootTeardownCarrier>), TeardownBindingFailure> {
        if let Err(refusal) = self.check(reader) {
            return Err(TeardownBindingFailure::guarded(self.guarded, refusal));
        }
        let BindingTeardownAdmission { cleared, binding } = self.guarded;
        let ClearedTeardownAdmission {
            admission,
            publication,
        } = cleared;
        let CarrierTeardownAdmission { admission, carrier } = admission;
        let TeardownAdmission {
            lease,
            configuration,
        } = admission;
        Ok((
            TeardownFinishAdmission {
                lease,
                configuration,
                binding,
                publication,
            },
            carrier,
        ))
    }
}

impl TeardownFinishAdmission {
    pub fn journal_root(&self) -> &Arc<JournalRoot> {
        self.lease.journal_root()
    }
    pub fn teardown_lease(&self) -> &Arc<TeardownLease> {
        &self.lease
    }
    pub fn publication(&self) -> &Arc<BoundConfigPublication> {
        &self.publication
    }
    pub fn binding_receipt(&self) -> &Arc<BindingRemovalReceipt> {
        self.binding
            .receipt()
            .expect("confirmed binding transition")
    }

    pub fn revalidate(&self) -> Result<(), TeardownBindingRefusal> {
        self.lease
            .revalidate()
            .map_err(|error| TeardownBindingRefusal::Admission(activity_refusal(error)))?;
        self.binding
            .revalidate_removed()
            .map_err(TeardownBindingRefusal::Binding)?;
        self.lease
            .revalidate()
            .map_err(|error| TeardownBindingRefusal::Admission(activity_refusal(error)))
    }

    /// Exclude only the actual original live carrier before its consuming call.
    pub fn check_before_retirement(
        &self,
        carrier: &RootTeardownCarrier,
        reader: &mut TeardownReader<'_>,
    ) -> Result<(), TeardownBindingRefusal> {
        let validate = || {
            self.revalidate()?;
            if !Arc::ptr_eq(carrier.teardown(), &self.lease)
                || !Arc::ptr_eq(carrier.journal_root(), self.journal_root())
            {
                return Err(TeardownBindingRefusal::Admission(
                    TeardownAdmissionRefusal::CarrierUnavailable,
                ));
            }
            carrier.revalidate().map_err(|_| {
                TeardownBindingRefusal::Admission(TeardownAdmissionRefusal::CarrierUnavailable)
            })?;
            self.revalidate()
        };
        validate()?;
        let result = reader(
            self.journal_root(),
            &self.configuration,
            Some(carrier.observation()),
        );
        validate()?;
        result.map_err(|refusal| {
            TeardownBindingRefusal::Admission(TeardownAdmissionRefusal::Reader(refusal))
        })
    }

    /// After an actual retirement return, require fresh complete coverage with
    /// no carrier exclusion. Earlier native receipt facts remain independent.
    pub fn check_after_retirement(
        &self,
        reader: &mut TeardownReader<'_>,
    ) -> Result<(), TeardownBindingRefusal> {
        self.revalidate()?;
        let result = reader(self.journal_root(), &self.configuration, None);
        self.revalidate()?;
        result.map_err(|refusal| {
            TeardownBindingRefusal::Admission(TeardownAdmissionRefusal::Reader(refusal))
        })
    }
}
