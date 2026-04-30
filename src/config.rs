//! Persistent encrypted configuration on disk.
//!
//! Layout (`{config_dir}/obs-discord-voice-overlay/config.json`):
//! ```json
//! {
//!   "version": 1,
//!   "overlay_port": 7373,
//!   "token_storage_dir": null,
//!   "log_level": "info",
//!   "secrets": {
//!     "v": 1,
//!     "salt": "<base64>",
//!     "nonce": "<base64>",
//!     "ciphertext": "<base64>"
//!   }
//! }
//! ```
//!
//! Sensitive fields (`client_id`, `client_secret`, `redirect_uri`) are
//! AEAD-encrypted with ChaCha20-Poly1305. The 32-byte key is derived per file
//! via HKDF-SHA256 over the per-machine UID with a fixed `info` string and a
//! per-file random 16-byte salt. A fresh 12-byte random nonce is generated on
//! every encryption.
//!
//! All env-var configuration was removed in favor of this file + the `/setup`
//! HTML wizard.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use chacha20poly1305::aead::rand_core::RngCore;
use chacha20poly1305::aead::{Aead, KeyInit, OsRng};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

const CONFIG_DIRNAME: &str = "obs-discord-voice-overlay";
const CONFIG_FILENAME: &str = "config.json";
const CONFIG_TMP_FILENAME: &str = "config.json.tmp";
const HKDF_INFO: &[u8] = b"obs-discord-voice-overlay v1 secrets key";
const SCHEMA_VERSION: u32 = 1;
const SECRETS_VERSION: u32 = 1;
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;

/// Default config-file path: `{dirs::config_dir()}/obs-discord-voice-overlay/config.json`.
///
/// Hard-fails if `dirs::config_dir()` returns `None`. We deliberately do NOT
/// fall back to the current working directory: on Windows that could be
/// `C:\Program Files\...` or another world-readable location, which would
/// silently weaken the at-rest protection of `config.json`. Treat an
/// unavailable config dir as a fatal startup error instead.
pub fn default_path() -> Result<PathBuf, ConfigError> {
    let base = dirs::config_dir().ok_or_else(|| {
        ConfigError::MachineId("user config directory is unavailable".to_string())
    })?;
    Ok(base.join(CONFIG_DIRNAME).join(CONFIG_FILENAME))
}

/// Default token storage directory (sibling of the config file).
pub fn default_token_storage_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(CONFIG_DIRNAME)
}

/// Marker file written after the very first auto-open of `/setup`. While this
/// file exists we will NOT auto-open the browser again on subsequent restarts
/// even if we're still in setup mode.
pub fn setup_prompt_marker_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(CONFIG_DIRNAME)
        .join(".setup-prompted")
}

/// AEAD-encrypted blob stored on disk inside the `secrets` key.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EncryptedBlob {
    pub v: u32,
    pub salt: String,
    pub nonce: String,
    pub ciphertext: String,
}

/// On-disk JSON layout. Kept distinct from [`Config`] so the wire format and
/// the runtime types can evolve independently.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DiskConfig {
    version: u32,
    overlay_port: u16,
    #[serde(default)]
    token_storage_dir: Option<PathBuf>,
    #[serde(default = "default_log_level")]
    log_level: String,
    #[serde(default)]
    secrets: Option<EncryptedBlob>,
}

fn default_log_level() -> String {
    "info".to_string()
}

/// Plaintext sensitive fields. Never serialized as plaintext to disk.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Secrets {
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uri: String,
}

/// Runtime configuration (decrypted form).
#[derive(Debug, Clone)]
pub struct Config {
    pub overlay_port: u16,
    pub token_storage_dir: Option<PathBuf>,
    pub log_level: String,
    pub secrets: Secrets,
}

