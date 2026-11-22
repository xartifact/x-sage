#!/usr/bin/env python3
"""Compare the fenced stores with their pre-activation captures without writing either root."""

import argparse
from contextlib import contextmanager
import fcntl
import hashlib
import json
import os
from pathlib import Path
import stat
import sys


def digest_stream(stream):
    checksum = hashlib.sha256()
    for chunk in iter(lambda: stream.read(64 * 1024), b""):
        checksum.update(chunk)
    return checksum.hexdigest()


def legacy_digest(root):
    digest = hashlib.sha256()
    for directory, names, files in os.walk(root, followlinks=False):
        names.sort(key=os.fsencode)
        for name in sorted(names + files, key=os.fsencode):
            path = Path(directory) / name
            stat_result = path.lstat()
            relative = os.fsencode(path.relative_to(root))
            mode = stat.S_IMODE(stat_result.st_mode)
            if stat.S_ISDIR(stat_result.st_mode):
                payload = b""
                kind = b"directory"
            elif stat.S_ISREG(stat_result.st_mode):
                descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
                with os.fdopen(descriptor, "rb") as stream:
                    payload = bytes.fromhex(digest_stream(stream))
                kind = b"file"
            else:
                raise ValueError("legacy root contains a symlink or unsupported file")
            for part in (kind, relative, mode.to_bytes(4, "little"), payload):
                digest.update(len(part).to_bytes(8, "little"))
                digest.update(part)
    return digest.hexdigest()


@contextmanager
def native_lock(root):
    root_fd = os.open(root, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        lock_fd = os.open("LOCK", os.O_RDONLY | os.O_NOFOLLOW, dir_fd=root_fd)
        try:
            if not stat.S_ISREG(os.fstat(lock_fd).st_mode):
                raise ValueError("database lock must be a regular file")
            try:
                fcntl.flock(lock_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError as error:
                raise ValueError("database still has a lock holder") from error
            yield root_fd
        finally:
            os.close(lock_fd)
    finally:
        os.close(root_fd)


def snapshot(native_root, legacy_root):
    native_root = native_root.resolve(strict=True)
    legacy_root = legacy_root.resolve(strict=True)
    if not native_root.is_dir() or not legacy_root.is_dir():
        raise ValueError("both authority roots must be existing directories")
    if native_root == legacy_root or native_root.is_relative_to(legacy_root) or legacy_root.is_relative_to(native_root):
        raise ValueError("authority roots must be disjoint")
    with native_lock(native_root) as native_fd:
        current_fd = os.open("CURRENT", os.O_RDONLY | os.O_NOFOLLOW, dir_fd=native_fd)
        with os.fdopen(current_fd, "rb") as stream:
            if not stat.S_ISREG(os.fstat(stream.fileno()).st_mode):
                raise ValueError("database CURRENT must be a regular file")
            current = stream.read(4097)
            if not current or len(current) > 4096:
                raise ValueError("database has no bounded CURRENT to compare")
        return {
            "format": 1,
            "nativeRoot": str(native_root),
            "legacyRoot": str(legacy_root),
            "nativeCurrentSha256": hashlib.sha256(current).hexdigest(),
            "legacyTreeSha256": legacy_digest(legacy_root),
        }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=("capture", "check"))
    parser.add_argument("native_root", type=Path)
    parser.add_argument("legacy_root", type=Path)
    parser.add_argument("--receipt", type=Path)
    arguments = parser.parse_args()
    if (arguments.operation == "check") != (arguments.receipt is not None):
        parser.error("provide --receipt only for check")
    try:
        measured = snapshot(arguments.native_root, arguments.legacy_root)
        if arguments.operation == "capture":
            print(json.dumps(measured, sort_keys=True))
        else:
            with arguments.receipt.open() as content:
                expected = json.load(content)
            if measured != expected:
                raise ValueError("the sealed pre-activation authorities differ")
            print("FENCED_AUTHORITIES_MATCH_PRE_ACTIVATION")
        return 0
    except (OSError, ValueError) as error:
        print(f"Rollback guard refused: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
