//! Todo 11 acceptance tests: incremental index persistence and filesystem
//! coherence.
//!
//! Exactly two test cases, matching the `qa/tasks/11.json` descriptor:
//! `index_persistence_happy_path` and `index_persistence_failure_paths`.
//!
//! Both cases run against a *copy* of the committed fixture project. The copy is
//! edited on purpose — that is the point of an incremental index — and the
//! committed fixture's bytes are compared before and after every case to prove the
//! repository tree itself is never mutated.
//!
//! Every project byte is read through Todo 8's [`ResolvedProject`], which is built
//! on Todo 2's held-object ownership layer (Unix object identity on this host), so
//! the tests assert the real behaviour on Unix and are compiled out elsewhere
//! rather than silently weakened.
//!
//! The debounce window is driven by a caller-supplied logical tick rather than by
//! sleeping, so the timing assertions are exact instead of flaky.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use godot_mcp_analysis::index::{
    Coherence, FileFingerprint, FileInventory, INDEX_MANIFEST_FILE, INDEX_SCHEMA,
    INDEX_SCHEMA_VERSION, INDEX_STORE_FILE, IndexCacheKey, IndexSnapshot, ProjectIndex,
    RescanReason, ScanKind, SliceDigest, StaleReason, WatchBatch, WatchDebouncer, WatchSignal,
};
use godot_mcp_analysis::project::{ProjectResolver, ResolvedProject};
use godot_mcp_domain::Confidence;
use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::contracts::ProvenanceSource;
use notify::event::{CreateKind, DataChange, EventAttributes, Flag, ModifyKind, RemoveKind};
use notify::{Event, EventKind};

/// Generation every fixture read is attributed to.
const GENERATION: u64 = 11;

/// Quiet window, in the logical ticks the tests supply.
const QUIET: u64 = 50;

/// Distinct pending signals a debouncer holds before degrading to an overflow.
const CAPACITY: usize = 4;

/// Every project-relative path the fixture project contains.
const INVENTORY: &[&str] = &[
    "project.godot",
    "main.tscn",
    "scenes/hud.tscn",
    "resources/buff.tres",
    "scripts/game_state.gd",
    "scripts/player.gd",
    "scripts/hud.gd",
    "scripts/dynamic.gd",
    "scripts/spare.gd",
];

fn fixture_project() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("index")
        .join("sample")
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("create destination directory");
    for entry in fs::read_dir(from).expect("read fixture directory") {
        let entry = entry.expect("fixture entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("fixture entry type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("copy fixture file");
        }
    }
}

fn snapshot_tree(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    collect(dir, dir, &mut out);
    out
}

fn collect(base: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
    for entry in fs::read_dir(dir).expect("read directory") {
        let entry = entry.expect("entry");
        let path = entry.path();
        if entry.file_type().expect("entry type").is_dir() {
            collect(base, &path, out);
        } else {
            let relative = path
                .strip_prefix(base)
                .expect("relative path")
                .to_path_buf();
            out.insert(relative, fs::read(&path).expect("read file"));
        }
    }
}

struct Fixture {
    _tmp: tempfile::TempDir,
    base: PathBuf,
    root: PathBuf,
    lease_state: PathBuf,
    index_state: PathBuf,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().canonicalize().expect("canonicalize temp base");
    let root = base.join("game");
    copy_tree(&fixture_project(), &root);
    Fixture {
        _tmp: tmp,
        lease_state: base.join("lease-state"),
        index_state: base.join("index-state"),
        base,
        root,
    }
}

fn inventory_of(paths: &[&str]) -> FileInventory {
    FileInventory::from_paths(paths.iter().copied()).expect("the declared inventory is in-root")
}

fn inventory() -> FileInventory {
    inventory_of(INVENTORY)
}

fn slices_of(snapshot: &IndexSnapshot) -> BTreeMap<String, SliceDigest> {
    snapshot.slices().clone()
}

fn modified(root: &Path, relative: &str) -> Event {
    Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Content)))
        .add_path(root.join(relative))
}

fn created(root: &Path, relative: &str) -> Event {
    Event::new(EventKind::Create(CreateKind::File)).add_path(root.join(relative))
}

fn removed(root: &Path, relative: &str) -> Event {
    Event::new(EventKind::Remove(RemoveKind::File)).add_path(root.join(relative))
}

fn overflow_event() -> Event {
    Event {
        kind: EventKind::Any,
        paths: Vec::new(),
        attrs: EventAttributes::new(),
    }
    .set_flag(Flag::Rescan)
}

/// Drive a debouncer through its quiet window and take the collapsed batch,
/// asserting the window really gates the flush.
fn coalesce(root: &Path, events: &[Event]) -> WatchBatch {
    let mut debouncer = WatchDebouncer::new(root, QUIET, CAPACITY).expect("build a debouncer");
    for (offset, event) in events.iter().enumerate() {
        debouncer.offer(event, offset as u64);
    }
    let last = events.len().saturating_sub(1) as u64;
    assert!(
        !debouncer.ready(last + QUIET - 1),
        "the batch must stay closed until the quiet window elapses"
    );
    assert!(
        debouncer.ready(last + QUIET),
        "the batch must open once the quiet window elapsed"
    );
    debouncer.flush()
}

