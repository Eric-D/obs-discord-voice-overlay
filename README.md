# OBS Discord Voice Overlay

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

A small Rust process that connects to your local Discord client over RPC IPC,
follows your active voice channel, and serves a customizable HTML overlay over
local HTTP/SSE that OBS loads as a Browser Source.

- No bot in the call. No `serenity`/`songbird`. No Gateway.
- Local-only. Nothing is sent off your machine except the OAuth token exchange
  with `discord.com/api/oauth2/token`.
- Three built-in themes (`default`, `minimal`, `neon`), selectable via URL
  query string.

## Discord application setup

1. Open <https://discord.com/developers/applications> and create a new
   application (or reuse an existing one).
2. **General Information** -> copy the **Application ID**. You will paste
   it into the setup wizard below.
3. **OAuth2** -> **Reset Secret** and copy the secret. Treat it like a
   password.
4. **OAuth2** -> **Redirects** -> add a redirect URL. The default expected by
   this overlay is `http://localhost:7373/callback`. Whatever you choose, you
   will paste the same value into the setup wizard. The redirect URI is used
   only as part of the OAuth handshake — the overlay does **not** open a port
   on it. It just has to match between the dev portal and the wizard form.
5. **Bot** tab -> turn **Public Bot OFF**. The overlay does not need a bot
   token; only an OAuth user authorization with the `rpc` and `rpc.voice.read`
   scopes.

## First-run setup

Configuration lives in an encrypted JSON file at:

| Platform | Path |
|---|---|
| Linux   | `~/.config/obs-discord-voice-overlay/config.json` |
| macOS   | `~/Library/Application Support/obs-discord-voice-overlay/config.json` |
| Windows | `%APPDATA%\obs-discord-voice-overlay\config.json` |

**There are no environment variables.** All configuration is sourced from
this file via a built-in HTML wizard.

1. Launch the binary (`cargo run --release` from a checkout, or double-click
   the `.exe` on Windows).
2. On first launch the tray icon will be **amber** and your default browser
   will auto-open to `http://localhost:7373/setup`.
   - If the browser does not auto-open (or you are running headless on Linux),
     visit that URL manually.
   - You can also right-click the tray icon and pick **Configure...** at any
     time.
3. Fill the form:
   - **Application ID** — from your Discord developer portal.
   - **Client Secret** — from OAuth2 -> Reset Secret. Stored encrypted.
   - **Redirect URI** — must exactly match a value listed under OAuth2
     Redirects in the dev portal. Default is
     `http://localhost:7373/callback`.
   - **Overlay Port** — local HTTP port, default `7373`. Anything in
     `[1024, 65535]` is accepted.
4. Click **Save**. The page confirms with "Saved — please restart the app."
5. Restart the binary. The first restart triggers a Discord consent prompt
   inside your Discord client — click **Authorize**. The OAuth token is
   cached at the same directory; subsequent restarts skip the prompt.

### Reconfiguring later

Right-click the tray icon and pick **Configure...** to reopen the wizard.
The port and redirect URI pre-fill with the current values; the
Application ID and Client Secret fields are intentionally left blank — you
must paste them again. Save, then restart the binary.

### Encryption-at-rest details

- The `client_id`, `client_secret`, and `redirect_uri` are encrypted with
  ChaCha20-Poly1305 (AEAD). The 32-byte key is derived via HKDF-SHA256 from
  this computer's machine identifier (Linux `/etc/machine-id`, Windows
  `HKLM\Software\Microsoft\Cryptography\MachineGuid`, macOS
  `IOPlatformUUID`) plus a per-file 16-byte random salt.
- A 12-byte random nonce is generated for every save.
- **Machine-bound caveat**: copying `config.json` to a different computer
  will not decrypt — that machine's UID is different, so the derived key is
  different. Re-run the wizard on the new machine instead.
- The file mode is `0o600` on Unix. On Windows the file lives in
  per-user `%APPDATA%`, which is already access-controlled by the OS.
- Non-sensitive fields (`overlay_port`, `log_level`, `token_storage_dir`)
  are stored as plaintext JSON.

#### Threat model — what this encryption does and does NOT protect

This is **not confidentiality from a local attacker on the same machine**.
The machine UID we derive the key from is *not* a secret on any of the
supported platforms:

- Linux: `/etc/machine-id` is mode `0644` and readable by any local user.
- Windows: any local user can read
  `HKLM\Software\Microsoft\Cryptography\MachineGuid`.
- macOS: `IOPlatformUUID` is similarly accessible to local processes.

What the encryption *does* give you:

- **Portability protection** — a copy of `config.json` exfiltrated to a
  different machine will not decrypt, because that host's machine UID is
  different and so the HKDF-derived key is different. Re-run the wizard on
  the new host.
- **Tamper detection** — ChaCha20-Poly1305 is an AEAD; any bit-flip in the
  on-disk ciphertext will be detected on next load and the binary will fall
  back into setup mode rather than authenticate with corrupted credentials.

What it does **not** give you:

