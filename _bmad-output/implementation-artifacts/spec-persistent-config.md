---
title: 'Persistent encrypted config + setup wizard'
type: 'feature'
created: '2026-04-30'
status: 'done'
context:
  - '_bmad-output/implementation-artifacts/spec-discord-voice-overlay.md'
  - '_bmad-output/implementation-artifacts/spec-desktop-tray-app.md'
baseline_commit: 'ee9e0c9e9434d01bd51d6ca19663b4236edb9925'
---

<frozen-after-approval reason="human-owned intent — do not modify unless human renegotiates">

## Intent

**Problem:** Configuration currently lives in 6 environment variables that the user has to set before every launch. Secrets land in shell history, there's no UX to update them, and the streamer-friendly "double-click and forget" workflow is broken on Windows release where there's no console.

**Approach:** Replace env-var config with a persistent JSON file at `{config_dir}/obs-discord-voice-overlay/config.json`. Sensitive fields (`client_id`, `client_secret`, `redirect_uri`) are AEAD-encrypted with a key derived from a per-machine identifier via HKDF-SHA256. Non-sensitive fields stay plaintext. (Re)configuration goes through an HTML wizard served at `/setup`. A new `NeedsSetup` tray state surfaces when config is missing or undecryptable.

## Boundaries & Constraints

**Always:**
- Config path: `dirs::config_dir()/obs-discord-voice-overlay/config.json`, mode `0o600` on Unix, atomic write (`.tmp` → fsync → rename).
- AEAD: `ChaCha20-Poly1305`. 32-byte key derived per-config via HKDF-SHA256 over the machine UID with fixed info string and a per-file 16-byte random salt stored next to the ciphertext. 12-byte random nonce per encryption.
- Machine UID via `machine-uid` (Linux `/etc/machine-id`, Windows `HKLM\...\Cryptography\MachineGuid`, macOS `IOPlatformUUID`). If unavailable: refuse to read/write secrets and stay in setup mode with an explicit banner.
- Schema (versioned): `{ "version": 1, "overlay_port": 7373, "token_storage_dir": null, "log_level": "info", "secrets": { "v": 1, "salt": "<b64>", "nonce": "<b64>", "ciphertext": "<b64>" } }`.
- Setup mode triggers on missing config, decrypt failure, JSON parse failure, OR machine-UID-unavailable: HTTP serves only `/setup`; every other path returns HTTP 503 pointing at `/setup`; IPC/OAuth do NOT start; tray icon = `NeedsSetup` (amber `#faa61a`).
- On decrypt failure, delete cached `{TOKEN_STORAGE_DIR}/token.json` before entering setup mode (the `client_secret` may have changed).
- "Configure..." menu item is always present (above "Quit"); opens `http://localhost:{OVERLAY_PORT}/setup` via `open::that`.
- Wizard fields: Application ID (text), Client Secret (password, blank on reconfigure), Redirect URI (text, default `http://localhost:1337/callback`), Overlay Port (number, default 7373). Submit writes atomically; success page says "Saved — please restart the app." No hot-reload in v1.

**Ask First:**
- Auto-open browser to `/setup` on first launch (zero-config UX) vs require user to right-click tray. **Propose**: auto-open on first launch only.
- Reconfigure form pre-filling: prefill `port` and `redirect_uri` with current values; leave `client_id` and `client_secret` blank (must be re-entered). **Propose** as stated.
- `--reconfigure` CLI flag — **propose**: not for v1, the tray menu + `/setup` URL are sufficient.

**Never:**
- No environment variables read for any config (entire `read_env` removed).
- No master password / passphrase prompt — encryption uses only the machine UID.
- No remote sync / cloud config.
- No native GUI window — only the HTML wizard.
- No second binary; this modifies the existing crate's startup + adds a config module.

## I/O & Edge-Case Matrix

