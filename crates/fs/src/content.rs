//! Bounded content reads for the matching funnel and for execution-time verification.
//! Never used by the inventory walk. Every file is opened through directory handles with
//! no-follow, its signature is checked against the caller's expectation before reading and
//! again after (a change means the result proves nothing).

use std::os::fd::{AsFd, OwnedFd};

use removarr_domain::error::{ErrorClass, PortError};
use removarr_domain::fs::{CompareOutcome, EntryKind, EntrySignature, SampleSpec};
use removarr_domain::path::RootedPath;
use removarr_domain::ports::ScanControl;
use rustix::fs::{Mode, OFlags, fstat, openat};
use rustix::io::pread;

use crate::ConfinedFs;
use crate::handle::{lstat_at, open_rel_dir, port_error, signature};

/// Minimum compare buffer (legacy CFG-10: chunk must be at least 64 KiB).
pub const MIN_CHUNK: usize = 64 * 1024;

pub(crate) struct OpenFile {
    pub fd: OwnedFd,
}

fn changed(what: &str) -> PortError {
    PortError::new(
        ErrorClass::StateChanged,
        format!("{what} changed since it was scanned"),
    )
}

impl ConfinedFs {
    /// Opens a regular file read-only, refusing symlinks, and checks it against `expect`.
    pub(crate) fn open_regular(
        &self,
        path: &RootedPath,
        expect: &EntrySignature,
    ) -> Result<OpenFile, PortError> {
        let root_fd = self.open_root(&path.root)?;
        let (Some(parent), Some(name)) = (path.rel.parent(), path.rel.file_name()) else {
            return Err(PortError::new(
                ErrorClass::Validation,
                "path is the root, not a file",
            ));
        };
        let dir = open_rel_dir(&root_fd, &parent)?;
        let cname = std::ffi::CString::new(name)
            .map_err(|_| PortError::new(ErrorClass::Validation, "NUL in file name"))?;
        let st = lstat_at(&dir, &cname).map_err(|e| port_error(e, "stat before read"))?;
        let sig = signature(&st);
        if sig.kind == EntryKind::Symlink {
            return Err(crate::handle::confinement_error(
                "refusing to read a symlink",
            ));
        }
        if sig.kind != EntryKind::File {
            return Err(PortError::new(ErrorClass::Rejected, "not a regular file"));
        }
        if !sig.matches(expect, false) {
            return Err(changed("file"));
        }
        let fd = openat(
            &dir,
            &cname,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map_err(|e| port_error(e, "opening file"))?;
        let opened = signature(&fstat(&fd).map_err(|e| port_error(e, "fstat"))?);
        if !opened.matches(expect, false) {
            return Err(changed("file"));
        }
        advise_sequential(&fd);
        Ok(OpenFile { fd })
    }
}

#[cfg(target_os = "linux")]
fn advise_sequential(fd: &OwnedFd) {
    let _ = rustix::fs::fadvise(fd, 0, None, rustix::fs::Advice::Sequential);
}
#[cfg(not(target_os = "linux"))]
fn advise_sequential(_fd: &OwnedFd) {}

#[cfg(target_os = "linux")]
fn advise_dontneed(fd: &OwnedFd) {
    let _ = rustix::fs::fadvise(fd, 0, None, rustix::fs::Advice::DontNeed);
}
#[cfg(not(target_os = "linux"))]
fn advise_dontneed(_fd: &OwnedFd) {}

fn still_matches(f: &OpenFile, expect: &EntrySignature) -> Result<bool, PortError> {
    let now = signature(&fstat(&f.fd).map_err(|e| port_error(e, "fstat after read"))?);
    Ok(now.matches(expect, false))
}

/// Reads exactly `buf.len()` bytes at `offset`; `Ok(false)` on a short file.
fn read_exact_at(fd: &OwnedFd, buf: &mut [u8], offset: u64) -> Result<bool, PortError> {
    let mut done = 0usize;
    while done < buf.len() {
        let n = pread(fd.as_fd(), &mut buf[done..], offset + done as u64)
            .map_err(|e| port_error(e, "reading file"))?;
        if n == 0 {
            return Ok(false);
        }
        done += n;
    }
    Ok(true)
}

/// Offset of sample `i` of `count` over `span = size - width`, rounded like the legacy tool.
fn sample_offset(i: u64, count: u64, span: u64) -> u64 {
    if count <= 1 {
        return 0;
    }
    let denom = count - 1;
    (i * span * 2 + denom) / (2 * denom)
}

impl removarr_domain::ports::ContentReader for ConfinedFs {
    fn samples_match(
        &self,
        a: &RootedPath,
        b: &RootedPath,
        expect: [&EntrySignature; 2],
        spec: SampleSpec,
        ctl: &ScanControl,
    ) -> Result<bool, PortError> {
        let _permit = self.content_gate.acquire();
        if expect[0].size != expect[1].size {
            return Ok(false);
        }
        let size = expect[0].size;
        if size == 0 {
            return Ok(true);
        }
        let fa = self.open_regular(a, expect[0])?;
        let fb = self.open_regular(b, expect[1])?;
        let width = u64::from(spec.size).min(size);
        let span = size - width;
        let count = u64::from(spec.count.max(1));
        let width_len = usize::try_from(width)
            .map_err(|_| PortError::new(ErrorClass::Validation, "sample too large"))?;
        let mut ba = vec![0u8; width_len];
        let mut bb = vec![0u8; width_len];
        let mut equal = true;
        for i in 0..count {
            if ctl.cancel.is_cancelled() {
                return Err(PortError::new(ErrorClass::Cancelled, "sampling cancelled"));
            }
            let off = sample_offset(i, count, span);
            if !read_exact_at(&fa.fd, &mut ba, off)? || !read_exact_at(&fb.fd, &mut bb, off)? {
                return Err(changed("file"));
            }
            if ba != bb {
                equal = false;
                break;
            }
        }
        if !still_matches(&fa, expect[0])? || !still_matches(&fb, expect[1])? {
            return Err(changed("file"));
        }
        advise_dontneed(&fa.fd);
        advise_dontneed(&fb.fd);
        Ok(equal)
    }

