// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Minimal descriptor-relative `NtCreateFile` binding shared by Windows I/O paths.

use std::ffi::OsStr;
use std::io;
use std::mem::{align_of, size_of};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{FromRawHandle, OwnedHandle, RawHandle};

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_RENAME_INFORMATION, FileRenameInformation, NtCreateFile, NtSetInformationFile,
};
use windows_sys::Win32::Foundation::{
    HANDLE, INVALID_HANDLE_VALUE, OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, STATUS_SUCCESS,
    UNICODE_STRING,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

/// Rename an already-open DELETE-capable stage into one direct name beneath a
/// retained directory handle. Native failures retain their raw OS error code;
/// buffer-validation failures are InvalidInput without a native-call receipt.
pub(crate) fn nt_rename_relative(
    parent: RawHandle,
    stage: RawHandle,
    name: &OsStr,
    replace: bool,
) -> io::Result<()> {
    let wide: Vec<u16> = name.encode_wide().collect();
    let name_bytes = wide
        .len()
        .checked_mul(size_of::<u16>())
        .and_then(|bytes| u32::try_from(bytes).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "rename buffer too large"))?;
    let extra = wide
        .len()
        .saturating_sub(1)
        .checked_mul(size_of::<u16>())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "rename buffer too large"))?;
    let bytes = size_of::<FILE_RENAME_INFORMATION>()
        .checked_add(extra)
        .and_then(|bytes| u32::try_from(bytes).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "rename buffer too large"))?;
    let mut buffer = vec![0_u64; (bytes as usize).div_ceil(size_of::<u64>())];
    let pointer = buffer.as_mut_ptr();
    debug_assert_eq!(
        pointer
            .cast::<u8>()
            .align_offset(align_of::<FILE_RENAME_INFORMATION>()),
        0
    );
    // SAFETY: the caller retains the directory and DELETE-capable stage handles.
    // The zeroed u64 buffer is aligned and sized for the native fixed header and
    // inline UTF-16 name; both it and status remain live for the synchronous call.
    #[allow(unsafe_code)]
    unsafe {
        let info = pointer.cast::<FILE_RENAME_INFORMATION>();
        (*info).Anonymous.ReplaceIfExists = replace;
        (*info).RootDirectory = parent as HANDLE;
        (*info).FileNameLength = name_bytes;
        std::ptr::copy_nonoverlapping(wide.as_ptr(), (*info).FileName.as_mut_ptr(), wide.len());
        let mut status = IO_STATUS_BLOCK::default();
        let result = NtSetInformationFile(
            stage,
            &mut status,
            pointer.cast(),
            bytes,
            FileRenameInformation,
        );
        if result != STATUS_SUCCESS {
            Err(io::Error::from_raw_os_error(
                RtlNtStatusToDosError(result) as i32
            ))
        } else {
            Ok(())
        }
    }
}

/// Open or create one native-name child relative to an already retained directory handle.
pub(crate) fn nt_create_relative(
    parent: RawHandle,
    name: &OsStr,
    desired_access: u32,
    disposition: u32,
    options: u32,
) -> io::Result<OwnedHandle> {
    nt_create_relative_with_attributes(
        parent,
        name,
        desired_access,
        disposition,
        options,
        OBJ_CASE_INSENSITIVE,
    )
}

/// Open or create one native-name child using an exact-case object-name match.
pub(crate) fn nt_create_relative_exact(
    parent: RawHandle,
    name: &OsStr,
    desired_access: u32,
    disposition: u32,
    options: u32,
) -> io::Result<OwnedHandle> {
    nt_create_relative_with_attributes(parent, name, desired_access, disposition, options, 0)
}

fn nt_create_relative_with_attributes(
    parent: RawHandle,
    name: &OsStr,
    desired_access: u32,
    disposition: u32,
    options: u32,
    object_attributes: u32,
) -> io::Result<OwnedHandle> {
    nt_create_relative_raw(
        parent,
        name,
        desired_access,
        disposition,
        options,
        object_attributes,
        windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ
            | windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE
            | windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE,
    )
}

/// Open or create one native-name child without sharing `DELETE`.
pub(crate) fn nt_create_relative_deny_delete_sharing(
    parent: RawHandle,
    name: &OsStr,
    desired_access: u32,
    disposition: u32,
    options: u32,
) -> io::Result<OwnedHandle> {
    nt_create_relative_raw(
        parent,
        name,
        desired_access,
        disposition,
        options,
        OBJ_CASE_INSENSITIVE,
        windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ
            | windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE,
    )
}

/// Open or create one native-name child sharing read and delete, but not write.
pub(crate) fn nt_create_relative_share_read_delete(
    parent: RawHandle,
    name: &OsStr,
    desired_access: u32,
    disposition: u32,
    options: u32,
) -> io::Result<OwnedHandle> {
    nt_create_relative_raw(
        parent,
        name,
        desired_access,
        disposition,
        options,
        OBJ_CASE_INSENSITIVE,
        windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ
            | windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE,
    )
}

/// Open or create an owned media leaf sharing READ only. The retained handle
/// excludes competing writers and deletion through preparation/publication.
pub(crate) fn nt_create_relative_share_read(
    parent: RawHandle,
    name: &OsStr,
    desired_access: u32,
    disposition: u32,
    options: u32,
) -> io::Result<OwnedHandle> {
    nt_create_relative_raw(
        parent,
        name,
        desired_access,
        disposition,
        options,
        OBJ_CASE_INSENSITIVE,
        windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ,
    )
}

fn nt_create_relative_raw(
    parent: RawHandle,
    name: &OsStr,
    desired_access: u32,
    disposition: u32,
    options: u32,
    object_attributes: u32,
    share_access: u32,
) -> io::Result<OwnedHandle> {
    let wide = name.encode_wide().collect::<Vec<_>>();
    let byte_length = wide
        .len()
        .checked_mul(size_of::<u16>())
        .and_then(|length| u16::try_from(length).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "relative name is too long"))?;
    let mut object_name = UNICODE_STRING {
        Length: byte_length,
        MaximumLength: byte_length,
        Buffer: wide.as_ptr().cast_mut(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent,
        ObjectName: &mut object_name,
        Attributes: object_attributes,
        SecurityDescriptor: std::ptr::null(),
        SecurityQualityOfService: std::ptr::null(),
    };
    let mut handle = INVALID_HANDLE_VALUE;
    let mut status = IO_STATUS_BLOCK::default();
    // SAFETY: `attributes` refers to the live UTF-16 component and retained parent handle;
    // all output pointers refer to initialized local storage, and the synchronous request
    // does not outlive them.
    #[allow(unsafe_code)]
    let result = unsafe {
        NtCreateFile(
            &mut handle,
            desired_access,
            &attributes,
            &mut status,
            std::ptr::null(),
            0,
            share_access,
            disposition,
            options,
            std::ptr::null(),
            0,
        )
    };
    if result != STATUS_SUCCESS {
        // SAFETY: `RtlNtStatusToDosError` converts the just-returned NTSTATUS without
        // borrowing caller memory.
        #[allow(unsafe_code)]
        let error = unsafe { RtlNtStatusToDosError(result) };
        return Err(io::Error::from_raw_os_error(error as i32));
    }
    // SAFETY: `NtCreateFile` returned one owned valid handle and the conversion occurs
    // exactly once.
    #[allow(unsafe_code)]
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}