| Scenario | Input / State | Expected Output / Behavior | Error Handling |
|---|---|---|---|
| Cold start, no config file | Binary launched fresh | Setup mode: HTTP up, `/setup` serves form, tray shows NeedsSetup, browser auto-opens to `/setup` once | Port already bound → log + tray error variant |
| Cold start, valid config | File exists, decrypts successfully | Normal flow: load config, start IPC + OAuth, tray shows Idle/InVoice | Token expired/revoked → existing refresh path applies |
| Cold start, decrypt fails | Wrong machine, corrupt secrets | Delete `token.json`; enter setup mode; warn-log with reason | N/A |
| Cold start, JSON corrupt OR machine UID unavailable | Truncated config / missing OS UID source | Same as missing → setup mode; banner explains cause; reads/writes refuse if UID gone | warn-log |
| `GET /setup` | User opens URL | HTML form: port + redirect_uri prefilled with current values, secret fields blank, encryption notice visible | N/A |
| `POST /setup` valid | Form submission with all required fields | Encrypt sensitive trio, atomic write 0o600, response page "Saved — please restart the app." | N/A |
| `POST /setup` invalid | Missing/empty `client_id` or `client_secret` | HTTP 400, form re-rendered with error message highlighted | N/A |
| Tray "Configure..." clicked | Menu event | `open::that("http://localhost:{port}/setup")` | `open` failure logs, no further action |
| Other routes in setup mode | `GET /`, `GET /events`, etc. | HTTP 503 with body referencing `/setup` | N/A |
| Reconfigure: new `client_secret` written | `POST /setup` while running | New blob persisted; restart required; on next launch the cached `token.json` is deleted because the secret changed | N/A |

</frozen-after-approval>

## Code Map

- `Cargo.toml` -- add `chacha20poly1305 = "0.10"`, `hkdf = "0.12"`, `sha2 = "0.10"`, `machine-uid = "0.5"`, `base64 = "0.22"`.
- `src/config.rs` -- new: `Config`, `Secrets`, `EncryptedBlob`, `ConfigError`; `load(path) -> Result<Option<Config>, _>` (`Ok(None)` = missing), `save(&self, path)`, `default_path()`, key derivation, encrypt/decrypt helpers.
- `src/main.rs` -- `read_env` replaced by `config::load`. On `Ok(None)` / decrypt error → setup mode: skip IPC/OAuth, HTTP-only, tray `NeedsSetup`; on decrypt error also delete cached token. Auto-open browser on first launch.
- `src/web.rs` -- `GET /setup` (embedded HTML form), `POST /setup` (validate + encrypt + write); 503 fallback for non-`/setup` routes when `setup_mode == true`. HTML includes the encryption-at-rest notice.
- `src/tray.rs` -- "Configure..." menu item (above Quit) → `open::that(setup_url)`; honor new `NeedsSetup` icon.
- `src/icons.rs` -- `NeedsSetup` amber `#faa61a` color.
- `src/state.rs` -- `TrayState::NeedsSetup`; `OverlayStore::set_needs_setup(bool)` overrides connected/channel mapping.
- `src/oauth.rs` -- consume `&Config` instead of env-derived struct.
- `README.md` -- replace env-var section with setup-wizard walkthrough + machine-bound caveat.

## Tasks & Acceptance

**Execution:**
- [x] `Cargo.toml` -- add crypto + machine-uid + base64 deps
- [x] `src/config.rs` -- struct, load/save, HKDF + ChaCha20-Poly1305, atomic 0o600 write, salt+nonce per encryption
- [x] `src/icons.rs` -- `NeedsSetup` amber variant
- [x] `src/state.rs` -- `TrayState::NeedsSetup` + setup-mode override on `OverlayStore`
- [x] `src/web.rs` -- `GET /setup` + `POST /setup` handlers, embedded HTML form, 503 fallback in setup mode
- [x] `src/tray.rs` -- "Configure..." menu item with `open::that`, `NeedsSetup` icon support
- [x] `src/main.rs` -- remove `read_env`, replace with `config::load`, setup-mode branch, token deletion on decrypt failure, auto-open browser on first launch
- [x] `src/oauth.rs` -- consume `Config` (or adapter) instead of env-derived struct
- [x] `README.md` -- replace env-var section with setup wizard walkthrough; document encrypted-at-rest + machine-bound caveat
- [x] Unit tests: `config::roundtrip_encrypt_decrypt`, `config::decrypt_with_wrong_key_fails`, `config::missing_file_returns_none`, `config::corrupt_json_returns_none_with_warn`, `config::write_uses_0o600` (Unix only)

