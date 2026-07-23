//! AES-256-GCM encryption at rest for the connection store, plus an
//! atomic-write/read wrapper ([`SecureStore`]) around it.
//!
//! ## Key derivation
//!
//! [`derive_key`] derives a deterministic 256-bit key from this machine's
//! hardware id (`machine_uid::get()`) via HKDF-SHA256 with a fixed salt.
//! Same machine => same key every time; a different machine derives a
//! different key, so a copied store file is simply unreadable elsewhere --
//! by design, not as an error case callers need to handle specially. If the
//! machine id itself can't be read, `derive_key` **fails closed**
//! (`AppError::KeyDerivationFailed`) rather than falling back to a fixed
//! IKM: a fallback constant would be visible in this public repo's source,
//! which would make it a well-known key rather than a machine-bound one --
//! silently encrypting under a public key is worse than refusing to encrypt
//! at all.
//!
//! ## API shape / testability
//!
//! [`encrypt`] and [`decrypt`] take the key as an explicit `&[u8; 32]`
//! parameter rather than deriving it themselves. [`SecureStore::load`] and
//! [`SecureStore::save`] call [`derive_key`] internally so production
//! call sites never have to think about key management, but tests exercise
//! `encrypt`/`decrypt` round-trips with a fixed, explicit test key so they
//! never depend on the actual machine id of the box running the tests.
//!
//! ## Wire format
//!
//! `encrypt` returns `nonce (12 random bytes) || AES-256-GCM ciphertext`.
//! `decrypt` rejects anything that doesn't parse as that shape, and
//! anything whose authentication tag doesn't check out, with
//! `AppError::DecryptFailed` -- never a panic and never surfaced as a
//! plain io error.
//!
//! ## Accepted threat model
//!
//! This store deliberately trades some cryptographic strength for "no
//! external dependency, no user-managed passphrase": the key is derived
//! from machine-uid entropy (not attacker-unpredictable, and machine ids
//! are sometimes readable by other local software) combined with a *public*
//! salt/info (visible in this repo's source, so it grants an attacker no
//! advantage to keep secret, but also grants the defender none either). The
//! design goal is **not** "resistant to an attacker who already has a
//! process running as the same local user" -- that attacker can typically
//! also just ask `machine_uid::get()` themselves, or read process memory,
//! or use OS APIs to read the plaintext connections through this app while
//! it's running. What machine-binding *does* defend against is casual file
//! exfiltration: someone who copies `connections.enc` off this machine (via
//! a stolen backup, a synced folder, a lost drive, ...) cannot decrypt it
//! elsewhere without also reproducing this exact machine's hardware id. An
//! OS keychain (Keychain/Credential Manager/Secret Service) would raise the
//! bar further (hardware-backed, per-user-session-gated secrets) at the
//! cost of a platform-specific dependency and its own failure modes; that
//! tradeoff was considered and explicitly not chosen for this iteration.
//! Future reviews should treat "weak key derivation" as a known, accepted
//! property of this design rather than a bug to re-flag, unless the actual
//! goal (defend against same-user local processes) changes.

use std::fs;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::PathBuf;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use aes_gcm::aead::{Aead, KeyInit, Nonce as AeadNonce};
use aes_gcm::{Aes256Gcm, Key};
use hkdf::Hkdf;
use sha2::Sha256;

use crate::error::{AppError, AppResult};
use crate::store::model::Connection;

/// Fixed HKDF salt binding derived keys to this application and store
/// format version. Bumping the suffix (e.g. to `-v2`) is how a future,
/// incompatible on-disk format would force re-derivation of a new key.
const HKDF_SALT: &[u8] = b"bucketcat-connections-v1";

/// HKDF "info" context, kept distinct from the salt so other keys could be
/// derived from the same machine id in the future without colliding with
/// this one.
const HKDF_INFO: &[u8] = b"bucketcat-connections-store-key";

/// AES-GCM standard nonce length in bytes.
const NONCE_LEN: usize = 12;

/// Derive this machine's deterministic 256-bit store encryption key.
///
/// Fails closed: if `machine_uid::get()` can't read a machine id (unusual
/// platforms/sandboxes), this returns `AppError::KeyDerivationFailed`
/// rather than silently falling back to some fixed, source-visible IKM --
/// see the module doc comment for why a fallback constant here would be
/// worse than refusing to encrypt.
pub fn derive_key() -> AppResult<[u8; 32]> {
    let ikm = machine_uid::get().map_err(|_| AppError::KeyDerivationFailed)?;

    let hk = Hkdf::<Sha256>::new(Some(HKDF_SALT), ikm.as_bytes());
    let mut okm = [0u8; 32];
    hk.expand(HKDF_INFO, &mut okm)
        .expect("32 bytes is always a valid HKDF-SHA256 output length");
    Ok(okm)
}

