#!/usr/bin/env python3
"""Assemble a self-contained Linux release directory and its tar.gz.

The packager takes an explicitly built executable and the project's runtime
resources, copies them into an exclusive output directory and writes a
matching archive next to it. It never builds Cargo, copies the source tree or
signs/publishes anything. Standard library only:

    python3 tools/package_dist.py --binary target/release/xeHentai --output dist/pkg
"""

import argparse
import os
import shutil
import sys
import tarfile
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# Runtime files the executable loads from its own directory. `locales` is
# compiled into the binary, but the plan ships the translation resources.
REQUIRED_FILES = (
    "rss/rpc/handlers.rss",
    "rss/filters/match.rss",
    "rss/updater/github.rss",
    "rss/updater/updater.rss",
    "ui/index.html",
    "ui/PROVENANCE",
    "config.example.rss",
    "LICENSE.txt",
)
REQUIRED_DIRS = ("ui/static", "locales")


class PackagingError(Exception):
    """A required input or an exclusive output precondition was not met."""


def _directory_files(source_root, relative):
    directory = source_root / relative
    files = sorted(path for path in directory.rglob("*") if path.is_file())
    if not files:
        raise PackagingError(f"required resource directory is empty: {relative}")
    return files


def plan(source_root, binary):
    """Validate the inputs and return (source, relative) copy pairs."""
    source_root = Path(source_root)
    binary = Path(binary)
    if not binary.is_file():
        raise PackagingError(f"binary is not a regular file: {binary}")
    pairs = [(binary, binary.name)]
    for relative in REQUIRED_FILES:
        source = source_root / relative
        if not source.is_file():
            raise PackagingError(f"required resource is missing: {relative}")
        pairs.append((source, relative))
    for relative in REQUIRED_DIRS:
        directory = source_root / relative
        if not directory.is_dir():
            raise PackagingError(f"required resource directory is missing: {relative}")
        pairs.extend(
            (source, str(source.relative_to(source_root)).replace(os.sep, "/"))
            for source in _directory_files(source_root, relative)
        )
    return pairs


def build(source_root, binary, output):
    """Copy the planned resources into `output` and create `output`.tar.gz`.

    The output directory is acquired with a single non-recursive mkdir after
    the input plan is validated; that mkdir is the exclusivity authority, so a
    directory that already exists (a previous package or a concurrent
    invocation) is never replaced or removed. Only the directory this
    invocation acquired is populated, and it is populated only from the
    completed private staging tree. The finished archive is published last
    with an atomic hard link to its final name, which fails instead of
    overwriting an archive that appeared while this invocation was staging.
    """
    output = Path(output)
    archive = output.with_name(output.name + ".tar.gz")
    if output.exists() or output.is_symlink():
        raise PackagingError(f"output already exists: {output}")
    if archive.exists() or archive.is_symlink():
        raise PackagingError(f"archive already exists: {archive}")
    pairs = plan(source_root, binary)

    output.parent.mkdir(parents=True, exist_ok=True)
    staging = Path(tempfile.mkdtemp(prefix=".package-", dir=output.parent))
    staged = staging / output.name
    temporary_archive = None
    acquired = False
    try:
        staged.mkdir()
        for source, relative in pairs:
            target = staged / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(source, target)
        handle, temporary_archive = tempfile.mkstemp(
            prefix=".package-", suffix=".tar.gz", dir=output.parent
        )
        os.close(handle)
        os.chmod(temporary_archive, 0o644)
        with tarfile.open(temporary_archive, "w:gz") as tar:
            tar.add(staged, arcname=output.name)
        try:
            output.mkdir()
        except FileExistsError as error:
            raise PackagingError(f"output already exists: {output}") from error
        acquired = True
        # This directory is empty and owned by this invocation, so moving the
        # staged children in cannot replace anything that already exists.
        for child in sorted(staged.iterdir()):
            os.replace(child, output / child.name)
        try:
            os.link(temporary_archive, archive)
        except FileExistsError as error:
            raise PackagingError(f"archive already exists: {archive}") from error
    except BaseException:
        if acquired:
            shutil.rmtree(output, ignore_errors=True)
        raise
    finally:
        if temporary_archive is not None:
            Path(temporary_archive).unlink(missing_ok=True)
        shutil.rmtree(staging, ignore_errors=True)
    return archive


def main(argv=None):
    parser = argparse.ArgumentParser(
        description="Assemble the runtime files around an already built xeHentai binary."
    )
    parser.add_argument("--binary", required=True, type=Path, help="built xeHentai executable")
    parser.add_argument("--output", required=True, type=Path, help="new, exclusive package directory")
    arguments = parser.parse_args(argv)
    try:
        archive = build(ROOT, arguments.binary, arguments.output)
    except (PackagingError, OSError) as error:
        print(f"package_dist: {error}", file=sys.stderr)
        return 1
    print(f"package: {arguments.output.resolve()}")
    print(f"archive: {archive.resolve()}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
