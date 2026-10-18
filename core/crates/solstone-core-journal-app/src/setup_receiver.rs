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

#[cfg(all(test, feature = "full-tests"))]
mod native_tests {
    use super::*;
    use crate::setup_handoff::get_contract;
    use std::fs;
    use std::time::{Duration, Instant};

    const TOOL_TIMEOUT: Duration = Duration::from_secs(60);

    // Use the gate's initialized MSVC toolchain, never a synthetic PE or a
    // second implementation of the resource-reading/launching boundary.
    fn run_tool(command: &mut Command) {
        let label = command.get_program().to_owned();
        let mut child = command
            .stdin(Stdio::null())
            .spawn()
            .unwrap_or_else(|error| panic!("required native fixture tool {label:?}: {error}"));
        let deadline = Instant::now() + TOOL_TIMEOUT;
        loop {
            if let Some(status) = child.try_wait().expect("poll native fixture tool") {
                assert!(status.success(), "native fixture tool {label:?}: {status}");
                return;
            }
            if Instant::now() >= deadline {
                child.kill().expect("terminate timed-out fixture tool");
                child.wait().expect("reap timed-out fixture tool");
                panic!("native fixture tool {label:?} exceeded its deadline");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    struct Fixture {
        directory: tempfile::TempDir,
        contract: HandoffContract,
    }

    impl Fixture {
        fn build() -> Self {
            let directory = tempfile::Builder::new()
                .prefix("journal receiver native ")
                .tempdir()
                .expect("native fixture directory");
            let contract = get_contract();
            assert!(
                contract
                    .argv
                    .cli_flag
                    .bytes()
                    .all(|byte| { byte.is_ascii_alphanumeric() || byte == b'-' })
            );
            // A finite, noninteractive executable: no credentials, browser,
            // IPC, installer state, inherited output, or arbitrary arguments.
            // Its receipt is published only after the exact argv check.
            let source = format!(
                r#"#include <windows.h>
#include <wchar.h>
static int record(HMODULE module, const wchar_t *suffix, const char *bytes, DWORD count) {{
    wchar_t path[32768];
    DWORD length = GetModuleFileNameW(module, path, 32768);
    if (!length || length >= 32750) return 10;
    if (wcscat_s(path, 32768, suffix) != 0) return 14;
    HANDLE file = CreateFileW(path, GENERIC_WRITE, 0, NULL, CREATE_NEW,
                             FILE_ATTRIBUTE_NORMAL, NULL);
    if (file == INVALID_HANDLE_VALUE) return 12;
    DWORD written = 0;
    BOOL ok = WriteFile(file, bytes, count, &written, NULL);
    CloseHandle(file);
    return ok && written == count ? 0 : 13;
}}
#ifdef INSPECTION_PROBE
BOOL WINAPI DllMain(HINSTANCE instance, DWORD reason, LPVOID reserved) {{
    (void)reserved;
    if (reason == DLL_PROCESS_ATTACH)
        return record(instance, L".executed", "executed\n", 9) == 0;
    return TRUE;
}}
#else
int wmain(int argc, wchar_t **argv) {{
    int result = record(NULL, L".executed", "executed\n", 9);
    if (result) return result;
    if (argc != 2 || wcscmp(argv[1], L"{}") != 0) return 11;
    return record(NULL, L".receipt", "exact-argv-ok\n", 14);
}}
#endif
"#,
                contract.argv.cli_flag
            );
            fs::write(directory.path().join("fixture.c"), source).unwrap();
            run_tool(Command::new("cl.exe").current_dir(directory.path()).args([
                "/nologo",
                "/W4",
                "/WX",
                "/c",
                "fixture.c",
                "/Fofixture.obj",
            ]));
            run_tool(Command::new("cl.exe").current_dir(directory.path()).args([
                "/nologo",
                "/W4",
                "/WX",
                "/c",
                "/DINSPECTION_PROBE",
                "fixture.c",
                "/Foprobe.obj",
            ]));
            Self {
                directory,
                contract,
            }
        }

        fn image(&self, case: &str, resource: Option<(&str, &[u8])>, probe: bool) -> PathBuf {
            let build = self.directory.path().join(case);
            let current = build.join("Solstone/current");
            fs::create_dir_all(&current).unwrap();
            let filename = if probe {
                "receiver.dll"
            } else {
                "receiver.exe"
            };
            let image = current.join(filename);
            let mut link = Command::new("link.exe");
            link.current_dir(&build)
                .args(["/NOLOGO", "/SUBSYSTEM:CONSOLE"])
                .arg(format!("/OUT:Solstone/current/{filename}"))
                .arg(
                    self.directory
                        .path()
                        .join(if probe { "probe.obj" } else { "fixture.obj" }),
                )
                .arg("kernel32.lib");
            if probe {
                link.arg("/DLL");
            }
            if let Some((resource_type, bytes)) = resource {
                fs::write(build.join("capability.json"), bytes).unwrap();
                fs::write(
                    build.join("fixture.rc"),
                    format!(
                        "\"{}\" {resource_type} \"capability.json\"\r\n",
                        self.contract.resource.name
                    ),
                )
                .unwrap();
                run_tool(Command::new("rc.exe").current_dir(&build).args([
                    "/nologo",
                    "/fo",
                    "fixture.res",
                    "fixture.rc",
                ]));
                link.arg("fixture.res");
            }
            run_tool(&mut link);
            image
        }

        fn valid_image(&self, case: &str) -> PathBuf {
            let bytes = serde_json::to_vec(&self.contract.capability_document).unwrap();
            self.image(case, Some(("RCDATA", &bytes)), false)
        }
    }

    fn evidence_path(image: &Path, suffix: &str) -> PathBuf {
        let mut path = image.as_os_str().to_owned();
        path.push(suffix);
        path.into()
    }

    fn denied(result: std::io::Result<impl Sized>) {
        match result {
            Err(error) => assert!(
                matches!(error.raw_os_error(), Some(5 | 32)),
                "expected access/sharing denial, not an unrelated fixture failure: {error}"
            ),
            Ok(_) => panic!("held receiver pathname was mutable"),
        }
    }

    #[test]
    fn prepared_receiver_reads_real_resources_and_guards_launch() {
        let fixture = Fixture::build();
        let contract = &fixture.contract;
        let valid = serde_json::to_vec(&contract.capability_document).unwrap();
        let unknown = serde_json::to_vec(&serde_json::json!({
            "schema": "unsupported-native-fixture-version",
            "capabilities": contract.capability_document.capabilities,
        }))
        .unwrap();
        let oversized = vec![b' '; contract.limits.capability_resource_max_bytes + 1];
        for (case, resource, expected) in [
            ("missing", None, CapabilityResult::NoResource),
            (
                "wrong-type",
                Some(("\"RT_RCDATA\"", valid.as_slice())),
                CapabilityResult::NoResource,
            ),
            (
                "malformed",
                Some(("RCDATA", b"{invalid}".as_slice())),
                CapabilityResult::Malformed,
            ),
            (
                "version",
                Some(("RCDATA", unknown.as_slice())),
                CapabilityResult::Unknown,
            ),
            (
                "oversized",
                Some(("RCDATA", oversized.as_slice())),
                CapabilityResult::Oversized,
            ),
        ] {
            let image = fixture.image(case, resource, false);
            assert_eq!(
                PreparedReceiver::open(&image, contract).err(),
                Some(expected)
            );
            assert!(
                !evidence_path(&image, ".executed").exists(),
                "inspection executed {case}"
            );
        }

        // Unlike an EXE's main, a DLL entry point runs during an ordinary
        // LoadLibraryExW. This control makes the non-executing assertion
        // sensitive to a regression in the production data-only flags.
        let probe = fixture.image("inspection-probe", Some(("RCDATA", &valid)), true);
        drop(PreparedReceiver::open(&probe, contract).expect("probe resource admitted"));
        assert!(!evidence_path(&probe, ".executed").exists());
        let wide_probe: Vec<u16> = probe.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: this is the test's own finite DLL fixture and a live,
        // NUL-terminated absolute path. Its only effect is the fixed marker.
        let module = unsafe { LoadLibraryExW(wide_probe.as_ptr(), std::ptr::null_mut(), 0) };
        assert!(!module.is_null(), "normal-load positive control");
        let module = ResourceModule(module);
        assert_eq!(
            fs::read(evidence_path(&probe, ".executed")).unwrap(),
            b"executed\n"
        );
        drop(module);

        let image = fixture.valid_image("supported");
        let receiver = PreparedReceiver::open(&image, contract).expect("real RCDATA admitted");
        assert!(
            !evidence_path(&image, ".executed").exists(),
            "inspection executed receiver"
        );
        let current = image.parent().unwrap();
        let root = current.parent().unwrap();
        let renamed_image = current.join("renamed.exe");
        let replacement = current.join("replacement.exe");
        fs::copy(&image, &replacement).unwrap();
        denied(OpenOptions::new().write(true).open(&image));
        denied(fs::remove_file(&image));
        denied(fs::rename(&image, &renamed_image));
        denied(fs::rename(&replacement, &image));
        denied(fs::rename(current, root.join("renamed-current")));
        denied(fs::rename(root, root.with_file_name("renamed-root")));
        drop(receiver);

        // Every denial has a positive control after the *production* guard is
        // dropped, before a running process could supply accidental protection.
        drop(OpenOptions::new().write(true).open(&image).unwrap());
        fs::rename(&image, &renamed_image).unwrap();
        fs::rename(&renamed_image, &image).unwrap();
        fs::rename(&replacement, &image).unwrap();
        fs::remove_file(&image).unwrap();
        fs::rename(current, root.join("renamed-current")).unwrap();
        fs::rename(root.join("renamed-current"), current).unwrap();
        fs::rename(root, root.with_file_name("renamed-root")).unwrap();
        fs::rename(root.with_file_name("renamed-root"), root).unwrap();

        let image = fixture.valid_image("launch");
        let receiver = PreparedReceiver::open(&image, contract).unwrap();
        receiver
            .launch(&contract.argv.cli_flag)
            .expect("guarded launch");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(bytes) = fs::read(evidence_path(&image, ".receipt")) {
                assert_eq!(bytes, b"exact-argv-ok\n");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "receiver did not publish argv receipt"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        // The guard stays alive throughout launch and receipt observation.
        denied(fs::rename(
            image.parent().unwrap(),
            image.parent().unwrap().with_file_name("moved"),
        ));
        drop(receiver);
        // The fixture has no wait/daemon path. Confirm that it released its
        // executable mapping before removing the temporary tree; observing a
        // receipt alone is not process-exit evidence.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match fs::remove_file(&image) {
                Ok(()) => break,
                Err(error) if matches!(error.raw_os_error(), Some(5 | 32)) => {
                    assert!(
                        Instant::now() < deadline,
                        "fixture image remained in use: {error}"
                    );
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => panic!("remove launched fixture: {error}"),
            }
        }
        fixture
            .directory
            .close()
            .expect("remove native fixture tree");
    }

    #[test]
    fn prepared_receiver_refuses_real_directory_junctions() {
        let fixture = Fixture::build();
        for component in ["root", "current"] {
            let image = fixture.valid_image(component);
            let current = image.parent().unwrap();
            let root = current.parent().unwrap();
            let path = if component == "root" { root } else { current };
            let target = path.with_file_name("junction-target");
            fs::rename(path, &target).unwrap();
            // Directory junctions do not require Developer Mode or the symlink
            // privilege. A failed fixture construction fails qualification.
            run_tool(
                Command::new("cmd.exe")
                    .args(["/d", "/c", "mklink", "/J"])
                    .arg(path)
                    .arg(&target),
            );
            assert_ne!(
                fs::symlink_metadata(path).unwrap().file_attributes()
                    & FILE_ATTRIBUTE_REPARSE_POINT,
                0
            );
            assert_eq!(
                PreparedReceiver::open(&image, &fixture.contract).err(),
                Some(CapabilityResult::Malformed)
            );
            assert!(!evidence_path(&image, ".executed").exists());
            fs::remove_dir(path).unwrap();
            fs::rename(&target, path).unwrap();
            drop(
                PreparedReceiver::open(&image, &fixture.contract)
                    .expect("regular directory control"),
            );
        }
        fixture
            .directory
            .close()
            .expect("remove junction fixture tree");
    }
}