    fn full_compare(
        &self,
        a: &RootedPath,
        b: &RootedPath,
        expect: [&EntrySignature; 2],
        chunk_bytes: u32,
        ctl: &ScanControl,
    ) -> Result<CompareOutcome, PortError> {
        let _permit = self.content_gate.acquire();
        let (fa, fb) = match (
            self.open_regular(a, expect[0]),
            self.open_regular(b, expect[1]),
        ) {
            (Ok(fa), Ok(fb)) => (fa, fb),
            (Err(e), _) | (_, Err(e)) if e.class == ErrorClass::StateChanged => {
                return Ok(CompareOutcome::Changed);
            }
            (Err(e), _) | (_, Err(e)) => return Err(e),
        };
        let size = expect[0].size;
        if size != expect[1].size {
            return Ok(CompareOutcome::Differs {
                offset: size.min(expect[1].size),
            });
        }
        let chunk = (chunk_bytes as usize).max(MIN_CHUNK);
        let mut ba = vec![0u8; chunk];
        let mut bb = vec![0u8; chunk];
        let mut offset = 0u64;
        while offset < size {
            if ctl.cancel.is_cancelled() {
                return Err(PortError::new(
                    ErrorClass::Cancelled,
                    "comparison cancelled",
                ));
            }
            let want = usize::try_from((size - offset).min(chunk as u64)).unwrap_or(chunk);
            if !read_exact_at(&fa.fd, &mut ba[..want], offset)?
                || !read_exact_at(&fb.fd, &mut bb[..want], offset)?
            {
                return Ok(CompareOutcome::Changed);
            }
            if ba[..want] != bb[..want] {
                let first = ba[..want]
                    .iter()
                    .zip(&bb[..want])
                    .position(|(x, y)| x != y)
                    .unwrap_or(0);
                // The difference only proves anything if the files were stable meanwhile.
                if !still_matches(&fa, expect[0])? || !still_matches(&fb, expect[1])? {
                    return Ok(CompareOutcome::Changed);
                }
                return Ok(CompareOutcome::Differs {
                    offset: offset + first as u64,
                });
            }
            offset += want as u64;
        }
        if !still_matches(&fa, expect[0])? || !still_matches(&fb, expect[1])? {
            return Ok(CompareOutcome::Changed);
        }
        advise_dontneed(&fa.fd);
        advise_dontneed(&fb.fd);
        Ok(CompareOutcome::Identical)
    }
}