/// Collapse already-normalized signals, one per tick.
fn coalesce_signals(root: &Path, signals: &[WatchSignal]) -> WatchBatch {
    let mut debouncer = WatchDebouncer::new(root, QUIET, CAPACITY).expect("build a debouncer");
    for (offset, signal) in signals.iter().enumerate() {
        debouncer.record(signal.clone(), offset as u64);
    }
    debouncer.flush()
}

fn append(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    let mut bytes = fs::read(&path).expect("read the fixture file");
    bytes.extend_from_slice(text.as_bytes());
    fs::write(&path, bytes).expect("write the fixture file");
}

/// A deterministic permutation of `len` indices, seeded by `seed`. No RNG, so a
/// failing order is reproducible from the seed alone.
fn permutation(len: usize, seed: u64) -> Vec<usize> {
    let mut order: Vec<usize> = (0..len).collect();
    let mut state = seed | 1;
    for position in (1..len).rev() {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let pick = (state >> 33) as usize % (position + 1);
        order.swap(position, pick);
    }
    order
}

/// Whether any file under `cache` contains the project root's path spelling.
///
/// The cache may remember slices, digests and the opaque project object id; it
/// must never persist the display path, because that would re-key ownership by
/// path through the back door.
fn cache_leaks_path(cache: &Path, root: &Path) -> bool {
    if !cache.exists() {
        return false;
    }
    let needle = root.to_string_lossy().into_owned().into_bytes();
    snapshot_tree(cache)
        .values()
        .any(|bytes| bytes.windows(needle.len()).any(|window| window == needle))
}

