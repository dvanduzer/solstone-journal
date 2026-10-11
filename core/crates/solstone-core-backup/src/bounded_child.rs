// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! The concrete Unix bounded child. Public results carry output, not writer authority.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::fcntl::{FcntlArg, FdFlag, fcntl};
use nix::sys::signal::{Signal, killpg};
use nix::unistd::{Pid, pipe};

pub struct Request<'a> {
    pub program: OsString,
    pub argv: Vec<OsString>,
    pub env: BTreeMap<OsString, OsString>,
    pub timeout: Option<Duration>,
    pub pass_fds: Vec<BorrowedFd<'a>>,
}

pub struct Output {
    pub returncode: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub(crate) struct CompletedChild {
    output: Output,
}

impl CompletedChild {
    pub(crate) fn successful(&self) -> bool {
        self.output.returncode == 0
    }

    pub(crate) fn output(&self) -> &Output {
        &self.output
    }

    pub(crate) fn into_output(self) -> Output {
        self.output
    }
}

pub(crate) struct ChildFailure {
    error: io::Error,
}

impl ChildFailure {
    fn new(error: io::Error) -> Self {
        Self { error }
    }

    fn into_error(self) -> io::Error {
        self.error
    }
}

pub fn run(request: &Request<'_>) -> io::Result<Output> {
    run_owned(request, None)
        .map(CompletedChild::into_output)
        .map_err(ChildFailure::into_error)
}

/// Only actual execution creates completion. A capture bound drains excess
/// bytes rather than leaving a child blocked on its output pipe.
pub(crate) fn run_owned(
    request: &Request<'_>,
    capture_limit: Option<usize>,
) -> Result<CompletedChild, ChildFailure> {
    execute(request, capture_limit).map_err(ChildFailure::new)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Observe,
    GroupCleanup,
    CloseOwned,
    Reap,
    CollectReaders,
}

trait SessionIo {
    fn observe_exit_without_reap(&mut self) -> io::Result<bool>;
    fn group_cleanup(&mut self) -> io::Result<()>;
    fn close_owned_endpoints(&mut self);
    fn reap_root(&mut self) -> io::Result<i32>;
    fn collect_readers(&mut self) -> io::Result<(Vec<u8>, Vec<u8>)>;
}

fn complete_session<S: SessionIo>(
    session: &mut S,
    mut trace: Option<&mut Vec<Step>>,
) -> io::Result<(i32, Vec<u8>, Vec<u8>)> {
    let mut record = |step| {
        if let Some(trace) = &mut trace {
            trace.push(step);
        }
    };
    record(Step::Observe);
    session.observe_exit_without_reap()?;
    record(Step::GroupCleanup);
    let cleanup_error = session.group_cleanup().err();
    record(Step::CloseOwned);
    session.close_owned_endpoints();
    record(Step::Reap);
    let reaped = session.reap_root();
    if let Some(error) = cleanup_error {
        // Survivors may hold pipe writers: do not join on failed group cleanup.
        return Err(error);
    }
    let status = reaped?;
    record(Step::CollectReaders);
    let (stdout, stderr) = session.collect_readers()?;
    Ok((status, stdout, stderr))
}

type OutputReader = thread::JoinHandle<io::Result<Vec<u8>>>;

struct SpawnedChild {
    child: Child,
    pgid: Pid,
    reaped: bool,
    stdout_w: Option<OwnedFd>,
    stderr_w: Option<OwnedFd>,
    readers: Option<(OutputReader, OutputReader)>,
}

impl Drop for SpawnedChild {
    fn drop(&mut self) {
        if !self.reaped {
            // The owned Child remains armed immediately after spawn, including
            // group confirmation and every later error/unwind path.
            let _ = kill_group(self.pgid);
            let _ = self.child.wait();
        }
    }
}

impl SessionIo for SpawnedChild {
    fn observe_exit_without_reap(&mut self) -> io::Result<bool> {
        observe_exit_without_reap(self.pgid)
    }

    fn group_cleanup(&mut self) -> io::Result<()> {
        kill_group(self.pgid)
    }

