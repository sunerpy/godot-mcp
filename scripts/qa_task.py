#!/usr/bin/env python3
"""qa-task runner: the sole normative mechanical entry point for every Todo N.

Loads qa/tasks/<N>.json, validates it structurally (fail closed on a missing
descriptor, wrong task number, empty happy/failure arrays, empty assertions, an
angle-bracket transaction token, command-reuse shorthand, an evidence path that
names a repo-relative file which does not exist, or an expected-exit mismatch),
then runs each descriptor-owned command and compares its actual exit code
against the recorded expectation. Exits 0 only when every command matches.

What this runner decides mechanically is exit codes, and only exit codes. The
`assertions` list is prose written for a person reviewing the run: this runner
validates that the list is non-empty and holds no blank entry, and then never
looks at it again. It does not parse, interpret or evaluate an assertion, so a
task exiting 0 means every command exited as recorded — not that its assertions
were confirmed. That residual gap is deliberate and disclosed; see
docs/contributor/workflow.md.

Evidence is checked as far as it can be. An entry prefixed with the literal
`<attemptDir>/` placeholder names a file in the harness attempt directory, which
lives outside this repository: it is resolved and required to exist only when
QA_ATTEMPT_DIR names an existing directory, and is otherwise reported as
unchecked rather than passing silently. Any other value is read as a
repo-relative path and must exist. `<attemptDir>` is the only placeholder
recognised; any other angle-bracket token is rejected.

A descriptor marked `"status": "cancelled"` belongs to a cancelled Todo that
ships no test. It owns zero commands: this runner executes nothing, prints the
recorded reason and date, and exits 0. That is a declaration, not an escape
hatch, so it is fail-closed in both directions — a cancelled descriptor with a
leftover happy/failure command, a fixture lifecycle, or a missing/blank
cancelled_reason/cancelled_on is rejected, and an active descriptor carrying
cancellation fields is rejected too.

Task 34 uses the fixture-lifecycle wrapper: setup runs first, every command runs
regardless of individual failures, cleanup runs unconditionally in a
finally-equivalent path, and the final exit is nonzero if setup failed, any
command mismatched, or cleanup failed.

This runner never invokes `make qa-task` / `qa-task` itself (no recursion).
"""
import json
import os
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
QA = os.path.join(ROOT, "qa", "tasks")

FORBIDDEN_SHORTHAND = ["same as", "as above", "see above", "inherit", "repeat the", "...", "etc."]
WRAPPER_STRINGS = ["make qa-task", "qa-task TASK="]

# The only evidence placeholder recognised. It names the harness attempt
# directory, which is outside this repository, hence resolvable only via the env.
ATTEMPT_DIR_TOKEN = "<attemptDir>/"
ATTEMPT_DIR_ENV = "QA_ATTEMPT_DIR"


def die(msg, code=2):
    print(f"qa-task: FAIL: {msg}", file=sys.stderr)
    sys.exit(code)


def has_angle_bracket_token(cmd):
    i = 0
    while i < len(cmd):
        if cmd[i] == "<":
            close = cmd.find(">", i + 1)
            if close != -1:
                inner = cmd[i + 1:close]
                if inner and " " not in inner:
                    return True
        i += 1
    return False


def validate_command(task, label, i, c):
    cmd = c.get("command", "")
    if not isinstance(cmd, str) or not cmd.strip():
        die(f"task {task}: {label}[{i}] empty command")
    if "expected_exit" not in c or not isinstance(c["expected_exit"], int):
        die(f"task {task}: {label}[{i}] missing/invalid expected_exit")
    assertions = c.get("assertions")
    if not isinstance(assertions, list) or not assertions or any(not str(a).strip() for a in assertions):
        die(f"task {task}: {label}[{i}] empty assertions")
    for w in WRAPPER_STRINGS:
        if w in cmd:
            die(f"task {task}: {label}[{i}] recursively invokes the qa-task wrapper")
    if has_angle_bracket_token(cmd):
        die(f"task {task}: {label}[{i}] contains an angle-bracket <...> token")
    for p in FORBIDDEN_SHORTHAND:
        if p in cmd:
            die(f"task {task}: {label}[{i}] contains forbidden shorthand '{p}'")


def validate_evidence(n, evidence):
    if evidence.startswith(ATTEMPT_DIR_TOKEN):
        tail = evidence[len(ATTEMPT_DIR_TOKEN):].strip()
        if not tail:
            die(f"task {n}: evidence is the bare {ATTEMPT_DIR_TOKEN!r} placeholder with no file name")
        if has_angle_bracket_token(tail):
            die(f"task {n}: evidence carries an unrecognised angle-bracket token: {evidence!r}")
        attempt_dir = os.environ.get(ATTEMPT_DIR_ENV, "").strip()
        if not attempt_dir:
            print(
                f"qa-task:   [UNCHECKED] evidence {evidence} is attempt-directory-relative; "
                f"export {ATTEMPT_DIR_ENV} to have its existence verified",
                file=sys.stderr,
            )
            return
        if not os.path.isdir(attempt_dir):
            die(f"task {n}: {ATTEMPT_DIR_ENV}={attempt_dir!r} is not a directory")
        resolved = os.path.join(attempt_dir, tail)
        if not os.path.exists(resolved):
            die(f"task {n}: evidence {evidence} resolves to {resolved} which does not exist")
        print(f"qa-task:   [PASS] evidence exists: {resolved}", file=sys.stderr)
        return
    if has_angle_bracket_token(evidence):
        die(f"task {n}: evidence carries an unrecognised angle-bracket token: {evidence!r}")
    resolved = os.path.join(ROOT, evidence)
    if not os.path.exists(resolved):
        die(f"task {n}: evidence {evidence} does not exist at {resolved}")
    print(f"qa-task:   [PASS] evidence exists: {resolved}", file=sys.stderr)


