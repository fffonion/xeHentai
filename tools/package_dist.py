#!/usr/bin/env python3
"""Assemble a self-contained release directory and its archive.

The packager takes an explicitly built executable and the project's runtime
resources, copies them into an exclusive output directory and writes a
matching archive next to it. The WebUI is deliberately not part of a package:
the host serves `/ui/` and `/static/` by proxying the fixed public origin
(`http://xehentai.yooooo.us`) and caches the answers locally. Unix hosts get
`tar.gz`; Windows gets `zip`, so the same tool packages all three release
platforms. It never builds Cargo, copies the source tree or signs/publishes
anything. Standard library only:

    python3 tools/package_dist.py --binary target/release/xeHentai --output dist/pkg
    python3 tools/package_dist.py --binary target/release/xeHentai.exe --output dist/pkg --format zip
"""

import argparse
import os
import shutil
import sys
import tarfile
import tempfile
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# Archive suffixes this tool can produce. `tar.gz` keeps the historical
# behavior and is the Unix default; `zip` is the Windows default.
ARCHIVE_SUFFIXES = {"tar.gz": ".tar.gz", "zip": ".zip"}

# RSS sources are embedded at build time and shipped as runtime overrides so
# signed updates can replace them. Locale YAML is embedded by rust-i18n; the
# distribution keeps its translation resources as well.
REQUIRED_FILES = (
    "rss/rpc/handlers.rss",
    "rss/filters/match.rss",
    "rss/updater/github.rss",
    "rss/updater/updater.rss",
    "config.example.rss",
    "LICENSE.txt",
)
REQUIRED_DIRS = ("locales",)


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


def archive_path(output, archive_format):
    """Return the archive path that belongs beside `output`."""
    try:
        suffix = ARCHIVE_SUFFIXES[archive_format]
    except KeyError as error:
        raise PackagingError(f"unsupported archive format: {archive_format}") from error
    return Path(output).with_name(Path(output).name + suffix)


def _write_archive(temporary_archive, staged, output, archive_format):
    if archive_format == "tar.gz":
        with tarfile.open(temporary_archive, "w:gz") as tar:
            tar.add(staged, arcname=output.name)
        return
    # `zip` stores no Unix mode bits that a Windows host would honor, and the
    # executable bit has no meaning there; keep the entry names relative to the
    # package root so both formats unpack to the same tree.
    with zipfile.ZipFile(
        temporary_archive, "w", compression=zipfile.ZIP_DEFLATED
    ) as archive:
        for path in sorted(p for p in staged.rglob("*") if p.is_file()):
            archive.write(path, arcname=f"{output.name}/{path.relative_to(staged).as_posix()}")


def _publish_archive(temporary_archive, archive, archive_format):
    """Publish the finished archive without ever overwriting an existing one.

    `tar.gz` keeps its historical atomic hard link. `zip` is published with an
    exclusive create plus a byte copy, because the Windows host this format
    targets cannot be assumed to support a hard link on every runner volume;
    the exclusive create is still the authority that refuses a second writer.
    """
    if archive_format == "tar.gz":
        try:
            os.link(temporary_archive, archive)
        except FileExistsError as error:
            raise PackagingError(f"archive already exists: {archive}") from error
        return
    try:
        descriptor = os.open(archive, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o644)
    except FileExistsError as error:
        raise PackagingError(f"archive already exists: {archive}") from error
    try:
        with os.fdopen(descriptor, "wb") as destination, open(
            temporary_archive, "rb"
        ) as source:
            shutil.copyfileobj(source, destination)
            destination.flush()
            os.fsync(destination.fileno())
    except BaseException:
        Path(archive).unlink(missing_ok=True)
        raise


def build(source_root, binary, output, archive_format="tar.gz"):
    """Copy the planned resources into `output` and create its archive.

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
    archive = archive_path(output, archive_format)
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
            prefix=".package-", suffix=ARCHIVE_SUFFIXES[archive_format], dir=output.parent
        )
        os.close(handle)
        os.chmod(temporary_archive, 0o644)
        _write_archive(temporary_archive, staged, output, archive_format)
        try:
            output.mkdir()
        except FileExistsError as error:
            raise PackagingError(f"output already exists: {output}") from error
        acquired = True
        # This directory is empty and owned by this invocation, so moving the
        # staged children in cannot replace anything that already exists.
        for child in sorted(staged.iterdir()):
            os.replace(child, output / child.name)
        _publish_archive(temporary_archive, archive, archive_format)
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
    parser.add_argument(
        "--format",
        choices=sorted(ARCHIVE_SUFFIXES),
        default="tar.gz",
        help="archive format written beside the package directory (default: tar.gz)",
    )
    arguments = parser.parse_args(argv)
    try:
        archive = build(ROOT, arguments.binary, arguments.output, arguments.format)
    except (PackagingError, OSError) as error:
        print(f"package_dist: {error}", file=sys.stderr)
        return 1
    print(f"package: {arguments.output.resolve()}")
    print(f"archive: {archive.resolve()}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