- Confidentiality against a local attacker who already has read access to
  your user's filesystem. They can read `/etc/machine-id` (or the platform
  equivalent), apply the same HKDF derivation against the on-disk salt, and
  decrypt `config.json`. If you need that level of protection, the secret
  must live in an OS-managed keystore (DPAPI on Windows, Keychain on macOS,
  Secret Service / `libsecret` on Linux). That is **deferred** for now;
  filing an issue is welcome if you need it.

## OBS Browser Source

Add a Browser Source in OBS with one of these URLs:

| URL | Theme |
|---|---|
| `http://localhost:7373/` | `default` (compact square avatars + name pill) |
| `http://localhost:7373/?theme=minimal` | `minimal` (transparent, names only) |
| `http://localhost:7373/?theme=neon` | `neon` (bright, gamer aesthetic) |
| `http://localhost:7373/?theme=does-not-exist` | falls back to `default` (HTTP 200) |

### URL options

Beyond `theme`, the URL accepts query parameters that customize behavior client-side. Combine them with `&`:

| Option | Values | Default | Effect |
|---|---|---|---|
| `theme` | `default` / `minimal` / `neon` | `default` | Visual theme (server-side asset routing) |
| `speaking_only` | `0` / `1` | `0` | When `1`, hide non-speaking participants — only currently-speaking cards render |
| `hide` | comma-separated user IDs | (empty) | Skip rendering specified users entirely (typical use: hide yourself) |
| `size` | integer 24-256 | theme default (80px) | Override avatar size in pixels (`default` and `neon` only — no effect on `minimal`) |

Examples:

- `http://localhost:7373/?speaking_only=1` — only show currently-speaking participants
- `http://localhost:7373/?theme=neon&speaking_only=1&size=96` — neon theme, speakers only, larger avatars
- `http://localhost:7373/?hide=4242424242,1111111111` — hide two specific users by Discord user ID

Discord user IDs are 18-19 digit snowflakes. To find yours: enable Developer Mode in Discord (User Settings → Advanced → Developer Mode), then right-click your name → "Copy User ID".

Recommended OBS settings:

- **Width / Height**: whatever fits your scene; 800x200 is a reasonable
  starting point for the default theme, smaller for `minimal`.
- **Custom CSS**: leave empty — themes ship their own.
- **Refresh browser when scene becomes active**: optional.
- The browser auto-reconnects to the SSE stream after any drop, so you do not
  need to refresh OBS when restarting the binary or when Discord briefly
  disconnects.

## Tray application

When the binary is launched, it places an icon in the OS system tray (or
notification area / menu bar) so it can run without a visible console
window. The icon color tracks the live Discord state:

| Icon color | Meaning |
|---|---|
| Amber (`#faa61a`) | Configuration is missing/unreadable — visit `/setup` |
| Grey (`#71808a`) | Discord client is not reachable over IPC |
| Discord blurple (`#5865f2`) | Discord connected, you are not in a voice channel |
| Discord green (`#57f287`) | Discord connected, you are in a voice channel |

The icon swaps within ~1s of the underlying state change, and the hover
tooltip is refreshed in lock-step (`Discord overlay - Idle`, etc.).

### Right-click menu

| Item | Action |
|---|---|
| **Open overlay preview** | Opens `http://localhost:{OVERLAY_PORT}/` in your default browser. Same URL you put in the OBS Browser Source. |
| **Auto-start on boot** | Toggles a per-user OS autostart entry. The checkmark always reflects the **current OS state**, queried each time the menu is opened — there is no separate config file. On Windows this writes to `HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Run`; on macOS it uses Login Items; on Linux it writes a `~/.config/autostart/*.desktop` entry (when available). |
| **Configure...** | Opens `http://localhost:{OVERLAY_PORT}/setup` in your default browser, where you can change the encrypted Application ID / Client Secret / Redirect URI / port. Always present, even when the overlay is fully configured. |
| **Quit** | Cancels the shared cancellation token, drains the HTTP server and IPC driver, then exits. |

### Console window behavior

- **Windows release builds** (`cargo run --release` / `cargo build --release`):
  the console window is **hidden** via `windows_subsystem = "windows"`.
  Logs still flow if a parent shell exists; otherwise tracing output is
  swallowed. This matches the streamer-friendly "launch and forget"
  experience.
- **Debug builds** (`cargo run`): the console **stays visible**. Ctrl-C in
  that console triggers the same graceful shutdown path as the tray "Quit"
  item.

### Linux dev environments

The tray UI is built only on Windows and macOS. On Linux the binary still
runs, but it does not place an icon in the tray; the existing IPC + HTTP
+ SSE stack works unchanged, and Ctrl-C is the only way to exit. The user
runtime for this project is Windows; the Linux build path exists primarily
for `cargo check` / `cargo test` on dev VMs that lack a system tray daemon.

## Architecture

```
Discord client  <--RPC IPC-->  obs-discord-voice-overlay  --HTTP/SSE-->  OBS Browser Source
                              (Rust process, this crate)
```

- `src/ipc.rs` — cross-platform IPC dispatcher (Unix sockets / Windows named
  pipes). 8-byte LE header + JSON body. Reconnect with exponential backoff.
