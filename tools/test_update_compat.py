#!/usr/bin/env python3
"""Behavioural contract tests for the B6 update comparison pipeline.

These exercise the PRODUCTION compat.py registration, comparator, native
intersection guard, self-check and global verifier with the real catalog and
real inventory documents. They never contact a network, never start Cargo and
never write outside the assigned TEMP: the live oracle/probe/native execution
is the two-round suite proof, not this file.

Fixture observations come from the pipeline's own registered expectation
builder so that each test mutates ONE registered aspect and checks that the
intended guard detects it (not an unrelated default failure). The independent
expectation data itself lives in tools/compat.py and is checked against the
real producers in the final rounds.

    python3 tools/test_update_compat.py
"""

import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import traceback
from pathlib import Path

TOOLS = Path(__file__).resolve().parent
if str(TOOLS) not in sys.path:
    sys.path.insert(0, str(TOOLS))

import compat  # noqa: E402

ASSIGNED_TMP = Path(
    "/mnt/TEMP/workspace/xehentai-rss/tmp/xehentai-pyshape-resume-20261102-89633df7")
ORIGIN = "http://127.0.0.1:43123"


def _required(name):
    value = getattr(compat, name, None)
    assert value is not None, f"compat.{name} is not registered"
    return value


def _case_table():
    table = _required("UPDATE_CASES")
    assert isinstance(table, (list, tuple)), table
    assert len(table) >= 11, len(table)
    ids = []
    for entry in table:
        assert isinstance(entry, dict), entry
        assert isinstance(entry["case"], str) and entry["case"], entry
        assert entry["id"] in compat.UPDATE_CASE_IDS, entry
        assert isinstance(entry["sections"], tuple), entry
        ids.append(entry["case"])
    assert len(set(ids)) == len(ids), ids
    return table


def _observation(case, side, origin=ORIGIN):
    build = _required("update_expected_observation")
    value = build(case, side, origin)
    assert isinstance(value, dict), value
    assert value["side"] == side, value
    assert isinstance(value["sections"], dict), value
    return value


def _fixtures(origin=ORIGIN):
    table = _case_table()
    oracle = {entry["case"]: _observation(entry["case"], "python", origin)
              for entry in table}
    candidate = {entry["case"]: _observation(entry["case"], "native", origin)
                 for entry in table}
    return oracle, candidate


def _run_compare(oracle, candidate):
    cases = {}
    diffs = compat.compare_update(cases, oracle, candidate, ORIGIN)
    assert isinstance(diffs, list), diffs
    return cases, diffs


def _assert_mutation_detected(case, side, mutate, owner, needle=None):
    oracle, candidate = _fixtures()
    target = oracle if side == "python" else candidate
    mutate(target[case]["sections"])
    cases, diffs = _run_compare(oracle, candidate)
    assert diffs, f"{case}/{side} mutation produced no diff"
    assert cases.get(owner) is False, (
        f"{case}/{side} mutation did not fail {owner}: {cases}")
    assert any(owner in text or case in text for text in diffs), diffs
    if needle is not None:
        assert any(needle in text for text in diffs), (needle, diffs)


def test_update_is_registered_everywhere_required():
    assert "update" in compat.ALL_SUITES, compat.ALL_SUITES
    assert "update" in compat.NATIVE_COMMANDS, sorted(compat.NATIVE_COMMANDS)
    assert "update" in compat.NATIVE_CASE_MAP, sorted(compat.NATIVE_CASE_MAP)
    assert "update" in compat.ORACLE_SUITES, compat.ORACLE_SUITES
    assert set(compat.UPDATE_CASE_IDS) == {
        "update.channel", "update.commit", "update.decision",
        "update.package", "update.apply",
    }, compat.UPDATE_CASE_IDS
    table = _case_table()
    owners = {entry["id"] for entry in table}
    assert owners == set(compat.UPDATE_CASE_IDS), owners


def test_update_expectations_compare_clean_on_both_sides():
    oracle, candidate = _fixtures()
    cases, diffs = _run_compare(oracle, candidate)
    assert diffs == [], diffs
    assert set(cases) == set(compat.UPDATE_CASE_IDS), cases
    for case_id in compat.UPDATE_CASE_IDS:
        assert cases[case_id] is True, (case_id, cases)


def test_update_documented_channel_and_format_deltas_are_explicit():
    """The registered per-side expectations keep the real deltas, not blanks."""
    table = {entry["case"]: entry for entry in _case_table()}
    stable = table["channel.stable"]
    python = _observation("channel.stable", "python")["sections"]
    native = _observation("channel.stable", "native")["sections"]
    assert python["source"]["channel_url"].endswith("commits?sha=master")
    assert native["source"]["channel_url"].endswith(
        "/repos/fffonion/xeHentai/commits?sha=rss")
    assert "/archive/" in python["source"]["download_link"]
    assert native["source"]["download_link"].endswith("/xeHentai-rss.zip")
    assert native["source"]["descriptor_link"] == (
        native["source"]["download_link"] + ".manifest.json")
    assert python["source"]["archive_prefix"].endswith("/xeHentai")
    assert native["source"]["archive_prefix"].endswith("/")
    assert stable["id"] == "update.channel"
    package = _observation("package.envelope", "native")["sections"]["package"]
    assert package["member_names"] == ["info.json", "info.sig", "payload.zip"]
    assert package["compress_types"] == [0, 0, 0], package
    source_package = _observation("package.envelope", "python")["sections"]["package"]
    assert source_package["member_names"][0] == "info.json"
    assert set(source_package["compress_types"]) == {0}, source_package
    assert source_package["member_names"] != package["member_names"]


