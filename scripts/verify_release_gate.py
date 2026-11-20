#!/usr/bin/env python3
"""Release gate and binary freshness verifier for Spotty-RS."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import shutil
import subprocess
import sys
from pathlib import Path

AUDIT_SCRIPT = Path(
    r"C:\Users\jmolina\.gemini\config\plugins\rust-pro\skills\rust-pro\scripts\google_rust_audit.py"
)


def run_step(name: str, cmd: list[str], cwd: Path) -> None:
    """Execute a release gate command and fail immediately on non-zero exit."""
    print(f"==> [{name}] {' '.join(cmd)}", flush=True)
    env = os.environ.copy()
    env["PYTHONIOENCODING"] = "utf-8"
    result = subprocess.run(cmd, cwd=cwd, env=env, check=False)
    if result.returncode != 0:
        raise RuntimeError(
            f"Gate step '{name}' failed with exit code {result.returncode}"
        )


def collect_source_mtimes(workspace: Path) -> tuple[float, Path]:
    """Return the newest mtime and its path across crates/**/*.rs and root manifests."""
    candidates: list[Path] = []
    crates_dir = workspace / "crates"
    if crates_dir.is_dir():
        candidates.extend(crates_dir.rglob("*.rs"))
    for root_file in ("Cargo.toml", "strings.txt", "install.xml"):
        path = workspace / root_file
        if path.is_file():
            candidates.append(path)

    if not candidates:
        raise RuntimeError("No source files found for freshness comparison")

    newest_path = max(candidates, key=lambda p: p.stat().st_mtime)
    return newest_path.stat().st_mtime, newest_path


def release_binary_path(workspace: Path) -> Path:
    """Return the platform-specific target/release/lms-spotty[.exe] path."""
    name = "lms-spotty.exe" if sys.platform == "win32" else "lms-spotty"
    return workspace / "target" / "release" / name


def os_bin_subdir() -> tuple[str, str]:
    """Return the LMS Bin/<perl-archname> directory and binary filename for this host."""
    machine = platform.machine().lower()
    if sys.platform == "win32":
        return "MSWin32-x64-multi-thread", "lms-spotty.exe"
    if sys.platform == "darwin":
        return "darwin-thread-multi-2level", "lms-spotty"
    if machine in ("aarch64", "arm64"):
        return "aarch64-linux-gnu-thread-multi", "lms-spotty"
    return "x86_64-linux-gnu-thread-multi", "lms-spotty"


def verify_freshness(workspace: Path) -> Path:
    """Verify release binary existence, non-empty size, mtime freshness, and --check output."""
    binary = release_binary_path(workspace)
    if not binary.is_file():
        raise RuntimeError(f"Release binary missing: {binary}")

    size = binary.stat().st_size
    if size <= 0:
        raise RuntimeError(f"Release binary is empty: {binary}")

    newest_src_mtime, newest_src_path = collect_source_mtimes(workspace)
    bin_mtime = binary.stat().st_mtime
    if bin_mtime < newest_src_mtime:
        raise RuntimeError(
            f"Release binary is stale ({bin_mtime:.3f} < {newest_src_mtime:.3f} from {newest_src_path})"
        )

    print(f"==> [freshness] {binary} ({size} bytes) newer than {newest_src_path}", flush=True)

    proc = subprocess.run(
        [str(binary), "-n", "Spotty", "--check"],
        cwd=workspace,
        capture_output=True,
        text=True,
        encoding="utf-8",
        check=False,
    )
    if proc.returncode != 0:
        raise RuntimeError(
            f"{binary} -n Spotty --check exited {proc.returncode}: {proc.stderr.strip()}"
        )

    lines = [line.strip() for line in proc.stdout.splitlines() if line.strip()]
    if len(lines) < 2:
        raise RuntimeError(f"Unexpected --check output: {proc.stdout!r}")

    if not lines[0].startswith("ok spotty v"):
        raise RuntimeError(
            f"Line 1 of --check output must start with 'ok spotty v', got: {lines[0]!r}"
        )

    json_payload = "\n".join(lines[1:])
    try:
        parsed = json.loads(json_payload)
    except json.JSONDecodeError as exc:
        raise RuntimeError(f"Invalid JSON capabilities from --check: {exc}") from exc

    if not isinstance(parsed, dict):
        raise RuntimeError(f"Expected JSON object capabilities, got {type(parsed).__name__}")

    print(f"==> [check] {lines[0]} (valid JSON capabilities)", flush=True)
    return binary


def sha256_file(path: Path) -> str:
    """Compute SHA-256 hex digest of a file."""
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(65536), b""):
            digest.update(chunk)
    return digest.hexdigest()


def stage_release_binary(workspace: Path, binary: Path) -> None:
    """Copy release binary into Bin/<arch>/ and verify SHA-256 byte parity."""
    subdir, filename = os_bin_subdir()
    dest_dir = workspace / "Bin" / subdir
    dest_dir.mkdir(parents=True, exist_ok=True)
    dest_path = dest_dir / filename

    shutil.copy2(binary, dest_path)

    src_hash = sha256_file(binary)
    dst_hash = sha256_file(dest_path)
    if src_hash != dst_hash:
        raise RuntimeError(
            f"SHA-256 mismatch after staging: {src_hash} != {dst_hash}"
        )

    print(f"==> [stage] {dest_path} staged (sha256={dst_hash})", flush=True)


def parse_args(argv: list[str]) -> argparse.Namespace:
    """Parse CLI arguments."""
    parser = argparse.ArgumentParser(
        description="Run Spotty-RS release verification gate and optional Bin staging."
    )
    parser.add_argument(
        "--workspace",
        default=".",
        help="Path to Spotty-RS Cargo workspace root (default: .)",
    )
    parser.add_argument(
        "--stage-release",
        action="store_true",
        help="Stage target/release/lms-spotty[.exe] into Bin/<arch>/ and verify SHA-256 parity.",
    )
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    """Run all release gate steps sequentially."""
    args = parse_args(argv)
    workspace = Path(args.workspace).resolve()

    run_step("cargo-fmt", ["cargo", "fmt", "--all", "--", "--check"], workspace)
    run_step(
        "cargo-clippy",
        ["cargo", "clippy", "--workspace", "--all-targets", "--", "-D", "warnings"],
        workspace,
    )
    run_step(
        "google-rust-audit",
        [sys.executable, str(AUDIT_SCRIPT), "crates", "--strict"],
        workspace,
    )
    run_step("cargo-test", ["cargo", "test", "--workspace"], workspace)
    run_step(
        "cargo-build-release",
        ["cargo", "build", "--release", "-p", "lms-spotty"],
        workspace,
    )

    binary = verify_freshness(workspace)
    if args.stage_release:
        stage_release_binary(workspace, binary)

    print("Release gate PASSED.", flush=True)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main(sys.argv[1:]))
    except Exception as exc:  # noqa: BLE001
        print(f"Release gate FAILED: {exc}", file=sys.stderr, flush=True)
        raise SystemExit(1)