/// Encrypt `plaintext` under `key`, returning `nonce (12 bytes) || ciphertext`.
pub fn encrypt(key: &[u8; 32], plaintext: &[u8]) -> AppResult<Vec<u8>> {
    let cipher = Aes256Gcm::new(&Key::<Aes256Gcm>::from(*key));

    let nonce_bytes: [u8; NONCE_LEN] = rand::random();
    let nonce = AeadNonce::<Aes256Gcm>::from(nonce_bytes);

    let ciphertext = cipher
        .encrypt(&nonce, plaintext)
        .map_err(|_| AppError::Internal {
            message: "failed to encrypt local connection store".to_string(),
        })?;

    let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Decrypt a `nonce || ciphertext` blob produced by [`encrypt`]. Any
/// malformed input or failed authentication returns
/// `AppError::DecryptFailed`.
pub fn decrypt(key: &[u8; 32], data: &[u8]) -> AppResult<Vec<u8>> {
    if data.len() < NONCE_LEN {
        return Err(AppError::DecryptFailed);
    }
    let (nonce_slice, ciphertext) = data.split_at(NONCE_LEN);
    let nonce_bytes: [u8; NONCE_LEN] = nonce_slice
        .try_into()
        .map_err(|_| AppError::DecryptFailed)?;
    let nonce = AeadNonce::<Aes256Gcm>::from(nonce_bytes);

    let cipher = Aes256Gcm::new(&Key::<Aes256Gcm>::from(*key));
    cipher
        .decrypt(&nonce, ciphertext)
        .map_err(|_| AppError::DecryptFailed)
}

/// Encrypted on-disk store for [`Connection`] records.
pub struct SecureStore {
    pub path: PathBuf,
}

impl SecureStore {
    /// Load and decrypt all saved connections. A missing (or empty) file
    /// is treated as "no connections saved yet" rather than an error.
    pub fn load(&self) -> AppResult<Vec<Connection>> {
        let data = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        if data.is_empty() {
            return Ok(Vec::new());
        }

        let key = derive_key()?;
        let plaintext = decrypt(&key, &data)?;
        serde_json::from_slice(&plaintext).map_err(|_| AppError::DecryptFailed)
    }

    /// Encrypt and atomically persist `connections`: write to a sibling
    /// `<path>.tmp` file, then `fs::rename` it over `self.path`, so a
    /// crash mid-write can never leave a half-written store in place. On
    /// unix, the tmp file is created with mode `0600` (owner read/write
    /// only) from the moment it's opened, before any ciphertext is
    /// written, and `fs::rename` preserves that mode onto the final path.
    pub fn save(&self, connections: &[Connection]) -> AppResult<()> {
        let key = derive_key()?;
        let plaintext = serde_json::to_vec(connections).map_err(|e| AppError::Internal {
            message: e.to_string(),
        })?;
        let ciphertext = encrypt(&key, &plaintext)?;

        let tmp_path = self.tmp_path();
        if let Err(e) = write_tmp_file(&tmp_path, &ciphertext) {
            let _ = fs::remove_file(&tmp_path);
            return Err(e.into());
        }
        if let Err(e) = fs::rename(&tmp_path, &self.path) {
            let _ = fs::remove_file(&tmp_path);
            return Err(e.into());
        }
        Ok(())
    }

    /// Sibling `<path>.tmp` used as the atomic-write staging file.
    fn tmp_path(&self) -> PathBuf {
        let mut tmp_os = self.path.clone().into_os_string();
        tmp_os.push(".tmp");
        PathBuf::from(tmp_os)
    }
}

/// Create (or truncate) `path` and write `data` to it. On unix the file is
/// opened with mode `0600` set at creation time via `OpenOptions::mode`, so
/// there is no window where the secret payload sits in a file with
/// world/group-readable permissions -- the mode is fixed before any byte of
/// ciphertext is written, not applied afterward with a separate
/// `set_permissions` call. On non-unix platforms this is a plain create +
/// write with the platform default permissions.
fn write_tmp_file(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    let mut open_options = OpenOptions::new();
    open_options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    open_options.mode(0o600);

    let mut file = open_options.open(path)?;
    file.write_all(data)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::model::ConnectionDto;

    fn sample_connection(id: &str) -> Connection {
        Connection {
            id: id.to_string(),
            provider: "aws".to_string(),
            name: "test bucket".to_string(),
            endpoint: "https://s3.amazonaws.com".to_string(),
            region: "us-east-1".to_string(),
            access_key_id: "AKIAEXAMPLE".to_string(),
            secret_access_key: "super-secret-value".to_string(),
            default_bucket: Some("my-bucket".to_string()),
        }
    }

    #[test]
    fn encrypt_decrypt_round_trip_preserves_connections() {
        let key = [7u8; 32];
        let connections = vec![sample_connection("c1"), sample_connection("c2")];
        let plaintext = serde_json::to_vec(&connections).expect("serialize");

        let ciphertext = encrypt(&key, &plaintext).expect("encrypt should succeed");
        let decrypted = decrypt(&key, &ciphertext).expect("decrypt should succeed");

        let round_tripped: Vec<Connection> =
            serde_json::from_slice(&decrypted).expect("deserialize");
        assert_eq!(round_tripped, connections);
    }

    #[test]
    fn ciphertext_differs_from_plaintext() {
        let key = [7u8; 32];
        let plaintext = br#"{"hello":"world"}"#.to_vec();

        let ciphertext = encrypt(&key, &plaintext).expect("encrypt should succeed");

        assert_ne!(ciphertext, plaintext);
        // Sanity: ciphertext also carries the 12-byte nonce prefix plus a
        // 16-byte GCM auth tag, so it must be longer than the plaintext.
        assert!(ciphertext.len() >= plaintext.len() + NONCE_LEN);
    }

    #[test]
    fn tampered_ciphertext_fails_to_decrypt() {
        let key = [7u8; 32];
        let plaintext = b"top secret payload".to_vec();
        let mut ciphertext = encrypt(&key, &plaintext).expect("encrypt should succeed");

        let last = ciphertext.len() - 1;
        ciphertext[last] ^= 0xFF;

        let err = decrypt(&key, &ciphertext).expect_err("tampered data must not decrypt");
        assert_eq!(err.code(), "local/decrypt-failed");
    }

    #[test]
    fn truncated_ciphertext_fails_to_decrypt_without_panicking() {
        let key = [7u8; 32];
        // Shorter than a single nonce -- must not panic on slicing.
        let data = vec![1, 2, 3];

        let err = decrypt(&key, &data).expect_err("truncated data must not decrypt");
        assert_eq!(err.code(), "local/decrypt-failed");
    }

    #[test]
    fn connection_dto_has_no_secret_field() {
        let conn = sample_connection("c1");
        let dto = ConnectionDto::from(&conn);
        let json = serde_json::to_string(&dto).expect("serialize dto");

        // Runtime backstop for what the type system already guarantees:
        // `ConnectionDto` has no `secret_access_key` field at all.
        assert!(!json.contains("secret"));
        assert!(!json.contains(&conn.secret_access_key));
        assert_eq!(dto.id, "c1");
        assert_eq!(dto.access_key_id, conn.access_key_id);
    }

    #[test]
    fn save_then_load_round_trips_and_cleans_up_tmp_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("connections.enc");
        let store = SecureStore { path: path.clone() };

        let connections = vec![sample_connection("c1"), sample_connection("c2")];
        store.save(&connections).expect("save should succeed");

        let mut tmp_os = path.clone().into_os_string();
        tmp_os.push(".tmp");
        let tmp_path = PathBuf::from(tmp_os);
        assert!(!tmp_path.exists(), "tmp file must not remain after save");
        assert!(path.exists(), "final file must exist after save");

        let loaded = store.load().expect("load should succeed");
        assert_eq!(loaded, connections);
    }

    #[cfg(unix)]
    #[test]
    fn saved_file_has_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("connections.enc");
        let store = SecureStore { path: path.clone() };

        store
            .save(&[sample_connection("c1")])
            .expect("save should succeed");

        let mode = fs::metadata(&path)
            .expect("saved file must exist")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "store file must be owner-only (0600)");
    }

    #[test]
    fn load_missing_file_returns_empty_vec() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("does-not-exist.enc");
        let store = SecureStore { path };

        let loaded = store.load().expect("missing file should load as empty");
        assert!(loaded.is_empty());
    }

    #[test]
    fn load_empty_file_returns_empty_vec() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("empty.enc");
        fs::write(&path, []).expect("create empty file");
        let store = SecureStore { path };

        let loaded = store.load().expect("empty file should load as empty");
        assert!(loaded.is_empty());
    }
}
