// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! iOS confidential GPU appraisal helper backed by the pinned local nvat SDK.

use libc::c_char;
#[cfg(target_os = "ios")]
use libc::c_void;
use serde::Deserialize;
use serde_json::{Value, json};
use std::ffi::{CStr, CString};
#[cfg(target_os = "ios")]
use std::ptr::null_mut;

#[cfg(target_os = "ios")]
const NVAT_RC_OK: u16 = 0;
#[cfg(target_os = "ios")]
const NVAT_RC_INTERNAL_ERROR: u16 = 2;
const NVAT_RC_NOT_INITIALIZED: u16 = 3;
const NVAT_RC_BAD_ARGUMENT: u16 = 4;
#[cfg(target_os = "ios")]
const NVAT_RC_OVERALL_RESULT_FALSE: u16 = 12;
#[cfg(target_os = "ios")]
const NVAT_GPU_CLAIMS_VERSION_V3: u8 = 3;
#[cfg(target_os = "ios")]
const MAX_SDK_JSON_BYTES: usize = 4 * 1024 * 1024;
#[cfg(target_os = "ios")]
const MAX_PROOFS_BYTES: usize = 16_384;

#[cfg(target_os = "ios")]
type Handle = *mut c_void;
#[cfg(target_os = "ios")]
type EvidenceArray = *mut Handle;

#[cfg(target_os = "ios")]
unsafe extern "C" {
    fn pthread_main_np() -> libc::c_int;
    fn nvat_rc_to_string(rc: u16) -> *const c_char;
    fn nvat_sdk_init(opts: Handle) -> u16;
    fn nvat_str_length(value: Handle, out_length: *mut usize) -> u16;
    fn nvat_str_get_data(value: Handle, out_data: *mut *mut c_char) -> u16;
    fn nvat_str_free(value: *mut Handle);
    fn nvat_ocsp_client_create_raw_proofs(
        out_client: *mut Handle,
        bundle_der: *const u8,
        bundle_len: usize,
        verification_time_unix: i64,
    ) -> u16;
    fn nvat_ocsp_client_free(client: *mut Handle);
    fn nvat_rim_store_create_filesystem(out_store: *mut Handle, base_path: *const c_char) -> u16;
    fn nvat_rim_store_free(store: *mut Handle);
    fn nvat_nonce_from_bytes(out_nonce: *mut Handle, bytes: *const c_char, length: usize) -> u16;
    fn nvat_nonce_free(nonce: *mut Handle);
    fn nvat_gpu_evidence_source_from_json_string(
        out_source: *mut Handle,
        evidence_json: *const c_char,
    ) -> u16;
    fn nvat_gpu_evidence_source_free(source: *mut Handle);
    fn nvat_gpu_evidence_collect(
        source: Handle,
        nonce: Handle,
        out_evidence: *mut EvidenceArray,
        out_count: *mut usize,
    ) -> u16;
    fn nvat_gpu_evidence_array_free(evidence: *mut EvidenceArray, count: usize);
    fn nvat_evidence_policy_create_default(out_policy: *mut Handle) -> u16;
    fn nvat_evidence_policy_set_gpu_claims_version(policy: Handle, version: u8) -> u16;
    fn nvat_evidence_policy_free(policy: *mut Handle);
    fn nvat_gpu_local_verifier_create(
        out_verifier: *mut Handle,
        rim_store: Handle,
        ocsp_client: Handle,
        detached_eat_options: Handle,
    ) -> u16;
    fn nvat_gpu_local_verifier_upcast(verifier: Handle) -> Handle;
    fn nvat_gpu_verifier_free(verifier: *mut Handle);
    fn nvat_verify_gpu_evidence(
        verifier: Handle,
        evidence: *const Handle,
        count: usize,
        policy: Handle,
        out_detached_eat: *mut Handle,
        out_claims: *mut Handle,
    ) -> u16;
    fn nvat_claims_collection_serialize_json(claims: Handle, out_json: *mut Handle) -> u16;
    fn nvat_claims_collection_free(claims: *mut Handle);
}

#[derive(Deserialize)]
#[cfg_attr(not(target_os = "ios"), allow(dead_code))]
struct AppraisalRequestPayload {
    profile_fingerprint: String,
    nonce_hex: String,
    evidence_json: Value,
    proofs_hex: String,
    verification_time_unix: i64,
    budget_ms: u64,
    bridge_attempt: u64,
}

