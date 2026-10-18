// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! One historical segment, without activity or daily work.

use std::time::Duration;

use crate::{
    args::ThinkArgs, context::ThinkContext, dispatch::ModeResult, run_log::RunLogWriter, segment,
};

pub(crate) fn run(
    context: &ThinkContext,
    log: &mut RunLogWriter,
    args: &ThinkArgs,
    timeout: Option<Duration>,
    child_environment: &solstone_core_system::process::ChildLaunchContext,
) -> Result<ModeResult, String> {
    let segment = args.segment.as_deref().ok_or("missing repair segment")?;
    let request = solstone_core_sense::batch::BatchRequest {
        day: context.day.clone(),
        jobs: args.jobs,
        reprocess: None,
        segment: Some(segment.to_owned()),
        stream: args.stream.clone(),
        dry_run: false,
        verbose: args.verbose,
        debug: args.debug,
    };
    if let Some(executor) = &context.sense_executor {
        solstone_core_sense::batch::run_batch_with_executor(
            &context.journal,
            &request,
            child_environment,
            Some(Duration::from_secs(1800)),
            std::sync::Arc::clone(executor),
        )
    } else {
        solstone_core_sense::batch::run_batch_with_environment_and_timeout(
            &context.journal,
            &request,
            child_environment,
            Some(Duration::from_secs(1800)),
        )
    }
    .map_err(|error| error.to_string())?;
    if (context.cancel_checkpoint)() {
        return Err("thinking cancelled".into());
    }
    // Thinking that is already current is skipped, so a segment owed only its
    // publication is published without a model call.
    segment::run_with(
        context,
        log,
        segment,
        false,
        args.stream.as_deref(),
        args.jobs,
        timeout,
        false,
        &[],
        segment::CurrentSegment::Skip,
    )
}
