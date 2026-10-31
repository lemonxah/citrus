#!/usr/bin/env python3
"""Read/bump Citrus's inherited workspace version, preserving manifest formatting."""

import re
import subprocess
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
manifest = ROOT / "Cargo.toml"
text = manifest.read_text()
current = tomllib.loads(text)["workspace"]["package"]["version"]

if len(sys.argv) == 1:
    print(current)
    sys.exit(0)
if len(sys.argv) != 2:
    sys.exit("usage: tools/version.py [new_version]")

version = sys.argv[1]
numeric = r"(?:0|[1-9][0-9]*)"
prerelease_id = rf"(?:{numeric}|[0-9]*[A-Za-z-][0-9A-Za-z-]*)"
semver = rf"{numeric}\.{numeric}\.{numeric}(?:-{prerelease_id}(?:\.{prerelease_id})*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?"
if not re.fullmatch(semver, version):
    sys.exit(f"invalid semantic version: {version!r}")
if version == current:
    sys.exit(f"workspace is already at {version}")

section = re.search(r"(?ms)^\[workspace\.package\]\s*\n(.*?)(?=^\[|\Z)", text)
if section is None:
    sys.exit("missing [workspace.package]")
body, count = re.subn(r'(?m)^(version\s*=\s*)"[^"]+"', rf'\g<1>"{version}"', section[1])
if count != 1:
    sys.exit("expected exactly one workspace version")

lock = ROOT / "Cargo.lock"
old_lock = lock.read_bytes() if lock.exists() else None
manifest.write_text(text[:section.start(1)] + body + text[section.end(1):])
try:
    subprocess.run(["cargo", "update", "--workspace", "--offline"], cwd=ROOT, check=True)
except (OSError, subprocess.CalledProcessError):
    manifest.write_text(text)
    if old_lock is not None:
        lock.write_bytes(old_lock)
    elif lock.exists():
        lock.unlink()
    sys.exit("version bump failed; restored Cargo.toml and Cargo.lock")
print(f"Bumped {current} → {version}")
