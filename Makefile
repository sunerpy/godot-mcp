# godot-mcp workspace Makefile.
#
# Aggregate gate targets frozen by Todo 1 (Verification strategy). Later Todos
# extend `godot-test` without changing these names. All Rust invocations pin the
# exact toolchain 1.96.0 (never the floating `stable` alias).

TOOLCHAIN ?= 1.96.0
CARGO := cargo +$(TOOLCHAIN)
PYTHON ?= python3
# The non-Rust formatter, pinned like every other tool; CI installs exactly this version.
OXFMT_VERSION := 0.64.0
CARGO_TARGET_DIR ?= target
NATIVE_RUST_TARGET := $(shell rustc +$(TOOLCHAIN) -vV | awk '/^host:/ { print $$2 }')
RELEASE_TARGET := $(if $(filter x86_64-unknown-linux-gnu,$(NATIVE_RUST_TARGET)),x86_64-unknown-linux-musl,$(NATIVE_RUST_TARGET))
EXE_SUFFIX := $(if $(filter x86_64-pc-windows-msvc,$(RELEASE_TARGET)),.exe,)
DEBUG_BUILD_DIR := $(CARGO_TARGET_DIR)/$(RELEASE_TARGET)/debug
RELEASE_BUILD_DIR := $(CARGO_TARGET_DIR)/$(RELEASE_TARGET)/release
RELEASE_PREFLIGHT = $(CARGO) run --locked -p godot-mcp-cli --features test-hooks \
	--bin release-package -- check-host

# ---------------------------------------------------------------------------
# Coverage gate (non-release). Single source of truth for the exclusion set.
#
# COVERAGE_IGNORE_REGEX is defined here and nowhere else: the workflow and any
# local run reach it only by invoking `make coverage` / `make coverage-gate`, so
# the measured number can never drift from the published one. The glob list in
# `codecov.yml` mirrors this regex for Codecov's own server-side filtering,
# because Codecov takes globs and not a regex; that file names this variable as
# the authority.
#
# What is excluded, and why each is defensible rather than gaming:
#   /tests/, /test/         integration test sources (currently matches nothing
#                           in the report universe; kept as a guard)
#   test_support            test-only harness code under src/
#   /fixture34/             test-only fixture builders under src/
#   /src/bin/               binary entrypoints; the logic they call lives in
#                           library crates that ARE measured
#   /qualification-tools/   release-orchestration tooling, not product code
#
# Deliberately NOT excluded: the cancelled-feature refusal branches. They are
# small, they carry a security-relevant fail-closed semantic, and the existing
# matrix tests already cover them.
COVERAGE_IGNORE_REGEX ?= (/tests?/|test_support|/fixture34/|/src/bin/|/qualification-tools/)

# The floor is a measured value, not an aspiration. See codecov.yml for the
# measurement provenance. Raising it is a deliberate ratchet step.
#
# It sits below the measured value on purpose. The provenance recorded in
# codecov.yml reports 83.17% (2026-08-06). A floor set flush against a
# single measurement would go red for normal instrumentation drift alone, and
# a gate that fails for reasons unrelated to the code gets deleted by the
# second person who hits it.
COVERAGE_FLOOR ?= 82.5

# llvm-cov's instrumented objects otherwise thrash the normal build cache, so
# coverage always builds into its own target directory.
COVERAGE_TARGET_DIR ?= $(CARGO_TARGET_DIR)/coverage
COVERAGE_DIR ?= $(CARGO_TARGET_DIR)/coverage-report
COVERAGE_LCOV := $(COVERAGE_DIR)/lcov.info
COVERAGE_JSON := $(COVERAGE_DIR)/coverage.json

COVERAGE_LLVM_COV = CARGO_TARGET_DIR=$(COVERAGE_TARGET_DIR) $(CARGO) llvm-cov

define COVERAGE_GATE_PY
import json, sys

path, floor = sys.argv[1], float(sys.argv[2])
lines = json.load(open(path))["data"][0]["totals"]["lines"]
count, covered = lines["count"], lines["covered"]
pct = (100.0 * covered / count) if count else 0.0
# Print the actual percentage on both paths, so a failure tells the reader how
# far off they are instead of only that they are off.
print(
    "coverage-gate: line coverage %.2f%% (%d/%d lines); floor %.2f%%"
    % (pct, covered, count, floor)
)
if count == 0:
    sys.stderr.write("coverage-gate: FAIL: the report contains no lines\n")
    sys.exit(1)