fn response(returncode: i32, result_code: u16, result_message: &str) -> Value {
    json!({
        "returncode": returncode,
        "result_code": result_code,
        "result_message": result_message,
        "claims": [],
        "detached_eat": [],
    })
}

struct AppraisalFailure {
    rc: u16,
    stage: &'static str,
}

fn failure(rc: u16, stage: &'static str) -> AppraisalFailure {
    AppraisalFailure { rc, stage }
}

fn encode_response(value: &Value) -> *mut c_char {
    CString::new(value.to_string())
        .unwrap_or_else(|_| CString::new("{\"returncode\":1,\"result_code\":2,\"result_message\":\"serialization failed\",\"claims\":[],\"detached_eat\":[]}").expect("literal"))
        .into_raw()
}

#[cfg(any(target_os = "ios", test))]
fn decode_hex(value: &str, cap: usize) -> Option<Vec<u8>> {
    if value.len() & 1 != 0 || value.len() / 2 > cap {
        return None;
    }
    let mut out = Vec::with_capacity(value.len() / 2);
    for pair in value.as_bytes().chunks_exact(2) {
        let high = (pair[0] as char).to_digit(16)?;
        let low = (pair[1] as char).to_digit(16)?;
        out.push(((high << 4) | low) as u8);
    }
    Some(out)
}

#[cfg(target_os = "ios")]
fn sdk_message(rc: u16) -> String {
    let ptr = unsafe { nvat_rc_to_string(rc) };
    if ptr.is_null() {
        format!("nvat error {rc}")
    } else {
        unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned()
    }
}

#[cfg(not(target_os = "ios"))]
fn sdk_message(rc: u16) -> String {
    format!("nvat unavailable ({rc})")
}

#[cfg(target_os = "ios")]
static SDK_INIT_RESULT: std::sync::OnceLock<u16> = std::sync::OnceLock::new();

#[cfg(target_os = "ios")]
#[unsafe(no_mangle)]
pub extern "C" fn ios_gpu_appraisal_initialize(budget_ms: u64) -> u16 {
    if budget_ms == 0 || budget_ms > 60_000 || unsafe { pthread_main_np() } != 1 {
        return NVAT_RC_BAD_ARGUMENT;
    }
    *SDK_INIT_RESULT.get_or_init(|| {
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(budget_ms));
            unsafe { libc::_exit(124) };
        });
        unsafe { nvat_sdk_init(null_mut()) }
    })
}

#[cfg(not(target_os = "ios"))]
#[unsafe(no_mangle)]
pub extern "C" fn ios_gpu_appraisal_initialize(_budget_ms: u64) -> u16 {
    NVAT_RC_NOT_INITIALIZED
}

#[cfg(target_os = "ios")]
struct NativeHandles {
    source: Handle,
    nonce: Handle,
    evidence: EvidenceArray,
    evidence_count: usize,
    store: Handle,
    ocsp: Handle,
    verifier: Handle,
    policy: Handle,
    detached: Handle,
    claims: Handle,
    serialized_claims: Handle,
}

#[cfg(target_os = "ios")]
impl NativeHandles {
    fn new() -> Self {
        Self {
            source: null_mut(),
            nonce: null_mut(),
            evidence: null_mut(),
            evidence_count: 0,
            store: null_mut(),
            ocsp: null_mut(),
            verifier: null_mut(),
            policy: null_mut(),
            detached: null_mut(),
            claims: null_mut(),
            serialized_claims: null_mut(),
        }
    }
}

#[cfg(target_os = "ios")]
impl Drop for NativeHandles {
    fn drop(&mut self) {
        unsafe {
            if !self.serialized_claims.is_null() {
                nvat_str_free(&raw mut self.serialized_claims);
            }
            if !self.detached.is_null() {
                nvat_str_free(&raw mut self.detached);
            }
            if !self.claims.is_null() {
                nvat_claims_collection_free(&raw mut self.claims);
            }
            if !self.policy.is_null() {
                nvat_evidence_policy_free(&raw mut self.policy);
            }
            if !self.verifier.is_null() {
                nvat_gpu_verifier_free(&raw mut self.verifier);
            }
            if !self.ocsp.is_null() {
                nvat_ocsp_client_free(&raw mut self.ocsp);
            }
            if !self.store.is_null() {
                nvat_rim_store_free(&raw mut self.store);
            }
            if !self.evidence.is_null() {
                nvat_gpu_evidence_array_free(&raw mut self.evidence, self.evidence_count);
            }
            if !self.nonce.is_null() {
                nvat_nonce_free(&raw mut self.nonce);
            }
            if !self.source.is_null() {
                nvat_gpu_evidence_source_free(&raw mut self.source);
            }
        }
    }
}

