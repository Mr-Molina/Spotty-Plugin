#!/usr/bin/env python3
"""Decoupled staging, install, and rollback deployment script for Lyrion 9.1.1."""

from __future__ import annotations

import argparse
import shlex
import subprocess
import sys
from pathlib import Path

ZIP_FILENAME = "Spotty-4.62.3-rust-x86_64-linux.zip"
DEFAULT_STAGE_DIR = "/tmp/spotty-rs-stage"
PM_FILES = (
    "Plugin.pm",
    "SpottyBridge.pm",
    "ProtocolHandler.pm",
    "Importer.pm",
    "Settings.pm",
    "Settings/Auth.pm",
    "Settings/Callback.pm",
    "Settings/Player.pm",
    "Settings/PlayerAuth.pm",
    "Settings/PlaylistFolders.pm",
)


def run_ssh(host: str, user: str | None, remote_cmd: str) -> str:
    """Run a non-interactive SSH command and return stdout."""
    target = f"{user}@{host}" if user else host
    cmd = ["ssh", "-o", "BatchMode=yes", target, remote_cmd]
    proc = subprocess.run(
        cmd,
        capture_output=True,
        text=True,
        encoding="utf-8",
        check=False,
    )
    if proc.returncode != 0:
        raise RuntimeError(
            f"SSH command failed (exit {proc.returncode}): {remote_cmd}\n"
            f"STDOUT:\n{proc.stdout}\nSTDERR:\n{proc.stderr}"
        )
    return proc.stdout


def run_scp(local_files: list[Path], host: str, user: str | None, remote_dir: str) -> None:
    """Copy local files to remote_dir via scp."""
    target = f"{user}@{host}" if user else host
    cmd = ["scp", "-o", "BatchMode=yes", *[str(p) for p in local_files], f"{target}:{remote_dir}/"]
    proc = subprocess.run(
        cmd,
        capture_output=True,
        text=True,
        encoding="utf-8",
        check=False,
    )
    if proc.returncode != 0:
        raise RuntimeError(
            f"SCP failed (exit {proc.returncode}): {proc.stderr.strip()}"
        )


def active_enter_timestamp(host: str, user: str | None) -> str:
    """Query lyrionmusicserver ActiveEnterTimestamp without modifying anything."""
    return run_ssh(
        host,
        user,
        "systemctl show lyrionmusicserver -p ActiveEnterTimestamp --value",
    ).strip()


def stage_only(
    host: str, user: str | None, dist_dir: Path, stage_dir: str
) -> None:
    """Stage ZIP, SHA256SUMS, and unpacked Spotty/ tree into /tmp/spotty-rs-stage/."""
    zip_path = dist_dir / ZIP_FILENAME
    sums_path = dist_dir / "SHA256SUMS"
    for req in (zip_path, sums_path):
        if not req.is_file():
            raise FileNotFoundError(
                f"Required artifact missing: {req}. Run scripts/package_lms_plugin.py first."
            )

    ts_before = active_enter_timestamp(host, user)
    print(f"==> [pre-check] lyrionmusicserver ActiveEnterTimestamp: {ts_before}")

    q_stage = shlex.quote(stage_dir)
    run_ssh(
        host,
        user,
        f"rm -rf {q_stage}/Spotty && mkdir -p {q_stage}",
    )

    print(f"==> [upload] Copying {zip_path.name} and SHA256SUMS to {host}:{stage_dir}/")
    run_scp([zip_path, sums_path], host, user, stage_dir)

    print("==> [verify] Checking SHA-256 and unpacking into stage directory")
    unpack_out = run_ssh(
        host,
        user,
        (
            f"cd {q_stage} && sha256sum -c SHA256SUMS && "
            f"python3 -m zipfile -e {shlex.quote(ZIP_FILENAME)} {q_stage} && "
            f"chmod 0755 {q_stage}/Spotty/Bin/x86_64-linux/lms-spotty "
            f"{q_stage}/Spotty/Bin/x86_64-linux-gnu-thread-multi/lms-spotty"
        ),
    )
    print(unpack_out.strip())

    print("==> [check] Running lms-spotty -n Spotty --check on target host")
    check_out = run_ssh(
        host,
        user,
        f"{q_stage}/Spotty/Bin/x86_64-linux/lms-spotty -n Spotty --check",
    )
    print(check_out.strip())

    print("==> [perl -c] Syntax-checking all 10 .pm files against Lyrion 9.1.1 Slim::*")
    pm_args = " ".join(
        shlex.quote(f"{stage_dir}/Spotty/{pm}") for pm in PM_FILES
    )
    perl_cmd = (
        f"for f in {pm_args}; do "
        f"perl -I/usr/share/perl5 -I/usr/share/squeezeboxserver/CPAN "
        f"-I{q_stage}/Spotty -c \"$f\" 2>&1 || exit 1; "
        f"done"
    )
    perl_out = run_ssh(host, user, perl_cmd)
    if perl_out.strip():
        print(perl_out.strip())

    ts_after = active_enter_timestamp(host, user)
    if ts_before != ts_after:
        raise RuntimeError(
            f"SAFETY VIOLATION: ActiveEnterTimestamp changed ({ts_before} -> {ts_after})"
        )
    print(
        f"==> [post-check] lyrionmusicserver ActiveEnterTimestamp UNCHANGED: {ts_after}"
    )