if pct + 1e-9 < floor:
    sys.stderr.write(
        "coverage-gate: FAIL: %.2f%% is %.2f points below the %.2f%% floor\n"
        % (pct, floor - pct, floor)
    )
    sys.exit(1)
print("coverage-gate: PASS")
endef
export COVERAGE_GATE_PY

.DEFAULT_GOAL := ci

.PHONY: fmt fmt-check lint test build deny sbom godot-test check ci hooks qa-task \
        release-gates-harness ci-matrix release-gate-specs \
        help host-package dist reproducibility coverage coverage-gate

## fmt: format the whole workspace in place: Rust with rustfmt, and the repository's own
## Markdown, YAML and JSON with oxfmt (.oxfmtignore keeps it out of the byte-pinned trees).
fmt:
	$(CARGO) fmt --all
	@command -v oxfmt >/dev/null 2>&1 \
		|| { echo "oxfmt is required: npm install --global oxfmt@$(OXFMT_VERSION)" >&2; exit 1; }
	oxfmt --write --no-error-on-unmatched-pattern --ignore-path .oxfmtignore .

## fmt-check: verify both formatters without writing (byte-stable).
fmt-check:
	$(CARGO) fmt --all -- --check
	@command -v oxfmt >/dev/null 2>&1 \
		|| { echo "oxfmt is required: npm install --global oxfmt@$(OXFMT_VERSION)" >&2; exit 1; }
	oxfmt --check --no-error-on-unmatched-pattern --ignore-path .oxfmtignore .

## lint: clippy with warnings denied across all targets and features.
lint:
	$(CARGO) clippy --workspace --all-targets --all-features -- -D warnings

## test: run the whole workspace test suite with all features.
test:
	$(CARGO) test --workspace --all-features

## build: locked release-shaped build of the whole workspace.
build:
	$(CARGO) build --workspace --all-features --locked

## deny: cargo-deny at the exact version tooling.lock.json pins.
deny:
	$(CARGO) deny check

## sbom: CycloneDX SBOM with the cargo-cyclonedx version tooling.lock.json pins.
sbom:
	$(CARGO) cyclonedx --format json

## godot-test: batch Godot fixture command. With no GODOT_ARTIFACT_URL it stays
## the stable no-op entry point Todo 1 froze. With GODOT_ARTIFACT_URL and
## GODOT_ARTIFACT_SHA256 (both supplied by the lock-derived CI matrix, never by
## hand) it fetches that exact artifact into a digest-keyed cache, verifies it,
## and runs the engine's locked-artifact check against it
## (crates/godot-mcp-engine/tests/locked_artifact.rs): an editor is probed and
## checks scripts, export templates must match the build, and with
## GODOT_DISPLAY_REQUIRED=1 the editor must also run windowed and render. A
## download that does not match the locked digest is a hard failure, so only
## hash-verified artifacts are ever cached or executed, and a run in which the
## check itself did not run fails too.
godot-test:
	@if [ -z "$(GODOT_ARTIFACT_URL)" ]; then \
		echo "godot-test: no locked Godot artifact requested (set GODOT_ARTIFACT_URL from make ci-matrix)"; \
		exit 0; \
	fi; \
	if [ -z "$(GODOT_ARTIFACT_SHA256)" ]; then \
		echo "godot-test: FAIL: GODOT_ARTIFACT_URL without GODOT_ARTIFACT_SHA256" >&2; exit 2; \
	fi; \
	artifact=$$(scripts/godot-artifact.sh --url "$(GODOT_ARTIFACT_URL)" \
		--sha256 "$(GODOT_ARTIFACT_SHA256)" --name "$(GODOT_ARTIFACT_NAME)") || exit 1; \
	echo "godot-test: verified artifact $$artifact"; \
	output=$$(GODOT_MCP_LOCKED_ARTIFACT="$$artifact" \
		GODOT_MCP_DISPLAY_REQUIRED="$(GODOT_DISPLAY_REQUIRED)" \
		$(CARGO) test -p godot-mcp-engine --all-features --test locked_artifact -- \
			--ignored --exact the_locked_artifact_serves_the_engine_plane 2>&1); \
	status=$$?; printf '%s\n' "$$output"; \
	[ "$$status" -eq 0 ] || exit "$$status"; \
	printf '%s\n' "$$output" | grep -q 'test result: ok. 1 passed' || { \
		echo "godot-test: FAIL: the locked-artifact check did not run" >&2; exit 1; }