#[cfg(target_os = "ios")]
unsafe fn sdk_json(value: Handle) -> Result<Value, u16> {
    if value.is_null() {
        return Err(NVAT_RC_INTERNAL_ERROR);
    }
    let mut length = 0usize;
    let rc = unsafe { nvat_str_length(value, &raw mut length) };
    if rc != NVAT_RC_OK || length > MAX_SDK_JSON_BYTES {
        return Err(if rc == NVAT_RC_OK {
            NVAT_RC_BAD_ARGUMENT
        } else {
            rc
        });
    }
    let mut bytes = null_mut();
    let rc = unsafe { nvat_str_get_data(value, &raw mut bytes) };
    if rc != NVAT_RC_OK || bytes.is_null() {
        return Err(if rc == NVAT_RC_OK {
            NVAT_RC_INTERNAL_ERROR
        } else {
            rc
        });
    }
    let slice = unsafe { std::slice::from_raw_parts(bytes.cast::<u8>(), length) };
    serde_json::from_slice(slice).map_err(|_| NVAT_RC_INTERNAL_ERROR)
}

#[cfg(target_os = "ios")]
fn run_appraisal(request: AppraisalRequestPayload) -> Result<Value, AppraisalFailure> {
    if request.profile_fingerprint.len() != 64
        || !request
            .profile_fingerprint
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        || request.bridge_attempt == 0
        || request.verification_time_unix <= 0
        || request.budget_ms == 0
        || request.budget_ms > 60_000
    {
        return Err(failure(NVAT_RC_BAD_ARGUMENT, "request"));
    }
    if SDK_INIT_RESULT.get().copied() != Some(NVAT_RC_OK) {
        return Err(failure(NVAT_RC_NOT_INITIALIZED, "sdk_init"));
    }
    let nonce = decode_hex(&request.nonce_hex, 32)
        .filter(|v| v.len() == 32)
        .ok_or_else(|| failure(NVAT_RC_BAD_ARGUMENT, "nonce_decode"))?;
    let proofs = decode_hex(&request.proofs_hex, MAX_PROOFS_BYTES)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| failure(NVAT_RC_BAD_ARGUMENT, "proofs_decode"))?;
    let evidence = serde_json::to_string(&request.evidence_json)
        .map_err(|_| failure(NVAT_RC_BAD_ARGUMENT, "evidence_encode"))?;
    if evidence.len() > 1_048_576 {
        return Err(failure(NVAT_RC_BAD_ARGUMENT, "evidence_size"));
    }
    let evidence_c =
        CString::new(evidence).map_err(|_| failure(NVAT_RC_BAD_ARGUMENT, "evidence_cstring"))?;
    let rims = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
        .filter(|p| p.is_dir())
        .ok_or_else(|| failure(NVAT_RC_BAD_ARGUMENT, "rims_discovery"))?;
    let rims_c = CString::new(rims.as_os_str().as_encoded_bytes())
        .map_err(|_| failure(NVAT_RC_BAD_ARGUMENT, "rims_path"))?;

    let mut native = NativeHandles::new();
    let step = |rc, stage| {
        if rc == NVAT_RC_OK {
            Ok(())
        } else {
            Err(failure(rc, stage))
        }
    };
    unsafe {
        step(
            nvat_gpu_evidence_source_from_json_string(&raw mut native.source, evidence_c.as_ptr()),
            "evidence_source",
        )?;
        step(
            nvat_nonce_from_bytes(&raw mut native.nonce, nonce.as_ptr().cast(), nonce.len()),
            "nonce_create",
        )?;
        step(
            nvat_gpu_evidence_collect(
                native.source,
                native.nonce,
                &raw mut native.evidence,
                &raw mut native.evidence_count,
            ),
            "evidence_collect",
        )?;
        step(
            nvat_rim_store_create_filesystem(&raw mut native.store, rims_c.as_ptr()),
            "rim_store",
        )?;
        step(
            nvat_ocsp_client_create_raw_proofs(
                &raw mut native.ocsp,
                proofs.as_ptr(),
                proofs.len(),
                request.verification_time_unix,
            ),
            "ocsp_proofs",
        )?;
        let mut local = null_mut();
        step(
            nvat_gpu_local_verifier_create(&raw mut local, native.store, native.ocsp, null_mut()),
            "local_verifier",
        )?;
        native.verifier = nvat_gpu_local_verifier_upcast(local);
        if native.verifier.is_null() {
            return Err(failure(NVAT_RC_INTERNAL_ERROR, "verifier_upcast"));
        }
        step(
            nvat_evidence_policy_create_default(&raw mut native.policy),
            "policy_create",
        )?;
        step(
            nvat_evidence_policy_set_gpu_claims_version(native.policy, NVAT_GPU_CLAIMS_VERSION_V3),
            "policy_claims_version",
        )?;
        let result_code = nvat_verify_gpu_evidence(
            native.verifier,
            native.evidence.cast_const(),
            native.evidence_count,
            native.policy,
            &raw mut native.detached,
            &raw mut native.claims,
        );
        if result_code != NVAT_RC_OK && result_code != NVAT_RC_OVERALL_RESULT_FALSE {
            return Err(failure(result_code, "verify_evidence"));
        }
        step(
            nvat_claims_collection_serialize_json(native.claims, &raw mut native.serialized_claims),
            "claims_serialize",
        )?;
        let claims =
            sdk_json(native.serialized_claims).map_err(|rc| failure(rc, "claims_decode"))?;
        let detached_eat =
            sdk_json(native.detached).map_err(|rc| failure(rc, "detached_eat_decode"))?;
        Ok(json!({
            "returncode": 0,
            "result_code": result_code,
            "result_message": if result_code == NVAT_RC_OK { "Ok" } else { "refused" },
            "claims": claims,
            "detached_eat": detached_eat,
        }))
    }
}

