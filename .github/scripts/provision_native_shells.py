#!/usr/bin/env python3
"""Provision pinned shells on disposable CI runners without resetting the runner's core tap."""

from __future__ import annotations

import os
import signal
import subprocess
import tempfile
from contextlib import contextmanager
from pathlib import Path
from collections.abc import Iterator


def run(*arguments: str) -> str:
    return subprocess.check_output(arguments, text=True).strip()


def move_to_absent(source: Path, destination: Path) -> None:
    """Refuse an occupied destination; this helper runs alone on a disposable runner."""
    if destination.exists() or destination.is_symlink():
        raise RuntimeError(f"refusing to replace existing destination: {destination}")
    source.rename(destination)


@contextmanager
def isolated_core(original: Path, prepared: Path, storage: Path) -> Iterator[None]:
    """Temporarily expose a prepared tap, retaining and restoring the original directory."""
    backup = storage / "original-core"
    used = storage / "used-core"
    for path in (backup, used):
        if path.exists() or path.is_symlink():
            raise RuntimeError(f"refusing to replace existing recovery path: {path}")
    if original.is_symlink() or not original.is_dir():
        raise RuntimeError(f"expected an ordinary core tap directory: {original}")
    if prepared.is_symlink() or not prepared.is_dir():
        raise RuntimeError(f"expected an ordinary prepared tap directory: {prepared}")
    identity = prepared.stat()
    move_to_absent(original, backup)
    try:
        move_to_absent(prepared, original)
    except BaseException:
        move_to_absent(backup, original)
        raise
    try:
        yield
    finally:
        try:
            observed = original.lstat()
        except OSError as error:
            raise RuntimeError(f"core tap unavailable; original retained at {backup}") from error
        if (observed.st_dev, observed.st_ino) != (identity.st_dev, identity.st_ino):
            raise RuntimeError(f"core tap changed identity; original retained at {backup}")
        # Preserve even the used clone rather than recursively deleting any directory.
        move_to_absent(original, used)
        try:
            move_to_absent(backup, original)
        except BaseException as error:
            raise RuntimeError(f"core restoration failed; original retained at {backup}") from error


def main() -> None:
    if os.environ.get("GITHUB_ACTIONS") != "true" or os.environ.get("RUNNER_OS") != "macOS":
        raise RuntimeError("shell provisioning is restricted to disposable macOS Actions runners")
    revision = os.environ["HOMEBREW_CORE_REVISION"]
    if len(revision) != 40 or any(character not in "0123456789abcdef" for character in revision):
        raise RuntimeError("expected an immutable lowercase core commit SHA")
    core = Path(run("brew", "--repository", "homebrew/core"))
    # Leave recovery directories available in RUNNER_TEMP even after a failed install/restore.
    storage = Path(tempfile.mkdtemp(prefix="skillmount-core-", dir=os.environ["RUNNER_TEMP"]))
    prepared = storage / "prepared-core"
    subprocess.run(["git", "init", str(prepared)], check=True)
    subprocess.run(["git", "-C", str(prepared), "remote", "add", "origin",
                    "https://github.com/Homebrew/homebrew-core"], check=True)
    subprocess.run(["git", "-C", str(prepared), "fetch", "--depth=1", "origin", revision], check=True)
    subprocess.run(["git", "-C", str(prepared), "checkout", "--detach", revision], check=True)
    if run("git", "-C", str(prepared), "rev-parse", "HEAD") != revision:
        raise RuntimeError("prepared core checkout does not match the pinned revision")
    def interrupted(number, _frame):
        raise SystemExit(128 + number)
    signals = (signal.SIGTERM, signal.SIGHUP)
    previous = {number: signal.signal(number, interrupted) for number in signals}
    try:
        with isolated_core(core, prepared, storage):
            # subprocess.run reaps its direct child on BaseException before tap restoration.
            subprocess.run(["brew", "install", "bash", "zsh", "fish"], check=True)
    finally:
        for number, handler in previous.items():
            signal.signal(number, handler)


if __name__ == "__main__":
    main()
