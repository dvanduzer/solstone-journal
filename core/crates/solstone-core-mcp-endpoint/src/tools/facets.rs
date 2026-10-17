// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use serde::Deserialize;
use serde_json::Value;

use super::{MAX_OPAQUE_REFERENCE_BYTES, ToolError, search::MAX_LIMIT};

pub(crate) struct ValidatedListFacets {
    pub(crate) limit: usize,
    pub(crate) cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Params {
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    cursor: Option<String>,
}

const fn default_limit() -> usize {
    100
}

pub(crate) fn validate(params: Option<&Value>) -> Result<ValidatedListFacets, ToolError> {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let params = serde_json::from_value::<Params>(params).map_err(|_| ToolError::InvalidInput)?;
    if !(1..=MAX_LIMIT).contains(&params.limit) {
        return Err(ToolError::InvalidInput);
    }
    if params
        .cursor
        .as_ref()
        .is_some_and(|value| value.is_empty() || value.len() > MAX_OPAQUE_REFERENCE_BYTES)
    {
        return Err(ToolError::InvalidInput);
    }
    Ok(ValidatedListFacets {
        limit: params.limit,
        cursor: params.cursor,
    })
}