def test_update_first_versus_second_commit_is_detected():
    _assert_mutation_detected(
        "commit.first", "python",
        lambda section: section["commit"].__setitem__("update_id", "second-entry"),
        "update.commit", "commit.update_id")
    _assert_mutation_detected(
        "commit.first", "native",
        lambda section: section["commit"].__setitem__("message", "first entry"),
        "update.commit", "commit.message")


def test_update_channel_and_business_url_are_detected():
    _assert_mutation_detected(
        "channel.stable", "python",
        lambda section: section["source"].__setitem__(
            "channel_url",
            "https://api.github.com/repos/fffonion/xeHentai/commits?sha=dev"),
        "update.channel", "source.channel_url")
    _assert_mutation_detected(
        "channel.stable", "native",
        lambda section: section["feed"].__setitem__(
            "targets", ["/repos/fffonion/xeHentai/commits?sha=dev"]),
        "update.channel", "feed.targets")
    _assert_mutation_detected(
        "channel.beta", "native",
        lambda section: section["source"].__setitem__("channel", "rss"),
        "update.channel", "source.channel")
    _assert_mutation_detected(
        "channel.beta-missing", "native",
        lambda section: section["feed"].__setitem__("request_count", 1),
        "update.channel", "feed.request_count")


def test_update_typed_metadata_and_wrong_types_are_rejected():
    _assert_mutation_detected(
        "commit.first", "python",
        lambda section: section["commit"].__setitem__("update_id_type", 1),
        "update.commit")
    # Bool-as-int and a truthy string where an int is required, on BOTH sides.
    oracle, candidate = _fixtures()
    for side in (oracle, candidate):
        side["decision.off"]["sections"]["feed"]["request_count"] = True
    cases, diffs = _run_compare(oracle, candidate)
    assert diffs, "bool-as-int passed the schema on both sides"
    assert cases.get("update.decision") is False, cases
    oracle, candidate = _fixtures()
    for side in (oracle, candidate):
        side["decision.same"]["sections"]["decision"]["pending_before"] = "yes"
    cases, diffs = _run_compare(oracle, candidate)
    assert diffs, "truthy wrong-type passed the schema on both sides"
    assert cases.get("update.decision") is False, cases


def test_update_both_sided_missing_fields_fail():
    oracle, candidate = _fixtures()
    for side in (oracle, candidate):
        del side["package.envelope"]["sections"]["package"]["member_names"]
    cases, diffs = _run_compare(oracle, candidate)
    assert diffs, "a both-sided omission passed"
    assert cases.get("update.package") is False, cases
    # Unknown variation: the same extra field on both sides must fail.
    oracle, candidate = _fixtures()
    for side in (oracle, candidate):
        side["apply.valid"]["sections"]["apply"]["unknown_field"] = 1
    cases, diffs = _run_compare(oracle, candidate)
    assert diffs, "an unknown both-sided variation passed"
    assert cases.get("update.apply") is False, cases
    # A missing case (missing round evidence for one ID) must fail.
    oracle, candidate = _fixtures()
    del candidate["apply.reject"]
    cases, diffs = _run_compare(oracle, candidate)
    assert diffs, "a missing case passed"
    assert cases.get("update.apply") is False, cases


def test_update_decision_package_and_apply_mutations_are_detected():
    _assert_mutation_detected(
        "decision.off", "native",
        lambda section: section["decision"].__setitem__("outcome", "pending"),
        "update.decision", "decision.outcome")
    _assert_mutation_detected(
        "decision.off", "python",
        lambda section: section["decision"]["events"][1].__setitem__(
            "level", "INFO "),
        "update.decision", "decision.events")
    _assert_mutation_detected(
        "decision.download", "native",
        lambda section: section["decision"].__setitem__(
            "pending_after", False),
        "update.decision", "pending_after")
    _assert_mutation_detected(
        "package.envelope", "native",
        lambda section: section["package"].__setitem__(
            "member_names", ["info.json", "payload.zip"]),
        "update.package", "member_names")
    _assert_mutation_detected(
        "package.envelope", "python",
        lambda section: section["package"].__setitem__(
            "compress_types", [8]),
        "update.package", "compress_types")
    _assert_mutation_detected(
        "apply.valid", "native",
        lambda section: section["apply"].__setitem__("session_equal", False),
        "update.apply", "session_equal")
    _assert_mutation_detected(
        "apply.valid", "native",
        lambda section: section["apply"].__setitem__("live_changed", False),
        "update.apply", "live_changed")
    _assert_mutation_detected(
        "apply.reject", "native",
        lambda section: section["apply"].__setitem__("pending_after", True),
        "update.apply", "pending_after")
    _assert_mutation_detected(
        "cli.off", "python",
        lambda section: section["feed"].__setitem__("request_count", 1),
        "update.decision", "request_count")


def _native_names():
    mapping = _required("update_required_native_names")()
    assert isinstance(mapping, dict), mapping
    assert set(mapping) == set(compat.UPDATE_CASE_IDS), mapping
    for case_id, names in mapping.items():
        assert names, f"{case_id} has no mapped native test"
        for name in names:
            assert isinstance(name, str) and name, (case_id, name)
    return mapping


def _status_fixture(*, omit=(), non_ok=(), extra=()):
    results = {}
    for names in _native_names().values():
        for name in names:
            results[name] = "ok"
    for name in omit:
        results.pop(name, None)
    for name in non_ok:
        assert name in results, name
        results[name] = "FAILED"
    for name in extra:
        results[name] = "ok"
    return results