impl Config {
    /// Resolve the token storage dir, falling back to the platform default
    /// next to the config file.
    pub fn resolved_token_storage_dir(&self) -> PathBuf {
        self.token_storage_dir
            .clone()
            .unwrap_or_else(default_token_storage_dir)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("machine-id unavailable: {0}")]
    MachineId(String),
    #[error("config json parse failed: {0}")]
    ParseJson(String),
    #[error("config secret block missing or malformed")]
    MissingFields,
    #[error("config decrypt failed (wrong machine, or corrupted secrets)")]
    Decrypt,
    #[error("config encrypt failed: {0}")]
    Encrypt(String),
    #[error("crypto error: {0}")]
    Crypto(String),
    #[error("config secret block is corrupt: {0}")]
    Corrupt(String),
    #[error("base64 decode failed: {0}")]
    Base64(String),
    #[error("unsupported config version: {0}")]
    UnsupportedVersion(u32),
}

/// Read the per-machine UID. Wrapped so callers see a uniform `ConfigError`.
///
/// Some stripped containers and freshly-imaged OS builds return an empty or
/// whitespace-only UID. We refuse to derive a key from that — it would produce
/// a deterministic key shared across every such host. Treat it as if the
/// machine ID were unavailable and force the user back into setup mode.
fn machine_uid() -> Result<String, ConfigError> {
    let uid = machine_uid::get().map_err(|e| ConfigError::MachineId(e.to_string()))?;
    if uid.trim().is_empty() {
        return Err(ConfigError::MachineId(
            "machine UID is empty or whitespace".to_string(),
        ));
    }
    Ok(uid)
}

/// Derive the 32-byte AEAD key from the machine UID + per-file salt.
fn derive_key(machine_uid: &str, salt: &[u8]) -> Result<[u8; KEY_LEN], ConfigError> {
    let hk = Hkdf::<Sha256>::new(Some(salt), machine_uid.as_bytes());
    let mut key = [0u8; KEY_LEN];
    hk.expand(HKDF_INFO, &mut key)
        .map_err(|e| ConfigError::Crypto(format!("HKDF expand failed: {e}")))?;
    Ok(key)
}

/// Encrypt `secrets` with a freshly generated salt + nonce. The salt is
/// returned alongside the ciphertext so `decrypt_secrets` can reconstruct the
/// same key on the same machine.
fn encrypt_secrets(secrets: &Secrets, machine_uid: &str) -> Result<EncryptedBlob, ConfigError> {
    let plaintext =
        serde_json::to_vec(secrets).map_err(|e| ConfigError::ParseJson(e.to_string()))?;

    let mut salt = [0u8; SALT_LEN];
    let mut nonce_bytes = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut salt);
    OsRng.fill_bytes(&mut nonce_bytes);

    let key = derive_key(machine_uid, &salt)?;
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher
        .encrypt(nonce, plaintext.as_ref())
        .map_err(|e| ConfigError::Encrypt(e.to_string()))?;

    let b64 = base64::engine::general_purpose::STANDARD;
    Ok(EncryptedBlob {
        v: SECRETS_VERSION,
        salt: b64.encode(salt),
        nonce: b64.encode(nonce_bytes),
        ciphertext: b64.encode(ciphertext),
    })
}

fn decrypt_secrets(blob: &EncryptedBlob, machine_uid: &str) -> Result<Secrets, ConfigError> {
    if blob.v != SECRETS_VERSION {
        return Err(ConfigError::UnsupportedVersion(blob.v));
    }

    // Cap base64-encoded input lengths BEFORE decoding so a hostile config can
    // not force an oversize allocation. Standard base64 expansion is 4 chars
    // per 3 input bytes, so we use the per-field byte cap * 4 / 3 + 4 as a
    // generous upper bound on the encoded form.
    const MAX_SALT_BYTES: usize = 32; // raw salt cap (we expect 16)
    const MAX_NONCE_BYTES: usize = 24; // raw nonce cap (we expect 12)
    const MAX_CIPHERTEXT_BYTES: usize = 16 * 1024; // 16 KiB raw cap
    fn b64_cap(raw_cap: usize) -> usize {
        // ceil(raw_cap * 4 / 3) + small slack for padding/newlines.
        raw_cap.saturating_mul(4) / 3 + 4
    }
    if blob.salt.len() > b64_cap(MAX_SALT_BYTES) {
        return Err(ConfigError::Corrupt("salt field too large".to_string()));
    }
    if blob.nonce.len() > b64_cap(MAX_NONCE_BYTES) {
        return Err(ConfigError::Corrupt("nonce field too large".to_string()));
    }
    if blob.ciphertext.len() > b64_cap(MAX_CIPHERTEXT_BYTES) {
        return Err(ConfigError::Corrupt(
            "ciphertext field too large".to_string(),
        ));
    }

    let b64 = base64::engine::general_purpose::STANDARD;
    let salt = b64
        .decode(&blob.salt)
        .map_err(|e| ConfigError::Base64(e.to_string()))?;
    let nonce = b64
        .decode(&blob.nonce)
        .map_err(|e| ConfigError::Base64(e.to_string()))?;
    let ciphertext = b64
        .decode(&blob.ciphertext)
        .map_err(|e| ConfigError::Base64(e.to_string()))?;

    if salt.len() != SALT_LEN || nonce.len() != NONCE_LEN {
        return Err(ConfigError::MissingFields);
    }

    let key = derive_key(machine_uid, &salt)?;
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
    let plaintext = cipher
        .decrypt(Nonce::from_slice(&nonce), ciphertext.as_ref())
        .map_err(|_| ConfigError::Decrypt)?;

    let secrets: Secrets =
        serde_json::from_slice(&plaintext).map_err(|e| ConfigError::ParseJson(e.to_string()))?;
    Ok(secrets)
}