**Acceptance Criteria:**
- Given no config file, when the binary launches fresh, then HTTP listens on the configured port, IPC/OAuth do NOT start, the tray icon shows `NeedsSetup` (amber), the default browser auto-opens `http://localhost:{port}/setup`, and any non-`/setup` request returns HTTP 503.
- Given the user fills the wizard with a valid client_id, client_secret, redirect_uri, and port and clicks Save, when the POST handler runs, then `config.json` is written atomically with mode `0o600` (Unix), the secrets block decrypts to the submitted values on the same machine, and the response is a success page instructing to restart.
- Given a valid config exists, when the binary restarts, then the IPC and OAuth flows start as before and the tray shows `Idle` or `InVoice` depending on Discord state.
- Given a config file copied from another machine (different machine UID), when the binary launches, then decryption fails, the cached `token.json` is removed, the binary enters setup mode with a warn log, and `/setup` is reachable.
- Given the tray icon is present, when the user clicks "Configure...", then the default browser opens to `http://localhost:{port}/setup` and the form pre-fills `port` and `redirect_uri` with current values while leaving `client_id` and `client_secret` blank.
- Given the user submits the form with an empty `client_id`, when the POST handler runs, then the response is HTTP 400 and the form is re-rendered with a clear error indicator; no file is written.
- Given env vars `DISCORD_CLIENT_ID`/`DISCORD_CLIENT_SECRET` are set in the shell, when the binary launches, then they are ignored (the env-var loader is gone) and config is sourced exclusively from `config.json`.
- Given `machine-uid` returns an error, when the binary launches, then setup mode is entered with a tray tooltip / log mentioning machine-id unavailable; encryption is never attempted with a degraded key.

## Design Notes

- HKDF: `Hkdf::<Sha256>::new(Some(&salt), machine_uid.as_bytes()).expand(b"obs-discord-voice-overlay v1 secrets key", &mut key)`.
- Encrypt: `Secrets { client_id, client_secret, redirect_uri }` → `serde_json::to_vec` → ChaCha20-Poly1305 with 12-byte random nonce → base64 ciphertext (AEAD tag included).
- Atomic save mirrors token.json: `OpenOptions::create_new + mode(0o600)`, `write_all`, `sync_data`, `tokio::fs::rename`.
- `setup_mode: Arc<AtomicBool>` on `AppState`; one middleware short-circuits to 503 except for `/setup` routes.
- HTML wizard inlined as `&'static str` in `src/web.rs`; minimal CSS; explicit notice that values are encrypted at rest with a machine-bound key.

## Verification

**Commands:**
- `cargo check` -- expected: clean.
- `cargo clippy --all-targets -- -D warnings` -- zero warnings.
- `cargo test` -- existing 41 tests + new config tests pass.

**Manual checks (Windows host):**
- Delete `%APPDATA%\obs-discord-voice-overlay\config.json` (if present), launch the binary → tray shows amber, browser opens to `/setup`.
- Fill the form with real Discord app values, submit → success page; verify `config.json` exists and is JSON with a `secrets` block whose values are base64.
- Restart the binary → IPC + OAuth start as before; tray shows `Idle` then `InVoice` after joining a voice channel.
- Click "Configure..." in the tray → browser opens; verify port + redirect_uri are pre-filled, secret fields blank.
- Copy `config.json` to a different Windows machine and launch the binary there → setup mode resumes, log mentions decrypt failure.
- Set `DISCORD_CLIENT_ID=xxx` in the shell and launch → confirm env var is ignored (config is sourced only from disk).