    fn close_owned_endpoints(&mut self) {
        drop(self.stdout_w.take());
        drop(self.stderr_w.take());
    }

    fn reap_root(&mut self) -> io::Result<i32> {
        let status = self.child.wait()?;
        self.reaped = true;
        Ok(status
            .code()
            .unwrap_or_else(|| -status.signal().unwrap_or(1)))
    }

    fn collect_readers(&mut self) -> io::Result<(Vec<u8>, Vec<u8>)> {
        let (stdout, stderr) = self.readers.take().expect("readers");
        // Join both even when the first reader failed.
        let stdout = stdout.join();
        let stderr = stderr.join();
        Ok((
            stdout.map_err(|_| io::Error::other("stdout reader panicked"))??,
            stderr.map_err(|_| io::Error::other("stderr reader panicked"))??,
        ))
    }
}

fn kill_group(pgid: Pid) -> io::Result<()> {
    match killpg(pgid, Signal::SIGKILL) {
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        #[cfg(target_os = "macos")]
        Err(Errno::EPERM) if matches!(nix::unistd::getpgid(Some(pgid)), Err(Errno::ESRCH)) => {
            Ok(())
        }
        Err(error) => Err(io::Error::from(error)),
    }
}

fn observe_exit_without_reap(pid: Pid) -> io::Result<bool> {
    let pid = rustix::process::Pid::from_raw(pid.as_raw())
        .ok_or_else(|| io::Error::other("invalid child pid"))?;
    rustix::process::waitid(
        rustix::process::WaitId::Pid(pid),
        rustix::process::WaitIdOptions::EXITED
            | rustix::process::WaitIdOptions::NOHANG
            | rustix::process::WaitIdOptions::NOWAIT,
    )
    .map(|status| status.is_some())
    .map_err(io::Error::from)
}

fn pipe_cloexec() -> io::Result<(OwnedFd, OwnedFd)> {
    let (read, write) = pipe().map_err(io::Error::from)?;
    for fd in [read.as_fd(), write.as_fd()] {
        let flags = fcntl(fd, FcntlArg::F_GETFD).map_err(io::Error::from)?;
        fcntl(
            fd,
            FcntlArg::F_SETFD(FdFlag::from_bits_truncate(flags) | FdFlag::FD_CLOEXEC),
        )
        .map_err(io::Error::from)?;
    }
    Ok((read, write))
}

fn inherit_in_child(command: &mut Command, descriptors: &[BorrowedFd<'_>]) -> io::Result<()> {
    let mut inherited = Vec::with_capacity(descriptors.len());
    for fd in descriptors {
        // Validate every descriptor before spawning; never mutate parent flags.
        let flags = fcntl(*fd, FcntlArg::F_GETFD).map_err(io::Error::from)?;
        inherited.push((fd.as_raw_fd(), flags & !FdFlag::FD_CLOEXEC.bits()));
    }
    // SAFETY: all allocation and validation happen above. The child hook calls
    // only async-signal-safe fcntl and constructs raw OS errors on failure.
    #[allow(unsafe_code)]
    unsafe {
        command.pre_exec(move || {
            for &(fd, flags) in &inherited {
                if nix::libc::fcntl(fd, nix::libc::F_SETFD, flags) == -1 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    Ok(())
}

fn read_output(fd: OwnedFd, limit: Option<usize>) -> io::Result<Vec<u8>> {
    let mut file = File::from(fd);
    let mut bytes = Vec::new();
    if let Some(limit) = limit {
        let mut buffer = [0u8; 8192];
        let mut exceeded = false;
        loop {
            let count = match file.read(&mut buffer) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => result?,
            };
            if count == 0 {
                break;
            }
            let retained = count.min(limit.saturating_sub(bytes.len()));
            bytes.extend_from_slice(&buffer[..retained]);
            exceeded |= retained != count;
        }
        if exceeded {
            return Err(io::Error::other("child output exceeded capture bound"));
        }
    } else {
        file.read_to_end(&mut bytes)?;
    }
    Ok(bytes)
}

fn execute(request: &Request<'_>, capture_limit: Option<usize>) -> io::Result<CompletedChild> {
    let (stdout_r, stdout_w) = pipe_cloexec()?;
    let (stderr_r, stderr_w) = pipe_cloexec()?;
    let mut command = Command::new(&request.program);
    command.args(&request.argv).env_clear().envs(&request.env);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_w.try_clone()?))
        .stderr(Stdio::from(stderr_w.try_clone()?));
    command.process_group(0);
    inherit_in_child(&mut command, &request.pass_fds)?;
    let child = command.spawn()?;
    let pgid = Pid::from_raw(child.id() as i32);
    let mut session = SpawnedChild {
        child,
        pgid,
        reaped: false,
        stdout_w: Some(stdout_w),
        stderr_w: Some(stderr_w),
        readers: None,
    };
    // Command owns cloned write ends until dropped, even after spawn.
    drop(command);
    nix::unistd::getpgid(Some(pgid)).map_err(io::Error::other)?;
    let stdout = thread::spawn(move || read_output(stdout_r, capture_limit));
    let stderr = thread::spawn(move || read_output(stderr_r, capture_limit));
    session.readers = Some((stdout, stderr));
    let start = Instant::now();
    let timed_out = loop {
        if session.observe_exit_without_reap()? {
            break false;
        }
        if request
            .timeout
            .is_some_and(|timeout| start.elapsed() >= timeout)
        {
            break true;
        }
        thread::sleep(Duration::from_millis(5));
    };
    let (status, stdout, stderr) = complete_session(&mut session, None)?;
    Ok(CompletedChild {
        output: Output {
            returncode: if timed_out {
                124
            } else if status >= 0 {
                status
            } else {
                1
            },
            stdout,
            stderr,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_reaps_before_collecting_and_preserves_cleanup_error() {
        struct Fake {
            cleanup_fails: bool,
        }
        impl SessionIo for Fake {
            fn observe_exit_without_reap(&mut self) -> io::Result<bool> {
                Ok(true)
            }
            fn group_cleanup(&mut self) -> io::Result<()> {
                if self.cleanup_fails {
                    Err(io::Error::other("cleanup"))
                } else {
                    Ok(())
                }
            }
            fn close_owned_endpoints(&mut self) {}
            fn reap_root(&mut self) -> io::Result<i32> {
                Ok(0)
            }
            fn collect_readers(&mut self) -> io::Result<(Vec<u8>, Vec<u8>)> {
                Ok((vec![], vec![]))
            }
        }
        for fails in [false, true] {
            let mut steps = Vec::new();
            let result = complete_session(
                &mut Fake {
                    cleanup_fails: fails,
                },
                Some(&mut steps),
            );
            let mut expected = vec![
                Step::Observe,
                Step::GroupCleanup,
                Step::CloseOwned,
                Step::Reap,
            ];
            if !fails {
                expected.push(Step::CollectReaders);
            }
            assert_eq!(steps, expected);
            assert_eq!(result.is_err(), fails);
        }
    }

    #[cfg(all(test, feature = "full-tests"))]
    #[test]
    fn child_setup_failure_after_first_descriptor_leaves_parent_flags_unchanged() {
        let directory = tempfile::tempdir().unwrap();
        let first = File::create(directory.path().join("first")).unwrap();
        let second = File::create(directory.path().join("second")).unwrap();
        let second_raw = second.as_raw_fd();
        let first_flags = fcntl(first.as_fd(), FcntlArg::F_GETFD).unwrap();
        let second_flags = fcntl(second.as_fd(), FcntlArg::F_GETFD).unwrap();
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg("exit 0");
        // SAFETY: this test fault closes only the child copy of a retained FD.
        #[allow(unsafe_code)]
        unsafe {
            command.pre_exec(move || {
                nix::libc::close(second_raw);
                Ok(())
            });
        }
        inherit_in_child(&mut command, &[first.as_fd(), second.as_fd()]).unwrap();
        assert!(command.spawn().is_err());
        assert_eq!(
            fcntl(first.as_fd(), FcntlArg::F_GETFD).unwrap(),
            first_flags
        );
        assert_eq!(
            fcntl(second.as_fd(), FcntlArg::F_GETFD).unwrap(),
            second_flags
        );
    }
}
