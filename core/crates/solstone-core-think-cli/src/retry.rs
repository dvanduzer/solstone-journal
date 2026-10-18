// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Execute the selected failed unit. Permission is consumed by its owning
//! writer, never by discovery or this process-launch boundary.
use crate::{context::ThinkContext, dispatch::ModeResult, run_log::RunLogWriter};
use solstone_core_system::work_retry::{self, Unit};
use std::time::Duration;

pub(crate) fn run(
    context: &ThinkContext,
    log: &mut RunLogWriter,
    id: &str,
    cause: &str,
    environment: &solstone_core_system::process::ChildLaunchContext,
) -> Result<ModeResult, String> {
    let record = work_retry::read(&context.journal, id)?.ok_or("retry evidence is missing")?;
    if record.unit.day() != context.day
        && !matches!(&record.unit, Unit::Daily { talent, .. } if talent == "daily_schedule")
    {
        return Err("retry day does not match".into());
    }
    match &record.unit {
        Unit::Raw { output, .. } => {
            let output = record
                .unit
                .segment_directory(&context.journal)?
                .join(output);
            let evidence = work_retry::raw::inspect_output(&context.journal, &output)?
                .ok_or("native output is no longer held")?;
            let attempt = record
                .permission(&evidence.input, work_retry::DECLARED_FIXES)
                .filter(|a| a.cause == cause && a.reason == evidence.reason)
                .ok_or("retry permission changed")?;
            let execution = solstone_core_sense::batch::retry_file(
                &context.journal,
                &record.unit,
                &attempt,
                environment,
                Duration::from_secs(1800),
                context.sense_executor.clone(),
            )
            .map_err(|e| e.to_string());
            let terminal = work_retry::raw::finish_output_checked(
                &context.journal,
                &record.unit,
                &attempt,
                &|| {
                    if (context.cancel_checkpoint)() {
                        Err("thinking cancelled".into())
                    } else {
                        Ok(())
                    }
                },
            );
            execution?;
            terminal?;
            Ok(ModeResult::default())
        }
        Unit::Daily { .. } => {
            let input = work_retry::daily::observe_with_roots_checked(
                &context.journal,
                &record.unit,
                &context.talent_root,
                &context.apps_root,
                &|| context.checkpoint(),
            )?
            .ok_or("daily unit is no longer held")?;
            let attempt = work_retry::read(&context.journal, id)?
                .ok_or("missing daily permission")?
                .permission(&input, work_retry::DECLARED_FIXES)
                .filter(|a| a.cause == cause)
                .ok_or("daily retry permission changed")?;
            let mut retry_context = context.clone();
            retry_context.retry = Some((record.unit.clone(), attempt.clone()));
            let result = crate::daily::run(&retry_context, log, None, false, 1)?;
            let current =
                work_retry::read(&context.journal, id)?.ok_or("missing daily permission")?;
            if current.attempt.as_ref() != Some(&attempt) && current.held {
                return Err("daily writer did not consume retry permission".into());
            }
            let daily = solstone_core_journal_io::load_daily_unit_record(
                &context.journal,
                &work_retry::daily::identity(&record.unit)?,
            )
            .map_err(|e| e.to_string())?
            .ok_or("missing daily terminal record")?;
            if daily.status == solstone_core_journal_io::DailyUnitStatus::Unfinished {
                return Err("daily retry is still unresolved".into());
            }
            let remaining = (!daily.status.is_terminal_success())
                .then(|| daily.reason_code.as_deref().unwrap_or("daily_failed"));
            work_retry::finish_checked(
                &context.journal,
                &record.unit,
                &attempt,
                remaining,
                &|| context.checkpoint(),
            )?;
            Ok(result)
        }
        Unit::Activity {
            facet, activity, ..
        } => {
            let input = crate::activity_work::activity_held_input(&context.journal, &record.unit)?
                .ok_or("activity is no longer set aside")?;
            let attempt = record
                .permission(&input, work_retry::DECLARED_FIXES)
                .filter(|a| a.cause == cause)
                .ok_or("activity retry permission changed")?;
            let mut retry_context = context.clone();
            retry_context.retry = Some((record.unit.clone(), attempt));
            crate::activity::run(&retry_context, log, activity, facet, false, false, 1)
        }
        Unit::Segment {
            segment,
            stream,
            talent,
            ..
        } => {
            let input = work_retry::input::current(&context.journal, &record.unit)?;
            let attempt = record
                .permission(&input, work_retry::DECLARED_FIXES)
                .filter(|a| a.cause == cause)
                .ok_or("segment retry permission changed")?;
            let mut retry_context = context.clone();
            retry_context.retry = Some((record.unit.clone(), attempt));
            crate::segment::retry_talent(&retry_context, log, segment, stream, talent)
        }
    }
}
