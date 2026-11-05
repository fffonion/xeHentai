#!/usr/bin/env python3
"""Content and failure tests for the self-contained release packager.

The packager is the standard-library entry point that copies an explicitly
built executable and the project's runtime resources into an exclusive output
directory and a matching archive. These tests exercise the real repository
resources and a fake executable only as command input: no Cargo build, no
network, no signing. Standard library only:

    python3 tools/test_package_dist.py
"""

import contextlib
import io
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
import threading
import traceback
import zipfile
from pathlib import Path

TOOLS = Path(__file__).resolve().parent
ROOT = TOOLS.parent
SCRIPT = TOOLS / "package_dist.py"

sys.path.insert(0, str(TOOLS))
import package_dist  # noqa: E402


def _tree_files(root):
    return sorted(
        str(path.relative_to(root)).replace(os.sep, "/")
        for path in root.rglob("*")
        if path.is_file()
    )


def _run(binary, output):
    return subprocess.run(
        [sys.executable, str(SCRIPT), "--binary", str(binary), "--output", str(output)],
        capture_output=True,
        text=True,
        cwd=tempfile.gettempdir(),
    )


def _run_zip(binary, output):
    return subprocess.run(
        [
            sys.executable,
            str(SCRIPT),
            "--binary",
            str(binary),
            "--output",
            str(output),
            "--format",
            "zip",
        ],
        capture_output=True,
        text=True,
        cwd=tempfile.gettempdir(),
    )


def _fake_binary(directory, name="xeHentai"):
    binary = Path(directory) / name
    binary.write_bytes(b"#!/bin/sh\nexit 0\n")
    binary.chmod(0o755)
    return binary


def test_package_contains_runtime_files_with_exact_bytes():
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        binary = _fake_binary(tmp)
        output = tmp / "dist" / "xehentai-linux-x86_64"
        result = _run(binary, output)
        assert result.returncode == 0, result.stderr
        assert output.is_dir()
        assert (output / "xeHentai").read_bytes() == binary.read_bytes()
        assert os.access(output / "xeHentai", os.X_OK)
        for relative in package_dist.REQUIRED_FILES:
            assert (output / relative).read_bytes() == (ROOT / relative).read_bytes(), relative
        for relative in package_dist.REQUIRED_DIRS:
            expected = _tree_files(ROOT / relative)
            assert expected, relative
            assert _tree_files(output / relative) == expected, relative
        assert not (output / "config.rss").exists()
        assert not (output / "config.py").exists()
        assert not (output / "target").exists()
        assert not (output / ".git").exists()
        # The WebUI is served from the remote origin, never shipped.
        assert not (output / "ui").exists()
        archive = output.with_name(output.name + ".tar.gz")
        assert archive.is_file()
        with tarfile.open(archive, "r:gz") as tar:
            members = {member.name: member for member in tar.getmembers() if member.isfile()}
            prefix = output.name + "/"
            names = {name[len(prefix):] for name in members if name.startswith(prefix)}
            assert names == set(_tree_files(output)), sorted(names ^ set(_tree_files(output)))
            for relative in [
                "xeHentai",
                "rss/rpc/handlers.rss",
                "locales/en_us.yml",
                "config.example.rss",
            ]:
                archived = tar.extractfile(members[prefix + relative]).read()
                assert archived == (output / relative).read_bytes(), relative


def test_missing_binary_fails_and_writes_nothing():
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        output = tmp / "dist" / "pkg"
        result = _run(tmp / "no-such-binary", output)
        assert result.returncode != 0
        assert "binary" in result.stderr.lower(), result.stderr
        assert not output.exists()
        assert not output.with_name(output.name + ".tar.gz").exists()


def test_existing_output_is_rejected_and_preserved():
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        output = tmp / "dist" / "pkg"
        output.mkdir(parents=True)
        sentinel = output / "keep.txt"
        sentinel.write_text("untouched")
        result = _run(_fake_binary(tmp), output)
        assert result.returncode != 0
        assert "exists" in result.stderr.lower(), result.stderr
        assert sentinel.read_text() == "untouched"
        assert _tree_files(output) == ["keep.txt"]
        assert not output.with_name(output.name + ".tar.gz").exists()


def _synthetic_source(root):
    for relative in package_dist.REQUIRED_FILES:
        target = root / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(("content of " + relative + "\n").encode())
    for relative in package_dist.REQUIRED_DIRS:
        target = root / relative
        target.mkdir(parents=True, exist_ok=True)
        (target / "sample.txt").write_bytes(b"sample resource\n")