/// The happy path: a cold scan persists, a warm reopen reproduces it byte for
/// byte without reparsing, and editing exactly one script moves exactly that
/// script's slice digest plus the whole-graph digest.
#[test]
#[allow(clippy::too_many_lines)]
fn index_persistence_happy_path() {
    let committed = snapshot_tree(&fixture_project());
    let fixture = fixture();
    let project_before = snapshot_tree(&fixture.root);
    let resolver = ProjectResolver::new(&fixture.lease_state);
    let project: ResolvedProject = resolver
        .resolve(&fixture.root, GENERATION)
        .expect("resolve the fixture project");
    let inventory = inventory();

    // -- 1. the cache is lease-owned and keyed by identity, never by path -----
    let mut index = ProjectIndex::open(&project, &fixture.index_state)
        .expect("open the lease-owned index cache");
    let key = index.cache_key().clone();
    assert_eq!(
        key,
        IndexCacheKey::from_identity(project.identity()),
        "the cache key is derived from the project identity alone"
    );
    assert!(
        index.cache_dir().starts_with(&fixture.index_state),
        "the cache lives in the per-user state directory: {:?}",
        index.cache_dir()
    );
    assert_eq!(
        index.cache_dir(),
        key.cache_dir(&fixture.index_state),
        "the cache directory is named by the key"
    );
    let spelling = key.directory_name().to_owned();
    assert!(
        !spelling.contains('/') && !spelling.contains('\\'),
        "the key is a single path component: {spelling}"
    );
    for leaked in ["game", "index-state", "tmp"] {
        assert!(
            !spelling.contains(leaked),
            "the key must not embed the opened path: {spelling}"
        );
    }
    assert_eq!(
        index.display_root(),
        fixture.root.as_path(),
        "the canonical display path is retained as metadata"
    );
    assert_eq!(index.project(), project.identity());

    // -- 2. the cold scan ----------------------------------------------------
    let cold = index.scan(&inventory).expect("cold scan");
    assert_eq!(cold.scan_kind(), ScanKind::Cold);
    assert!(cold.scan_kind().is_full());
    assert_eq!(cold.scan_kind().code(), "cold");
    assert_eq!(cold.schema(), INDEX_SCHEMA);
    assert_eq!(cold.schema_version(), INDEX_SCHEMA_VERSION);
    assert_eq!(cold.project(), project.identity());
    assert_eq!(cold.project().generation(), GENERATION);
    assert_eq!(cold.graph().project_generation(), GENERATION);
    assert_eq!(cold.coherence(), &Coherence::Coherent);
    assert_eq!(cold.coherence().code(), "coherent");
    assert!(cold.is_current(), "a fresh cold scan is current");
    assert_eq!(
        cold.provenance().source(),
        ProvenanceSource::StaticIndex,
        "a coherent snapshot is complete index evidence"
    );
    assert_eq!(
        cold.provenance().confidence(),
        Confidence::Exact,
        "a coherent snapshot may answer exactly"
    );
    assert_eq!(cold.provenance().project_generation(), GENERATION);

    // Every declared file has a slice, and each slice fingerprints its own bytes.
    assert_eq!(cold.slices().len(), INVENTORY.len());
    for relative in INVENTORY {
        let slice = cold
            .slice(relative)
            .unwrap_or_else(|| panic!("{relative} has a slice"));
        assert_eq!(slice.file(), *relative);
        let bytes = fs::read(fixture.root.join(relative)).expect("read the fixture file");
        assert_eq!(
            slice.fingerprint(),
            &FileFingerprint::of_bytes(&bytes),
            "{relative} fingerprints its own bytes"
        );
        assert!(!slice.digest().is_empty(), "{relative} has a slice digest");
    }
    assert!(!cold.graph_digest().is_empty());

    // A fingerprint is a function of bytes only, so equal bytes never force a
    // reparse and changed bytes always do.
    assert_eq!(
        FileFingerprint::of_bytes(b"extends Node\n"),
        FileFingerprint::of_bytes(b"extends Node\n")
    );
    assert_ne!(
        FileFingerprint::of_bytes(b"extends Node\n"),
        FileFingerprint::of_bytes(b"extends Node2D\n")
    );
    assert!(!FileFingerprint::of_bytes(b"").is_empty());

    // -- 3. serialization is byte-stable ------------------------------------
    let cold_json = cold.to_canonical_json().expect("render the cold snapshot");
    assert_eq!(
        cold.to_canonical_json().expect("render it again"),
        cold_json,
        "repeated serialization of one snapshot is byte-identical"
    );
    let cold_graph_json = cold
        .graph()
        .to_canonical_json()
        .expect("render the cold graph");
    let cold_slices = slices_of(&cold);
    let cold_digest = cold.graph_digest().to_owned();

    // -- 4. the store is on disk, and nothing in-project was written ---------
    assert!(
        index.cache_dir().join(INDEX_STORE_FILE).is_file(),
        "the scan persisted a store"
    );
    assert!(
        index.cache_dir().join(INDEX_MANIFEST_FILE).is_file(),
        "the scan persisted a manifest"
    );
    assert_eq!(
        index
            .stored_schema_version()
            .expect("read the stored layout version"),
        Some(INDEX_SCHEMA_VERSION)
    );
    assert_eq!(
        snapshot_tree(&fixture.root),
        project_before,
        "indexing rewrote no byte of the project"
    );
    assert!(
        !cache_leaks_path(&fixture.index_state, &fixture.root),
        "the cache never persists the project's display path"
    );

    // -- 5. concurrent readers observe one immutable snapshot ---------------
    let reader = index.reader().expect("take a reader");
    assert!(reader.is_current());
    assert_eq!(reader.scan_kind(), ScanKind::Cold);
    assert_eq!(reader.graph_digest(), cold_digest);
    assert_eq!(reader.coherence(), &Coherence::Coherent);
    std::thread::scope(|scope| {
        for _ in 0..4 {
            let reader = reader.clone();
            let expected_digest = cold_digest.clone();
            let expected_graph = cold_graph_json.clone();
            scope.spawn(move || {
                assert_eq!(reader.graph_digest(), expected_digest);
                assert!(reader.is_current());
                assert_eq!(
                    reader
                        .graph()
                        .to_canonical_json()
                        .expect("render from a reader"),
                    expected_graph
                );
                assert_eq!(reader.snapshot().graph_digest(), expected_digest);
            });
        }
    });

    // -- 6. cold/warm parity across a fresh open ----------------------------
    drop(reader);
    drop(index);
    let mut warm_index =
        ProjectIndex::open(&project, &fixture.index_state).expect("reopen the index cache");
    assert_eq!(warm_index.cache_key(), &key, "the cache is reused");
    assert!(
        warm_index.snapshot().is_none(),
        "a freshly opened index has produced no snapshot yet"
    );
    assert_eq!(
        warm_index
            .coherence(&inventory)
            .expect("classify the reopened store"),
        Coherence::Coherent,
        "nothing changed on disk, so the store is coherent"
    );
    let warm = warm_index.scan(&inventory).expect("warm scan");
    assert_eq!(
        warm.scan_kind(),
        ScanKind::Warm,
        "a coherent store is reused without reparsing"
    );
    assert!(!warm.scan_kind().is_full());
    assert!(warm.is_current());
    assert_eq!(warm.graph_digest(), cold_digest, "warm parity: same digest");
    assert_eq!(slices_of(&warm), cold_slices, "warm parity: same slices");
    assert_eq!(
        warm.graph()
            .to_canonical_json()
            .expect("render the warm graph"),
        cold_graph_json,
        "warm parity: byte-identical graph"
    );
    assert_eq!(
        warm.to_canonical_json().expect("render the warm snapshot"),
        cold_json,
        "a warm snapshot is byte-identical to the cold one it reused"
    );

    // -- 7. editing one script moves exactly one slice ----------------------
    append(
        &fixture.root,
        "scripts/game_state.gd",
        "\nfunc reset() -> void:\n\thits = 0\n",
    );
    let verdict = warm_index
        .coherence(&inventory)
        .expect("classify after the edit");
    assert_eq!(
        verdict.stale_reason(),
        Some(StaleReason::FileContentChanged),
        "an edited file makes the store stale, not unusable: {verdict:?}"
    );
    assert_eq!(
        verdict.subjects(),
        ["scripts/game_state.gd".to_owned()],
        "the verdict names the exact file"
    );
    assert!(!verdict.requires_rescan());
    assert!(!verdict.is_coherent());
    assert_eq!(verdict.code(), "stale");

    let batch = coalesce(
        &fixture.root,
        &[modified(&fixture.root, "scripts/game_state.gd")],
    );
    assert!(!batch.overflowed());
    assert!(!batch.is_empty());
    assert_eq!(
        batch.changed().collect::<Vec<_>>(),
        ["scripts/game_state.gd"]
    );
    assert_eq!(batch.removed().count(), 0);
    assert_eq!(batch.renamed().count(), 0);
    assert_eq!(batch.touched(), 1);

    let edited = warm_index
        .apply(&batch, &inventory)
        .expect("apply the one-file change");
    assert_eq!(edited.scan_kind(), ScanKind::Incremental);
    assert_eq!(edited.scan_kind().code(), "incremental");
    assert!(edited.is_current(), "the applied result is current");
    assert_ne!(
        edited.graph_digest(),
        cold_digest,
        "the whole-graph digest moves when a slice moves"
    );
    let edited_slices = slices_of(&edited);
    assert_ne!(
        edited_slices["scripts/game_state.gd"].digest(),
        cold_slices["scripts/game_state.gd"].digest(),
        "the edited file's slice digest moved"
    );
    assert_ne!(
        edited_slices["scripts/game_state.gd"].fingerprint(),
        cold_slices["scripts/game_state.gd"].fingerprint(),
        "and so did its fingerprint"
    );
    for untouched in INVENTORY
        .iter()
        .filter(|path| **path != "scripts/game_state.gd")
    {
        assert_eq!(
            edited_slices[*untouched], cold_slices[*untouched],
            "{untouched} was not edited, so its slice is byte-identical"
        );
    }

    // -- 8. the incremental result is the full-scan result ------------------
    let full = warm_index
        .rescan(&inventory)
        .expect("forced full scan of the edited tree");
    assert_eq!(full.scan_kind(), ScanKind::ForcedFull);
    assert!(full.scan_kind().is_full());
    assert_eq!(
        full.graph_digest(),
        edited.graph_digest(),
        "one reparse converges on the full-scan digest"
    );
    assert_eq!(slices_of(&full), edited_slices);
    assert_eq!(
        full.graph()
            .to_canonical_json()
            .expect("render the full graph"),
        edited
            .graph()
            .to_canonical_json()
            .expect("render the incremental graph"),
        "incremental and full scans are byte-identical"
    );

    assert_eq!(
        snapshot_tree(&fixture_project()),
        committed,
        "the committed fixture tree is never mutated"
    );
}

