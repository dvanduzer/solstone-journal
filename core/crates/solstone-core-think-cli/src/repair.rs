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
    solstone_core_sense::batch::run_batch_with_environment_and_timeout(
        &context.journal,
        &request,
        child_environment,
        Some(Duration::from_secs(1800)),
    )
    .map_err(|error| error.to_string())?;
    let result = segment::run_with(
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
    )?;
    if result.failed != 0 {
        return Ok(result);
    }
    let directory = solstone_core_system_health::find_segment_dir(
        &context.journal,
        &context.day,
        segment,
        args.stream.as_deref(),
    )
    .ok_or("repair segment no longer exists")?;
    let before = solstone_core_system_health::newest_segment_input_ms(&directory)
        .map_err(|error| error.to_string())?;
    solstone_core_system::segment_publication::publish(
        &context.journal,
        &directory,
        before,
        |journal, path| context.index.rescan_file(journal, path),
    )?;
    if before
        != solstone_core_system_health::newest_segment_input_ms(&directory)
            .map_err(|error| error.to_string())?
    {
        return Err("segment input changed during publication".into());
    }
    Ok(result)
}
