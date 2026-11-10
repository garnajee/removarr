//! Lexical absolute-path helpers on `/`-separated strings (Linux container paths).
//! No filesystem access here.

/// Why a path was rejected. Never includes the path itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathReject {
    Empty,
    NotAbsolute,
    Nul,
    DotComponent,
    ParentComponent,
}

impl PathReject {
    #[must_use]
    pub fn reason(self) -> &'static str {
        match self {
            PathReject::Empty => "path is empty",
            PathReject::NotAbsolute => "path must be absolute",
            PathReject::Nul => "path contains a NUL byte",
            PathReject::DotComponent => "path contains a `.` component",
            PathReject::ParentComponent => "path contains a `..` component",
        }
    }
}

/// Collapses duplicate `/` and a trailing `/`. Rejects relative paths, `.` and `..`.
pub fn normalize_abs(raw: &str) -> Result<String, PathReject> {
    if raw.is_empty() {
        return Err(PathReject::Empty);
    }
    if raw.contains('\0') {
        return Err(PathReject::Nul);
    }
    if !raw.starts_with('/') {
        return Err(PathReject::NotAbsolute);
    }
    let mut out = String::with_capacity(raw.len());
    for c in raw.split('/').filter(|c| !c.is_empty()) {
        match c {
            "." => return Err(PathReject::DotComponent),
            ".." => return Err(PathReject::ParentComponent),
            _ => {
                out.push('/');
                out.push_str(c);
            }
        }
    }
    if out.is_empty() {
        out.push('/');
    }
    Ok(out)
}

/// For normalized paths: the remainder of `path` below `prefix` (no leading `/`; empty when
/// equal), matched on component boundaries. `/data` is not a prefix of `/data2`.
#[must_use]
pub fn strip_prefix<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
    if prefix == "/" {
        return path.strip_prefix('/');
    }
    let rest = path.strip_prefix(prefix)?;
    if rest.is_empty() {
        Some("")
    } else {
        rest.strip_prefix('/')
    }
}

/// `child` equals `ancestor` or lies below it (normalized inputs).
#[must_use]
pub fn is_within(child: &str, ancestor: &str) -> bool {
    strip_prefix(child, ancestor).is_some()
}

/// Joins a normalized absolute base and a relative remainder.
#[must_use]
pub fn join(base: &str, rest: &str) -> String {
    match (base, rest) {
        (_, "") => base.to_string(),
        ("/", r) => format!("/{r}"),
        (b, r) => format!("{b}/{r}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes() {
        assert_eq!(normalize_abs("//a///b/").unwrap(), "/a/b");
        assert_eq!(normalize_abs("/").unwrap(), "/");
        assert_eq!(normalize_abs("a"), Err(PathReject::NotAbsolute));
        assert_eq!(normalize_abs("/a/../b"), Err(PathReject::ParentComponent));
        assert_eq!(normalize_abs("/a/./b"), Err(PathReject::DotComponent));
        assert_eq!(normalize_abs("/a\0b"), Err(PathReject::Nul));
        assert_eq!(normalize_abs(""), Err(PathReject::Empty));
    }

    #[test]
    fn prefix_is_component_bounded() {
        assert_eq!(strip_prefix("/data/x", "/data"), Some("x"));
        assert_eq!(strip_prefix("/data", "/data"), Some(""));
        assert_eq!(strip_prefix("/data2/x", "/data"), None);
        assert_eq!(strip_prefix("/a/b", "/"), Some("a/b"));
        assert_eq!(join("/", "a"), "/a");
        assert_eq!(join("/a", ""), "/a");
    }
}
