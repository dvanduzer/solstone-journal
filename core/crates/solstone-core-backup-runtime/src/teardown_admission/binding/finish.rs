// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Original local guards across the consuming native carrier boundary.

use super::*;

/// Keeps the actual local transition and both guards while the owning coordinator
/// settles and retires its carrier. It supplies no remote/cooperation authority.
pub struct TeardownFinishAdmission {
    lease: Arc<TeardownLease>,
    configuration: TeardownConfiguration,
    /// None for own storage, which never had a hosted binding.
    binding: Option<BindingFinalization>,
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
                binding: Some(binding),
                publication,
            },
            carrier,
        ))
    }
}

impl ClearedTeardownAdmission {
    /// Own storage carries no hosted binding to remove. Finish directly, once the
    /// configuration captured without one is still the published clear.
    pub fn into_finish_without_binding(
        self,
        reader: &mut TeardownReader<'_>,
    ) -> Result<(TeardownFinishAdmission, Arc<RootTeardownCarrier>), TeardownBindingFailure> {
        if self.configuration().binding().is_some() {
            return Err(TeardownBindingFailure::cleared(
                self,
                TeardownBindingRefusal::Binding(BindingFinalizationRefusal::BindingChanged),
            ));
        }
        if let Err(refusal) = self.check(reader) {
            return Err(TeardownBindingFailure::cleared(
                self,
                TeardownBindingRefusal::Admission(refusal),
            ));
        }
        if let Err(refusal) = self
            .configuration()
            .revalidate_published_clear(self.publication())
        {
            return Err(TeardownBindingFailure::cleared(
                self,
                TeardownBindingRefusal::Binding(BindingFinalizationRefusal::Configuration(refusal)),
            ));
        }
        let ClearedTeardownAdmission {
            admission,
            publication,
        } = self;
        let CarrierTeardownAdmission { admission, carrier } = admission;
        let TeardownAdmission {
            lease,
            configuration,
        } = admission;
        Ok((
            TeardownFinishAdmission {
                lease,
                configuration,
                binding: None,
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
    /// The confirmed removal, or None for own storage.
    pub fn binding_receipt(&self) -> Option<&Arc<BindingRemovalReceipt>> {
        self.binding
            .as_ref()
            .map(|binding| binding.receipt().expect("confirmed binding transition"))
    }

    pub fn revalidate(&self) -> Result<(), TeardownBindingRefusal> {
        self.lease
            .revalidate()
            .map_err(|error| TeardownBindingRefusal::Admission(activity_refusal(error)))?;
        match &self.binding {
            Some(binding) => binding
                .revalidate_removed()
                .map_err(TeardownBindingRefusal::Binding)?,
            None => self
                .configuration
                .revalidate_published_clear(&self.publication)
                .map_err(|refusal| {
                    TeardownBindingRefusal::Binding(BindingFinalizationRefusal::Configuration(
                        refusal,
                    ))
                })?,
        }
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
