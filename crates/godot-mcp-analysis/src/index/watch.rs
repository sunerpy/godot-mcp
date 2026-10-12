//! Filesystem watch signals, their normalization, and an order-independent
//! debounce.
//!
//! The debouncer owns **no thread and no clock**. The caller supplies a
//! monotonic logical tick with every signal and asks whether the quiet window has
//! elapsed. That keeps the collapse deterministic and testable without sleeping,
//! and keeps this module free of any runtime dependency.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Component;
use std::path::{Path, PathBuf};

use super::error::IndexError;

/// One normalized filesystem signal about a project-relative path.
///
/// Deliberately not a re-export of a `notify` event: the index reasons about four
/// outcomes only, and a backend that dropped events must be able to say so rather
/// than report a plausible-looking subset. Every path is project-relative with
/// forward slashes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WatchSignal {
    /// The file's bytes may have changed, or the file may be new.
    Changed {
        /// Project-relative path.
        file: String,
    },
    /// The file is gone.
    Removed {
        /// Project-relative path.
        file: String,
    },
    /// The file's bytes moved from one project-relative path to another.
    Renamed {
        /// Project-relative path the bytes left.
        from: String,
        /// Project-relative path the bytes arrived at.
        to: String,
    },
    /// The backend dropped events. The change set is unknowable, so no
    /// incremental answer derived from it could be honest.
    Overflow,
}

impl WatchSignal {
    /// Normalize one `notify` event into this vocabulary.
    ///
    /// `root` is the display root the event paths are relative to; it is used for
    /// stripping only and is never opened, re-resolved or followed here. An event
    /// about a path outside the root, or inside the `.godot/` engine cache, is
    /// dropped rather than reported. A rescan-flagged event becomes
    /// [`WatchSignal::Overflow`] whatever paths it carries.
    #[must_use]
    pub fn from_notify(event: &notify::Event, root: &Path) -> Vec<Self> {
        if event.need_rescan() {
            return vec![Self::Overflow];
        }

        let paths: Vec<String> = event
            .paths
            .iter()
            .filter_map(|path| normalize_event_path(path, root))
            .collect();

        match event.kind {
            notify::EventKind::Create(notify::event::CreateKind::File)
            | notify::EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Content,
            )) => paths
                .into_iter()
                .map(|file| Self::Changed { file })
                .collect(),
            notify::EventKind::Remove(notify::event::RemoveKind::File) => paths
                .into_iter()
                .map(|file| Self::Removed { file })
                .collect(),
            notify::EventKind::Modify(notify::event::ModifyKind::Name(_)) => {
                if let [from, to] = paths.as_slice() {
                    vec![Self::Renamed {
                        from: from.clone(),
                        to: to.clone(),
                    }]
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
        }
    }

    /// Stable wire code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Changed { .. } => "changed",
            Self::Removed { .. } => "removed",
            Self::Renamed { .. } => "renamed",
            Self::Overflow => "overflow",
        }
    }

    /// Whether this signal alone forces a full rescan.
    #[must_use]
    pub const fn forces_rescan(&self) -> bool {
        matches!(self, Self::Overflow)
    }
}

/// The debounced, deduplicated result of a burst of watch signals.
///
/// Order-independent by construction: the same *set* of signals in any order
/// produces the same batch. Paths live in [`BTreeSet`]s, so no hash iteration
/// order can reach the applied result.
///
/// An overflowed batch reports **no** paths. Publishing a partial path list
/// beside "I also lost events" invites a caller to treat the known part as the
/// whole change set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WatchBatch {
    changed: BTreeSet<String>,
    removed: BTreeSet<String>,
    renamed: BTreeSet<(String, String)>,
    overflowed: bool,
}

impl WatchBatch {
    /// An explicitly overflowed batch, carrying no paths.
    #[must_use]
    pub fn overflow() -> Self {
        Self {
            overflowed: true,
            ..Self::default()
        }
    }

    /// Project-relative paths whose bytes must be re-read, in deterministic
    /// order.
    pub fn changed(&self) -> impl Iterator<Item = &str> {
        self.changed.iter().map(String::as_str)
    }

    /// Project-relative paths whose slices must be retired, in deterministic
    /// order.
    pub fn removed(&self) -> impl Iterator<Item = &str> {
        self.removed.iter().map(String::as_str)
    }

    /// `(from, to)` pairs whose slices must be re-keyed, in deterministic order.
    pub fn renamed(&self) -> impl Iterator<Item = (&str, &str)> {
        self.renamed
            .iter()
            .map(|(from, to)| (from.as_str(), to.as_str()))
    }

    /// Whether the backend dropped events, which forces a full rescan.
    #[must_use]
    pub const fn overflowed(&self) -> bool {
        self.overflowed
    }

    /// Whether the batch carries no work at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.overflowed
            && self.changed.is_empty()
            && self.removed.is_empty()
            && self.renamed.is_empty()
    }

    /// Total number of distinct project-relative paths this batch touches.
    #[must_use]
    pub fn touched(&self) -> usize {
        self.changed.len() + self.removed.len() + self.renamed.len() * 2
    }
}