def test_missing_required_resource_is_rejected_without_partial_output():
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        source = tmp / "source"
        _synthetic_source(source)
        shutil.rmtree(source / "locales")
        output = tmp / "dist" / "pkg"
        try:
            package_dist.build(source, _fake_binary(tmp), output)
        except package_dist.PackagingError as error:
            assert "locales" in str(error), error
        else:
            raise AssertionError("a missing required resource must be rejected")
        assert not output.exists()
        assert not output.with_name(output.name + ".tar.gz").exists()
        assert not (tmp / "dist").exists() or sorted(
            path.name for path in (tmp / "dist").iterdir()
        ) == []


def test_copy_failure_removes_partial_staging_output():
    if hasattr(os, "geteuid") and os.geteuid() == 0:
        print("skipped: unreadable fixture is not enforced for root")
        return
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        source = tmp / "source"
        _synthetic_source(source)
        blocked = source / "config.example.rss"
        blocked.chmod(0o000)
        output = tmp / "dist" / "pkg"
        try:
            package_dist.build(source, _fake_binary(tmp), output)
        except (package_dist.PackagingError, OSError):
            pass
        else:
            raise AssertionError("an unreadable required resource must fail the package")
        assert not output.exists()
        assert not output.with_name(output.name + ".tar.gz").exists()
        assert sorted(path.name for path in (tmp / "dist").iterdir()) == []


@contextlib.contextmanager
def _after_first_copy(callback):
    """Run `callback` once after the first real staging copy, then restore."""
    real_copy2 = shutil.copy2
    fired = []

    def hooked(source, target, **kwargs):
        result = real_copy2(source, target, **kwargs)
        if not fired:
            fired.append(True)
            callback()
        return result

    package_dist.shutil.copy2 = hooked
    try:
        yield fired
    finally:
        package_dist.shutil.copy2 = real_copy2


def test_archive_created_while_staging_is_refused_without_overwrite():
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        source = tmp / "source"
        _synthetic_source(source)
        output = tmp / "dist" / "pkg"
        archive = output.with_name(output.name + ".tar.gz")
        sentinel = b"concurrent archive sentinel\n"

        def publish_competitor_archive():
            archive.write_bytes(sentinel)

        with _after_first_copy(publish_competitor_archive) as fired:
            try:
                package_dist.build(source, _fake_binary(tmp), output)
            except package_dist.PackagingError as error:
                assert "archive" in str(error).lower(), error
            else:
                raise AssertionError(
                    "an archive created while staging must be refused, not overwritten"
                )
            assert fired, "the archive race was never injected"
            assert archive.is_file()
            assert archive.read_bytes() == sentinel
            assert not output.exists() and not output.is_symlink()
            assert sorted(path.name for path in (tmp / "dist").iterdir()) == [archive.name]


def test_empty_output_directory_created_while_staging_is_preserved():
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        source = tmp / "source"
        _synthetic_source(source)
        output = tmp / "dist" / "pkg"
        archive = output.with_name(output.name + ".tar.gz")
        identities = []

        def publish_competitor_directory():
            output.mkdir()
            identities.append(os.stat(output).st_ino)

        with _after_first_copy(publish_competitor_directory) as fired:
            try:
                package_dist.build(source, _fake_binary(tmp), output)
            except package_dist.PackagingError as error:
                assert "output" in str(error).lower(), error
            else:
                raise AssertionError(
                    "an output directory created while staging must be refused, not replaced"
                )
            assert fired, "the output-directory race was never injected"
            assert identities, "the competing directory was never recorded"
            assert output.is_dir() and not output.is_symlink()
            assert os.stat(output).st_ino == identities[0]
            assert sorted(path.name for path in output.iterdir()) == []
            assert not archive.exists() and not archive.is_symlink()


