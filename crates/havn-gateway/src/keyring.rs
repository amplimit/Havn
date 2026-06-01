//! Credential encryption-at-rest (spec §13 Phase 3).
//!
//! Single passphrase from the `HAVN_AGE_KEY` environment variable,
//! age symmetric encryption (scrypt-based passphrase mode). Operator
//! owns the lifecycle of the key — havn never persists it.
//!
//! Design notes:
//!
//! - **Symmetric passphrase, not X25519 keypair.** Asymmetric would
//!   let encryption happen on a separate machine without the decrypt
//!   key; we don't have that use case (gateway both encrypts on
//!   write and decrypts on use). One env var is the simplest
//!   operator UX.
//! - **Key in env, ciphertext on disk.** Spec §1.6 design philosophy:
//!   "havn reads `HAVN_AGE_KEY` from env, encrypts/decrypts; never
//!   persists the key." So an attacker who steals only the SQLite
//!   file (backup leak / stolen disk image) has useless ciphertext;
//!   they need the gateway's `/proc/<pid>/environ` too.
//! - **Fail-closed boot.** If the env is missing AND credentials
//!   exist, the gateway refuses to start (better visible breakage
//!   than silent fallback to plaintext). If the env is missing AND
//!   no credentials exist (fresh install), boot OK with a warn so
//!   the operator can install the env before the first
//!   `credential add`.
//! - **Detection-by-header.** age ciphertexts begin with the magic
//!   `age-encryption.org/v1\n` line; plaintext API keys never do.
//!   That's enough to distinguish an unencrypted (legacy) row from
//!   an encrypted one without a separate state column.
//! - **Rotation deferred.** Spec doesn't require it. If we need it
//!   later, the path is a `havn credential migrate-key OLD NEW`
//!   subcommand that decrypts with OLD and re-encrypts with NEW.

use age::secrecy::SecretString;
use std::io::{Read as _, Write as _};
use thiserror::Error;

/// The fixed prefix every age ciphertext begins with. We use it to
/// detect whether a stored credential row is already encrypted.
pub const AGE_HEADER: &[u8] = b"age-encryption.org/v1\n";

/// Env var name the operator uses to provide the passphrase. Held
/// here as a const so tests + the boot guard agree.
pub const ENV_VAR: &str = "HAVN_AGE_KEY";

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum KeyringError {
    #[error("encryption: {0}")]
    Encrypt(String),
    #[error("decryption: {0}")]
    Decrypt(String),
    #[error("env var {ENV_VAR} not set — see deployment guide")]
    MissingEnv,
}

/// In-memory handle to the operator's passphrase. Constructed once
/// at gateway startup; passed by reference into encrypt/decrypt
/// sites. Wraps `SecretString` so the bytes don't get logged via
/// stray `Debug` derives.
#[derive(Clone)]
pub struct KeyRing {
    passphrase: SecretString,
}

impl std::fmt::Debug for KeyRing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never leak even the length — that's a side-channel hint
        // about the operator's key strength.
        f.debug_struct("KeyRing")
            .field("passphrase", &"<redacted>")
            .finish()
    }
}

impl KeyRing {
    /// Build from a raw passphrase. Useful in tests + when the
    /// operator passes one explicitly (e.g. a future
    /// `havn credential migrate-key`).
    pub fn from_passphrase(passphrase: impl Into<String>) -> Self {
        Self {
            passphrase: SecretString::from(passphrase.into()),
        }
    }

    /// Read `HAVN_AGE_KEY` from the process environment. Returns
    /// `MissingEnv` if unset OR set-but-empty (an empty passphrase
    /// would silently produce trivially crackable ciphertext, so we
    /// treat it the same as missing).
    pub fn load_from_env() -> Result<Self, KeyringError> {
        let raw = std::env::var(ENV_VAR).map_err(|_| KeyringError::MissingEnv)?;
        if raw.is_empty() {
            return Err(KeyringError::MissingEnv);
        }
        Ok(Self::from_passphrase(raw))
    }

    /// Encrypt `plaintext` to age ciphertext bytes. Output begins
    /// with [`AGE_HEADER`] so `is_age_ciphertext` can distinguish
    /// it from legacy plaintext rows.
    pub fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>, KeyringError> {
        let encryptor = age::Encryptor::with_user_passphrase(self.passphrase.clone());
        let mut out = Vec::with_capacity(plaintext.len() + 256);
        let mut writer = encryptor
            .wrap_output(&mut out)
            .map_err(|e| KeyringError::Encrypt(e.to_string()))?;
        writer
            .write_all(plaintext)
            .map_err(|e| KeyringError::Encrypt(e.to_string()))?;
        writer
            .finish()
            .map_err(|e| KeyringError::Encrypt(e.to_string()))?;
        Ok(out)
    }

    /// Decrypt age ciphertext bytes back to plaintext. Wrong-key
    /// errors and corrupted-ciphertext errors both surface as
    /// `Decrypt` — we deliberately don't distinguish them at the
    /// API surface (an attacker with access to error messages
    /// shouldn't get a "key was wrong" oracle).
    pub fn decrypt(&self, ciphertext: &[u8]) -> Result<Vec<u8>, KeyringError> {
        let decryptor =
            age::Decryptor::new(ciphertext).map_err(|e| KeyringError::Decrypt(e.to_string()))?;
        let identity = age::scrypt::Identity::new(self.passphrase.clone());
        let mut reader = decryptor
            .decrypt(std::iter::once(&identity as &dyn age::Identity))
            .map_err(|e| KeyringError::Decrypt(e.to_string()))?;
        let mut out = Vec::with_capacity(ciphertext.len());
        reader
            .read_to_end(&mut out)
            .map_err(|e| KeyringError::Decrypt(e.to_string()))?;
        Ok(out)
    }

