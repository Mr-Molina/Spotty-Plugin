#!/usr/bin/env python3
"""Build reproducible LMS plugin ZIP, repo.xml, and SHA256SUMS for Spotty-RS."""

from __future__ import annotations

import argparse
import fnmatch
import hashlib
import os
import sys
import xml.etree.ElementTree as ET
import zipfile
from pathlib import Path
from xml.sax.saxutils import escape

DEFAULT_VERSION = "4.62.3"
ZIP_FILENAME = "Spotty-4.62.3-rust-x86_64-linux.zip"
SOURCE_BIN_REL = Path("Bin/x86_64-linux-gnu-thread-multi/lms-spotty")
LMS_FIND_BIN_REL = Path("Bin/x86_64-linux/lms-spotty")

# Files/directories that are repo/build metadata rather than runtime plugin assets.
ALWAYS_EXCLUDED_PATTERNS = (
    ".git",
    ".git/**",
    ".github",
    ".github/**",
    ".gitignore",
    ".DS_Store",
    "exclude.lst",
    "dist",
    "dist/**",
    "target",
    "target/**",
    "__pycache__",
    "__pycache__/**",
    "*.pyc",
    "Bin/MSWin32-x64-multi-thread",
    "Bin/MSWin32-x64-multi-thread/**",
    "Bin/README.md",
)


def load_exclude_patterns(exclude_file: Path) -> list[str]:
    """Load Spotty/... patterns from exclude.lst as workspace-relative globs."""
    patterns: list[str] = []
    if not exclude_file.is_file():
        return patterns
    for raw_line in exclude_file.read_text(encoding="utf-8").splitlines():
        line = raw_line.strip()
        if not line or line.startswith("#"):
            continue
        if line.startswith("Spotty/"):
            line = line[len("Spotty/") :]
        patterns.append(line)
    return patterns


def matches_pattern(rel_posix: str, pattern: str) -> bool:
    """Match a workspace-relative POSIX path against an exclude.lst glob."""
    if pattern.endswith("/**"):
        prefix = pattern[:-3].rstrip("/")
        if rel_posix == prefix or rel_posix.startswith(prefix + "/"):
            return True
    if "**/" in pattern:
        suffix = pattern.split("**/", 1)[1]
        if fnmatch.fnmatch(Path(rel_posix).name, suffix) or fnmatch.fnmatch(
            rel_posix, suffix
        ):
            return True
    return fnmatch.fnmatch(rel_posix, pattern)


def is_excluded(rel_posix: str, patterns: list[str]) -> bool:
    """Return True if rel_posix matches any exclusion pattern."""
    for pat in (*ALWAYS_EXCLUDED_PATTERNS, *patterns):
        if matches_pattern(rel_posix, pat):
            return True
    return False


def collect_plugin_files(workspace: Path, patterns: list[str]) -> list[tuple[Path, str]]:
    """Collect (source_path, archive_rel_path) pairs sorted deterministically."""
    collected: list[tuple[Path, str]] = []
    for root, dirs, files in os.walk(workspace):
        root_path = Path(root)
        rel_dir = root_path.relative_to(workspace)
        # Prune excluded directories early.
        kept_dirs: list[str] = []
        for d in sorted(dirs):
            rel_d = (rel_dir / d).as_posix() if rel_dir != Path(".") else d
            if not is_excluded(rel_d, patterns):
                kept_dirs.append(d)
        dirs[:] = kept_dirs

        for f in sorted(files):
            rel_f = (rel_dir / f).as_posix() if rel_dir != Path(".") else f
            if is_excluded(rel_f, patterns):
                continue
            collected.append((root_path / f, f"Spotty/{rel_f}"))

    collected.sort(key=lambda item: item[1])
    return collected


def write_zip_entry(
    zf: zipfile.ZipFile, src_path: Path, arcname: str, executable: bool
) -> None:
    """Write a file into ZipFile with explicit UNIX mode bits."""
    data = src_path.read_bytes()
    info = zipfile.ZipInfo(arcname)
    info.create_system = 3  # UNIX
    mode = 0o100755 if executable else 0o100644
    info.external_attr = mode << 16
    info.compress_type = zipfile.ZIP_DEFLATED
    zf.writestr(info, data)


def digest_file(path: Path, algorithm: str) -> str:
    """Compute hex digest for a file."""
    h = hashlib.new(algorithm)
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(65536), b""):
            h.update(chunk)
    return h.hexdigest()


def parse_install_xml(install_xml: Path) -> dict[str, str]:
    """Extract plugin metadata from install.xml."""
    tree = ET.parse(install_xml)
    root = tree.getroot()
    target_app = root.find("targetApplication")
    min_target = (
        target_app.findtext("minVersion", "7.7").strip()
        if target_app is not None
        else "7.7"
    )
    max_target = (
        target_app.findtext("maxVersion", "*").strip()
        if target_app is not None
        else "*"
    )
    return {
        "version": root.findtext("version", DEFAULT_VERSION).strip(),
        "creator": root.findtext("creator", "Michael Herger").strip(),
        "email": root.findtext("email", "slim@herger.net").strip(),
        "minTarget": min_target,
        "maxTarget": max_target,
    }