## Suggested Review Order

**Entry & lifecycle**

- Sync main: parses no env, calls `config::load`, branches to setup mode on missing/decrypt/parse error.
  [`main.rs:57`](../../src/main.rs#L57)

- `async fn run`: token invalidation parity (Decrypt + ParseJson + UnsupportedVersion all delete cached `token.json`); marker-file invariant (deleted when `config.json` is absent).
  [`main.rs:249`](../../src/main.rs#L249)

**Crypto module (this spec's heart)**

- `decrypt_secrets`: input length caps before base64 decode, AEAD verify, full chain.
  [`config.rs:210`](../../src/config.rs#L210)

- `encrypt_secrets`: uses `ConfigError::Encrypt` (distinct from `Decrypt`); fresh 16-byte salt + 12-byte nonce per call.
  [`config.rs:185`](../../src/config.rs#L185)

- `derive_key`: HKDF-SHA256 with `?` propagation into `ConfigError::Crypto` — no panic path.
  [`config.rs:174`](../../src/config.rs#L174)

- `machine_uid`: rejects empty/whitespace UIDs (containers, fresh OS images).
  [`config.rs:163`](../../src/config.rs#L163)

- `Config::save` (atomic write): mode `0o600` on Unix, parent dir `0o700`, `sync_data` + `rename` + parent `sync_all`. See lines 271-onwards in `config.rs` (load path) and the save companion.
  [`config.rs:271`](../../src/config.rs#L271)

- `default_path` returns `Result` — no CWD fallback if `dirs::config_dir()` is `None`.
  [`config.rs:55`](../../src/config.rs#L55)

**Setup wizard + 503 fallback**

- `setup_mode_guard` middleware: short-circuits non-`/setup` routes with HTTP 503 when `setup_mode == true`.
  [`web.rs:99`](../../src/web.rs#L99)

- `setup_post`: CSRF guard via `check_origin`, control-char rejection, redirect_uri scheme restricted to http/https, `save_lock` Mutex serializes concurrent saves, deletes `token.json` on `client_secret` rotation.
  [`web.rs:391`](../../src/web.rs#L391)

- `check_origin`: lightweight CSRF — Host must match `localhost:{port}` / `127.0.0.1:{port}`, Origin must match if present.
  [`web.rs:543`](../../src/web.rs#L543)

- `setup_get`: pre-fills `port` and `redirect_uri`, leaves `client_id` and `client_secret` blank on reconfigure.
  [`web.rs:373`](../../src/web.rs#L373)

- `render_setup_page`: HTML-escaped values, encryption-at-rest notice, dark minimal styling.
  [`web.rs:300`](../../src/web.rs#L300)

- `router`: 64 KiB body limit globally; setup_mode middleware applied last.
  [`web.rs:72`](../../src/web.rs#L72)

**Tray integration**

- `NeedsSetup` icon variant (amber `#faa61a`).
  [`icons.rs:27`](../../src/icons.rs#L27)

- `TrayState::NeedsSetup` and the override on `OverlayStore` (set_needs_setup wins over connected/channel mapping).
  [`state.rs:53`](../../src/state.rs#L53)

- "Configure..." menu item construction (`tray.rs:121` Windows/macOS path) — appended above "Quit", opens `/setup` via `open::that`.
  [`tray.rs:122`](../../src/tray.rs#L122)

**Setup**

- `Cargo.toml`: new crypto deps (`chacha20poly1305`, `hkdf`, `sha2`, `base64`, `machine-uid`) and `tower-http` body limit feature.
  [`Cargo.toml`](../../Cargo.toml)

- README: first-run wizard walkthrough, reconfigure flow, and the explicit threat-model honesty about machine UID being non-secret on Linux/Windows.
  [`README.md`](../../README.md)
