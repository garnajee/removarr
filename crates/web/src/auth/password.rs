//! Argon2 password verification (architecture 12.3) and the hash helper behind
//! `removarr hash-password`.
//!
//! The configured PHC string is parsed once at startup (an invalid or non-Argon2 hash refuses
//! to start). Verification uses the parameters of that string; the final comparison of the
//! derived output is constant time (`password-hash` compares `Output` with `subtle`). It runs in
//! `spawn_blocking` behind a one-permit semaphore so a burst of attempts can never use more
//! than one core and one Argon2 memory block.

use std::sync::Arc;

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Argon2, Params};
use tokio::sync::Semaphore;

/// Maximum accepted password length in bytes (longer input fails without hashing).
pub const MAX_PASSWORD_BYTES: usize = 1024;

/// The configured hash is not a usable Argon2 PHC string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidHash;

#[derive(Debug, Clone)]
pub struct Verifier {
    phc: Arc<str>,
    permit: Arc<Semaphore>,
}

impl Verifier {
    /// Accepts `$argon2id$`, `$argon2i$` and `$argon2d$` PHC strings with valid parameters.
    pub fn new(phc: &str) -> Result<Self, InvalidHash> {
        let h = PasswordHash::new(phc).map_err(|_| InvalidHash)?;
        if !matches!(h.algorithm.as_str(), "argon2id" | "argon2i" | "argon2d") {
            return Err(InvalidHash);
        }
        Params::try_from(&h).map_err(|_| InvalidHash)?;
        if h.hash.is_none() || h.salt.is_none() {
            return Err(InvalidHash);
        }
        Ok(Self {
            phc: Arc::from(phc),
            permit: Arc::new(Semaphore::new(1)),
        })
    }

    /// `true` only when `password` matches. Any internal failure is a mismatch.
    pub async fn verify(&self, password: String) -> bool {
        let Ok(_permit) = self.permit.acquire().await else {
            return false;
        };
        let phc = self.phc.clone();
        tokio::task::spawn_blocking(move || {
            PasswordHash::new(&phc).is_ok_and(|h| {
                Argon2::default()
                    .verify_password(password.as_bytes(), &h)
                    .is_ok()
            })
        })
        .await
        .unwrap_or(false)
    }
}

/// Hashing failed (no randomness or invalid input).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HashError;

/// Argon2id PHC string of `password` with the library defaults (m = 19 MiB, t = 2, p = 1)
/// and a random 16-byte salt.
pub fn hash_password(password: &str) -> Result<String, HashError> {
    if password.is_empty() || password.len() > MAX_PASSWORD_BYTES {
        return Err(HashError);
    }
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).map_err(|_| HashError)?;
    let salt = SaltString::encode_b64(&salt).map_err(|_| HashError)?;
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|_| HashError)
}

#[cfg(test)]
mod tests {
    use super::*;
    use argon2::{Algorithm, Version};

    /// Cheap parameters for tests only (the PHC string carries them).
    fn cheap_hash(pw: &str) -> String {
        let a = Argon2::new(
            Algorithm::Argon2id,
            Version::V0x13,
            Params::new(256, 1, 1, None).unwrap(),
        );
        let salt = SaltString::encode_b64(b"fixed-test-salt!").unwrap();
        a.hash_password(pw.as_bytes(), &salt).unwrap().to_string()
    }

    #[tokio::test]
    async fn verifies_only_the_right_password() {
        let v = Verifier::new(&cheap_hash("fake-test-password")).unwrap();
        assert!(v.verify("fake-test-password".into()).await);
        assert!(!v.verify("fake-test-passwore".into()).await);
        assert!(!v.verify(String::new()).await);
    }

    #[test]
    fn rejects_unusable_hashes() {
        assert_eq!(Verifier::new("").unwrap_err(), InvalidHash);
        assert_eq!(Verifier::new("plaintext").unwrap_err(), InvalidHash);
        assert_eq!(
            Verifier::new("$argon2id$v=19$m=256,t=1,p=1$c2FsdHNhbHQ").unwrap_err(),
            InvalidHash,
            "no output"
        );
        assert_eq!(
            Verifier::new("$pbkdf2-sha256$i=1000$c2FsdA$aGFzaGhhc2hoYXNoaGFzaA").unwrap_err(),
            InvalidHash
        );
    }

    #[tokio::test]
    async fn hash_password_round_trips_with_default_parameters() {
        let phc = hash_password("another fake password").unwrap();
        assert!(phc.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"));
        assert!(!phc.contains("another fake password"));
        let v = Verifier::new(&phc).unwrap();
        assert!(v.verify("another fake password".into()).await);
        assert_ne!(
            phc,
            hash_password("another fake password").unwrap(),
            "salted"
        );
        assert_eq!(hash_password(""), Err(HashError));
    }
}