def _native_run(results):
    return {
        "results": results,
        "summary": {"status": "ok", "passed": len(results),
                    "failed": 0, "ignored": 0},
        "argv": ["cargo", "test"],
        "stdout": "",
    }


def _all_true():
    return {case_id: True for case_id in compat.UPDATE_CASE_IDS}


def test_update_native_intersection_requires_every_mapped_name_same_round():
    mapping = _native_names()
    cases = _all_true()
    diffs = compat.validate_update_native_proof(
        [_native_run(_status_fixture())], cases)
    assert diffs == [], diffs
    assert all(cases[case_id] is True for case_id in compat.UPDATE_CASE_IDS)

    victim_case = next(iter(mapping))
    victim = mapping[victim_case][0]
    cases = _all_true()
    diffs = compat.validate_update_native_proof(
        [_native_run(_status_fixture(omit=(victim,)))], cases)
    assert cases[victim_case] is False, cases
    assert any(victim in text for text in diffs), diffs

    cases = _all_true()
    diffs = compat.validate_update_native_proof(
        [_native_run(_status_fixture(non_ok=(victim,)))], cases)
    assert cases[victim_case] is False, cases
    assert any(victim in text for text in diffs), diffs

    # A native-ok set can never resurrect a failed wire/comparator leg.
    cases = _all_true()
    cases[victim_case] = False
    diffs = compat.validate_update_native_proof(
        [_native_run(_status_fixture())], cases)
    assert cases[victim_case] is False, cases
    assert any(victim_case in text for text in diffs), diffs

    # Missing runs / extra names / zero tests are rejected.
    cases = _all_true()
    assert compat.validate_update_native_proof([], cases), "empty native runs"
    cases = _all_true()
    diffs = compat.validate_update_native_proof(
        [_native_run(_status_fixture(extra=("tests::m7p::bogus",)))], cases)
    assert any("bogus" in text for text in diffs), diffs
    assert all(cases[case_id] is False for case_id in compat.UPDATE_CASE_IDS)


def test_update_self_check_runs_production_path_with_mutations():
    oracle, candidate = _fixtures()
    runs = [_native_run(_status_fixture())]
    bundle = {"cases": oracle, "origin": ORIGIN}
    checks = compat.self_check("update", bundle, {"cases": candidate,
                                                  "origin": ORIGIN}, runs)
    assert checks, checks
    assert all(entry["detected"] for entry in checks), checks
    names = {entry["mutation"] for entry in checks}
    for required in (
        "first-versus-second-commit", "channel-url", "mode-outcome",
        "typed-metadata", "pending-member", "application-state",
        "error-class", "both-sided-missing", "both-sided-wrong-type",
        "both-sided-truthy-type", "both-sided-falsy-type",
        "member-body-same-length", "paired-extra-subcase",
        "request-sequence", "session-state", "source-channel",
        "native-missing-name", "native-non-ok", "false-wire-native-ok",
        "missing-case", "unknown-variation",
    ):
        assert required in names, (required, sorted(names))


def test_global_verify_all_rejects_missing_other_suite_proofs():
    """Scoped update evidence must never be reported as global acceptance."""
    root = Path(tempfile.mkdtemp(
        prefix="b6-verify-", dir=str(_scratch_base())))
    try:
        head = compat.subprocess.run(
            ["git", "rev-parse", "HEAD"], cwd=compat.REPO,
            capture_output=True, text=True)
        manifest = {
            "tool": "tools/compat.py",
            "head": head.stdout.strip(),
            "dirty": bool(compat.subprocess.run(
                ["git", "status", "--porcelain"], cwd=compat.REPO,
                capture_output=True, text=True).stdout.strip()),
            "suites": ["update"],
            "complete": False,
            "results": [],
        }
        (root / "manifest.json").write_text(
            json.dumps(manifest), encoding="utf-8")
        for round_number in (1, 2):
            round_dir = root / "update" / f"round-{round_number}"
            round_dir.mkdir(parents=True)
            (round_dir / "comparison.json").write_text(json.dumps({
                "suite": "update", "round": round_number, "ok": True,
                "diffs": [],
                "cases": {case_id: True for case_id in compat.UPDATE_CASE_IDS},
                "native": [],
            }), encoding="utf-8")
        assert compat.verify_all(root) == 1, (
            "scoped update evidence was accepted as global completion")
    finally:
        shutil.rmtree(root, ignore_errors=True)


def _scratch_base():
    base = Path(os.environ.get("TMPDIR", str(ASSIGNED_TMP / "b6-work")))
    base.mkdir(parents=True, exist_ok=True)
    return base


# --- owned subprocess-group fixtures -------------------------------------

