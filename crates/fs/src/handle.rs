//! Directory-handle primitives. Every path component is reached with
//! `openat(O_NOFOLLOW | O_DIRECTORY)` and `fstatat(AT_SYMLINK_NOFOLLOW)`, so a symlink can never
//! be followed, whatever the string form of the path (ADR-0004).
//!
//! These primitives are public so the mutation adapter (`mutate`, agent 13) reuses exactly the
//! same confinement code. Nothing here modifies the filesystem.

use std::ffi::CStr;
use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;

use removarr_domain::error::{ErrorClass, PortError};
use removarr_domain::fs::{EntryKind, EntrySignature, IoKind};
use removarr_domain::path::RelPath;
use removarr_domain::time::MtimeNs;
use rustix::fs::{AtFlags, FileType, Mode, OFlags, Stat, fstat, openat, statat};
use rustix::io::Errno;

#[must_use]
pub fn io_kind(e: Errno) -> IoKind {
    match e {
        Errno::NOENT => IoKind::NotFound,
        Errno::ACCESS | Errno::PERM => IoKind::PermissionDenied,
        Errno::XDEV => IoKind::CrossDevice,
        Errno::EXIST => IoKind::AlreadyExists,
        Errno::NOTDIR => IoKind::NotADirectory,
        _ => IoKind::Other,
    }
}

#[must_use]
pub fn io_message(e: Errno) -> String {
    std::io::Error::from(e).to_string()
}

#[must_use]
pub fn port_error(e: Errno, context: &str) -> PortError {
    PortError::new(
        ErrorClass::Io(io_kind(e)),
        format!("{context}: {}", io_message(e)),
    )
}

#[must_use]
pub fn confinement_error(detail: impl Into<String>) -> PortError {
    PortError::new(ErrorClass::Confinement, detail)
}

fn dir_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

fn kind_of(mode: u32) -> EntryKind {
    match FileType::from_raw_mode(mode as rustix::fs::RawMode) {
        FileType::RegularFile => EntryKind::File,
        FileType::Directory => EntryKind::Dir,
        FileType::Symlink => EntryKind::Symlink,
        _ => EntryKind::Other,
    }
}

/// The single place where a `stat` becomes an `EntrySignature`. Scan, `stat`, snapshots and
/// content reads all go through it, so signatures taken at different times are comparable.
/// `size` is only meaningful (and only kept) for regular files and symlinks.
#[allow(clippy::unnecessary_cast, clippy::cast_sign_loss)]
#[must_use]
pub fn signature(st: &Stat) -> EntrySignature {
    let mode = st.st_mode as u32;
    let kind = kind_of(mode);
    let size = match kind {
        EntryKind::File | EntryKind::Symlink => st.st_size as u64,
        EntryKind::Dir | EntryKind::Other => 0,
    };
    let mtime_ns = i64::from(st.st_mtime_nsec as i32)
        .saturating_add((st.st_mtime as i64).saturating_mul(1_000_000_000));
    EntrySignature {
        kind,
        dev: st.st_dev as u64,
        ino: st.st_ino as u64,
        size,
        nlink: st.st_nlink as u64,
        mtime_ns: MtimeNs(mtime_ns),
        mode: mode & 0o7777,
        uid: st.st_uid as u32,
        gid: st.st_gid as u32,
    }
}

/// `st_blocks * 512` for regular files, 0 otherwise.
#[allow(clippy::unnecessary_cast, clippy::cast_sign_loss)]
#[must_use]
pub fn allocated_bytes(st: &Stat) -> u64 {
    if kind_of(st.st_mode as u32) == EntryKind::File {
        (st.st_blocks as u64).saturating_mul(512)
    } else {
        0
    }
}

/// `lstat` of `name` inside the directory handle.
pub fn lstat_at(dir: impl AsFd, name: &CStr) -> Result<Stat, Errno> {
    statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)
}

/// Opens one directory component without following symlinks. A symlink (or a non-directory)
/// is reported as `Confinement` or `Io(NotADirectory)` after an `lstat` to tell them apart.
pub fn open_dir_at(parent: impl AsFd, name: &CStr) -> Result<OwnedFd, PortError> {
    match openat(parent.as_fd(), name, dir_flags(), Mode::empty()) {
        Ok(fd) => Ok(fd),
        Err(Errno::LOOP | Errno::NOTDIR) => match lstat_at(parent.as_fd(), name) {
            Ok(st) if kind_of(st.st_mode as u32) == EntryKind::Symlink => Err(confinement_error(
                format!("symlinked path component {name:?} is never followed"),
            )),
            Ok(_) => Err(port_error(
                Errno::NOTDIR,
                "path component is not a directory",
            )),
            Err(e2) => Err(port_error(e2, "opening path component")),
        },
        Err(e) => Err(port_error(e, "opening path component")),
    }
}

/// Opens an absolute path starting from `/`, one component at a time with no-follow. Ancestors
/// of a root are opened for traversal only, siblings never. A symlinked ancestor is refused.
pub fn open_abs_dir(path: &Path) -> Result<OwnedFd, PortError> {
    let text = path
        .to_str()
        .ok_or_else(|| confinement_error("root path is not valid UTF-8"))?;
    if !text.starts_with('/') {
        return Err(confinement_error("root path is not absolute"));
    }
    let mut cur = openat(rustix::fs::CWD, c"/", dir_flags(), Mode::empty())
        .map_err(|e| port_error(e, "opening filesystem root"))?;
    for comp in text.split('/').filter(|c| !c.is_empty()) {
        if comp == "." || comp == ".." {
            return Err(confinement_error("root path is not normalized"));
        }
        let c = std::ffi::CString::new(comp).map_err(|_| confinement_error("NUL in root path"))?;
        cur = open_dir_at(&cur, &c)?;
    }
    Ok(cur)
}

/// Walks the components of `rel` below `root`, with no-follow at every step.
pub fn open_rel_dir(root: impl AsFd, rel: &RelPath) -> Result<OwnedFd, PortError> {
    let mut cur = rustix::io::fcntl_dupfd_cloexec(root.as_fd(), 0)
        .map_err(|e| port_error(e, "duplicating root handle"))?;
    for comp in rel.components() {
        let c =
            std::ffi::CString::new(comp).map_err(|_| confinement_error("NUL in path component"))?;
        cur = open_dir_at(&cur, &c)?;
    }
    Ok(cur)
}

/// Signature of the directory behind a handle.
pub fn handle_signature(fd: impl AsFd) -> Result<(EntrySignature, Stat), PortError> {
    let st = fstat(fd).map_err(|e| port_error(e, "fstat"))?;
    Ok((signature(&st), st))
}