/// Collapses a burst of [`WatchSignal`]s into one [`WatchBatch`].
///
/// Three properties the incremental index depends on:
///
/// * **Quiet window.** [`Self::ready`] stays `false` until `quiet_ticks` have
///   passed since the last recorded signal, so an editor's
///   write-temp-then-rename dance is one project state rather than three.
/// * **Order independence.** [`Self::flush`] is a function of the recorded set,
///   not of arrival order.
/// * **Bounded memory.** More than `capacity` distinct pending signals degrades
///   to [`WatchBatch::overflow`] rather than growing without limit — and says so,
///   instead of silently forgetting the tail.
#[derive(Debug, Clone)]
pub struct WatchDebouncer {
    root: PathBuf,
    quiet_ticks: u64,
    capacity: usize,
    last_tick: Option<u64>,
    overflowed: bool,
    pending: BTreeMap<WatchSignal, u64>,
}

impl WatchDebouncer {
    /// Build a debouncer for events under `root`.
    ///
    /// # Errors
    /// Returns [`IndexError::InvalidInput`] for a relative `root`, a zero
    /// `quiet_ticks` (a debouncer that never coalesces) or a zero `capacity` (a
    /// debouncer that can hold nothing).
    pub fn new(root: &Path, quiet_ticks: u64, capacity: usize) -> Result<Self, IndexError> {
        if !root.is_absolute() {
            return Err(IndexError::InvalidInput {
                reason: "watch root must be absolute".to_owned(),
            });
        }
        if quiet_ticks == 0 {
            return Err(IndexError::InvalidInput {
                reason: "quiet window must be greater than zero".to_owned(),
            });
        }
        if capacity == 0 {
            return Err(IndexError::InvalidInput {
                reason: "watch capacity must be greater than zero".to_owned(),
            });
        }

        Ok(Self {
            root: root.to_path_buf(),
            quiet_ticks,
            capacity,
            last_tick: None,
            overflowed: false,
            pending: BTreeMap::new(),
        })
    }

    /// The display root event paths are stripped against. Metadata only: it is
    /// never opened here.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The quiet window, in caller-supplied ticks.
    #[must_use]
    pub const fn quiet_ticks(&self) -> u64 {
        self.quiet_ticks
    }

    /// Maximum number of distinct pending signals before degrading to overflow.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Record one already-normalized signal, observed at `tick`.
    pub fn record(&mut self, signal: WatchSignal, tick: u64) {
        self.last_tick = Some(tick);
        if self.overflowed || signal.forces_rescan() {
            self.overflowed = true;
            self.pending.clear();
            return;
        }

        self.pending.insert(signal, tick);
        if self.pending.len() > self.capacity {
            self.overflowed = true;
            self.pending.clear();
        }
    }

    /// Normalize and record one `notify` event observed at `tick`.
    pub fn offer(&mut self, event: &notify::Event, tick: u64) {
        for signal in WatchSignal::from_notify(event, &self.root) {
            self.record(signal, tick);
        }
    }

    /// Record every signal in `signals` at `tick`.
    pub fn extend<I>(&mut self, signals: I, tick: u64)
    where
        I: IntoIterator<Item = WatchSignal>,
    {
        for signal in signals {
            self.record(signal, tick);
        }
    }

    /// Whether the quiet window has elapsed as of `tick`.
    ///
    /// Answers `false` when nothing is pending: there is no batch to take.
    #[must_use]
    pub fn ready(&self, tick: u64) -> bool {
        (self.overflowed || !self.pending.is_empty())
            && self
                .last_tick
                .is_some_and(|last_tick| tick.saturating_sub(last_tick) >= self.quiet_ticks)
    }

    /// Number of distinct signals recorded but not yet flushed.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Whether an overflow is already latched for the next flush.
    #[must_use]
    pub const fn overflowed(&self) -> bool {
        self.overflowed
    }

    /// Tick of the most recently recorded signal, when any.
    #[must_use]
    pub const fn last_tick(&self) -> Option<u64> {
        self.last_tick
    }

    /// Collapse everything recorded so far into one batch and reset.
    ///
    /// A create-then-rename-then-edit burst and the same events shuffled flush to
    /// the identical batch.
    pub fn flush(&mut self) -> WatchBatch {
        let batch = if self.overflowed {
            WatchBatch::overflow()
        } else {
            let mut batch = WatchBatch::default();
            for signal in self.pending.keys() {
                match signal {
                    WatchSignal::Changed { file } => {
                        batch.changed.insert(file.clone());
                    }
                    WatchSignal::Removed { file } => {
                        batch.removed.insert(file.clone());
                    }
                    WatchSignal::Renamed { from, to } => {
                        batch.renamed.insert((from.clone(), to.clone()));
                    }
                    WatchSignal::Overflow => {
                        batch = WatchBatch::overflow();
                        break;
                    }
                }
            }
            batch
        };

        self.pending.clear();
        self.last_tick = None;
        self.overflowed = false;
        batch
    }
}

fn normalize_event_path(path: &Path, root: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let mut components = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(value) => components.push(value.to_str()?),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if components.is_empty() || components.first().is_some_and(|value| *value == ".godot") {
        return None;
    }
    Some(components.join("/"))
}
