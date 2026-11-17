#![allow(dead_code, clippy::unwrap_used)]

use std::fs;
use std::path::PathBuf;

use removarr_config::Env;
use tempfile::TempDir;

/// 64+ character dummy session key and a dummy PHC-shaped hash. Not real secrets.
pub const DUMMY_SESSION_KEY: &str =
    "dummy-session-key-0123456789-0123456789-0123456789-0123456789-abcdef";
pub const DUMMY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$ZHVtbXlzYWx0$ZHVtbXloYXNoZHVtbXloYXNo";

pub struct Fixture {
    pub tmp: TempDir,
    pub base: PathBuf,
}

impl Fixture {
    pub fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(tmp.path()).unwrap();
        for d in ["completed", "incomplete", "media1", "media2", "data"] {
            fs::create_dir(base.join(d)).unwrap();
        }
        Self { tmp, base }
    }

    pub fn p(&self, rel: &str) -> String {
        self.base.join(rel).to_str().unwrap().to_string()
    }

    /// Minimal valid environment.
    pub fn env(&self) -> Env {
        let mut e = Env::new();
        e.insert("REMOVARR_COMPLETED_ROOT".into(), self.p("completed"));
        e.insert("REMOVARR_MEDIA_ROOTS".into(), self.p("media1"));
        e.insert("REMOVARR_DATA_DIR".into(), self.p("data"));
        e.insert("REMOVARR_PASSWORD_HASH".into(), DUMMY_HASH.into());
        e.insert("REMOVARR_SESSION_KEY".into(), DUMMY_SESSION_KEY.into());
        e
    }
}

pub fn set(env: &mut Env, k: &str, v: &str) {
    env.insert(k.to_string(), v.to_string());
}