/// The failure paths: a dropped or capacity-exceeded watch batch and a foreign or
/// corrupt store force a safe rescan instead of a stale success; an
/// under-reported batch is marked stale rather than claimed current; deletes and
/// renames are cleaned up and any event order converges on the full-scan bytes;
/// and a refused path never reaches the index.
#[test]
#[allow(clippy::too_many_lines)]
fn index_persistence_failure_paths() {
    let committed = snapshot_tree(&fixture_project());
    let fixture = fixture();
    let resolver = ProjectResolver::new(&fixture.lease_state);
    let project = resolver
        .resolve(&fixture.root, GENERATION)
        .expect("resolve the fixture project");
    let inventory = inventory();

    let mut index =
        ProjectIndex::open(&project, &fixture.index_state).expect("open the index cache");
    let baseline = index.scan(&inventory).expect("cold scan");
    let baseline_digest = baseline.graph_digest().to_owned();

    // -- 1. an unusable declared path is refused before any read ------------
    for rejected in [
        "",
        "   ",
        "/etc/passwd",
        "../escape.gd",
        "scripts/../../out.gd",
    ] {
        let error = FileInventory::from_paths([rejected])
            .expect_err("an out-of-root or empty path must be refused");
        assert_eq!(
            error.error_kind(),
            ErrorKind::InvalidInput,
            "{rejected:?} must be refused as invalid input"
        );
    }

    // A degenerate debouncer is refused rather than silently never coalescing.
    assert_eq!(
        WatchDebouncer::new(&fixture.root, 0, CAPACITY)
            .expect_err("a zero quiet window never coalesces")
            .error_kind(),
        ErrorKind::InvalidInput
    );
    assert_eq!(
        WatchDebouncer::new(&fixture.root, QUIET, 0)
            .expect_err("a zero-capacity debouncer can hold nothing")
            .error_kind(),
        ErrorKind::InvalidInput
    );
    assert_eq!(
        WatchDebouncer::new(Path::new("relative/root"), QUIET, CAPACITY)
            .expect_err("a relative root is not a usable strip base")
            .error_kind(),
        ErrorKind::InvalidInput
    );

    // -- 2. watcher overflow forces a safe rescan ---------------------------
    append(
        &fixture.root,
        "scripts/hud.gd",
        "\nfunc _on_overflow_probe() -> void:\n\tpass\n",
    );
    let overflow = coalesce(
        &fixture.root,
        &[modified(&fixture.root, "scripts/hud.gd"), overflow_event()],
    );
    assert!(
        overflow.overflowed(),
        "a rescan-flagged event sets the overflow bit"
    );
    assert!(
        overflow.changed().count() == 0
            && overflow.removed().count() == 0
            && overflow.renamed().count() == 0,
        "an overflowed batch must not pretend to know which paths changed"
    );
    assert!(!overflow.is_empty(), "an overflow is work, not silence");
    assert_eq!(overflow, WatchBatch::overflow());
    assert!(WatchSignal::Overflow.forces_rescan());
    assert_eq!(WatchSignal::Overflow.code(), "overflow");

    let recovered = index
        .apply(&overflow, &inventory)
        .expect("an overflowed batch still yields an honest snapshot");
    assert_eq!(
        recovered.scan_kind(),
        ScanKind::ForcedFull,
        "an unknowable change set escalates to a full rebuild"
    );
    assert_eq!(recovered.scan_kind().code(), "forced_full");
    assert!(
        recovered.is_current(),
        "the rebuilt snapshot is current, never stale-but-claimed-fresh"
    );
    assert_ne!(
        recovered.graph_digest(),
        baseline_digest,
        "the rebuild observed the edit the watcher lost"
    );
    let full_after_overflow = index
        .rescan(&inventory)
        .expect("a full scan of the same bytes");
    assert_eq!(
        recovered.graph_digest(),
        full_after_overflow.graph_digest(),
        "overflow recovery equals a full scan"
    );
    let settled_digest = full_after_overflow.graph_digest().to_owned();

    // A capacity overflow degrades the same way, and says so.
    let mut flooded =
        WatchDebouncer::new(&fixture.root, QUIET, CAPACITY).expect("build a debouncer");
    for (offset, relative) in INVENTORY.iter().enumerate() {
        flooded.record(
            WatchSignal::Changed {
                file: (*relative).to_owned(),
            },
            offset as u64,
        );
    }
    assert!(
        flooded.overflowed(),
        "more distinct pending signals than capacity latches an overflow"
    );
    assert_eq!(flooded.last_tick(), Some(INVENTORY.len() as u64 - 1));
    assert!(flooded.ready(INVENTORY.len() as u64 - 1 + QUIET));
    let flood = flooded.flush();
    assert!(flood.overflowed());
    assert_eq!(flood.changed().count(), 0);
    assert_eq!(
        flooded.pending(),
        0,
        "flushing resets the debouncer, overflow included"
    );
    assert!(!flooded.overflowed());

    // -- 3. a foreign schema version rebuilds; it is never migrated in place --
    let store = index.cache_dir().join(INDEX_STORE_FILE);
    let manifest = index.cache_dir().join(INDEX_MANIFEST_FILE);
    let good_manifest = fs::read(&manifest).expect("read the manifest");
    drop(index);
    fs::write(
        &manifest,
        br#"{"schema":"godot-mcp.index.v1","schema_version":99}"#,
    )
    .expect("stamp a foreign layout version");
    let mut foreign =
        ProjectIndex::open(&project, &fixture.index_state).expect("open a foreign store");
    assert_eq!(
        foreign
            .stored_schema_version()
            .expect("read the foreign version"),
        Some(99),
        "opening reports the stored version rather than assuming its own"
    );
    let verdict = foreign
        .coherence(&inventory)
        .expect("classify the foreign store");
    assert!(
        verdict.requires_rescan(),
        "a foreign layout version is never read incrementally: {verdict:?}"
    );
    assert_eq!(
        verdict.rescan_reason(),
        Some(&RescanReason::SchemaMismatch {
            stored: 99,
            expected: INDEX_SCHEMA_VERSION,
        }),
        "the reason names both versions: {verdict:?}"
    );
    assert_eq!(verdict.code(), "rescan_required");
    assert_eq!(
        verdict
            .rescan_reason()
            .expect("the rescan reason is present")
            .code(),
        "schema_mismatch"
    );
    let migrated = foreign
        .scan(&inventory)
        .expect("a foreign store is rebuilt through the held lease");
    assert_eq!(
        migrated.scan_kind(),
        ScanKind::ForcedFull,
        "migration is a rebuild, not an in-place edit"
    );
    assert!(migrated.is_current());
    assert_eq!(
        migrated.graph_digest(),
        settled_digest,
        "the rebuilt store equals the full-scan bytes"
    );
    assert_eq!(
        foreign
            .stored_schema_version()
            .expect("read the rewritten version"),
        Some(INDEX_SCHEMA_VERSION)
    );

    // A store whose bytes are not a store is corrupt, never silently empty.
    drop(foreign);
    fs::write(&store, b"this is not a sqlite database").expect("corrupt the store");
    fs::write(&manifest, &good_manifest).expect("restore the manifest");
    let mut corrupt =
        ProjectIndex::open(&project, &fixture.index_state).expect("open a corrupt store");
    let corrupt_verdict = corrupt
        .coherence(&inventory)
        .expect("classify the corrupt store");
    assert!(
        corrupt_verdict.requires_rescan(),
        "a corrupt store is never read: {corrupt_verdict:?}"
    );
    assert!(matches!(
        corrupt_verdict.rescan_reason(),
        Some(RescanReason::StoreCorrupt { .. })
    ));
    let repaired = corrupt.scan(&inventory).expect("rebuild the corrupt store");
    assert_eq!(repaired.scan_kind(), ScanKind::ForcedFull);
    assert_eq!(repaired.graph_digest(), settled_digest);

    // -- 4. an under-reported batch is stale, never claimed current ---------
    append(
        &fixture.root,
        "scripts/hud.gd",
        "\nfunc _on_second_probe() -> void:\n\tpass\n",
    );
    append(
        &fixture.root,
        "scripts/player.gd",
        "\nfunc heal(amount: int) -> void:\n\thealth_changed.emit(amount)\n",
    );
    let partial = coalesce(&fixture.root, &[modified(&fixture.root, "scripts/hud.gd")]);
    let understated = corrupt
        .apply(&partial, &inventory)
        .expect("apply the partial batch");
    assert!(
        !understated.is_current(),
        "a snapshot that does not cover every changed file is not current"
    );
    assert_eq!(
        understated.coherence().stale_reason(),
        Some(StaleReason::FileContentChanged),
        "and it says why: {:?}",
        understated.coherence()
    );
    assert_eq!(
        understated.coherence().subjects(),
        ["scripts/player.gd".to_owned()],
        "naming exactly the file the batch missed"
    );
    assert_eq!(
        understated.provenance().source(),
        ProvenanceSource::Heuristic,
        "a stale snapshot is not complete index evidence"
    );
    assert_eq!(
        understated.provenance().confidence(),
        Confidence::Inferred,
        "a stale snapshot is never labelled exact"
    );
    let reader = corrupt
        .reader()
        .expect("take a reader on the stale snapshot");
    assert!(!reader.is_current(), "a reader inherits the honest verdict");
    assert_eq!(reader.coherence(), understated.coherence());
    drop(reader);
    let caught_up = corrupt.scan(&inventory).expect("a scan catches up");
    assert!(caught_up.is_current());
    assert_eq!(
        caught_up.scan_kind(),
        ScanKind::Incremental,
        "catching up reparses only what moved"
    );

    // -- 5. deletion and rename clean up, in any event order ----------------
    fs::remove_file(fixture.root.join("scripts/dynamic.gd")).expect("delete a script");
    fs::rename(
        fixture.root.join("scripts/spare.gd"),
        fixture.root.join("scripts/renamed.gd"),
    )
    .expect("rename a script");
    append(
        &fixture.root,
        "scripts/game_state.gd",
        "\nfunc bonus() -> void:\n\thits += 2\n",
    );
    let moved: Vec<&str> = INVENTORY
        .iter()
        .copied()
        .filter(|path| *path != "scripts/dynamic.gd" && *path != "scripts/spare.gd")
        .chain(std::iter::once("scripts/renamed.gd"))
        .collect();
    let moved_inventory = inventory_of(&moved);

    let golden = corrupt
        .rescan(&moved_inventory)
        .expect("full scan after the delete and rename");
    let golden_digest = golden.graph_digest().to_owned();
    let golden_slices = slices_of(&golden);
    let golden_graph = golden
        .graph()
        .to_canonical_json()
        .expect("render the golden graph");
    assert!(
        golden.slice("scripts/dynamic.gd").is_none(),
        "a deleted file keeps no slice"
    );
    assert!(
        golden.slice("scripts/spare.gd").is_none(),
        "a renamed file's old key is retired"
    );
    let renamed_slice = golden
        .slice("scripts/renamed.gd")
        .expect("the new key is registered");
    assert_eq!(
        renamed_slice.fingerprint(),
        &FileFingerprint::of_bytes(
            &fs::read(fixture.root.join("scripts/renamed.gd")).expect("read the renamed file")
        ),
        "identical bytes fingerprint identically, so a rename needs no reparse"
    );
    assert!(renamed_slice.node_count() > 0 || renamed_slice.edge_count() > 0);

    let events = [
        removed(&fixture.root, "scripts/dynamic.gd"),
        removed(&fixture.root, "scripts/spare.gd"),
        created(&fixture.root, "scripts/renamed.gd"),
        modified(&fixture.root, "scripts/game_state.gd"),
    ];
    let signals = [
        WatchSignal::Removed {
            file: "scripts/dynamic.gd".to_owned(),
        },
        WatchSignal::Renamed {
            from: "scripts/spare.gd".to_owned(),
            to: "scripts/renamed.gd".to_owned(),
        },
        WatchSignal::Changed {
            file: "scripts/game_state.gd".to_owned(),
        },
    ];

    // Debouncing is a function of the recorded set, not of arrival order.
    let baseline_batch = coalesce_signals(&fixture.root, &signals);
    for seed in [1_u64, 7, 31, 4_099] {
        let shuffled: Vec<WatchSignal> = permutation(signals.len(), seed)
            .into_iter()
            .map(|index| signals[index].clone())
            .collect();
        assert_eq!(
            coalesce_signals(&fixture.root, &shuffled),
            baseline_batch,
            "event order {seed} must debounce to one identical batch"
        );
    }
    assert_eq!(baseline_batch.touched(), 4);
    assert_eq!(
        baseline_batch.removed().collect::<Vec<_>>(),
        ["scripts/dynamic.gd"]
    );
    assert_eq!(
        baseline_batch.renamed().collect::<Vec<_>>(),
        [("scripts/spare.gd", "scripts/renamed.gd")]
    );
    assert_eq!(
        baseline_batch.changed().collect::<Vec<_>>(),
        ["scripts/game_state.gd"]
    );

    // A raw notify burst normalizes to the same work, whatever order it arrives
    // in, and replaying it incrementally converges on the full-scan golden.
    for seed in [2_u64, 11, 97] {
        let shuffled: Vec<Event> = permutation(events.len(), seed)
            .into_iter()
            .map(|index| events[index].clone())
            .collect();
        let replayed = coalesce(&fixture.root, &shuffled);
        let replay_state = fixture.base.join(format!("replay-{seed}"));
        let mut replay =
            ProjectIndex::open(&project, &replay_state).expect("open a fresh replay cache");
        let seeded = replay
            .scan(&moved_inventory)
            .expect("seed the replay cache");
        assert_eq!(seeded.scan_kind(), ScanKind::Cold);
        let applied = replay
            .apply(&replayed, &moved_inventory)
            .expect("replay the shuffled burst");
        assert!(applied.is_current());
        assert_eq!(
            applied.graph_digest(),
            golden_digest,
            "event order {seed} must converge on the full-scan digest"
        );
        assert_eq!(slices_of(&applied), golden_slices);
        assert_eq!(
            applied
                .graph()
                .to_canonical_json()
                .expect("render the replayed graph"),
            golden_graph
        );
    }

    // -- 6. a refused path never reaches the index -------------------------
    let mut refuser = ProjectIndex::open(&project, &fixture.base.join("refuse-state"))
        .expect("open a scratch cache");
    fs::create_dir_all(fixture.root.join(".godot")).expect("create the engine cache directory");
    fs::write(fixture.root.join(".godot/uid_cache.bin"), b"cache")
        .expect("write an engine cache file");
    let engine_cache = refuser
        .rescan(&inventory_of(&["project.godot", ".godot/uid_cache.bin"]))
        .expect_err("the engine cache is not project source");
    assert_eq!(engine_cache.error_kind(), ErrorKind::PermissionDenied);

    let outside = fixture.base.join("outside.gd");
    fs::write(&outside, b"extends Node\n").expect("write a file outside the root");
    std::os::unix::fs::symlink(&outside, fixture.root.join("escape.gd"))
        .expect("create an in-root symlink");
    let symlinked = refuser
        .rescan(&inventory_of(&["project.godot", "escape.gd"]))
        .expect_err("an in-root symlink is never followed");
    assert_eq!(symlinked.error_kind(), ErrorKind::PermissionDenied);
    fs::remove_file(fixture.root.join("escape.gd")).expect("remove the symlink");

    let absent = refuser
        .rescan(&inventory_of(&[
            "project.godot",
            "scripts/never_existed.gd",
        ]))
        .expect_err("a declared file that does not exist is reported, not skipped");
    assert_eq!(absent.error_kind(), ErrorKind::NotFound);

    // -- 7. a generation bump is a new snapshot, not a new cache ------------
    let later = resolver
        .resolve(&fixture.root, GENERATION + 1)
        .expect("resolve at a later generation");
    assert!(later.same_project(&project), "it is the same project");
    let generation_state = fixture.base.join("generation-state");
    let mut first = ProjectIndex::open(&project, &generation_state).expect("seed a cache");
    let seeded_digest = first
        .scan(&moved_inventory)
        .expect("seed at the first generation")
        .graph_digest()
        .to_owned();
    drop(first);
    let mut advanced = ProjectIndex::open(&later, &generation_state)
        .expect("open the same cache at a later generation");
    assert_eq!(
        advanced.cache_key(),
        &IndexCacheKey::from_identity(project.identity()),
        "the generation is not part of the cache key"
    );
    let advanced_verdict = advanced
        .coherence(&moved_inventory)
        .expect("classify across a generation bump");
    assert_eq!(
        advanced_verdict.stale_reason(),
        Some(StaleReason::GenerationAdvanced),
        "a store written for an earlier generation is not current: {advanced_verdict:?}"
    );
    assert_eq!(
        advanced_verdict
            .stale_reason()
            .expect("a stale reason is present")
            .code(),
        "generation_advanced"
    );
    let restamped = advanced
        .scan(&moved_inventory)
        .expect("scan at the later generation");
    assert!(restamped.is_current());
    assert_eq!(restamped.project().generation(), GENERATION + 1);
    assert_eq!(restamped.graph().project_generation(), GENERATION + 1);
    assert_ne!(
        restamped.graph_digest(),
        seeded_digest,
        "the snapshot identity of the answer moves with the generation"
    );

    // -- 8. cache ownership follows identity, not the spelling --------------
    let nested = resolver
        .resolve(&fixture.root.join("scripts"), GENERATION)
        .expect("resolve through a nested path");
    assert!(nested.same_project(&project));
    assert_eq!(
        IndexCacheKey::from_identity(nested.identity()),
        IndexCacheKey::from_identity(project.identity()),
        "an alias of the same objects is the same cache"
    );

    let other_root = fixture.base.join("other-game");
    copy_tree(&fixture_project(), &other_root);
    let other = resolver
        .resolve(&other_root, GENERATION)
        .expect("resolve a second project");
    assert!(!other.same_project(&project));
    assert_ne!(
        IndexCacheKey::from_identity(other.identity()),
        IndexCacheKey::from_identity(project.identity()),
        "two projects never share one cache"
    );

    assert_eq!(
        snapshot_tree(&fixture_project()),
        committed,
        "the committed fixture tree is never mutated"
    );
}

