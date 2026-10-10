// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nix::fcntl::{AtFlags, OFlag, openat};
use nix::sys::stat::{Mode, SFlag, fstatat, mkdirat};
use nix::sys::statvfs::fstatvfs;
use nix::unistd::{UnlinkatFlags, linkat, unlinkat};

use super::{JournalRoot, MediaIdentity, changed, invalid};

const DIRECTORY: OFlag = OFlag::O_RDONLY
    .union(OFlag::O_DIRECTORY)
    .union(OFlag::O_NOFOLLOW)
    .union(OFlag::O_CLOEXEC);
const PLAIN: OFlag = OFlag::O_RDONLY
    .union(OFlag::O_NOFOLLOW)
    .union(OFlag::O_NONBLOCK)
    .union(OFlag::O_CLOEXEC);

struct Node {
    name: OsString,
    file: File,
    identity: MediaIdentity,
}

pub(super) struct Directory {
    root: Arc<JournalRoot>,
    nodes: Vec<Node>,
    root_file: File,
    root_identity: MediaIdentity,
    path: PathBuf,
}

impl AsFd for Directory {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.nodes
            .last()
            .map_or_else(|| self.root_file.as_fd(), |node| node.file.as_fd())
    }
}

impl Directory {
    pub(super) fn open(root: Arc<JournalRoot>, names: &[OsString]) -> io::Result<Self> {
        root.revalidate_canonical_binding()
            .map_err(io::Error::other)?;
        let root_file = File::from(root.as_fd().try_clone_to_owned()?);
        let root_identity = identity(&root_file)?;
        let mut directory = Self {
            path: root.canonical_path().to_path_buf(),
            root,
            root_file,
            root_identity,
            nodes: Vec::new(),
        };
        for name in names {
            let file = File::from(openat(
                &directory,
                name.as_os_str(),
                DIRECTORY,
                Mode::empty(),
            )?);
            let id = identity(&file)?;
            check_leaf(&directory, name, id)?;
            directory.nodes.push(Node {
                name: name.clone(),
                file,
                identity: id,
            });
            directory.path.push(name);
        }
        directory.revalidate()?;
        Ok(directory)
    }

    pub(super) fn open_child(&self, name: &OsStr) -> io::Result<Self> {
        self.revalidate()?;
        let mut nodes = Vec::with_capacity(self.nodes.len() + 1);
        for node in &self.nodes {
            nodes.push(Node {
                name: node.name.clone(),
                file: node.file.try_clone()?,
                identity: node.identity,
            });
        }
        let file = File::from(openat(self, name, DIRECTORY, Mode::empty())?);
        let id = identity(&file)?;
        check_leaf(self, name, id)?;
        nodes.push(Node {
            name: name.to_os_string(),
            file,
            identity: id,
        });
        let directory = Self {
            root: self.root.clone(),
            root_file: self.root_file.try_clone()?,
            root_identity: self.root_identity,
            nodes,
            path: self.path.join(name),
        };
        directory.revalidate()?;
        Ok(directory)
    }

    pub(super) fn identity(&self) -> MediaIdentity {
        self.nodes
            .last()
            .map_or(self.root_identity, |node| node.identity)
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn revalidate(&self) -> io::Result<()> {
        self.root
            .revalidate_canonical_binding()
            .map_err(io::Error::other)?;
        let mut parent = &self.root_file;
        for node in &self.nodes {
            let stat = fstatat(parent, node.name.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW)?;
            let id = MediaIdentity::Unix {
                device: stat.st_dev as u64,
                inode: stat.st_ino as u64,
            };
            if SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT != SFlag::S_IFDIR
                || id != node.identity
                || identity(&node.file)? != node.identity
            {
                return Err(changed());
            }
            parent = &node.file;
        }
        Ok(())
    }

    pub(super) fn available_bytes(&self) -> io::Result<u64> {
        let stat = fstatvfs(self)?;
        #[allow(
            clippy::unnecessary_cast,
            reason = "fsblkcnt_t is 32-bit on macOS and 64-bit on Linux"
        )]
        let blocks = stat.blocks_available() as u64;
        blocks
            .checked_mul(stat.fragment_size())
            .ok_or_else(|| invalid("media capacity overflow"))
    }

    pub(super) fn sync(&self) -> io::Result<()> {
        self.nodes
            .last()
            .map_or(&self.root_file, |node| &node.file)
            .sync_all()
    }
}

pub(super) fn supported_mutation() -> io::Result<()> {
    Ok(())
}

pub(super) fn identity(file: &File) -> io::Result<MediaIdentity> {
    let metadata = file.metadata()?;
    Ok(MediaIdentity::Unix {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

pub(super) type Stamp = (u64, i64, i64, i64, i64);
pub(super) fn stamp(file: &File) -> io::Result<Stamp> {
    let m = file.metadata()?;
    Ok((
        m.len(),
        m.mtime(),
        m.mtime_nsec(),
        m.ctime(),
        m.ctime_nsec(),
    ))
}

pub(super) fn check_leaf(
    directory: &Directory,
    name: &OsStr,
    expected: MediaIdentity,
) -> io::Result<()> {
    let stat = fstatat(directory, name, AtFlags::AT_SYMLINK_NOFOLLOW)?;
    if (MediaIdentity::Unix {
        device: stat.st_dev as u64,
        inode: stat.st_ino as u64,
    }) != expected
    {
        return Err(changed());
    }
    Ok(())
}

pub(super) fn open_plain(directory: &Directory, name: &OsStr) -> io::Result<Option<File>> {
    // Classify before open, so a FIFO/device cannot become a media reader.
    let stat = match fstatat(directory, name, AtFlags::AT_SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(nix::errno::Errno::ENOENT) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT != SFlag::S_IFREG {
        return Err(invalid("media entry is not a plain file"));
    }
    let file = File::from(openat(directory, name, PLAIN, Mode::empty())?);
    if !file.metadata()?.is_file() {
        return Err(invalid("media entry is not a plain file"));
    }
    let expected = MediaIdentity::Unix {
        device: stat.st_dev as u64,
        inode: stat.st_ino as u64,
    };
    if identity(&file)? != expected {
        return Err(changed());
    }
    Ok(Some(file))
}

pub(super) fn create_directory(parent: &Directory, name: &OsStr) -> io::Result<()> {
    mkdirat(parent, name, Mode::S_IRWXU).map_err(Into::into)
}

pub(super) fn create_file(parent: &Directory, name: &OsStr) -> io::Result<File> {
    Ok(File::from(openat(
        parent,
        name,
        OFlag::O_RDWR
            .union(OFlag::O_CREAT)
            .union(OFlag::O_EXCL)
            .union(OFlag::O_NOFOLLOW)
            .union(OFlag::O_CLOEXEC),
        Mode::S_IRUSR.union(Mode::S_IWUSR),
    )?))
}

pub(super) fn publish(
    source: &Directory,
    source_name: &OsStr,
    _file: &File,
    destination: &Directory,
    name: &OsStr,
) -> io::Result<()> {
    linkat(source, source_name, destination, name, AtFlags::empty()).map_err(Into::into)
}

pub(super) fn remove_leaf(parent: &Directory, name: &OsStr, _file: &File) -> io::Result<()> {
    unlinkat(parent, name, UnlinkatFlags::NoRemoveDir).map_err(Into::into)
}

pub(super) fn remove_directory(
    parent: &Directory,
    name: &OsStr,
    directory: &Directory,
) -> io::Result<()> {
    check_leaf(parent, name, directory.identity())?;
    unlinkat(parent, name, UnlinkatFlags::RemoveDir).map_err(Into::into)
}
