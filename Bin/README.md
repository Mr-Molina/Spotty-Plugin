# Building `lms-spotty` Binaries

Spotty's LMS daemon, library scanner, and audio transcode helper are unified into the `lms-spotty` Rust binary.

## Build from the Cargo Workspace

```sh
cargo build --release -p lms-spotty
```

The compiled binary is written to:

- `target/release/lms-spotty` (Linux / macOS)
- `target/release/lms-spotty.exe` (Windows)

## Staging into `Bin/<arch>/`

`Plugins::Spotty::SpottyBridge` looks for `Bin/<perl-archname>/lms-spotty[.exe]`, `Bin/lms-spotty[.exe]`, or `lms-spotty` on `PATH`:

- Windows x64: `Bin/MSWin32-x64-multi-thread/lms-spotty.exe`
- Linux x86_64: `Bin/x86_64-linux-gnu-thread-multi/lms-spotty`
- Linux ARM64: `Bin/aarch64-linux-gnu-thread-multi/lms-spotty`
- macOS Apple Silicon: `Bin/darwin-thread-multi-2level/lms-spotty`