- `src/oauth.rs` — `AUTHORIZE` -> token exchange -> `AUTHENTICATE`. Token
  persisted to `{TOKEN_STORAGE_DIR}/token.json` with mode `0600` on Unix.
- `src/events.rs` — typed `VoiceState` / `Speaking` / `ChannelSelect` payloads
  + `SUBSCRIBE` helpers.
- `src/state.rs` — `OverlayState` with `participants` map + `speaking` set,
  exposed via `tokio::sync::watch` (snapshots) and `broadcast` (deltas).
- `src/themes.rs` — compile-time theme registry; each theme bundles
  `overlay.html` + `style.css` + `app.js` via `include_str!`.
- `src/web.rs` — axum router. `GET /` resolves `?theme=`; `GET
  /themes/<name>/{style.css,app.js}`; `GET /events` SSE with auto keep-alive.
- `src/main.rs` — wires it all together. Builds the multi-thread tokio
  runtime explicitly, spawns the IPC + HTTP lifecycle on it, and runs the
  tray UI on the main thread. Quit / Ctrl-C share a single
  `tokio_util::sync::CancellationToken`.
- `src/tray.rs` — system tray icon, menu, and `tao` event loop. No-op stub
  on Linux.
- `src/icons.rs` — programmatically-generated 16×16 RGBA icons (one per
  tray state). Never loaded from disk.
- `src/autostart.rs` — thin wrapper around the `auto-launch` crate. Resolves
  the binary path via `std::env::current_exe()`.

## Security notes

- The Discord client secret and the cached OAuth tokens never leave the Rust
  process. The browser only sees voice-channel display data (display name,
  avatar URL, mute/deaf flags, speaking flag). The client secret is also
  encrypted at rest with a machine-bound key — see "Encryption-at-rest
  details" above.
- Avatar URLs use the public Discord CDN (`cdn.discordapp.com`), so no auth
  is needed to fetch them from the browser.
- The HTTP server binds to `127.0.0.1` only — it is not reachable from the
  network.

## Troubleshooting

- **"No Discord IPC endpoint found"**: the Discord client isn't running, or
  it's running in an isolated environment (Flatpak, Snap) that this overlay's
  prefix list doesn't cover. Linux paths probed:
  `$XDG_RUNTIME_DIR/{,snap.discord/,app/com.discordapp.Discord/}discord-ipc-{0..9}`.
- **Consent prompt never appears**: confirm the Application ID and Client
  Secret belong to the same application, and that the Redirect URI in the
  setup wizard matches one listed under OAuth2 -> Redirects in the dev
  portal. Re-run the wizard via the tray's **Configure...** menu item if
  in doubt.
- **Tray icon is amber after restart**: the on-disk config could not be
  decrypted. The most common cause is that you copied `config.json` from
  another machine — encryption is bound to the local machine ID. Click
  **Configure...** to re-enter your credentials.
- **OBS Browser Source shows nothing when not in a voice channel**: that is
  the intended empty-state behavior. Join a voice channel and cards will
  appear.

## Releases

Pre-built Windows binaries are produced by GitHub Actions, **strictly
on a SemVer-shaped tag push** (`vMAJOR.MINOR.PATCH`, e.g. `v1.0.0`).
Tags that do not match the pattern `v[0-9]+.[0-9]+.[0-9]+` are ignored —
no pre-release suffixes (`v1.0.0-alpha`), no uppercase `V`, no manual
workflow trigger.

To cut a release:

1. Bump the `version` in `Cargo.toml` (`cargo set-version` or by hand).
2. Commit, tag, and push:
   ```sh
   git commit -am "chore(release): v1.0.0"
   git tag v1.0.0
   git push && git push --tags
   ```
3. The `Release` workflow (`.github/workflows/release.yml`) builds
   `target/release/obs-discord-voice-overlay.exe` on `windows-latest`
   and attaches **two assets** to the GitHub release:
   - `obs-discord-voice-overlay-v1.0.0-x86_64-pc-windows-msvc.exe` —
     the standalone binary, downloadable directly.
   - `obs-discord-voice-overlay-v1.0.0-x86_64-pc-windows-msvc.zip` —
     the same binary bundled with `README.md`, `LICENSE`, and
     `docs/ARCHITECTURE.md`.

   Release notes are auto-generated from the commit log between tags.

The CI workflow (`.github/workflows/ci.yml`) runs on every push and pull
request: `cargo check`, `cargo clippy --all-targets -- -D warnings`, and
`cargo test --all-targets` on Linux, plus `cargo build --release` and
`cargo test` on Windows.

> **Note**: this repository is hosted on GitLab; the workflows assume the
> source is also pushed (or mirrored) to a GitHub repository. To use these
> workflows, create a GitHub mirror (or migrate) and the YAML files in
> `.github/workflows/` will be picked up automatically. If you would prefer
> a GitLab CI configuration instead, the equivalent `.gitlab-ci.yml` is
> straightforward to derive — open an issue or ask.

## License

[MIT](LICENSE) © Eric-D
