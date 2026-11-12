//! The report directory (T-18): reports are written only under `<data_dir>/reports`, never
//! inside a media or download root, with a safe file name, a size bound and an atomic write.

use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use removarr_domain::error::{ErrorClass, PortError};

/// Largest report file written (64 MiB).
pub const MAX_REPORT_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct ReportDir {
    dir: PathBuf,
}

fn invalid(msg: &str) -> PortError {
    PortError::new(ErrorClass::Validation, msg)
}

/// Canonical form of the longest existing ancestor, with the missing tail appended, so a
/// directory that does not exist yet still resolves symlinks in its existing prefix.
fn canonical_or_self(p: &Path) -> PathBuf {
    let mut tail = Vec::new();
    let mut cur = p;
    loop {
        if let Ok(c) = fs::canonicalize(cur) {
            return tail.iter().rev().fold(c, |acc, part| acc.join(part));
        }
        match (cur.parent(), cur.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name.to_os_string());
                cur = parent;
            }
            _ => return p.to_path_buf(),
        }
    }
}

impl ReportDir {
    /// `roots` are the configured completed, incomplete and media roots (absolute paths).
    /// Fails when `data_dir` is relative, equals or lies inside a root, or contains one.
    pub fn new(data_dir: &Path, roots: &[&Path]) -> Result<Self, PortError> {
        if !data_dir.is_absolute() || data_dir.components().any(|c| c == Component::ParentDir) {
            return Err(invalid("data_dir must be an absolute path without '..'"));
        }
        let dir = canonical_or_self(data_dir).join("reports");
        for r in roots {
            let root = canonical_or_self(r);
            if dir.starts_with(&root) || root.starts_with(canonical_or_self(data_dir)) {
                return Err(invalid(
                    "the report directory must be outside every media and download root",
                ));
            }
        }
        Ok(Self { dir })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// File names are `[A-Za-z0-9._-]`, 1..=128 chars, not starting with a dot.
    pub fn safe_name(name: &str) -> Result<&str, PortError> {
        let ok = !name.is_empty()
            && name.len() <= 128
            && !name.starts_with('.')
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
        if ok {
            Ok(name)
        } else {
            Err(invalid("unsafe report file name"))
        }
    }

    /// Writes `bytes` atomically (temporary file, then rename). Refuses a symlinked directory.
    pub fn write(&self, name: &str, bytes: &[u8]) -> Result<PathBuf, PortError> {
        let name = Self::safe_name(name)?;
        if bytes.len() > MAX_REPORT_BYTES {
            return Err(invalid("report is too large"));
        }
        let io = |_: std::io::Error| {
            PortError::new(ErrorClass::Storage, "report file could not be written")
        };
        fs::create_dir_all(&self.dir).map_err(io)?;
        if fs::symlink_metadata(&self.dir)
            .map_err(io)?
            .file_type()
            .is_symlink()
        {
            return Err(invalid("the report directory is a symlink"));
        }
        let target = self.dir.join(name);
        if fs::symlink_metadata(&target).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(invalid("the report file is a symlink"));
        }
        let tmp = self.dir.join(format!(".{name}.tmp"));
        {
            let mut f = fs::File::create(&tmp).map_err(io)?;
            f.write_all(bytes).map_err(io)?;
            f.sync_all().map_err(io)?;
        }
        fs::rename(&tmp, &target).map_err(io)?;
        Ok(target)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn report_directory_cannot_be_under_or_around_a_root() {
        let t = tempfile::tempdir().unwrap();
        let media = t.path().join("media");
        let data = t.path().join("data");
        fs::create_dir_all(&media).unwrap();
        fs::create_dir_all(&data).unwrap();
        assert!(ReportDir::new(&data, &[&media]).is_ok());
        assert!(ReportDir::new(&media.join("data"), &[&media]).is_err());
        assert!(ReportDir::new(&media, &[&media]).is_err());
        assert!(
            ReportDir::new(t.path(), &[&media]).is_err(),
            "data dir containing a root"
        );
        assert!(ReportDir::new(Path::new("relative/data"), &[]).is_err());
    }

    #[test]
    fn names_are_validated_and_writes_are_atomic() {
        let t = tempfile::tempdir().unwrap();
        let d = ReportDir::new(t.path(), &[]).unwrap();
        for bad in ["", "../x", "a/b", ".hidden", "a b", "x\0"] {
            assert!(d.write(bad, b"x").is_err(), "{bad:?}");
        }
        let p = d.write("report.json", b"{}").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"{}");
        assert!(p.starts_with(d.path()));
        assert!(!d.path().join(".report.json.tmp").exists());
        let big = vec![0u8; MAX_REPORT_BYTES + 1];
        assert!(d.write("big.bin", &big).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_target_is_refused() {
        let t = tempfile::tempdir().unwrap();
        let d = ReportDir::new(t.path(), &[]).unwrap();
        d.write("a.txt", b"1").unwrap();
        let outside = t.path().join("secret.txt");
        fs::write(&outside, b"keep").unwrap();
        std::os::unix::fs::symlink(&outside, d.path().join("link.txt")).unwrap();
        assert!(d.write("link.txt", b"x").is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"keep");
    }
}