/// A store a process left half-written — its manifest empty or cut short, as
/// when the process ends between truncating and rewriting the file — is
/// rebuilt from the project, by a scan and by a batch alike, never fatal.
#[test]
fn a_store_left_half_written_is_rebuilt() {
    let fixture = fixture();
    let resolver = ProjectResolver::new(&fixture.lease_state);
    let project = resolver
        .resolve(&fixture.root, GENERATION)
        .expect("resolve the fixture project");
    let inventory = inventory();
    let mut index =
        ProjectIndex::open(&project, &fixture.index_state).expect("open the index cache");
    let baseline = index.scan(&inventory).expect("cold scan");
    let manifest = index.cache_dir().join(INDEX_MANIFEST_FILE);
    drop(index);

    for torn in [&b""[..], b"{\"schema\":"] {
        fs::write(&manifest, torn).expect("tear the manifest");
        let mut reopened =
            ProjectIndex::open(&project, &fixture.index_state).expect("open a torn store");
        let verdict = reopened
            .coherence(&inventory)
            .expect("classify a torn store");
        assert!(verdict.requires_rescan(), "{verdict:?}");
        let rebuilt = reopened
            .scan(&inventory)
            .expect("a scan rebuilds a torn store");
        assert_eq!(rebuilt.scan_kind(), ScanKind::ForcedFull);
        assert!(rebuilt.is_current());
        assert_eq!(rebuilt.graph_digest(), baseline.graph_digest());
        drop(reopened);

        fs::write(&manifest, torn).expect("tear the manifest again");
        let mut reopened =
            ProjectIndex::open(&project, &fixture.index_state).expect("open a torn store");
        let batch = coalesce(&fixture.root, &[modified(&fixture.root, "scripts/hud.gd")]);
        let applied = reopened
            .apply(&batch, &inventory)
            .expect("a batch over a torn store rebuilds it");
        assert_eq!(applied.scan_kind(), ScanKind::ForcedFull);
        assert!(applied.is_current());
        drop(reopened);
    }
}

