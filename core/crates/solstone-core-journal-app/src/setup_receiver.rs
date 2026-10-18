// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Inspect the installed receiver as data and keep that image fixed through launch.

use std::fs::{File, OpenOptions};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use windows_sys::Win32::Foundation::{FreeLibrary, HMODULE};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_SHARE_READ, FILE_SHARE_WRITE,
};
use windows_sys::Win32::System::LibraryLoader::{
    FindResourceW, LOAD_LIBRARY_AS_DATAFILE_EXCLUSIVE, LOAD_LIBRARY_AS_IMAGE_RESOURCE,
    LoadLibraryExW, LoadResource, LockResource, SizeofResource,
};

use crate::setup_handoff::{
    CapabilityResult, HandoffContract, admit_capability_document, resource_type_id,
};

pub struct PreparedReceiver {
    path: PathBuf,
    // Denying write and delete sharing prevents an updater replacing the image
    // after capability admission. Keep this handle until CreateProcess returns.
    _image: File,
    // The installed root and current directory cannot be renamed or replaced
    // while their fixed pathname is being inspected and launched.
    _directories: [File; 2],
}

struct ResourceModule(HMODULE);

impl Drop for ResourceModule {
    fn drop(&mut self) {
        // SAFETY: this handle came from LoadLibraryExW and is released once.
        unsafe { FreeLibrary(self.0) };
    }
}

impl PreparedReceiver {
    pub fn open(path: &Path, contract: &HandoffContract) -> Result<Self, CapabilityResult> {
        let current = path.parent().ok_or(CapabilityResult::Malformed)?;
        let root = current.parent().ok_or(CapabilityResult::Malformed)?;
        let directories = [hold_directory(root)?, hold_directory(current)?];
        let image = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    CapabilityResult::Absent
                } else {
                    CapabilityResult::Malformed
                }
            })?;
        let metadata = image.metadata().map_err(|_| CapabilityResult::Malformed)?;
        if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(CapabilityResult::Malformed);
        }
        let wide_path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: the absolute installed path is NUL-terminated and remains
        // alive. These flags map resources only, without executing the image
        // or loading its imports. The file handle above prevents replacement.
        let module = unsafe {
            LoadLibraryExW(
                wide_path.as_ptr(),
                std::ptr::null_mut(),
                LOAD_LIBRARY_AS_DATAFILE_EXCLUSIVE | LOAD_LIBRARY_AS_IMAGE_RESOURCE,
            )
        };
        if module.is_null() {
            return Err(CapabilityResult::Malformed);
        }
        let module = ResourceModule(module);
        let name: Vec<u16> = contract
            .resource
            .name
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let kind = resource_type_id(&contract.resource.resource_type) as usize;
        // SAFETY: module is live, name is NUL-terminated, and the integer
        // resource type is passed with MAKEINTRESOURCE pointer semantics.
        let resource =
            unsafe { FindResourceW(module.0, name.as_ptr(), std::ptr::without_provenance(kind)) };
        if resource.is_null() {
            return Err(CapabilityResult::NoResource);
        }
        // SAFETY: resource belongs to this still-live module.
        let size = unsafe { SizeofResource(module.0, resource) } as usize;
        if size > contract.limits.capability_resource_max_bytes {
            return Err(CapabilityResult::Oversized);
        }
        if size == 0 {
            return Err(CapabilityResult::Malformed);
        }
        // SAFETY: these calls access that resource as data. The slice is bounded
        // by SizeofResource and used before ResourceModule drops.
        let data = unsafe { LoadResource(module.0, resource) };
        if data.is_null() {
            return Err(CapabilityResult::Malformed);
        }
        let bytes = unsafe { LockResource(data) };
        if bytes.is_null() {
            return Err(CapabilityResult::Malformed);
        }
        let document = unsafe { std::slice::from_raw_parts(bytes.cast::<u8>(), size) };
        let admitted = admit_capability_document(
            document,
            &contract.capability_document,
            contract.limits.capability_resource_max_bytes,
        );
        if admitted != CapabilityResult::Supported {
            return Err(admitted);
        }
        Ok(Self {
            path: path.to_path_buf(),
            _image: image,
            _directories: directories,
        })
    }

    pub fn launch(&self, flag: &str) -> Result<(), ()> {
        // Command invokes the exact executable without file associations or a
        // shell. No output or private capability enters inherited job handles.
        Command::new(&self.path)
            .arg(flag)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map(drop)
            .map_err(|_| ())
    }
}

fn hold_directory(path: &Path) -> Result<File, CapabilityResult> {
    let directory = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                CapabilityResult::Absent
            } else {
                CapabilityResult::Malformed
            }
        })?;
    let metadata = directory
        .metadata()
        .map_err(|_| CapabilityResult::Malformed)?;
    if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(CapabilityResult::Malformed);
    }
    Ok(directory)
}