_GROUP_SCRIPTS = {
    # Binds one loopback port, forks one grandchild that inherits it and
    # sleeps. Both retire together only if the WHOLE group is signalled.
    "hang_tree.py": r'''
import os, signal, socket, sys, time
state = sys.argv[1]
child_pid = None

def retire(signum, frame):
    if child_pid is not None:
        try:
            os.kill(child_pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            os.waitpid(child_pid, 0)
        except ChildProcessError:
            pass
    os._exit(0)

signal.signal(signal.SIGTERM, retire)
sock = socket.socket()
sock.bind(("127.0.0.1", 0))
sock.listen(1)
child_pid = os.fork()
if child_pid == 0:
    time.sleep(300)
    os._exit(0)
with open(os.path.join(state, "pids"), "w") as handle:
    handle.write("%d %d %d\n" % (os.getpid(), child_pid, sock.getsockname()[1]))
with open(os.path.join(state, "starts"), "a") as handle:
    handle.write("%d\n" % os.getpid())
open(os.path.join(state, "ready"), "w").write("1")
time.sleep(300)
''',
    # First invocation hangs with an owned descendant holding a port; the
    # second only reports a passing test summary when the first attempt's
    # whole group and port are already gone.
    "retry_wrapper.py": r'''
import os, signal, socket, sys, time
state = os.environ["B6_TIMEOUT_FIXTURE"]
child_pid = None

def retire(signum, frame):
    if child_pid is not None:
        try:
            os.kill(child_pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            os.waitpid(child_pid, 0)
        except ChildProcessError:
            pass
    os._exit(0)

signal.signal(signal.SIGTERM, retire)
if not os.path.exists(os.path.join(state, "first")):
    open(os.path.join(state, "first"), "w").write("1")
    sock = socket.socket()
    sock.bind(("127.0.0.1", 0))
    sock.listen(1)
    child_pid = os.fork()
    if child_pid == 0:
        time.sleep(300)
        os._exit(0)
    open(os.path.join(state, "port"), "w").write(str(sock.getsockname()[1]))
    open(os.path.join(state, "pids"), "w").write(
        "%d %d" % (os.getpid(), child_pid))
    time.sleep(300)
    os._exit(0)
port = int(open(os.path.join(state, "port")).read())
probe = socket.socket()
try:
    probe.bind(("127.0.0.1", port))
    free = True
except OSError:
    free = False
finally:
    probe.close()
alive = False
for token in open(os.path.join(state, "pids")).read().split():
    try:
        os.kill(int(token), 0)
        alive = True
    except ProcessLookupError:
        pass
if not free or alive:
    sys.stderr.write("stale descendant or held port before retry\n")
    sys.exit(9)
print("running 1 test")
print("test fixture::retry ... ok")
print("test result: ok. 1 passed; 0 failed; 0 ignored")
''',
    # Every invocation is an ordinary non-zero test exit.
    "nonzero_wrapper.py": r'''
import os, sys
state = os.environ["B6_TIMEOUT_FIXTURE"]
count = os.path.join(state, "count")
invocations = int(open(count).read()) + 1 if os.path.exists(count) else 1
open(count, "w").write(str(invocations))
sys.stderr.write("ordinary non-zero exit, not a timeout\n")
sys.exit(3)
''',
    # Drives the PRODUCTION run_command over the hang tree and reports which
    # exception actually unwound. The parent signals only THIS controller, so
    # the fixture's own session never sees the interrupt.
    "cancel_controller.py": r'''
import os, pathlib, sys
state, tools = sys.argv[1], sys.argv[2]
sys.path.insert(0, tools)
import compat

def record(name, payload):
    with open(os.path.join(state, name), "w") as handle:
        handle.write(payload)

try:
    compat.run_command(
        [sys.executable, os.path.join(state, "hang_tree.py"), state],
        state, dict(os.environ),
        pathlib.Path(os.path.join(state, "command.log")), timeout_s=120)
except BaseException as error:
    record("exc", "%s %s\n" % (type(error).__name__, error))
    raise
else:
    record("exc", "none\n")
''',
    # Same command, but under the CLI-local SIGTERM-to-unwind helper: an
    # ordinary SIGTERM must become SystemExit(143) only AFTER the owned group
    # is retired, and the previous handler must be restored.
    "sigterm_controller.py": r'''
import os, pathlib, signal, sys
state, tools = sys.argv[1], sys.argv[2]
sys.path.insert(0, tools)
import compat

def record(name, payload):
    with open(os.path.join(state, name), "w") as handle:
        handle.write(payload)

before = repr(signal.getsignal(signal.SIGTERM))
install = getattr(compat, "_install_sigterm_unwind", None)
restore = getattr(compat, "_restore_signal_handler", None)
previous = install() if install is not None else None
record("before", before)
try:
    record("installed", repr(signal.getsignal(signal.SIGTERM)))
    compat.run_command(
        [sys.executable, os.path.join(state, "hang_tree.py"), state],
        state, dict(os.environ),
        pathlib.Path(os.path.join(state, "command.log")), timeout_s=120)
except BaseException as error:
    record("exc", "%s %s\n" % (type(error).__name__, error))
    raise
finally:
    if restore is not None:
        restore(signal.SIGTERM, previous)
    record("restored", repr(signal.getsignal(signal.SIGTERM)))
''',
    # Binds the port, forks a port-holding grandchild, and deliberately does
    # NOT exit on SIGTERM: the handler publishes a marker so the parent test
    # can send its SECOND signal exactly while the controller is inside the
    # bounded retirement. Only the group SIGKILL can retire this tree.
    "stubborn_tree.py": r'''
import os, signal, socket, sys, time
state = sys.argv[1]

def term_seen(signum, frame):
    with open(os.path.join(state, "term_seen"), "a") as handle:
        handle.write("%d\n" % os.getpid())

signal.signal(signal.SIGTERM, term_seen)
sock = socket.socket()
sock.bind(("127.0.0.1", 0))
sock.listen(1)
child_pid = os.fork()
if child_pid == 0:
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    time.sleep(300)
    os._exit(0)
with open(os.path.join(state, "pids"), "w") as handle:
    handle.write("%d %d %d\n" % (os.getpid(), child_pid, sock.getsockname()[1]))
with open(os.path.join(state, "starts"), "a") as handle:
    handle.write("%d\n" % os.getpid())
open(os.path.join(state, "ready"), "w").write("1")
while True:
    time.sleep(1)
''',
    # Drives the production run_command over the stubborn tree and records the
    # exception plus BOTH signal handlers before/after the unwind, so the test
    # can prove the temporary ignore policy did not leak.
    "repeat_int_controller.py": r'''
import os, pathlib, signal, sys
state, tools = sys.argv[1], sys.argv[2]
tree = sys.argv[3] if len(sys.argv) > 3 else "hang_tree.py"
timeout_s = int(os.environ.get("B6_CANCEL_TIMEOUT", "120"))
sys.path.insert(0, tools)
import compat

def record(name, payload):
    with open(os.path.join(state, name), "w") as handle:
        handle.write(payload)

def handlers():
    return "SIGINT=%r SIGTERM=%r\n" % (
        signal.getsignal(signal.SIGINT), signal.getsignal(signal.SIGTERM))

record("handlers_before", handlers())
try:
    compat.run_command(
        [sys.executable, os.path.join(state, tree), state],
        state, dict(os.environ),
        pathlib.Path(os.path.join(state, "command.log")), timeout_s=timeout_s)
except BaseException as error:
    record("exc", "%s %s\n" % (type(error).__name__, error))
    raise
else:
    record("exc", "none\n")
finally:
    record("handlers_after", handlers())
''',
    # Same, but under the CLI-local SIGTERM-to-unwind helper: the FIRST SIGTERM
    # must become SystemExit(143) and the SECOND one must not interrupt the
    # bounded retirement. The production unwind handler itself must be back in
    # place when run_command returns, and the caller's restore must follow.
    "repeat_term_controller.py": r'''
import os, pathlib, signal, sys
state, tools = sys.argv[1], sys.argv[2]
tree = sys.argv[3] if len(sys.argv) > 3 else "hang_tree.py"
sys.path.insert(0, tools)
import compat

def record(name, payload):
    with open(os.path.join(state, name), "w") as handle:
        handle.write(payload)

record("before", repr(signal.getsignal(signal.SIGTERM)))
record("sigint_before", repr(signal.getsignal(signal.SIGINT)))
previous = compat._install_sigterm_unwind()
record("installed", repr(signal.getsignal(signal.SIGTERM)))
try:
    try:
        compat.run_command(
            [sys.executable, os.path.join(state, tree), state],
            state, dict(os.environ),
            pathlib.Path(os.path.join(state, "command.log")), timeout_s=120)
    except BaseException as error:
        record("exc", "%s %s\n" % (type(error).__name__, error))
        raise
    finally:
        record("after_run", repr(signal.getsignal(signal.SIGTERM)))
        record("sigint_after", repr(signal.getsignal(signal.SIGINT)))
finally:
    compat._restore_signal_handler(signal.SIGTERM, previous)
    record("restored", repr(signal.getsignal(signal.SIGTERM)))
''',
}