## check: the local gate to run before pushing: formatting, lint, tests, build, licences.
check: fmt-check lint test build deny

## ci: the full aggregate gate sequence.
ci: check sbom godot-test

## hooks: install the pre-commit (make fmt) and pre-push (make lint test) hooks.
hooks:
	@command -v pre-commit >/dev/null 2>&1 \
		|| { echo "pre-commit is required: https://pre-commit.com/#install" >&2; exit 1; }
	pre-commit install --hook-type pre-commit --hook-type pre-push

## host-package: build the host platform's published archive and write the Task-34 host-package
## manifest. Every qa/tasks/34.json repair command resolves that manifest instead
## of an ambient-PATH binary, so this must exist before Task 34 runs. Fails closed
## if the release-graph audit reports anything.
host-package:
	$(RELEASE_PREFLIGHT)
	$(CARGO) build --locked --target $(RELEASE_TARGET) -p godot-mcp-cli --features http,auth --bin godot-mcp
	$(CARGO) build --locked --target $(RELEASE_TARGET) -p godot-mcp-engine --bin godot-mcp-supervisor
	$(CARGO) run --locked -p godot-mcp-cli --features test-hooks \
		--bin release-package -- host-package \
		--executable $(DEBUG_BUILD_DIR)/godot-mcp$(EXE_SUFFIX) \
		--supervisor $(DEBUG_BUILD_DIR)/godot-mcp-supervisor$(EXE_SUFFIX) --profile debug

## dist: publish the release archive set, SHA256SUMS.txt and the provenance
## attestation for every target this host can actually build. Targets without a
## toolchain here are recorded as not built rather than fabricated.
dist:
	$(RELEASE_PREFLIGHT)
	$(CARGO) build --locked --release --target $(RELEASE_TARGET) -p godot-mcp-cli --features http,auth --bin godot-mcp
	$(CARGO) build --locked --release --target $(RELEASE_TARGET) -p godot-mcp-engine --bin godot-mcp-supervisor
	$(CARGO) run --locked -p godot-mcp-cli --features test-hooks \
		--bin release-package -- dist \
		--executable $(RELEASE_BUILD_DIR)/godot-mcp$(EXE_SUFFIX) \
		--supervisor $(RELEASE_BUILD_DIR)/godot-mcp-supervisor$(EXE_SUFFIX) --profile release

## reproducibility: two clean release builds of the host platform target, compared byte for
## byte. Fails the gate unless the declared result holds.
reproducibility:
	$(RELEASE_PREFLIGHT)
	$(CARGO) clean --release --target $(RELEASE_TARGET) -p godot-mcp-cli
	$(CARGO) build --locked --release --target $(RELEASE_TARGET) -p godot-mcp-cli --features http,auth --bin godot-mcp
	mkdir -p $(CARGO_TARGET_DIR)/reproducibility
	cp $(RELEASE_BUILD_DIR)/godot-mcp$(EXE_SUFFIX) $(CARGO_TARGET_DIR)/reproducibility/build-1$(EXE_SUFFIX)
	$(CARGO) clean --release --target $(RELEASE_TARGET) -p godot-mcp-cli
	$(CARGO) build --locked --release --target $(RELEASE_TARGET) -p godot-mcp-cli --features http,auth --bin godot-mcp
	cp $(RELEASE_BUILD_DIR)/godot-mcp$(EXE_SUFFIX) $(CARGO_TARGET_DIR)/reproducibility/build-2$(EXE_SUFFIX)
	$(CARGO) run --locked -p godot-mcp-cli --features test-hooks \
		--bin release-package -- reproducibility \
		--first $(CARGO_TARGET_DIR)/reproducibility/build-1$(EXE_SUFFIX) \
		--second $(CARGO_TARGET_DIR)/reproducibility/build-2$(EXE_SUFFIX)