    /// Cheap unit-equality for tests that want to assert two
    /// `KeyRing` instances were built from the same passphrase
    /// without exposing the bytes. Constant-time would be nicer
    /// but timing oracles on test code don't matter.
    #[cfg(test)]
    fn passphrase_str(&self) -> &str {
        use secrecy::ExposeSecret as _;
        self.passphrase.expose_secret()
    }
}

/// Returns true iff `bytes` looks like an age ciphertext (starts
/// with the magic header). Used by the startup migration to skip
/// rows that are already encrypted. False for both legacy plaintext
/// rows AND for empty/short rows — the migration treats those as
/// "encrypt me" which is the right behaviour either way (a 0-byte
/// row is malformed; we'd rather fix it than leave it).
pub fn is_age_ciphertext(bytes: &[u8]) -> bool {
    bytes.starts_with(AGE_HEADER)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    #[test]
    fn round_trip_recovers_plaintext() {
        let ring = KeyRing::from_passphrase("a-test-passphrase-for-havn-tests");
        let pt = b"sk-ant-api03-redacted-1234567890";
        let ct = ring.encrypt(pt).expect("encrypt");
        assert_ne!(ct, pt, "ciphertext must differ from plaintext");
        assert!(is_age_ciphertext(&ct), "ciphertext should carry age header");
        let recovered = ring.decrypt(&ct).expect("decrypt");
        assert_eq!(recovered, pt);
    }

    #[test]
    fn ciphertexts_are_non_deterministic() {
        // age uses random salt + nonce per encryption; same plaintext
        // under same passphrase MUST yield different ciphertext bytes.
        // If this ever fails, the crate has regressed catastrophically.
        let ring = KeyRing::from_passphrase("k");
        let pt = b"hello";
        let a = ring.encrypt(pt).expect("a");
        let b = ring.encrypt(pt).expect("b");
        assert_ne!(a, b, "age ciphertexts MUST be non-deterministic");
    }

    #[test]
    fn wrong_passphrase_fails() {
        let writer = KeyRing::from_passphrase("correct");
        let reader = KeyRing::from_passphrase("wrong");
        let ct = writer.encrypt(b"secret").expect("encrypt");
        let r = reader.decrypt(&ct);
        assert!(matches!(r, Err(KeyringError::Decrypt(_))));
    }

    #[test]
    fn corrupt_ciphertext_fails() {
        let ring = KeyRing::from_passphrase("k");
        let mut ct = ring.encrypt(b"data").expect("encrypt");
        // Flip a byte deep in the ciphertext (past the header).
        let i = ct.len() - 4;
        ct[i] ^= 0xff;
        let r = ring.decrypt(&ct);
        assert!(matches!(r, Err(KeyringError::Decrypt(_))));
    }

    #[test]
    fn legacy_plaintext_does_not_look_like_age_ciphertext() {
        // The header detection is the linchpin of the startup
        // migration: any realistic plaintext API key must NOT begin
        // with the age magic. Sample a few common provider key
        // prefixes to be safe.
        for k in [
            &b"sk-ant-api03-..."[..],
            &b"sk-..."[..],
            &b"sk-or-..."[..], // openrouter
            &b"AKIA..."[..],   // aws-style (unrelated, just paranoia)
        ] {
            assert!(
                !is_age_ciphertext(k),
                "plaintext {:?} must not look like ciphertext",
                std::str::from_utf8(k).unwrap_or("?")
            );
        }
    }

    #[test]
    fn header_constant_matches_actual_output() {
        // Defends against a future age version silently changing the
        // header — would break our migration detection.
        let ring = KeyRing::from_passphrase("k");
        let ct = ring.encrypt(b"x").expect("encrypt");
        assert!(
            ct.starts_with(AGE_HEADER),
            "AGE_HEADER constant out of sync with the age crate"
        );
    }

    #[test]
    fn debug_does_not_leak_passphrase() {
        let ring = KeyRing::from_passphrase("super-secret-do-not-leak");
        let dbg = format!("{ring:?}");
        assert!(!dbg.contains("super-secret-do-not-leak"));
        assert!(dbg.contains("redacted"));
    }

    #[test]
    fn load_from_env_missing_returns_missing_env() {
        // Use a definitely-unset name (env::remove_var semantics differ
        // across test orderings; safest is to load a name we control
        // for this test that we KNOW is unset).
        // SAFETY: single-threaded test; env mutation OK.
        unsafe {
            std::env::remove_var(ENV_VAR);
        }
        let r = KeyRing::load_from_env();
        assert!(matches!(r, Err(KeyringError::MissingEnv)));
    }

    #[test]
    fn passphrase_str_round_trips_for_test_assertion() {
        let ring = KeyRing::from_passphrase("abc");
        assert_eq!(ring.passphrase_str(), "abc");
    }
}
