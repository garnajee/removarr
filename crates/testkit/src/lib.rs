//! Deterministic test support. No real service endpoints or credentials.

pub mod clock;
pub mod fake_arr;
pub mod fake_jellyfin;
pub mod fake_qbit;
pub mod fixture;
pub mod ids;

#[cfg(test)]
mod tests {
    #[test]
    fn scratch_directory_is_available_without_network() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert!(dir.path().is_dir());
    }
}
