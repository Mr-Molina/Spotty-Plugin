# Spotty-RS Architecture & Release Guide

Spotty-RS replaces the legacy Perl/helper runtime with a 10-crate Rust workspace compiled into a single unified binary: `lms-spotty`.

## 1. 10-Crate Workspace Architecture

| Crate | Responsibility |
|---|---|
| `crates/spotty-types` | Shared domain types (`SpotifyUri`, `Bitrate`, `IconCode`, `SpottyError`, `Result`) |
| `crates/spotty-cache` | Dual-layer persistence (`redb` key-value cache + `spotty/` credential/account store) |
| `crates/spotty-api` | Spotify Web API client, `TokenManager`, rate-limit gate, and async request pipeline |
| `crates/spotty-audio` | Audio streaming (`TrackStreamer`), Zeroconf/OAuth authentication, and legacy `spotty` helper CLI parser/capabilities |
| `crates/spotty-playlist-folders` | Spotify playlist folder hierarchy parsing and tree resolution |
| `crates/spotty-opml` | OPML feed generation and formatting for LMS menus |
| `crates/spotty-importer` | Library synchronization runner (`ImporterRunner`, `ScanScope`, `SqliteLibrarySink`) targeting LMS `library.db` |
| `crates/spotty-integrations` | External integrations (Last.fm / scrobbling and auxiliary metadata hooks) |
| `crates/spotty-web` | Local HTTP/web UI callbacks and OAuth/Connect web endpoints |
| `crates/lms-spotty` | Unified binary entrypoint: JSON-RPC 2.0 daemon, `scan`, `stream`, `check`, and drop-in `spotty` helper mode |

## 2. `SpottyBridge` JSON-RPC 2.0 Daemon & CLI Reference

`Plugins::Spotty::SpottyBridge` locates `lms-spotty` in `Bin/<perl-archname>/lms-spotty[.exe]`, `Bin/lms-spotty[.exe]`, or on `PATH`.

### Subcommands

- **JSON-RPC 2.0 Daemon** (newline-delimited JSON-RPC 2.0 over TCP):
  ```sh
  lms-spotty daemon --cache-dir <path> [--endpoint-file <path>] [--bind 127.0.0.1:0]
  ```
- **Library Scanner** (writes into LMS SQLite `library.db`):
  ```sh
  lms-spotty scan --cache-dir <path> --lms-db <path> [--scope full|library-only|playlists-only]
  ```
- **Audio Streamer** (outputs raw PCM S16LE to stdout):
  ```sh
  lms-spotty stream --cache-dir <path> --uri <spotify:track:...> [--start-position <seconds>]
  ```
- **Capabilities Check**:
  ```sh
  lms-spotty check
  ```

### Drop-In Legacy `spotty` Helper Flags

`lms-spotty` also accepts legacy `spotty` helper flags directly for backward-compatible LMS invocations:

- `--check` (e.g. `lms-spotty -n Spotty --check`): prints `ok spotty v<version>` on line 1 followed by the JSON capabilities object.
- `--single-track <uri>`: streams a single Spotify track URI in legacy helper mode.

## 3. Cross-Compilation Target Matrix

| Platform | Rust Target Triple | LMS `Bin/<arch>/` Destination |
|---|---|---|
| Windows x64 | `x86_64-pc-windows-msvc` | `Bin/MSWin32-x64-multi-thread/lms-spotty.exe` |
| Linux x86_64 | `x86_64-unknown-linux-gnu` | `Bin/x86_64-linux-gnu-thread-multi/lms-spotty` |
| Linux ARM64 | `aarch64-unknown-linux-gnu` | `Bin/aarch64-linux-gnu-thread-multi/lms-spotty` |
| Linux ARMv7 (hf) | `arm-unknown-linux-gnueabihf` | `Bin/arm-linux-gnueabihf-thread-multi-64int/lms-spotty` |
| macOS Apple Silicon | `aarch64-apple-darwin` | `Bin/darwin-thread-multi-2level/lms-spotty` |

Build a release binary for the host target:

```sh
cargo build --release -p lms-spotty
```

## 4. Release Gate & Freshness Verification

Run the automated release gate from the repository root:

```powershell
python scripts/verify_release_gate.py --workspace .
```

To also stage `target/release/lms-spotty[.exe]` into the host `Bin/<arch>/` directory and verify SHA-256 byte parity:

```powershell
python scripts/verify_release_gate.py --workspace . --stage-release
```

The release gate enforces sequentially:
1. `cargo fmt --all -- --check`
2. `cargo clippy --workspace --all-targets -- -D warnings`
3. `google_rust_audit.py crates --strict`
4. `cargo test --workspace`
5. `cargo build --release -p lms-spotty`
6. Release binary freshness (`target/release/lms-spotty[.exe]` exists, is non-empty, `mtime >= max(crates/**/*.rs, Cargo.toml, strings.txt, install.xml)`, and `-n Spotty --check` exits `0` with `ok spotty v` + valid JSON capabilities)
7. Optional `--stage-release` copy + SHA-256 verification
