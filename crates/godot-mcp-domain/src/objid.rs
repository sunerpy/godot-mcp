//! Stable OS-object identity and alias evidence used to key project leases.
//!
//! Two distinct facts are modelled here and must never be conflated:
//!
//! * [`StableObjectId`] — the *ownership* identity of one opened filesystem
//!   object, derived from a held handle (never from a canonicalised path
//!   string). On Unix this is `(device, inode)`; on Windows it is
//!   `(volume serial, 128-bit file id)`. Two differently spelled paths that
//!   reach the same object — including a supported bind-mount or junction alias
//!   — produce the *same* `StableObjectId`, so they converge onto one lease. A
//!   path retargeted to a different object produces a different one.
//!
//! * [`AliasWitness`] — how the object was reached: the mount ids and followed
//!   reparse points along the traversed spelling. This is *not* part of the
//!   ownership key (that would split supported same-object aliases into two
//!   owners). It is retained so that a namespace/bind substitution or a
//!   retargeted junction is detected and rejected before every effect, even in
//!   the pathological case where the substituted path still resolves to the same
//!   object.

/// A stable identity for one opened filesystem object, bound to a held handle.
///
/// The variants are platform-tagged so a Unix identity can never compare equal
/// to a Windows identity and the lock-key encoding stays unambiguous.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StableObjectId {
    /// Unix `(st_dev, st_ino)` read from a held descriptor.
    Unix {
        /// Device number the object lives on.
        dev: u64,
        /// Inode number of the object.
        ino: u64,
    },
    /// Windows `FILE_ID_INFO` read from a held handle.
    Windows {
        /// Serial number of the volume containing the object.
        volume_serial: u64,
        /// 128-bit file id, unique within the volume.
        file_id: [u8; 16],
    },
}

impl StableObjectId {
    /// The identity as one platform-tagged record string, e.g.
    /// `unix:dev=1,ino=3` or `windows:volume=7,file_id=<32 hex digits>`.
    #[must_use]
    pub fn to_record_string(self) -> String {
        match self {
            Self::Unix { dev, ino } => format!("unix:dev={dev},ino={ino}"),
            Self::Windows {
                volume_serial,
                file_id,
            } => {
                let mut hex = String::with_capacity(32);
                for byte in file_id {
                    use std::fmt::Write as _;
                    let _ = write!(&mut hex, "{byte:02x}");
                }
                format!("windows:volume={volume_serial},file_id={hex}")
            }
        }
    }

    /// Parse [`Self::to_record_string`]'s spelling; `None` for anything else.
    #[must_use]
    pub fn parse_record_string(text: &str) -> Option<Self> {
        if let Some(fields) = text.strip_prefix("unix:") {
            let (dev, ino) = fields.split_once(',')?;
            return Some(Self::Unix {
                dev: dev.strip_prefix("dev=")?.parse().ok()?,
                ino: ino.strip_prefix("ino=")?.parse().ok()?,
            });
        }
        let fields = text.strip_prefix("windows:")?;
        let (volume, file_id) = fields.split_once(',')?;
        let hex = file_id.strip_prefix("file_id=")?;
        if hex.len() != 32 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        let mut bytes = [0_u8; 16];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).ok()?;
        }
        Some(Self::Windows {
            volume_serial: volume.strip_prefix("volume=")?.parse().ok()?,
            file_id: bytes,
        })
    }

    /// Bind every field of this identity into `sink` with explicit tags, so the
    /// encoding is unambiguous across platforms and field values.
    pub(crate) fn encode(self, mut sink: impl FnMut(&[u8])) {
        match self {
            Self::Unix { dev, ino } => {
                sink(b"unix");
                sink(b"dev");
                sink(&dev.to_le_bytes());
                sink(b"ino");
                sink(&ino.to_le_bytes());
            }
            Self::Windows {
                volume_serial,
                file_id,
            } => {
                sink(b"windows");
                sink(b"volume_serial");
                sink(&volume_serial.to_le_bytes());
                sink(b"file_id");
                sink(&file_id);
            }
        }
    }
}

/// The composite key identifying a leased project: the stable identity of the
/// project root object composed with the stable identity of its `project.godot`
/// object. Both are bound, so replacing either is a different lease.
///
/// Mount/reparse evidence is deliberately absent: a supported same-object
/// bind-mount or junction alias must converge onto exactly this key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LeaseKey {
    /// Stable identity of the project root directory object.
    pub root: StableObjectId,
    /// Stable identity of the `project.godot` file object.
    pub project_file: StableObjectId,
}

/// Mount/reparse evidence for one traversed path component.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct AliasEntry {
    /// Kernel mount id of the component, where the platform reports it
    /// (`STATX_MNT_ID` on Linux). `None` elsewhere.
    pub mount_id: Option<u64>,
    /// Reparse tag of a *followed* alias at this component (a Windows
    /// junction / mount point). `None` when the component is an ordinary
    /// directory. Symlink and unknown reparse tags are rejected during the walk
    /// and therefore never recorded here.
    pub followed_reparse_tag: Option<u32>,
    /// Stable identity of the followed alias's own reparse-point object, so a
    /// junction retargeted to a different object is detected even when the
    /// resolved target happens to be unchanged.
    pub followed_reparse_object: Option<StableObjectId>,
    /// Exact mount-point substitute-name units returned by
    /// `FSCTL_GET_REPARSE_POINT`. This detects an in-place junction retarget that
    /// preserves the reparse-point object's file id. The value is alias evidence
    /// only and is deliberately absent from [`LeaseKey`].
    pub followed_reparse_target: Option<Vec<u16>>,
}

/// Ordered mount/reparse evidence for a whole traversed spelling.
///
/// Compared for equality before every effect: an equal witness proves the
/// display path still traverses the same mounts and the same (still
/// same-target) aliases it did at acquisition. It is never part of
/// [`LeaseKey`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AliasWitness {
    entries: Vec<AliasEntry>,
}

impl AliasWitness {
    /// An empty witness (no component traversed yet).
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Record one traversed component's evidence.
    pub fn push(&mut self, entry: AliasEntry) {
        self.entries.push(entry);
    }

    /// Drop the most recently recorded component (a `..` traversal).
    pub fn pop(&mut self) {
        self.entries.pop();
    }

    /// Keep only the first `len` components, matching a truncated spelling.
    pub fn truncate(&mut self, len: usize) {
        self.entries.truncate(len);
    }

    /// Number of recorded components.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no component evidence was recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The recorded per-component evidence in traversal order.
    #[must_use]
    pub fn entries(&self) -> &[AliasEntry] {
        &self.entries
    }
}