## coverage: instrumented coverage over the whole workspace. Writes an lcov file
## and a machine-readable JSON summary into the COVERAGE_DIR report directory,
## and prints the per-file human-readable table. Uses the single
## COVERAGE_IGNORE_REGEX exclusion set, and builds into its own
## COVERAGE_TARGET_DIR so llvm-cov's instrumented objects do not thrash the
## normal build cache. Runs the same tests as `make test`, so expect a
## comparable duration.
coverage:
	@mkdir -p $(COVERAGE_DIR)
	$(COVERAGE_LLVM_COV) --workspace --all-features --locked \
		--ignore-filename-regex '$(COVERAGE_IGNORE_REGEX)' \
		--lcov --output-path $(COVERAGE_LCOV)
	$(COVERAGE_LLVM_COV) report \
		--ignore-filename-regex '$(COVERAGE_IGNORE_REGEX)' --summary-only
	$(COVERAGE_LLVM_COV) report \
		--ignore-filename-regex '$(COVERAGE_IGNORE_REGEX)' \
		--json --output-path $(COVERAGE_JSON)
	@echo "coverage: lcov $(COVERAGE_LCOV)"
	@echo "coverage: json $(COVERAGE_JSON)"

## coverage-gate: fail unless line coverage is at or above COVERAGE_FLOOR. Prints
## the actual percentage on both the pass and the fail path. Deliberately not
## part of `ci`: that sequence mirrors the frozen required release checks, and
## coverage is a new non-release gate with its own workflow.
coverage-gate: coverage
	@$(PYTHON) -c "$$COVERAGE_GATE_PY" $(COVERAGE_JSON) $(COVERAGE_FLOOR)

## qa-task: run one QA task descriptor's commands. Usage: make qa-task TASK=<N>.
## Fails closed on a missing/invalid descriptor or any command exit mismatch.
## Task 34 additionally needs its host package on disk first; no other task's
## behavior changes.
qa-task:
	@if [ -z "$(TASK)" ]; then \
		echo "qa-task: FAIL: TASK=<N> is required" >&2; exit 2; \
	fi
	@if [ "$(TASK)" = "34" ]; then $(MAKE) host-package; fi
	$(PYTHON) scripts/qa_task.py $(TASK)

## release-gates-harness: the Todo-35-owned release-gates harness. Joins the
## lock-derived required-check catalogue to a candidate's deposited CI
## observations and emits the release-gates document plus its strict sidecar.
## Required variables, all seven, no defaults: CANDIDATE_WORKTREE SOURCE_COMMIT
## SOURCE_TREE ARCHIVES_DIR OUTPUT OUTPUT_SHA QUALIFYING. QUALIFYING=0 is an
## in-progress observation and may never write under .omo/evidence/; QUALIFYING=1
## is Todo 37's single final emission and is refused without its producer-owned
## orchestration context.
RELEASE_GATES_VARS := CANDIDATE_WORKTREE SOURCE_COMMIT SOURCE_TREE ARCHIVES_DIR \
                      OUTPUT OUTPUT_SHA QUALIFYING

release-gates-harness:
	@missing=""; \
	for v in $(RELEASE_GATES_VARS); do \
		eval "value=\$$$$v"; \
		if [ -z "$$value" ]; then missing="$$missing $$v"; fi; \
	done; \
	if [ -n "$$missing" ]; then \
		echo "release-gates-harness: FAIL: required variable(s) not set:$$missing" >&2; \
		echo "release-gates-harness: required variables are $(RELEASE_GATES_VARS)" >&2; \
		exit 2; \
	fi
	$(CARGO) run --locked -p godot-mcp-cli --features test-hooks \
		--bin release-package -- release-gates \
		--candidate-worktree "$(CANDIDATE_WORKTREE)" \
		--source-commit "$(SOURCE_COMMIT)" \
		--source-tree "$(SOURCE_TREE)" \
		--archives-dir "$(ARCHIVES_DIR)" \
		--output "$(OUTPUT)" \
		--output-sha "$(OUTPUT_SHA)" \
		--qualifying "$(QUALIFYING)"

## ci-matrix: print the lock-derived CI matrices (engine/editor and packaged
## smoke). The workflows call this; no version, URL or digest is written by hand.
ci-matrix:
	@$(CARGO) run --locked --quiet -p godot-mcp-cli --features test-hooks \
		--bin release-package -- ci-matrix

## release-gate-specs: print every required release-gate check and the exact
## command its runner must execute.
release-gate-specs:
	@$(CARGO) run --locked --quiet -p godot-mcp-cli --features test-hooks \
		--bin release-package -- release-gate-specs

## help: list the documented targets.
help:
	@grep -E '^## ' $(MAKEFILE_LIST) | sed 's/^## //'