def install_staged(host: str, user: str | None, stage_dir: str) -> None:
    """Phase 2 (requires explicit approval): backup existing Spotty, install staged, restart LMS."""
    q_stage = shlex.quote(stage_dir)
    remote_script = (
        f"set -euo pipefail; "
        f"test -d {q_stage}/Spotty; "
        f"TS=$(date +%Y%m%d-%H%M%S); "
        f"BACKUP=/var/tmp/spotty-backup-$TS.tar.gz; "
        f"TARGETS=(); "
        f"for p in "
        f"/var/lib/squeezeboxserver/cache/InstalledPlugins/Plugins/Spotty "
        f"/var/lib/squeezeboxserver/Plugins/Spotty "
        f"/var/lib/squeezeboxserver/prefs/plugin/spotty.prefs "
        f"/var/lib/squeezeboxserver/cache/spotty; do "
        f"  [ -e \"$p\" ] && TARGETS+=(\"$p\"); "
        f"done; "
        f"sudo tar -czf \"$BACKUP\" \"${{TARGETS[@]}}\"; "
        f"echo \"Backup created: $BACKUP\"; "
        f"sudo rm -rf /var/lib/squeezeboxserver/cache/InstalledPlugins/Plugins/Spotty "
        f"/var/lib/squeezeboxserver/Plugins/Spotty; "
        f"sudo cp -a {q_stage}/Spotty /var/lib/squeezeboxserver/Plugins/Spotty; "
        f"sudo chown -R squeezeboxserver:nogroup /var/lib/squeezeboxserver/Plugins/Spotty; "
        f"sudo systemctl restart lyrionmusicserver; "
        f"systemctl is-active lyrionmusicserver"
    )
    out = run_ssh(host, user, remote_script)
    print(out.strip())


def rollback_install(
    host: str, user: str | None, backup_path: str | None
) -> None:
    """Phase 2 rollback: restore timestamped Spotty backup and restart lyrionmusicserver."""
    if backup_path:
        q_backup = shlex.quote(backup_path)
        find_backup = f"BACKUP={q_backup}; test -f \"$BACKUP\""
    else:
        find_backup = "BACKUP=$(ls -1t /var/tmp/spotty-backup-*.tar.gz | head -n1)"

    remote_script = (
        f"set -euo pipefail; "
        f"{find_backup}; "
        f"echo \"Restoring backup: $BACKUP\"; "
        f"sudo rm -rf /var/lib/squeezeboxserver/Plugins/Spotty "
        f"/var/lib/squeezeboxserver/cache/InstalledPlugins/Plugins/Spotty; "
        f"sudo tar -xzf \"$BACKUP\" -C /; "
        f"sudo systemctl restart lyrionmusicserver; "
        f"systemctl is-active lyrionmusicserver"
    )
    out = run_ssh(host, user, remote_script)
    print(out.strip())


def main(argv: list[str]) -> int:
    """CLI entry point."""
    parser = argparse.ArgumentParser(
        description="Decoupled staging, install, and rollback tool for Spotty-RS on Lyrion."
    )
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument(
        "--stage-only",
        action="store_true",
        help="Upload and verify under /tmp/spotty-rs-stage without touching LMS.",
    )
    mode.add_argument(
        "--install",
        action="store_true",
        help="Phase 2: backup existing Spotty, install staged tree, and restart LMS.",
    )
    mode.add_argument(
        "--rollback",
        action="store_true",
        help="Phase 2: restore latest or --backup tarball and restart LMS.",
    )
    parser.add_argument("--host", required=True, help="Target Lyrion host IP/name.")
    parser.add_argument("--user", default=None, help="Optional SSH username.")
    parser.add_argument(
        "--dist-dir",
        default="dist",
        help="Local dist directory containing ZIP and SHA256SUMS (default: dist).",
    )
    parser.add_argument(
        "--stage-dir",
        default=DEFAULT_STAGE_DIR,
        help=f"Remote staging directory (default: {DEFAULT_STAGE_DIR}).",
    )
    parser.add_argument(
        "--backup",
        default=None,
        help="Specific backup tarball path for --rollback.",
    )
    args = parser.parse_args(argv)

    if args.stage_only:
        stage_only(args.host, args.user, Path(args.dist_dir).resolve(), args.stage_dir)
    elif args.install:
        install_staged(args.host, args.user, args.stage_dir)
    elif args.rollback:
        rollback_install(args.host, args.user, args.backup)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