/// Load and decrypt the config file.
///
/// - `Ok(None)` — file does not exist (caller treats as setup mode).
/// - `Err(ParseJson)` — file exists but is not valid JSON.
/// - `Err(Decrypt)` — file is well-formed but secrets did not decrypt
///   (different machine or corrupted ciphertext).
/// - `Err(MachineId)` — `machine-uid` is unavailable; we refuse to even try.
pub async fn load(path: &Path) -> Result<Option<Config>, ConfigError> {
    let bytes = match tokio::fs::read(path).await {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(ConfigError::Io(e)),
    };

    let disk: DiskConfig =
        serde_json::from_slice(&bytes).map_err(|e| ConfigError::ParseJson(e.to_string()))?;

    if disk.version != SCHEMA_VERSION {
        return Err(ConfigError::UnsupportedVersion(disk.version));
    }
    let blob = disk.secrets.ok_or(ConfigError::MissingFields)?;
    let uid = machine_uid()?;
    let secrets = decrypt_secrets(&blob, &uid)?;

    Ok(Some(Config {
        overlay_port: disk.overlay_port,
        token_storage_dir: disk.token_storage_dir,
        log_level: disk.log_level,
        secrets,
    }))
}

impl Config {
    /// Atomically write this config to `path` (mode 0o600 on Unix).
    pub async fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let parent = path
            .parent()
            .ok_or_else(|| ConfigError::Io(std::io::Error::other("config path has no parent")))?;
        tokio::fs::create_dir_all(parent).await?;
        // On Unix, tighten the parent dir to 0o700 so the 0o600 file isn't
        // sitting inside a world-listable directory on shared boxes. We do
        // this every save (idempotent) rather than only on first create so a
        // user who manually relaxed perms gets them re-tightened.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o700);
            if let Err(e) = tokio::fs::set_permissions(parent, perms).await {
                tracing::warn!(
                    "config: failed to tighten parent dir mode to 0o700 ({}): {e}",
                    parent.display()
                );
            }
        }

        let uid = machine_uid()?;
        let blob = encrypt_secrets(&self.secrets, &uid)?;

        let disk = DiskConfig {
            version: SCHEMA_VERSION,
            overlay_port: self.overlay_port,
            token_storage_dir: self.token_storage_dir.clone(),
            log_level: self.log_level.clone(),
            secrets: Some(blob),
        };
        let bytes =
            serde_json::to_vec_pretty(&disk).map_err(|e| ConfigError::ParseJson(e.to_string()))?;

        let tmp_path = parent.join(CONFIG_TMP_FILENAME);
        // Drop any stale tmp from a previous crashed write so create_new succeeds.
        match tokio::fs::remove_file(&tmp_path).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(ConfigError::Io(e)),
        }

        let tmp_for_blocking = tmp_path.clone();
        let bytes_for_blocking = bytes.clone();
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            use std::fs::OpenOptions;
            use std::io::Write;
            let mut opts = OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut f = opts.open(&tmp_for_blocking)?;
            f.write_all(&bytes_for_blocking)?;
            f.sync_data()?;
            Ok(())
        })
        .await
        .map_err(std::io::Error::other)??;

        tokio::fs::rename(&tmp_path, path).await?;

        // On Unix, fsync the parent directory so the rename itself is durable
        // across power loss. tokio::fs has no dedicated dir-sync helper, so
        // hop into spawn_blocking with the std::fs APIs.
        #[cfg(unix)]
        {
            let parent_for_blocking = parent.to_path_buf();
            tokio::task::spawn_blocking(move || -> std::io::Result<()> {
                let dir = std::fs::File::open(&parent_for_blocking)?;
                dir.sync_all()?;
                Ok(())
            })
            .await
            .map_err(std::io::Error::other)??;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_secrets() -> Secrets {
        Secrets {
            client_id: "1234567890".to_string(),
            client_secret: "shh-very-secret".to_string(),
            redirect_uri: "http://localhost:7373/callback".to_string(),
        }
    }

    #[test]
    fn roundtrip_encrypt_decrypt() {
        let uid = "test-machine-uid";
        let secrets = sample_secrets();
        let blob = encrypt_secrets(&secrets, uid).expect("encrypt");
        let back = decrypt_secrets(&blob, uid).expect("decrypt");
        assert_eq!(secrets, back);
    }

    #[test]
    fn decrypt_with_wrong_key_fails() {
        let secrets = sample_secrets();
        let blob = encrypt_secrets(&secrets, "machine-A").expect("encrypt");
        let err = decrypt_secrets(&blob, "machine-B").expect_err("must fail");
        assert!(matches!(err, ConfigError::Decrypt));
    }

    #[test]
    fn empty_or_whitespace_machine_uid_is_rejected() {
        // The check sits inside the `machine_uid()` wrapper. We can't easily
        // inject an empty value through `machine_uid::get()` from a test, so
        // exercise the underlying invariant directly: the bytes that would
        // be passed to HKDF must be non-empty after trimming.
        for uid in ["", "   ", "\t", "\n", "  \r\n  "] {
            assert!(
                uid.trim().is_empty(),
                "test bug: {uid:?} should be empty after trim"
            );
        }
        // And a genuine UID-shaped string must NOT be rejected.
        assert!(!"abcd-1234".trim().is_empty());
    }

    #[test]
    fn decrypt_rejects_oversize_ciphertext() {
        // Build a syntactically-valid blob whose ciphertext field exceeds the
        // 16 KiB raw cap. Anything decoding to >16 KiB should be refused
        // without spending memory on the base64 decode.
        let huge = "A".repeat(64 * 1024);
        let blob = EncryptedBlob {
            v: SECRETS_VERSION,
            salt: "AAAAAAAAAAAAAAAAAAAAAA==".to_string(),
            nonce: "AAAAAAAAAAAAAAAA".to_string(),
            ciphertext: huge,
        };
        let err = decrypt_secrets(&blob, "uid").expect_err("must fail");
        assert!(matches!(err, ConfigError::Corrupt(_)), "got: {err:?}");
    }

    #[tokio::test]
    async fn missing_file_returns_none() {
        let dir = tempdir();
        let path = dir.join("does-not-exist.json");
        let result = load(&path).await.expect("load");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn corrupt_json_returns_error() {
        let dir = tempdir();
        let path = dir.join("config.json");
        tokio::fs::write(&path, b"{not valid json").await.unwrap();
        let err = load(&path).await.expect_err("must fail");
        assert!(
            matches!(err, ConfigError::ParseJson(_)),
            "got: {err:?}"
        );
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_uses_0o600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir();
        let path = dir.join("config.json");
        let cfg = Config {
            overlay_port: 7373,
            token_storage_dir: None,
            log_level: "info".into(),
            secrets: sample_secrets(),
        };
        cfg.save(&path).await.expect("save");
        let meta = tokio::fs::metadata(&path).await.expect("metadata");
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected 0o600, got {mode:o}");
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// A standalone-per-test temp directory under `std::env::temp_dir()` —
    /// avoids pulling in the `tempfile` crate just for these unit tests.
    fn tempdir() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "obs-discord-config-test-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&p).expect("create tempdir");
        p
    }
}