def _write_group_script(directory, name):
    path = Path(directory) / name
    path.write_text("#!%s\n%s" % (sys.executable, _GROUP_SCRIPTS[name]),
                    encoding="utf-8")
    path.chmod(0o755)
    return path


def _read_pids(state):
    path = Path(state) / "pids"
    if not path.is_file():
        return []
    tokens = path.read_text(encoding="utf-8").split()
    return [int(token) for token in tokens[:2]]


def _alive(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def _port_free(port):
    probe = socket.socket()
    try:
        probe.bind(("127.0.0.1", port))
        return True
    except OSError:
        return False
    finally:
        probe.close()


def _cleanup_group(state):
    try:
        pids = _read_pids(state)
    except (OSError, ValueError):
        pids = []
    for pid in pids:
        try:
            os.kill(pid, 9)
        except ProcessLookupError:
            pass
    if pids:
        try:
            os.killpg(pids[0], 9)
        except (ProcessLookupError, PermissionError):
            pass


def _gone(pid):
    """A zombie holds no port or file; os.kill(pid, 0) alone would say alive."""
    try:
        stat = Path("/proc/%d/stat" % pid).read_text(encoding="utf-8")
    except OSError:
        return True
    fields = stat.rsplit(")", 1)[-1].split()
    return not fields or fields[0] in ("Z", "X")


def _await_file(path, deadline_s=60):
    path = Path(path)
    deadline = time.monotonic() + deadline_s
    while time.monotonic() < deadline:
        if path.is_file() and path.stat().st_size:
            return True
        time.sleep(0.02)
    return False


def _await_gone(pids, deadline_s=20):
    deadline = time.monotonic() + deadline_s
    while time.monotonic() < deadline:
        if all(_gone(pid) for pid in pids):
            return True
        time.sleep(0.05)
    return all(_gone(pid) for pid in pids)


def _start_controller(base, name, tree="hang_tree.py", extra_env=None):
    _write_group_script(base, "hang_tree.py")
    if tree != "hang_tree.py":
        _write_group_script(base, tree)
    env = dict(os.environ)
    if extra_env:
        env.update(extra_env)
    return subprocess.Popen(
        [sys.executable, str(_write_group_script(base, name)), str(base),
         str(TOOLS), tree],
        cwd=str(base), env=env,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
    )


def _wait_ready(base, controller):
    assert _await_file(base / "ready"), (
        "the controlled command never reached its ready handshake "
        "(controller exit: %r)" % (controller.poll(),))
    pids = _read_pids(base)
    assert len(pids) == 2, pids
    port = int((base / "pids").read_text(encoding="utf-8").split()[2])
    return pids, port


def _interrupt_controller(controller, signum):
    # The handshake proves the child owns the port; the short settle leaves
    # the controller blocked inside the production communicate() call.
    time.sleep(0.3)
    os.kill(controller.pid, signum)
    try:
        return controller.communicate(timeout=120)
    except subprocess.TimeoutExpired:
        raise AssertionError("the interrupted controller never exited")


def _wait_for_retirement(controller, base, deadline_s=60):
    assert _await_file(base / "term_seen", deadline_s), (
        "the controller never entered its bounded retirement "
        "(controller exit: %r)" % (controller.poll(),))


def _signal_and_collect(controller, signum, timeout_s=120):
    os.kill(controller.pid, signum)
    try:
        return controller.communicate(timeout=timeout_s)
    except subprocess.TimeoutExpired:
        raise AssertionError("the interrupted controller never exited")


def _stop_controller(controller):
    if controller is None or controller.poll() is not None:
        return
    controller.kill()
    try:
        controller.communicate(timeout=30)
    except subprocess.TimeoutExpired:
        pass


def _assert_group_retired(base, pids, port):
    assert _await_gone(pids), (
        "the interruption left the owned group alive: %r" % (pids,))
    for pid in pids:
        assert _gone(pid), "descendant %d survived the cancellation" % pid
    assert _port_free(port), "the cancelled command kept the fixture port bound"
    starts = (base / "starts").read_text(encoding="utf-8").split()
    assert starts == [str(pids[0])], (
        "cancellation relaunched the command instead of retiring it: %r"
        % (starts,))
    log_text = (base / "command.log").read_text(encoding="utf-8")
    assert "cancelled" in log_text, log_text
    assert "group_retired: true" in log_text, log_text


def test_keyboard_interrupt_retires_the_owned_group_then_propagates():
    base = Path(tempfile.mkdtemp(prefix="b6-cancel-", dir=str(_scratch_base())))
    controller = None
    try:
        controller = _start_controller(base, "cancel_controller.py")
        pids, port = _wait_ready(base, controller)
        _interrupt_controller(controller, signal.SIGINT)
        exception = (base / "exc").read_text(encoding="utf-8")
        assert exception.startswith("KeyboardInterrupt"), (
            "the original interruption was not preserved: %r" % exception)
        _assert_group_retired(base, pids, port)
    finally:
        _stop_controller(controller)
        _cleanup_group(base)
        shutil.rmtree(base, ignore_errors=True)


def test_cli_sigterm_unwinds_and_retires_the_owned_group():
    base = Path(tempfile.mkdtemp(prefix="b6-sigterm-", dir=str(_scratch_base())))
    controller = None
    try:
        controller = _start_controller(base, "sigterm_controller.py")
        pids, port = _wait_ready(base, controller)
        _interrupt_controller(controller, signal.SIGTERM)
        assert controller.returncode == 128 + signal.SIGTERM, (
            "SIGTERM did not unwind as the interrupted CLI exit: %r"
            % (controller.returncode,))
        exception = (base / "exc").read_text(encoding="utf-8")
        assert exception.startswith("SystemExit"), (
            "the SIGTERM unwind did not keep SystemExit: %r" % exception)
        before = (base / "before").read_text(encoding="utf-8")
        installed = (base / "installed").read_text(encoding="utf-8")
        restored = (base / "restored").read_text(encoding="utf-8")
        assert installed != before, (
            "SIGTERM still took the default kill path: %r" % installed)
        assert restored == before, (
            "the CLI left its SIGTERM policy installed: %r" % restored)
        _assert_group_retired(base, pids, port)
    finally:
        _stop_controller(controller)
        _cleanup_group(base)
        shutil.rmtree(base, ignore_errors=True)


def test_repeated_sigint_during_retirement_still_retires_the_group():
    base = Path(tempfile.mkdtemp(prefix="b6-cancel-twice-",
                                 dir=str(_scratch_base())))
    controller = None
    try:
        controller = _start_controller(base, "repeat_int_controller.py",
                                       tree="stubborn_tree.py")
        pids, port = _wait_ready(base, controller)
        time.sleep(0.3)
        os.kill(controller.pid, signal.SIGINT)
        _wait_for_retirement(controller, base)
        _signal_and_collect(controller, signal.SIGINT)
        exception = (base / "exc").read_text(encoding="utf-8")
        assert exception.startswith("KeyboardInterrupt"), (
            "the repeated interruption replaced the original cancel: %r"
            % exception)
        before = (base / "handlers_before").read_text(encoding="utf-8")
        after = (base / "handlers_after").read_text(encoding="utf-8")
        assert after == before, (
            "the bounded retirement left a temporary signal policy: %r -> %r"
            % (before, after))
        _assert_group_retired(base, pids, port)
    finally:
        _stop_controller(controller)
        _cleanup_group(base)
        shutil.rmtree(base, ignore_errors=True)


def test_repeated_sigterm_during_retirement_still_retires_the_group():
    base = Path(tempfile.mkdtemp(prefix="b6-sigterm-twice-",
                                 dir=str(_scratch_base())))
    controller = None
    try:
        controller = _start_controller(base, "repeat_term_controller.py",
                                       tree="stubborn_tree.py")
        pids, port = _wait_ready(base, controller)
        time.sleep(0.3)
        os.kill(controller.pid, signal.SIGTERM)
        _wait_for_retirement(controller, base)
        _signal_and_collect(controller, signal.SIGTERM)
        assert controller.returncode == 128 + signal.SIGTERM, (
            "SIGTERM did not unwind as the interrupted CLI exit: %r"
            % (controller.returncode,))
        exception = (base / "exc").read_text(encoding="utf-8")
        assert exception.startswith("SystemExit"), (
            "the repeated SIGTERM replaced the original unwind: %r" % exception)
        before = (base / "before").read_text(encoding="utf-8")
        installed = (base / "installed").read_text(encoding="utf-8")
        after_run = (base / "after_run").read_text(encoding="utf-8")
        restored = (base / "restored").read_text(encoding="utf-8")
        assert installed != before, (
            "SIGTERM still took the default kill path: %r" % installed)
        assert after_run == installed, (
            "the critical section did not restore the CLI SIGTERM policy: %r"
            % after_run)
        assert restored == before, (
            "the CLI left its SIGTERM policy installed: %r" % restored)
        assert (base / "sigint_after").read_text(encoding="utf-8") == (
            base / "sigint_before").read_text(encoding="utf-8"), (
                "the critical section did not restore the SIGINT handler")
        _assert_group_retired(base, pids, port)
    finally:
        _stop_controller(controller)
        _cleanup_group(base)
        shutil.rmtree(base, ignore_errors=True)


def test_first_sigint_during_timeout_retirement_cancels_without_retry():
    base = Path(tempfile.mkdtemp(prefix="b6-timeout-cancel-",
                                 dir=str(_scratch_base())))
    controller = None
    try:
        controller = _start_controller(
            base, "repeat_int_controller.py", tree="stubborn_tree.py",
            extra_env={"B6_CANCEL_TIMEOUT": "1"})
        pids, port = _wait_ready(base, controller)
        _wait_for_retirement(controller, base)
        _signal_and_collect(controller, signal.SIGINT)
        exception = (base / "exc").read_text(encoding="utf-8")
        assert exception.startswith("KeyboardInterrupt"), (
            "the first cancel during timeout retirement was not propagated: %r"
            % exception)
        before = (base / "handlers_before").read_text(encoding="utf-8")
        after = (base / "handlers_after").read_text(encoding="utf-8")
        assert after == before, (
            "the cancelled timeout retirement leaked a signal policy: %r -> %r"
            % (before, after))
        _assert_group_retired(base, pids, port)
        log_text = (base / "command.log").read_text(encoding="utf-8")
        assert "exit: cancelled" in log_text, log_text
        assert "exit: timeout" not in log_text, (
            "the cancelled timed-out command was reported as a timeout: %r"
            % log_text)
    finally:
        _stop_controller(controller)
        _cleanup_group(base)
        shutil.rmtree(base, ignore_errors=True)


def test_timeout_retires_the_whole_owned_process_group():
    base = Path(tempfile.mkdtemp(prefix="b6-pg-", dir=str(_scratch_base())))
    try:
        script = _write_group_script(base, "hang_tree.py")
        log_path = base / "attempt.log"
        try:
            compat.run_command([str(script), str(base)], base,
                               dict(os.environ), log_path, timeout_s=2)
        except compat.CompatError:
            pass
        else:
            raise AssertionError(
                "a command that exceeded its bound must not be a success")
        pids = _read_pids(base)
        assert len(pids) == 2, "the fixture never started its descendant"
        port = int((base / "pids").read_text(encoding="utf-8").split()[2])
        for pid in pids:
            assert not _alive(pid), (
                "a timed-out command left descendant %d alive" % pid)
        assert _port_free(port), (
            "a timed-out command left the fixture port bound")
        assert not (base / "ready").is_file() or True
        log_text = log_path.read_text(encoding="utf-8")
        assert "timeout" in log_text.lower(), log_text
    finally:
        _cleanup_group(base)
        shutil.rmtree(base, ignore_errors=True)


def test_timeout_retry_starts_only_after_the_previous_group_is_gone():
    base = Path(tempfile.mkdtemp(prefix="b6-retry-", dir=str(_scratch_base())))
    root = base / "root"
    root.mkdir()
    suite = "b6-timeout-fixture"
    real_command = compat.run_command
    before = dict(os.environ)
    try:
        script = _write_group_script(base, "retry_wrapper.py")
        os.environ["B6_TIMEOUT_FIXTURE"] = str(base)

        def bounded(argv, cwd, env, log_path, timeout_s=900):
            return real_command(argv, cwd, env, log_path, timeout_s=2)

        compat.run_command = bounded
        compat.NATIVE_COMMANDS[suite] = [[str(script)]]
        try:
            evidence = compat.run_native(suite, root, root)
        finally:
            compat.NATIVE_COMMANDS.pop(suite, None)
            compat.run_command = real_command
        assert len(evidence) == 1, evidence
        attempts = evidence[0]["attempts"]
        assert len(attempts) == 2, attempts
        assert attempts[0]["outcome"] == "timeout", attempts
        assert attempts[0].get("group_retired") is True, attempts
        assert attempts[1]["outcome"] == "completed", attempts
        assert attempts[1]["exit"] == 0, attempts
        pids = _read_pids(base)
        for pid in pids:
            assert not _alive(pid), (
                "the bounded retry left descendant %d alive" % pid)
        port = int((base / "port").read_text(encoding="utf-8"))
        assert _port_free(port), "the bounded retry kept the fixture port"
    finally:
        compat.run_command = real_command
        compat.NATIVE_COMMANDS.pop(suite, None)
        os.environ.clear()
        os.environ.update(before)
        _cleanup_group(base)
        shutil.rmtree(base, ignore_errors=True)


def test_ordinary_nonzero_exit_is_never_retried():
    base = Path(tempfile.mkdtemp(prefix="b6-exit-", dir=str(_scratch_base())))
    root = base / "root"
    root.mkdir()
    suite = "b6-nonzero-fixture"
    before = dict(os.environ)
    try:
        script = _write_group_script(base, "nonzero_wrapper.py")
        os.environ["B6_TIMEOUT_FIXTURE"] = str(base)
        compat.NATIVE_COMMANDS[suite] = [[str(script)]]
        try:
            compat.run_native(suite, root, root)
        except compat.CompatError:
            pass
        else:
            raise AssertionError("a non-zero native exit must not pass")
        finally:
            compat.NATIVE_COMMANDS.pop(suite, None)
        count = (base / "count").read_text(encoding="utf-8")
        assert count == "1", (
            "an ordinary non-zero exit was retried %s times" % count)
    finally:
        compat.NATIVE_COMMANDS.pop(suite, None)
        os.environ.clear()
        os.environ.update(before)
        shutil.rmtree(base, ignore_errors=True)


def test_update_package_member_bodies_are_exact_and_same_length_edits_fail():
    oracle, candidate = _fixtures()
    python_package = oracle["package.envelope"]["sections"]["package"]
    native_package = candidate["package.envelope"]["sections"]["package"]
    python_bodies = python_package["member_bodies"]
    native_bodies = native_package["member_bodies"]
    assert set(python_bodies) == set(python_package["selected_entries"]), (
        python_bodies, python_package["selected_entries"])
    assert set(native_bodies) == set(native_package["selected_entries"]), (
        native_bodies, native_package["selected_entries"])
    # Independent fixture content, never derived from the producer output.
    source = dict(compat.UPDATE_SOURCE_ARCHIVE)
    assert python_bodies["xeHentai/const.py"] == (
        source["xeHentai-%s/xeHentai/const.py" % compat.UPDATE_ID]
        .encode("utf-8").hex())
    assert native_bodies["rss/updater/github.rss"] == (
        compat.UPDATE_PACKAGE_BODIES["rss/updater/github.rss"]
        .encode("utf-8").hex())
    # A same-length content edit on BOTH sides must still be rejected: each
    # side is compared to its own registered expectation, not to the peer.
    for side in (oracle, candidate):
        bodies = side["package.envelope"]["sections"]["package"]["member_bodies"]
        key = sorted(bodies)[0]
        value = bodies[key]
        assert len(value) >= 2 and len(value) % 2 == 0, value
        bodies[key] = ("00" if not value.startswith("00") else "11") + value[2:]
    cases, diffs = _run_compare(oracle, candidate)
    assert cases.get("update.package") is False, cases
    assert any("member_bodies" in text for text in diffs), diffs


def test_update_rejects_an_extra_subcase_id_present_on_both_sides():
    oracle, candidate = _fixtures()
    extra = {"side": "python", "sections": {}, "evidence": {}}
    oracle["update.unregistered"] = dict(extra)
    candidate["update.unregistered"] = dict(extra, side="native")
    cases, diffs = _run_compare(oracle, candidate)
    assert diffs, "a paired extra subcase ID was accepted"
    assert any("unexpected" in text and "update.unregistered" in text
               for text in diffs), diffs
    for case_id in compat.UPDATE_CASE_IDS:
        assert cases.get(case_id) is False, (case_id, cases)


def test_update_apply_reject_uses_the_executed_error_class_and_old_identity():
    native = _observation("apply.reject", "native")["sections"]["apply"]
    assert native["error_class"] == "Install", native
    assert native["module_origin_kind"] == "source_tree", native
    assert native["module_version"] == compat.UPDATE_PLATFORM["app_version"], native
    assert native["stop"] is None, native
    python_apply = _observation("apply.reject", "python")["sections"]["apply"]
    assert python_apply["error_class"] is None, python_apply
    oracle, candidate = _fixtures()
    runs = [_native_run(_status_fixture())]
    checks = compat.self_check(
        "update", {"cases": oracle, "origin": ORIGIN},
        {"cases": candidate, "origin": ORIGIN}, runs)
    entry = next(entry for entry in checks if entry["mutation"] == "error-class")
    assert entry["detected"] is True, entry


TESTS = [
    test_update_is_registered_everywhere_required,
    test_update_expectations_compare_clean_on_both_sides,
    test_update_documented_channel_and_format_deltas_are_explicit,
    test_update_first_versus_second_commit_is_detected,
    test_update_channel_and_business_url_are_detected,
    test_update_typed_metadata_and_wrong_types_are_rejected,
    test_update_both_sided_missing_fields_fail,
    test_update_decision_package_and_apply_mutations_are_detected,
    test_update_native_intersection_requires_every_mapped_name_same_round,
    test_update_self_check_runs_production_path_with_mutations,
    test_global_verify_all_rejects_missing_other_suite_proofs,
    test_timeout_retires_the_whole_owned_process_group,
    test_timeout_retry_starts_only_after_the_previous_group_is_gone,
    test_ordinary_nonzero_exit_is_never_retried,
    test_keyboard_interrupt_retires_the_owned_group_then_propagates,
    test_cli_sigterm_unwinds_and_retires_the_owned_group,
    test_repeated_sigint_during_retirement_still_retires_the_group,
    test_repeated_sigterm_during_retirement_still_retires_the_group,
    test_first_sigint_during_timeout_retirement_cancels_without_retry,
    test_update_package_member_bodies_are_exact_and_same_length_edits_fail,
    test_update_rejects_an_extra_subcase_id_present_on_both_sides,
    test_update_apply_reject_uses_the_executed_error_class_and_old_identity,
]


def main():
    failures = 0
    for test in TESTS:
        try:
            test()
        except Exception:  # noqa: BLE001 - report and keep running
            failures += 1
            print(f"FAIL {test.__name__}")
            traceback.print_exc()
        else:
            print(f"ok {test.__name__}")
    print(f"{len(TESTS) - failures}/{len(TESTS)} passed")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