def render_repo_xml(meta: dict[str, str], zip_name: str, sha1_hex: str) -> str:
    """Render LMS Extension Downloader repository XML."""
    version = escape(meta["version"])
    creator = escape(meta["creator"])
    email = escape(meta["email"])
    min_target = escape(meta["minTarget"])
    max_target = escape(meta["maxTarget"])
    zip_escaped = escape(zip_name)
    sha1_escaped = escape(sha1_hex)

    return (
        '<?xml version="1.0" encoding="UTF-8"?>\n'
        "<extensions>\n"
        "\t<details>\n"
        '\t\t<title lang="EN">Spotty (Rust)</title>\n'
        "\t</details>\n"
        "\t<plugins>\n"
        f'\t\t<plugin name="Spotty" version="{version}" '
        f'minTarget="{min_target}" maxTarget="{max_target}">\n'
        '\t\t\t<title lang="EN">Spotty (Rust)</title>\n'
        '\t\t\t<desc lang="EN">Spotify Connect and Spotify music service plugin '
        "powered by lms-spotty (Rust).</desc>\n"
        f"\t\t\t<url>{zip_escaped}</url>\n"
        f"\t\t\t<sha>{sha1_escaped}</sha>\n"
        f"\t\t\t<creator>{creator}</creator>\n"
        f"\t\t\t<email>{email}</email>\n"
        "\t\t\t<category>musicservices</category>\n"
        f"\t\t\t<targetApplication>\n"
        f"\t\t\t\t<id>SqueezeCenter</id>\n"
        f"\t\t\t\t<minVersion>{min_target}</minVersion>\n"
        f"\t\t\t\t<maxVersion>{max_target}</maxVersion>\n"
        f"\t\t\t</targetApplication>\n"
        "\t\t</plugin>\n"
        "\t</plugins>\n"
        "</extensions>\n"
    )


def build_package(workspace: Path, dist_dir: Path) -> None:
    """Build ZIP, repo.xml, and SHA256SUMS into dist_dir."""
    source_bin = workspace / SOURCE_BIN_REL
    if not source_bin.is_file():
        raise FileNotFoundError(
            f"Staged release binary not found: {source_bin}. "
            "Run scripts/verify_release_gate.py --stage-release first."
        )

    install_xml = workspace / "install.xml"
    if not install_xml.is_file():
        raise FileNotFoundError(f"install.xml not found: {install_xml}")

    patterns = load_exclude_patterns(workspace / "exclude.lst")
    files = collect_plugin_files(workspace, patterns)

    dist_dir.mkdir(parents=True, exist_ok=True)
    zip_path = dist_dir / ZIP_FILENAME

    lms_find_arcname = f"Spotty/{LMS_FIND_BIN_REL.as_posix()}"
    existing_arcs = {arc for _, arc in files}

    with zipfile.ZipFile(zip_path, "w", compression=zipfile.ZIP_DEFLATED) as zf:
        for src_path, arcname in files:
            is_exec = src_path.name == "lms-spotty"
            write_zip_entry(zf, src_path, arcname, executable=is_exec)
        # Also include Bin/x86_64-linux/lms-spotty so Lyrion's Slim::Utils::Misc::findbin
        # resolves [lms-spotty] in custom-convert.conf natively before initPlugin runs.
        if lms_find_arcname not in existing_arcs:
            write_zip_entry(zf, source_bin, lms_find_arcname, executable=True)

    sha1_hex = digest_file(zip_path, "sha1")
    sha256_zip = digest_file(zip_path, "sha256")

    meta = parse_install_xml(install_xml)
    repo_xml_path = dist_dir / "repo.xml"
    repo_xml_path.write_text(
        render_repo_xml(meta, ZIP_FILENAME, sha1_hex), encoding="utf-8"
    )

    sha256_sums_path = dist_dir / "SHA256SUMS"
    sha256_sums_path.write_text(
        f"{sha256_zip}  {ZIP_FILENAME}\n",
        encoding="utf-8",
    )

    print(f"ZIP:      {zip_path} ({zip_path.stat().st_size} bytes)")
    print(f"SHA-1:    {sha1_hex}")
    print(f"SHA-256:  {sha256_zip}")
    print(f"repo.xml: {repo_xml_path} ({repo_xml_path.stat().st_size} bytes)")
    print(f"SUMS:     {sha256_sums_path}")


def main(argv: list[str]) -> int:
    """CLI entry point."""
    parser = argparse.ArgumentParser(
        description="Package Spotty-RS LMS plugin ZIP, repo.xml, and SHA256SUMS."
    )
    parser.add_argument(
        "--workspace",
        default=".",
        help="Path to Spotty-RS workspace root (default: .)",
    )
    parser.add_argument(
        "--dist-dir",
        default="dist",
        help="Output directory relative to workspace or absolute (default: dist)",
    )
    args = parser.parse_args(argv)
    workspace = Path(args.workspace).resolve()
    dist_arg = Path(args.dist_dir)
    dist_dir = dist_arg if dist_arg.is_absolute() else workspace / dist_arg

    build_package(workspace, dist_dir)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
