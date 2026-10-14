// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! iOS confidential GPU appraisal out-of-process helper.
//!
//! Exposes C entry points for the Swift ExtensionKit extension (`Appraisal.appex`).
//! FFI is blocked because no `nvat.h` SDK header is bound in this tree.

use libc::c_char;
use serde_json::json;
use std::ffi::CString;

#[cfg(target_os = "ios")]
use serde::Deserialize;
#[cfg(target_os = "ios")]
use std::ffi::CStr;
#[cfg(target_os = "ios")]
use std::sync::mpsc;
#[cfg(target_os = "ios")]
use std::time::Duration;

#[cfg(target_os = "ios")]
#[derive(Deserialize)]
struct AppraisalRequestPayload {
    #[serde(default)]
    budget_ms: u64,
}

fn refusal_json() -> String {
    json!({
        "returncode": 1,
        "result_code": 1,
        "result_message": "sdk header is not bound",
        "claims": [],
        "detached_eat": []
    })
    .to_string()
}

#[cfg(not(target_os = "ios"))]
/// # Safety
/// `_input_ptr` must be a valid NUL-terminated C string or null.
/// Caller must free returned pointer with [`ios_gpu_appraisal_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ios_gpu_appraisal_execute(_input_ptr: *const c_char) -> *mut c_char {
    let s = CString::new(refusal_json()).unwrap();
    s.into_raw()
}

#[cfg(not(target_os = "ios"))]
/// # Safety
/// `ptr` must be a pointer returned from [`ios_gpu_appraisal_execute`] and not previously freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ios_gpu_appraisal_free(ptr: *mut c_char) {
    if !ptr.is_null() {
        drop(unsafe { CString::from_raw(ptr) });
    }
}

#[cfg(target_os = "ios")]
/// # Safety
/// `input_ptr` must be a valid NUL-terminated C string or null.
/// Caller must free returned pointer with [`ios_gpu_appraisal_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ios_gpu_appraisal_execute(input_ptr: *const c_char) -> *mut c_char {
    if input_ptr.is_null() {
        let s = CString::new(refusal_json()).unwrap();
        return s.into_raw();
    }

    let input_str = match unsafe { CStr::from_ptr(input_ptr) }.to_str() {
        Ok(s) => s,
        Err(_) => {
            let s = CString::new(refusal_json()).unwrap();
            return s.into_raw();
        }
    };

    let req: AppraisalRequestPayload = match serde_json::from_str(input_str) {
        Ok(r) => r,
        Err(_) => {
            let s = CString::new(refusal_json()).unwrap();
            return s.into_raw();
        }
    };

    // Budget 0 returns a refusal and does not start work.
    if req.budget_ms == 0 {
        let s = CString::new(refusal_json()).unwrap();
        return s.into_raw();
    }

    // Budget above 60000 is capped at 60000.
    let budget_ms = req.budget_ms.min(60_000);

    // Start watchdog thread first.
    let (disarm_tx, disarm_rx) = mpsc::channel();
    std::thread::spawn(move || {
        if disarm_rx
            .recv_timeout(Duration::from_millis(budget_ms))
            .is_err()
        {
            // Watchdog deadline expired.
            std::process::exit(124);
        }
    });

    // FFI is blocked; return refusal JSON without process::exit.
    // Disarm watchdog before returning.
    let _ = disarm_tx.send(());

    let s = CString::new(refusal_json()).unwrap();
    s.into_raw()
}

#[cfg(target_os = "ios")]
/// # Safety
/// `ptr` must be a pointer returned from [`ios_gpu_appraisal_execute`] and not previously freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ios_gpu_appraisal_free(ptr: *mut c_char) {
    if !ptr.is_null() {
        drop(unsafe { CString::from_raw(ptr) });
    }
}