/// Rewriting the store replaces it whole: whatever reads the cache while a
/// rescan rewrites it finds a decodable manifest and a store file, the old ones
/// or the new ones, never a gap — so a process that ends mid-write leaves a
/// usable store behind.
#[test]
fn a_store_being_rewritten_is_always_whole() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let fixture = fixture();
    let resolver = ProjectResolver::new(&fixture.lease_state);
    let project = resolver
        .resolve(&fixture.root, GENERATION)
        .expect("resolve the fixture project");
    let inventory = inventory();
    let mut index =
        ProjectIndex::open(&project, &fixture.index_state).expect("open the index cache");
    index.scan(&inventory).expect("cold scan");
    let cache = index.cache_dir().to_path_buf();
    let stop = Arc::new(AtomicBool::new(false));
    let watching = {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut gaps = Vec::new();
            while !stop.load(Ordering::SeqCst) {
                match fs::read(cache.join(INDEX_MANIFEST_FILE)) {
                    Ok(bytes) if serde_json::from_slice::<serde_json::Value>(&bytes).is_ok() => {}
                    Ok(bytes) => {
                        gaps.push(format!("a manifest of {} undecodable bytes", bytes.len()))
                    }
                    Err(error) => gaps.push(format!("no manifest: {error}")),
                }
                if !cache.join(INDEX_STORE_FILE).is_file() {
                    gaps.push("no store file".to_owned());
                }
            }
            gaps
        })
    };
    for _ in 0..40 {
        index.rescan(&inventory).expect("rewrite the store");
    }
    stop.store(true, Ordering::SeqCst);
    let gaps = watching.join().expect("the watching thread");
    assert!(
        gaps.is_empty(),
        "{} gaps, the first: {:?}",
        gaps.len(),
        gaps.iter().take(3).collect::<Vec<_>>()
    );
}