def validate_cancelled(n, d):
    for key in ("happy", "failure"):
        arr = d.get(key, [])
        if not isinstance(arr, list):
            die(f"task {n}: cancelled descriptor has a non-list {key} field")
        if arr:
            die(f"task {n}: cancelled descriptor still carries {len(arr)} {key} command(s)")
    if d.get("fixture_lifecycle") is not None:
        die(f"task {n}: cancelled descriptor still carries a fixture lifecycle")
    if d.get("wrapper") == "fixture-lifecycle":
        die(f"task {n}: cancelled descriptor must not use the fixture-lifecycle wrapper")
    for key in ("cancelled_reason", "cancelled_on"):
        value = d.get(key)
        if not isinstance(value, str) or not value.strip():
            die(f"task {n}: cancelled descriptor without a {key}")


def load_and_validate(n):
    path = os.path.join(QA, f"{n}.json")
    if not os.path.isfile(path):
        die(f"missing descriptor {path}")
    try:
        d = json.load(open(path, encoding="utf-8"))
    except Exception as e:
        die(f"parse {path}: {e}")
    if d.get("task") != n:
        die(f"wrong task number: file says {d.get('task')} expected {n}")
    evidence = d.get("evidence", "")
    if not isinstance(evidence, str) or not evidence.strip():
        die(f"task {n}: empty evidence path")
    validate_evidence(n, evidence.strip())
    status = d.get("status", "active")
    if status not in ("active", "cancelled"):
        die(f"task {n}: unknown status {status!r}")
    if status == "cancelled":
        validate_cancelled(n, d)
        return d
    for key in ("cancelled_reason", "cancelled_on"):
        if d.get(key) is not None:
            die(f"task {n}: {key} on an active descriptor")
    for key in ("happy", "failure"):
        arr = d.get(key)
        if not isinstance(arr, list) or not arr:
            die(f"task {n}: empty {key} command array")
        for i, c in enumerate(arr):
            validate_command(n, key, i, c)
    if d.get("wrapper") == "fixture-lifecycle":
        fl = d.get("fixture_lifecycle")
        if not fl or not fl.get("setup") or not fl.get("cleanup"):
            die(f"task {n}: fixture-lifecycle wrapper without setup/cleanup")
    return d


def run(cmd):
    print(f"qa-task: run: {cmd}", file=sys.stderr)
    return subprocess.run(cmd, shell=True, cwd=ROOT).returncode


def check(cmd, expected):
    actual = run(cmd)
    ok = actual == expected
    status = "PASS" if ok else "FAIL"
    print(f"qa-task:   [{status}] exit {actual} (expected {expected}): {cmd}", file=sys.stderr)
    return ok


def main():
    if len(sys.argv) != 2:
        die("usage: qa_task.py <TASK-NUMBER>")
    try:
        n = int(sys.argv[1])
    except ValueError:
        die(f"task number must be an integer, got {sys.argv[1]!r}")
    if not (1 <= n <= 37):
        die(f"task number out of range: {n}")

    d = load_and_validate(n)

    if d.get("status") == "cancelled":
        print(
            f"qa-task: task {n} is CANCELLED on {d['cancelled_on']}; "
            f"0 commands executed. Reason: {d['cancelled_reason']}",
            file=sys.stderr,
        )
        sys.exit(0)

    commands = d["happy"] + d["failure"]

    if d.get("wrapper") == "fixture-lifecycle":
        fl = d["fixture_lifecycle"]
        setup_rc = run(fl["setup"])
        all_ok = setup_rc == 0
        if setup_rc != 0:
            print(f"qa-task:   [FAIL] setup exit {setup_rc}", file=sys.stderr)
        results = []
        if setup_rc == 0:
            for c in commands:
                results.append(check(c["command"], c["expected_exit"]))
        # finally-equivalent cleanup, always.
        cleanup_rc = run(fl["cleanup"])
        if cleanup_rc != 0:
            print(f"qa-task:   [FAIL] cleanup exit {cleanup_rc}", file=sys.stderr)
        final_ok = (setup_rc == 0) and all(results) and (cleanup_rc == 0)
        sys.exit(0 if final_ok else 1)

    ok = all(check(c["command"], c["expected_exit"]) for c in commands)
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