#[cfg(not(target_os = "ios"))]
fn run_appraisal(_request: AppraisalRequestPayload) -> Result<Value, AppraisalFailure> {
    Err(failure(NVAT_RC_NOT_INITIALIZED, "unsupported_platform"))
}

/// # Safety
/// `input_ptr` must be null or a valid NUL-terminated UTF-8 string. The returned
/// pointer must be released with [`ios_gpu_appraisal_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ios_gpu_appraisal_execute(input_ptr: *const c_char) -> *mut c_char {
    if input_ptr.is_null() {
        return encode_response(&response(1, NVAT_RC_BAD_ARGUMENT, "invalid request"));
    }
    let input = match unsafe { CStr::from_ptr(input_ptr) }.to_str() {
        Ok(input) => input,
        Err(_) => return encode_response(&response(1, NVAT_RC_BAD_ARGUMENT, "invalid request")),
    };
    let request = match serde_json::from_str(input) {
        Ok(request) => request,
        Err(_) => return encode_response(&response(1, NVAT_RC_BAD_ARGUMENT, "invalid request")),
    };
    match run_appraisal(request) {
        Ok(value) => encode_response(&value),
        Err(error) => encode_response(&response(
            0,
            error.rc,
            &format!("{}: {}", sdk_message(error.rc), error.stage),
        )),
    }
}

/// # Safety
/// `ptr` must be null or a pointer returned by [`ios_gpu_appraisal_execute`]
/// that has not previously been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ios_gpu_appraisal_free(ptr: *mut c_char) {
    if !ptr.is_null() {
        drop(unsafe { CString::from_raw(ptr) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_decoder_enforces_shape_and_cap() {
        assert_eq!(decode_hex("00ff", 2), Some(vec![0, 255]));
        assert_eq!(decode_hex("0", 2), None);
        assert_eq!(decode_hex("zz", 2), None);
        assert_eq!(decode_hex("0000", 1), None);
    }

    #[test]
    fn non_ios_entrypoint_refuses_without_sdk() {
        let payload = json!({
            "profile_fingerprint": "a".repeat(64),
            "nonce_hex": "00".repeat(32),
            "evidence_json": [{}],
            "proofs_hex": "00",
            "verification_time_unix": 1,
            "budget_ms": 1,
            "bridge_attempt": 1,
        });
        let input = CString::new(payload.to_string()).unwrap();
        let ptr = unsafe { ios_gpu_appraisal_execute(input.as_ptr()) };
        let value: Value =
            serde_json::from_str(unsafe { CStr::from_ptr(ptr) }.to_str().unwrap()).unwrap();
        unsafe { ios_gpu_appraisal_free(ptr) };
        assert_eq!(value["result_code"], NVAT_RC_NOT_INITIALIZED);
    }
}