def test_overlapping_invocations_yield_one_winner_and_one_refusal():
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        source = tmp / "source"
        _synthetic_source(source)
        labels = ("first", "second")
        binaries = {}
        for label in labels:
            binary_directory = tmp / label
            binary_directory.mkdir()
            binaries[label] = _fake_binary(binary_directory)
        output = tmp / "dist" / "pkg"
        archive = output.with_name(output.name + ".tar.gz")
        copies_per_build = len(package_dist.plan(source, binaries[labels[0]]))
        arrival = threading.Barrier(len(labels))
        real_copy2 = shutil.copy2
        counts = {}
        lock = threading.Lock()
        outcomes = {}

        def gated_copy2(copy_source, copy_target, **kwargs):
            result = real_copy2(copy_source, copy_target, **kwargs)
            label = threading.current_thread().name
            with lock:
                counts[label] = counts.get(label, 0) + 1
                staged = counts[label] == copies_per_build
            if staged:
                arrival.wait(timeout=30)
            return result

        def run(label):
            try:
                package_dist.build(source, binaries[label], output)
            except BaseException as error:  # noqa: BLE001 - classified below
                outcomes[label] = error
            else:
                outcomes[label] = None

        package_dist.shutil.copy2 = gated_copy2
        threads = [
            threading.Thread(target=run, args=(label,), name=label) for label in labels
        ]
        try:
            for thread in threads:
                thread.start()
            for thread in threads:
                thread.join(timeout=120)
        finally:
            package_dist.shutil.copy2 = real_copy2
        for thread in threads:
            assert not thread.is_alive(), thread.name
        assert sorted(outcomes) == sorted(labels), outcomes

        winners = [label for label, error in outcomes.items() if error is None]
        refused = [
            label for label, error in outcomes.items()
            if isinstance(error, package_dist.PackagingError)
        ]
        assert len(winners) == 1, outcomes
        assert len(refused) == 1, outcomes
        assert "exists" in str(outcomes[refused[0]]).lower(), outcomes[refused[0]]
        winner = winners[0]
        winner_binary = binaries[winner].read_bytes()

        assert output.is_dir() and not output.is_symlink()
        assert (output / "xeHentai").read_bytes() == winner_binary
        assert sorted(path.name for path in (tmp / "dist").iterdir()) == sorted(
            [output.name, archive.name]
        )
        expected = {relative for _, relative in package_dist.plan(source, binaries[winner])}
        assert set(_tree_files(output)) == expected
        prefix = output.name + "/"
        with tarfile.open(archive, "r:gz") as tar:
            members = {
                member.name: member for member in tar.getmembers() if member.isfile()
            }
            archived_names = {
                name[len(prefix):] for name in members if name.startswith(prefix)
            }
            assert archived_names == expected
            for relative in sorted(expected):
                archived = tar.extractfile(members[prefix + relative]).read()
                assert archived == (output / relative).read_bytes(), relative


def test_zip_format_contains_the_same_tree_with_exact_bytes():
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        binary = _fake_binary(tmp, "xeHentai.exe")
        output = tmp / "dist" / "xehentai-windows-x86_64"
        result = _run_zip(binary, output)
        assert result.returncode == 0, result.stderr
        archive = output.with_name(output.name + ".zip")
        assert archive.is_file()
        assert not output.with_name(output.name + ".tar.gz").exists()
        with zipfile.ZipFile(archive) as package:
            prefix = output.name + "/"
            names = {
                info.filename[len(prefix):]
                for info in package.infolist()
                if info.filename.startswith(prefix) and not info.is_dir()
            }
            expected = set(_tree_files(output))
            assert names == expected, sorted(names ^ expected)
            assert "xeHentai.exe" in names
            assert not any(name.startswith("ui/") for name in names), sorted(names)
            for relative in ["xeHentai.exe", "rss/rpc/handlers.rss", "locales/en_us.yml"]:
                assert package.read(prefix + relative) == (output / relative).read_bytes(), relative


def test_existing_zip_archive_is_refused_without_overwrite():
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        binary = _fake_binary(tmp, "xeHentai.exe")
        output = tmp / "dist" / "xehentai-windows-x86_64"
        output.parent.mkdir(parents=True)
        archive = output.with_name(output.name + ".zip")
        archive.write_bytes(b"existing archive\n")
        result = _run_zip(binary, output)
        assert result.returncode != 0
        assert "already exists" in result.stderr, result.stderr
        assert archive.read_bytes() == b"existing archive\n"
        assert not output.exists()


def test_unknown_archive_format_is_rejected():
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        try:
            package_dist.archive_path(tmp / "pkg", "7z")
        except package_dist.PackagingError as error:
            assert "unsupported archive format" in str(error), error
        else:
            raise AssertionError("an unknown archive format must be rejected")
        result = subprocess.run(
            [
                sys.executable,
                str(SCRIPT),
                "--binary",
                str(_fake_binary(tmp)),
                "--output",
                str(tmp / "dist" / "pkg"),
                "--format",
                "7z",
            ],
            capture_output=True,
            text=True,
            cwd=tempfile.gettempdir(),
        )
        assert result.returncode != 0
        assert "--format" in result.stderr, result.stderr


TESTS = [
    test_package_contains_runtime_files_with_exact_bytes,
    test_missing_binary_fails_and_writes_nothing,
    test_existing_output_is_rejected_and_preserved,
    test_missing_required_resource_is_rejected_without_partial_output,
    test_copy_failure_removes_partial_staging_output,
    test_archive_created_while_staging_is_refused_without_overwrite,
    test_empty_output_directory_created_while_staging_is_preserved,
    test_overlapping_invocations_yield_one_winner_and_one_refusal,
    test_zip_format_contains_the_same_tree_with_exact_bytes,
    test_existing_zip_archive_is_refused_without_overwrite,
    test_unknown_archive_format_is_rejected,
]


def main():
    failures = 0
    for test in TESTS:
        try:
            test()
        except Exception:  # noqa: BLE001 - report, keep running
            failures += 1
            print(f"FAIL {test.__name__}")
            traceback.print_exc()
        else:
            print(f"ok {test.__name__}")
    print(f"{len(TESTS) - failures}/{len(TESTS)} passed")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
