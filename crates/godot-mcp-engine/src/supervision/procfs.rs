//! Live OS process inspection.
//!
//! Reconciliation is never allowed to guess: every disposition it records must
//! come from a durable record **plus** a live observation made here. The
//! observations this module offers are deliberately narrow and exact:
//!
//! * a process's PID **and** its start time, so PID reuse can never be mistaken
//!   for the same process;
//! * its process group, which is only ever used *together* with the PID and
//!   start time — a bare PGID is never an identity or a proof;
//! * whether its argument vector still carries
//!   [`BARRIER_ARGV_MARKER`](super::boundary::BARRIER_ARGV_MARKER), which proves
//!   the process has not yet replaced its image with user code;
//! * whether it holds a descriptor onto one exact socket, and whether one exact
//!   256-bit nonce appears in its environment — together, authenticated peer
//!   credentials for a private control endpoint.
//!
//! On Linux every observation comes from `procfs`. On macOS it comes from
//! `sysctl` and `libproc`, the interfaces `ps` and `lsof` read there; a process
//! of another user is as opaque as an unreadable `/proc/<pid>` entry is on
//! Linux, so it is never proven to hold an endpoint or carry a nonce. On Windows
//! a process is observed through a limited-query handle: its creation time, its
//! image, its command line and its parent, plus which process owns a loopback
//! listener. Windows has no process groups, and another process's environment
//! and handles are not read there, so the weak and adapter boundaries' peer
//! authentication stays unavailable on it ([`observations_available`] is
//! `false`). On any other platform the observations are reported as
//! unavailable rather than approximated, so a caller can only ever quarantine,
//! never guess.

use std::io;
use std::path::PathBuf;

use super::boundary::BARRIER_ARGV_MARKER;

/// One live process, as observed from the OS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessIdentity {
    /// The observed PID.
    pub pid: u32,
    /// The process's start time, in platform-native units. `None` when the
    /// platform cannot report one, which makes every identity comparison fail
    /// closed.
    pub start_time: Option<String>,
    /// The process group. Only ever used together with `pid` + `start_time`.
    pub process_group: Option<u32>,
    /// The process's argument vector, when readable.
    pub argv: Vec<String>,
}

impl ProcessIdentity {
    /// Whether this process is still the gated barrier, i.e. provably has not
    /// replaced its image with user code yet. The barrier's own role and marker
    /// open its argument vector; a wrapper that merely carries the barrier's
    /// arguments further along (an adapter, a sandbox) is not the barrier.
    #[must_use]
    pub fn is_gated_barrier(&self) -> bool {
        self.argv.get(1).map(String::as_str) == Some("barrier")
            && self.argv.get(2).map(String::as_str) == Some(BARRIER_ARGV_MARKER)
    }

    /// Whether this observation is the exact process a durable record named.
    ///
    /// All of PID, start time and (when recorded) process group must match. A
    /// missing start time on either side is a mismatch, never a match, so PID or
    /// PGID reuse can never be trusted as the same process.
    #[must_use]
    pub fn matches_recorded(
        &self,
        pid: u32,
        start_time: Option<&str>,
        process_group: Option<u32>,
    ) -> bool {
        if self.pid != pid {
            return false;
        }
        let (Some(observed), Some(recorded)) = (self.start_time.as_deref(), start_time) else {
            return false;
        };
        if observed != recorded {
            return false;
        }
        match (self.process_group, process_group) {
            (Some(observed_group), Some(recorded_group)) => observed_group == recorded_group,
            (_, None) => true,
            (None, Some(_)) => false,
        }
    }
}

/// Observe one process. `Ok(None)` means the process is not alive.
///
/// # Errors
/// Returns an error when this platform cannot report process identity at all, so
/// the caller quarantines instead of assuming absence.
pub fn process_identity(pid: u32) -> io::Result<Option<ProcessIdentity>> {
    imp::process_identity(pid)
}

/// Every live PID on the host.
///
/// # Errors
/// Returns an error when the platform cannot enumerate processes.
pub fn live_pids() -> io::Result<Vec<u32>> {
    imp::live_pids()
}

/// Whether `pid` holds any descriptor onto the socket whose
/// [`socket_identity`] is `inode`.
///
/// # Errors
/// Returns an error when the platform cannot inspect a process's descriptors.
pub fn holds_socket_inode(pid: u32, inode: u64) -> io::Result<bool> {
    imp::holds_socket_inode(pid, inode)
}

/// The identity of the socket `socket` refers to, in the form
/// [`holds_socket_inode`] matches another process's descriptors against: its
/// inode on Linux, the kernel's opaque handle of the socket on macOS. Either
/// stays the same for as long as any process holds the socket open.
///
/// # Errors
/// Returns an error when `socket` is not a socket or the platform cannot
/// identify one.
#[cfg(unix)]
pub fn socket_identity(socket: std::os::fd::BorrowedFd<'_>) -> io::Result<u64> {
    imp::socket_identity(socket)
}

/// The identity of the TCP socket listening on `127.0.0.1:<port>`, if any. With
/// [`holds_socket_inode`] it answers which process owns a loopback listener,
/// which a bare successful connect never proves.
///
/// # Errors
/// Returns an error when the platform cannot inspect its socket table.
pub fn loopback_listener_inode(port: u16) -> io::Result<Option<u64>> {
    imp::loopback_listener_inode(port)
}

/// A path that reads as the executable `pid` is running, or `None` when the
/// process is gone. On Linux it is `/proc/<pid>/exe`, which reads as the
/// running image even after the file was replaced; on macOS it is the path
/// the kernel recorded at `exec`.
///
/// # Errors
/// Returns an error when the platform cannot name a process's executable.
pub fn executable(pid: u32) -> io::Result<Option<PathBuf>> {
    imp::executable(pid)
}

/// The process that created `pid`, or `None` when `pid` is gone. A parent
/// that has exited since is still named, and its id may have been reused, so a
/// caller compares creation times before trusting the relation.
///
/// # Errors
/// Returns an error when the process table cannot be read.
#[cfg(windows)]
pub fn parent_pid(pid: u32) -> io::Result<Option<u32>> {
    imp::parent_pid(pid)
}

/// The process that owns the TCP socket listening on `127.0.0.1:<port>`, if
/// any — Windows' own answer to which process owns a loopback listener.
///
/// # Errors
/// Returns an error when the TCP table cannot be read.
#[cfg(windows)]
pub fn loopback_listener_owner(port: u16) -> io::Result<Option<u32>> {
    imp::loopback_listener_owner(port)
}

/// Who holds the TCP listener on `127.0.0.1:<port>`, seen from the launch whose
/// gated process is `pid`, created at `start_time`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchListener {
    /// Nothing listens there.
    Absent,
    /// The launch does. `program` runs the program the launch ran: the gated
    /// process itself where the barrier execs in place (Unix), the process
    /// directly below the barrier on Windows.
    Launch {
        /// The process running the launched program.
        program: u32,
    },
    /// Some other process does.
    Foreign,
}

/// Attribute the loopback listener on `port` to the launch whose gated
/// process is `pid`: by the socket's identity on Unix, where the gated
/// process must hold it, and on Windows by the listener's owner descending
/// from the barrier ([`launched_program_in`]). A connect proves nothing, since
/// any listener accepts one.
///
/// # Errors
/// Returns an error when the platform cannot attribute a listener at all.
pub fn launch_listener(
    port: u16,
    pid: u32,
    start_time: Option<&str>,
) -> io::Result<LaunchListener> {
    #[cfg(unix)]
    {
        let _ = start_time;
        let Some(inode) = loopback_listener_inode(port)? else {
            return Ok(LaunchListener::Absent);
        };
        Ok(if holds_socket_inode(pid, inode)? {
            LaunchListener::Launch { program: pid }
        } else {
            LaunchListener::Foreign
        })
    }
    #[cfg(windows)]
    {
        let Some(owner) = loopback_listener_owner(port)? else {
            return Ok(LaunchListener::Absent);
        };
        // Without the barrier's creation time no lineage can be trusted.
        let Some(start_time) = start_time else {
            return Ok(LaunchListener::Foreign);
        };
        Ok(match launched_program(owner, pid, start_time)? {
            Some(program) => LaunchListener::Launch { program },
            None => LaunchListener::Foreign,
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (port, pid, start_time);
        imp::loopback_listener_inode(0).map(|_| LaunchListener::Absent)
    }
}

/// Whether this process's window station has visible display surfaces: the
/// interactive `WinSta0` of a logged-on session, as opposed to the hidden
/// station of a service or a container, where no window can be shown.
///
/// # Errors
/// Returns an error when the station cannot be queried.
#[cfg(windows)]
pub fn window_station_is_visible() -> io::Result<bool> {
    imp::window_station_is_visible()
}

/// The process running the program a Windows launch ran, when the loopback
/// listener `owner` belongs to that launch; see [`launched_program_in`].
///
/// # Errors
/// Returns an error when the process table cannot be read.
#[cfg(windows)]
pub fn launched_program(owner: u32, barrier: u32, barrier_start: &str) -> io::Result<Option<u32>> {
    let lookup = |pid: u32| {
        let created = process_identity(pid)
            .ok()
            .flatten()?
            .start_time?
            .parse()
            .ok()?;
        Some((parent_pid(pid).ok().flatten(), created))
    };
    Ok(launched_program_in(&lookup, owner, barrier, barrier_start))
}

/// Windows has no `exec`: a launch's barrier — the process its receipt names
/// — runs the program as its child, and Godot's console executable in turn
/// runs the windowed engine as its own child, which is the process that
/// listens. `owner` belongs to the launch when it descends from the barrier
/// within those two levels, every parent created no later than its child (a
/// recycled id names a younger process), and the barrier was created at the
/// recorded `barrier_start`. The answer is the process directly below the
/// barrier: the program it ran. `lookup` answers a live process's parent and
/// creation time.
#[cfg(any(windows, test))]
fn launched_program_in(
    lookup: &dyn Fn(u32) -> Option<(Option<u32>, u64)>,
    owner: u32,
    barrier: u32,
    barrier_start: &str,
) -> Option<u32> {
    let mut child = owner;
    let (mut parent, mut created) = lookup(owner)?;
    for _ in 0..2 {
        let parent_pid = parent?;
        let (grandparent, parent_created) = lookup(parent_pid)?;
        if parent_created > created {
            return None;
        }
        if parent_pid == barrier {
            return (parent_created.to_string() == barrier_start).then_some(child);
        }
        child = parent_pid;
        parent = grandparent;
        created = parent_created;
    }
    None
}

/// Whether `pid`'s environment contains exactly `nonce` as the value of
/// `variable`.
///
/// # Errors
/// Returns an error when the platform cannot inspect a process's environment.
pub fn environment_carries(pid: u32, variable: &str, nonce: &str) -> io::Result<bool> {
    imp::environment_carries(pid, variable, nonce)
}

/// Whether this platform can make the live observations reconciliation needs.
#[must_use]
pub fn observations_available() -> bool {
    imp::AVAILABLE
}

/// `SIGKILL` one process. A failure because the process is already gone is not
/// an error.
///
/// # Errors
/// Returns an error when the signal could not be delivered for any other reason.
pub fn kill_pid(pid: u32) -> io::Result<()> {
    imp::kill_pid(pid)
}

/// `SIGKILL` an entire process group.
///
/// # Errors
/// Returns an error when the signal could not be delivered.
pub fn kill_process_group(pgid: u32) -> io::Result<()> {
    imp::kill_process_group(pgid)
}

/// A process's exec-time argument area, as macOS's `KERN_PROCARGS2` returns it.
#[cfg(any(target_os = "macos", test))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct ArgumentArea {
    argv: Vec<String>,
    environment: Vec<Vec<u8>>,
}

#[cfg(any(target_os = "macos", test))]
impl ArgumentArea {
    /// Whether the environment holds exactly `variable=value`.
    fn carries(&self, variable: &str, value: &str) -> bool {
        let needle = format!("{variable}={value}");
        self.environment
            .iter()
            .any(|entry| entry.as_slice() == needle.as_bytes())
    }
}

/// Parse a `KERN_PROCARGS2` area: the argument count as a native `int`, the
/// executable path, the NULs that align what follows, the arguments, the
/// environment up to an empty string, then the kernel's own strings, which are
/// not part of the environment. `None` when the area is shorter than its own
/// argument count says, so a truncated read never passes for a process.
#[cfg(any(target_os = "macos", test))]
fn parse_argument_area(area: &[u8]) -> Option<ArgumentArea> {
    let count = usize::try_from(i32::from_ne_bytes(area.get(..4)?.try_into().ok()?)).ok()?;
    let strings = &area[4..];
    let mut cursor = strings.iter().position(|byte| *byte == 0)?;
    while strings.get(cursor) == Some(&0) {
        cursor += 1;
    }
    // Only a NUL-terminated piece is a string: whatever follows the last NUL
    // was cut short.
    let mut terminated = strings[cursor..]
        .split_inclusive(|byte| *byte == 0)
        .filter_map(|piece| piece.strip_suffix(&[0]));
    let argv: Vec<String> = terminated
        .by_ref()
        .take(count)
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect();
    if argv.len() != count {
        return None;
    }
    let environment = terminated
        .take_while(|part| !part.is_empty())
        .map(<[u8]>::to_vec)
        .collect();
    Some(ArgumentArea { argv, environment })
}

/// The signals both observable platforms deliver through `rustix`.
/// A Windows command line split into the `argv` the C runtime gave that
/// process: the program name ends at the first unquoted space or tab and takes
/// no escapes; in the arguments, `2n` backslashes before a quote are `n`
/// backslashes and the quote delimits, `2n + 1` are `n` and a literal quote, a
/// doubled quote inside quotes is one literal quote, and every other backslash
/// is literal.
#[cfg(any(windows, test))]
fn split_command_line(line: &str) -> Vec<String> {
    let mut characters = line.chars().peekable();
    let mut argv = Vec::new();
    if characters.peek().is_none() {
        return argv;
    }
    let mut current = String::new();
    let mut quoted = false;
    for character in characters.by_ref() {
        match character {
            '"' => quoted = !quoted,
            ' ' | '\t' if !quoted => break,
            other => current.push(other),
        }
    }
    argv.push(std::mem::take(&mut current));
    while characters
        .next_if(|next| matches!(next, ' ' | '\t'))
        .is_some()
    {}
    let mut quoted = false;
    while let Some(character) = characters.next() {
        match character {
            ' ' | '\t' if !quoted => {
                argv.push(std::mem::take(&mut current));
                while characters
                    .next_if(|next| matches!(next, ' ' | '\t'))
                    .is_some()
                {}
            }
            '\\' => {
                let mut backslashes = 1;
                while characters.next_if_eq(&'\\').is_some() {
                    backslashes += 1;
                }
                if characters.peek() == Some(&'"') {
                    current.extend(std::iter::repeat_n('\\', backslashes / 2));
                    if backslashes % 2 == 1 {
                        characters.next();
                        current.push('"');
                    }
                } else {
                    current.extend(std::iter::repeat_n('\\', backslashes));
                }
            }
            '"' if quoted => match characters.peek() {
                Some('"') => {
                    current.push('"');
                    characters.next();
                }
                Some(_) => quoted = false,
                // The line ends inside quotes: the argument is kept, even empty.
                None => break,
            },
            '"' => quoted = true,
            other => current.push(other),
        }
    }
    if !current.is_empty() || quoted {
        argv.push(current);
    }
    argv
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod signals {
    use std::io;

    pub(super) fn kill_pid(pid: u32) -> io::Result<()> {
        let Some(pid) = rustix::process::Pid::from_raw(pid.cast_signed()) else {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        };
        match rustix::process::kill_process(pid, rustix::process::Signal::KILL) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
            Err(error) => Err(io::Error::from(error)),
        }
    }

    pub(super) fn kill_process_group(pgid: u32) -> io::Result<()> {
        let Some(pgid) = rustix::process::Pid::from_raw(pgid.cast_signed()) else {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        };
        match rustix::process::kill_process_group(pgid, rustix::process::Signal::KILL) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
            Err(error) => Err(io::Error::from(error)),
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::io;
    use std::path::PathBuf;

    use super::ProcessIdentity;

    pub(super) use super::signals::{kill_pid, kill_process_group};

    pub(super) const AVAILABLE: bool = true;

    pub(super) fn process_identity(pid: u32) -> io::Result<Option<ProcessIdentity>> {
        let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        // `comm` is parenthesised and may itself contain spaces and parens, so the
        // numeric fields are parsed from after the *last* `)`.
        let tail = stat
            .rfind(')')
            .map(|index| &stat[index + 1..])
            .unwrap_or_default();
        let fields: Vec<&str> = tail.split_whitespace().collect();
        // `tail` starts at field 3 (`state`), so field N is at index N - 3.
        // A zombie has already exited: it executes nothing and can never execute
        // again, so it is reported as gone. `kill(pid, 0)` cannot make that
        // distinction, which is why containment emptiness is proven here rather
        // than from a signal probe.
        if fields.first() == Some(&"Z") {
            return Ok(None);
        }
        let process_group = fields.get(2).and_then(|value| value.parse::<u32>().ok());
        let start_time = fields.get(19).map(|value| (*value).to_owned());
        let argv = std::fs::read(format!("/proc/{pid}/cmdline"))
            .map(|bytes| {
                bytes
                    .split(|byte| *byte == 0)
                    .filter(|part| !part.is_empty())
                    .map(|part| String::from_utf8_lossy(part).into_owned())
                    .collect()
            })
            .unwrap_or_default();
        Ok(Some(ProcessIdentity {
            pid,
            start_time,
            process_group,
            argv,
        }))
    }

    pub(super) fn live_pids() -> io::Result<Vec<u32>> {
        let mut pids = Vec::new();
        for entry in std::fs::read_dir("/proc")? {
            let entry = entry?;
            if let Some(name) = entry.file_name().to_str()
                && let Ok(pid) = name.parse::<u32>()
            {
                pids.push(pid);
            }
        }
        pids.sort_unstable();
        Ok(pids)
    }

    pub(super) fn loopback_listener_inode(port: u16) -> io::Result<Option<u64>> {
        // `/proc/net/tcp`: "sl local_address rem_address st ... inode", with the
        // local address as little-endian hex `0100007F:<port>` and `0A` LISTEN.
        let table = std::fs::read_to_string("/proc/net/tcp")?;
        let wanted = format!("0100007F:{port:04X}");
        for line in table.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() > 9 && fields[1] == wanted && fields[3] == "0A" {
                return Ok(fields[9].parse::<u64>().ok().filter(|inode| *inode != 0));
            }
        }
        Ok(None)
    }

    pub(super) fn holds_socket_inode(pid: u32, inode: u64) -> io::Result<bool> {
        let expected = format!("socket:[{inode}]");
        let entries = match std::fs::read_dir(format!("/proc/{pid}/fd")) {
            Ok(entries) => entries,
            // A process we cannot inspect is not proven to hold the endpoint.
            Err(_) => return Ok(false),
        };
        for entry in entries.flatten() {
            if let Ok(target) = std::fs::read_link(entry.path())
                && target.to_string_lossy() == expected
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(super) fn environment_carries(pid: u32, variable: &str, nonce: &str) -> io::Result<bool> {
        let Ok(bytes) = std::fs::read(format!("/proc/{pid}/environ")) else {
            return Ok(false);
        };
        let needle = format!("{variable}={nonce}");
        Ok(bytes
            .split(|byte| *byte == 0)
            .any(|entry| entry == needle.as_bytes()))
    }

    pub(super) fn socket_identity(socket: std::os::fd::BorrowedFd<'_>) -> io::Result<u64> {
        Ok(rustix::fs::fstat(socket).map_err(io::Error::from)?.st_ino)
    }

    pub(super) fn executable(pid: u32) -> io::Result<Option<PathBuf>> {
        let link = PathBuf::from(format!("/proc/{pid}/exe"));
        match std::fs::symlink_metadata(&link) {
            Ok(_) => Ok(Some(link)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }
}

#[cfg(target_os = "macos")]
#[allow(
    unsafe_code,
    reason = "sysctl and libproc have no safe binding; every call below hands the kernel a buffer \
              of exactly the size it names and checks the returned length before reading it"
)]
mod imp {
    //! Every structure here is the macOS SDK's own layout. The sizes and
    //! offsets asserted at compile time below were measured from
    //! `<sys/sysctl.h>` and `<sys/proc_info.h>` for both arm64 and x86_64,
    //! which agree, so a layout this file got wrong does not compile.

    use std::ffi::{c_int, c_uint, c_void};
    use std::io;
    use std::mem::{MaybeUninit, offset_of, size_of};
    use std::os::fd::{AsRawFd as _, BorrowedFd};
    use std::os::unix::ffi::OsStringExt as _;
    use std::path::PathBuf;
    use std::ptr;

    use super::{ArgumentArea, ProcessIdentity, parse_argument_area};

    pub(super) use super::signals::{kill_pid, kill_process_group};

    pub(super) const AVAILABLE: bool = true;

    const ESRCH: i32 = 3;
    const CTL_KERN: c_int = 1;
    const KERN_ARGMAX: c_int = 8;
    const KERN_PROC: c_int = 14;
    const KERN_PROC_PID: c_int = 1;
    const KERN_PROCARGS2: c_int = 49;
    const SZOMB: u8 = 5;
    const PROC_PIDLISTFDS: c_int = 1;
    const PROC_PIDFDSOCKETINFO: c_int = 3;
    const PROX_FDTYPE_SOCKET: u32 = 2;
    const PROC_PIDPATHINFO_MAXSIZE: u32 = 4096;
    const SOCKINFO_TCP: c_int = 2;
    const TSI_S_LISTEN: c_int = 1;
    const INI_IPV4: u8 = 0x1;

    unsafe extern "C" {
        fn sysctl(
            name: *mut c_int,
            namelen: c_uint,
            oldp: *mut c_void,
            oldlenp: *mut usize,
            newp: *mut c_void,
            newlen: usize,
        ) -> c_int;
        fn proc_listallpids(buffer: *mut c_void, buffersize: c_int) -> c_int;
        fn proc_pidinfo(
            pid: c_int,
            flavor: c_int,
            arg: u64,
            buffer: *mut c_void,
            buffersize: c_int,
        ) -> c_int;
        fn proc_pidfdinfo(
            pid: c_int,
            fd: c_int,
            flavor: c_int,
            buffer: *mut c_void,
            buffersize: c_int,
        ) -> c_int;
        fn proc_pidpath(pid: c_int, buffer: *mut c_void, buffersize: u32) -> c_int;
    }

    /// `struct kinfo_proc`, read by offset: the start time
    /// (`kp_proc.p_starttime`), the state (`kp_proc.p_stat`), the PID
    /// (`kp_proc.p_pid`) and the process group (`kp_eproc.e_pgid`). Unlike
    /// `libproc`'s BSD info, `sysctl` describes every user's process.
    #[repr(C, align(8))]
    struct KinfoProc([u8; KINFO_PROC_SIZE]);

    const KINFO_PROC_SIZE: usize = 648;
    const START_SECONDS: usize = 0;
    const START_MICROSECONDS: usize = 8;
    const STATE: usize = 36;
    const PID: usize = 40;
    const PROCESS_GROUP: usize = 564;

    impl KinfoProc {
        fn field<const N: usize>(&self, offset: usize) -> [u8; N] {
            let mut bytes = [0; N];
            bytes.copy_from_slice(&self.0[offset..offset + N]);
            bytes
        }
    }

    /// `struct proc_fdinfo`.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct ProcFdInfo {
        proc_fd: i32,
        proc_fdtype: u32,
    }

    /// `struct in_sockinfo`. Each address is a 16-byte union whose IPv4 form
    /// keeps the address in its last four bytes.
    #[repr(C)]
    #[derive(Clone, Copy)]
    #[allow(
        dead_code,
        reason = "the SDK layout, field for field; only some are read"
    )]
    struct InSockInfo {
        insi_fport: c_int,
        insi_lport: c_int,
        insi_gencnt: u64,
        insi_flags: u32,
        insi_flow: u32,
        insi_vflag: u8,
        insi_ip_ttl: u8,
        rfu_1: u32,
        insi_faddr: [u8; 16],
        insi_laddr: [u8; 16],
        insi_v4: u8,
        insi_v6: [u32; 3],
    }

    /// `struct tcp_sockinfo`.
    #[repr(C)]
    #[derive(Clone, Copy)]
    #[allow(
        dead_code,
        reason = "the SDK layout, field for field; only some are read"
    )]
    struct TcpSockInfo {
        tcpsi_ini: InSockInfo,
        tcpsi_state: c_int,
        tcpsi_timer: [c_int; 4],
        tcpsi_mss: c_int,
        tcpsi_flags: u32,
        rfu_1: u32,
        tcpsi_tp: u64,
    }

    /// `socket_info.soi_proto`, as large as its largest member
    /// (`struct un_sockinfo`).
    #[repr(C)]
    #[derive(Clone, Copy)]
    #[allow(dead_code, reason = "the SDK layout; only the TCP member is read")]
    union SocketProto {
        pri_tcp: TcpSockInfo,
        extent: [u64; 66],
    }

    /// `struct socket_info`, after its `struct vinfo_stat` (which the kernel
    /// leaves zeroed for a socket) and up to the protocol union.
    #[repr(C)]
    #[derive(Clone, Copy)]
    #[allow(
        dead_code,
        reason = "the SDK layout, field for field; only some are read"
    )]
    struct SocketInfo {
        soi_stat: [u64; 17],
        /// The kernel's opaque handle of the socket: one value for every process
        /// holding a descriptor onto it.
        soi_so: u64,
        soi_pcb: u64,
        soi_type: c_int,
        soi_protocol: c_int,
        soi_family: c_int,
        soi_options: i16,
        soi_linger: i16,
        soi_state: i16,
        soi_qlen: i16,
        soi_incqlen: i16,
        soi_qlimit: i16,
        soi_timeo: i16,
        soi_error: u16,
        soi_oobmark: u32,
        soi_rcv: [u32; 6],
        soi_snd: [u32; 6],
        soi_kind: c_int,
        rfu_1: u32,
        soi_proto: SocketProto,
    }

    /// `struct socket_fdinfo`: a `struct proc_fileinfo`, then the socket.
    #[repr(C)]
    #[derive(Clone, Copy)]
    #[allow(dead_code, reason = "the SDK layout; the file part is never read")]
    struct SocketFdInfo {
        pfi: [u64; 3],
        psi: SocketInfo,
    }

    const _: () = {
        assert!(size_of::<KinfoProc>() == KINFO_PROC_SIZE);
        assert!(size_of::<ProcFdInfo>() == 8);
        assert!(size_of::<InSockInfo>() == 80);
        assert!(offset_of!(InSockInfo, insi_lport) == 4);
        assert!(offset_of!(InSockInfo, insi_vflag) == 24);
        assert!(offset_of!(InSockInfo, insi_laddr) == 48);
        assert!(size_of::<TcpSockInfo>() == 120);
        assert!(offset_of!(TcpSockInfo, tcpsi_state) == 80);
        assert!(size_of::<SocketProto>() == 528);
        assert!(offset_of!(SocketInfo, soi_so) == 136);
        assert!(offset_of!(SocketInfo, soi_rcv) == 184);
        assert!(offset_of!(SocketInfo, soi_kind) == 232);
        assert!(offset_of!(SocketInfo, soi_proto) == 240);
        assert!(offset_of!(SocketFdInfo, psi) == 24);
        assert!(size_of::<SocketFdInfo>() == 792);
    };

    /// `PROC_PIDFDSOCKETINFO_SIZE`.
    const SOCKET_FDINFO_SIZE: c_int = 792;

    fn raw(pid: u32) -> Option<c_int> {
        c_int::try_from(pid).ok().filter(|pid| *pid > 0)
    }

    pub(super) fn process_identity(pid: u32) -> io::Result<Option<ProcessIdentity>> {
        let Some(raw_pid) = raw(pid) else {
            return Ok(None);
        };
        let mut name = [CTL_KERN, KERN_PROC, KERN_PROC_PID, raw_pid];
        let mut info = KinfoProc([0; KINFO_PROC_SIZE]);
        let mut length = KINFO_PROC_SIZE;
        // SAFETY: `name` is a four-element MIB and `info` is `length` writable
        // bytes; nothing is written to the kernel.
        let status = unsafe {
            sysctl(
                name.as_mut_ptr(),
                4,
                info.0.as_mut_ptr().cast(),
                &mut length,
                ptr::null_mut(),
                0,
            )
        };
        if status != 0 {
            return Err(io::Error::last_os_error());
        }
        // No such process: the kernel answers with nothing at all.
        if length == 0 {
            return Ok(None);
        }
        if length != KINFO_PROC_SIZE || i32::from_ne_bytes(info.field(PID)) != raw_pid {
            return Err(io::Error::other(format!(
                "the kernel described process {pid} in a shape this build does not read"
            )));
        }
        // A zombie has already exited: it executes nothing and can never execute
        // again, so it is reported as gone.
        if info.0[STATE] == SZOMB {
            return Ok(None);
        }
        let seconds = i64::from_ne_bytes(info.field(START_SECONDS));
        let microseconds = i32::from_ne_bytes(info.field(START_MICROSECONDS));
        Ok(Some(ProcessIdentity {
            pid,
            start_time: Some(format!("{seconds}.{microseconds:06}")),
            process_group: u32::try_from(i32::from_ne_bytes(info.field(PROCESS_GROUP))).ok(),
            argv: arguments(raw_pid).map(|area| area.argv).unwrap_or_default(),
        }))
    }

    /// `KERN_ARGMAX`: the size of the largest argument area, which is the buffer
    /// `KERN_PROCARGS2` needs. It is fixed for the boot, so it is asked once.
    fn argument_capacity() -> Option<usize> {
        static CAPACITY: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
        *CAPACITY.get_or_init(argument_max)
    }

    fn argument_max() -> Option<usize> {
        let mut name = [CTL_KERN, KERN_ARGMAX];
        let mut value: c_int = 0;
        let mut length = size_of::<c_int>();
        // SAFETY: `name` is a two-element MIB and `value` is an `int`, `length`
        // bytes long.
        let status = unsafe {
            sysctl(
                name.as_mut_ptr(),
                2,
                (&raw mut value).cast(),
                &mut length,
                ptr::null_mut(),
                0,
            )
        };
        if status == 0 {
            usize::try_from(value).ok()
        } else {
            None
        }
    }

    /// The exec-time argument area of `pid`, or `None` when it is not readable:
    /// another user's process, or one that is exiting.
    fn arguments(pid: c_int) -> Option<ArgumentArea> {
        let capacity = argument_capacity()?;
        let mut area = vec![0_u8; capacity];
        let mut length = capacity;
        let mut name = [CTL_KERN, KERN_PROCARGS2, pid];
        // SAFETY: `name` is a three-element MIB and `area` is `length` writable
        // bytes.
        let status = unsafe {
            sysctl(
                name.as_mut_ptr(),
                3,
                area.as_mut_ptr().cast(),
                &mut length,
                ptr::null_mut(),
                0,
            )
        };
        if status != 0 {
            return None;
        }
        area.truncate(length);
        parse_argument_area(&area)
    }

    pub(super) fn live_pids() -> io::Result<Vec<u32>> {
        // SAFETY: a null buffer only asks how many processes there are.
        let estimate = unsafe { proc_listallpids(ptr::null_mut(), 0) };
        // At least this process is alive, so no count at all is a failure, and
        // never an empty host.
        let mut capacity = usize::try_from(estimate)
            .ok()
            .filter(|count| *count > 0)
            .ok_or_else(io::Error::last_os_error)?;
        loop {
            // Room for the processes started since the last count.
            capacity += 64;
            let mut pids: Vec<c_int> = vec![0; capacity];
            let bytes = c_int::try_from(capacity * size_of::<c_int>()).map_err(io::Error::other)?;
            // SAFETY: `pids` is `bytes` writable bytes of `int`s.
            let count = unsafe { proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
            let count = usize::try_from(count)
                .ok()
                .filter(|count| *count > 0)
                .ok_or_else(io::Error::last_os_error)?;
            // A full buffer may have been cut short: count again, larger.
            if count >= capacity {
                capacity = count * 2;
                continue;
            }
            let mut live: Vec<u32> = pids[..count]
                .iter()
                .filter_map(|pid| u32::try_from(*pid).ok())
                .filter(|pid| *pid > 0)
                .collect();
            live.sort_unstable();
            live.dedup();
            return Ok(live);
        }
    }

    /// The descriptors `pid` holds, or `None` when it is not inspectable:
    /// another user's process, or one that has exited.
    fn descriptors(pid: c_int) -> Option<Vec<ProcFdInfo>> {
        // SAFETY: a null buffer only asks for the size the list needs.
        let needed = unsafe { proc_pidinfo(pid, PROC_PIDLISTFDS, 0, ptr::null_mut(), 0) };
        let mut capacity = usize::try_from(needed).ok()? / size_of::<ProcFdInfo>();
        loop {
            // Room for the descriptors opened since the last count.
            capacity += 32;
            let mut list = vec![
                ProcFdInfo {
                    proc_fd: -1,
                    proc_fdtype: 0,
                };
                capacity
            ];
            let bytes = c_int::try_from(capacity * size_of::<ProcFdInfo>()).ok()?;
            // SAFETY: `list` is `bytes` writable bytes laid out as `struct
            // proc_fdinfo`.
            let written =
                unsafe { proc_pidinfo(pid, PROC_PIDLISTFDS, 0, list.as_mut_ptr().cast(), bytes) };
            let count = usize::try_from(written).ok()? / size_of::<ProcFdInfo>();
            if count == 0 {
                return None;
            }
            // A full buffer may have been cut short: list again, larger.
            if count >= capacity {
                capacity = count * 2;
                continue;
            }
            list.truncate(count);
            return Some(list);
        }
    }

    fn socket_info(pid: c_int, fd: c_int) -> Option<SocketFdInfo> {
        let mut info = MaybeUninit::<SocketFdInfo>::zeroed();
        // SAFETY: `info` is `SOCKET_FDINFO_SIZE` writable bytes laid out as the
        // kernel's `struct socket_fdinfo`.
        let written = unsafe {
            proc_pidfdinfo(
                pid,
                fd,
                PROC_PIDFDSOCKETINFO,
                info.as_mut_ptr().cast(),
                SOCKET_FDINFO_SIZE,
            )
        };
        if written != SOCKET_FDINFO_SIZE {
            return None;
        }
        // SAFETY: every field is a plain integer, so the zeroed bytes the kernel
        // has overwritten are a valid value whatever it wrote.
        Some(unsafe { info.assume_init() })
    }

    fn sockets(pid: c_int) -> impl Iterator<Item = SocketFdInfo> {
        descriptors(pid)
            .unwrap_or_default()
            .into_iter()
            .filter(|descriptor| descriptor.proc_fdtype == PROX_FDTYPE_SOCKET)
            .filter_map(move |descriptor| socket_info(pid, descriptor.proc_fd))
    }

    pub(super) fn holds_socket_inode(pid: u32, inode: u64) -> io::Result<bool> {
        // A process this user cannot inspect is not proven to hold the endpoint.
        Ok(raw(pid).is_some_and(|pid| sockets(pid).any(|socket| socket.psi.soi_so == inode)))
    }

    pub(super) fn socket_identity(socket: BorrowedFd<'_>) -> io::Result<u64> {
        let own = c_int::try_from(std::process::id()).map_err(io::Error::other)?;
        match socket_info(own, socket.as_raw_fd()) {
            Some(info) if info.psi.soi_so != 0 => Ok(info.psi.soi_so),
            _ => Err(io::Error::other(
                "the kernel did not describe the descriptor as a socket",
            )),
        }
    }

    pub(super) fn loopback_listener_inode(port: u16) -> io::Result<Option<u64>> {
        for pid in live_pids()? {
            let Some(pid) = raw(pid) else {
                continue;
            };
            for socket in sockets(pid) {
                if socket.psi.soi_kind != SOCKINFO_TCP {
                    continue;
                }
                // SAFETY: `soi_kind` says the kernel filled the TCP member.
                let tcp = unsafe { socket.psi.soi_proto.pri_tcp };
                let local = tcp.tcpsi_ini;
                // The local port is the network-order `u_short` the kernel keeps.
                let listening_port = u16::try_from(local.insi_lport).ok().map(u16::from_be);
                if tcp.tcpsi_state == TSI_S_LISTEN
                    && local.insi_vflag & INI_IPV4 != 0
                    && local.insi_laddr[12..] == [127, 0, 0, 1]
                    && listening_port == Some(port)
                {
                    return Ok(Some(socket.psi.soi_so));
                }
            }
        }
        Ok(None)
    }

    pub(super) fn environment_carries(pid: u32, variable: &str, nonce: &str) -> io::Result<bool> {
        // An environment this user cannot read is not proven to carry the nonce.
        Ok(raw(pid)
            .and_then(arguments)
            .is_some_and(|area| area.carries(variable, nonce)))
    }

    pub(super) fn executable(pid: u32) -> io::Result<Option<PathBuf>> {
        let Some(raw_pid) = raw(pid) else {
            return Ok(None);
        };
        let mut path = vec![0_u8; PROC_PIDPATHINFO_MAXSIZE as usize];
        // SAFETY: `path` is `PROC_PIDPATHINFO_MAXSIZE` writable bytes.
        let length =
            unsafe { proc_pidpath(raw_pid, path.as_mut_ptr().cast(), PROC_PIDPATHINFO_MAXSIZE) };
        if length <= 0 {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(ESRCH) {
                Ok(None)
            } else {
                Err(error)
            };
        }
        path.truncate(usize::try_from(length).map_err(io::Error::other)?);
        Ok(Some(PathBuf::from(std::ffi::OsString::from_vec(path))))
    }
}

#[cfg(windows)]
#[allow(
    unsafe_code,
    reason = "the process, ToolHelp, IP helper and window-station APIs have no safe binding; \
              every handle opened below is owned and closed by `Handle`, and every buffer handed \
              to Windows is exactly the size the call is told"
)]
mod imp {
    use std::ffi::OsString;
    use std::io;
    use std::mem::size_of;
    use std::net::Ipv4Addr;
    use std::os::windows::ffi::OsStringExt as _;
    use std::path::PathBuf;
    use std::ptr;

    use windows_sys::Wdk::System::Threading::{
        NtQueryInformationProcess, ProcessCommandLineInformation,
    };
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_PARAMETER, FILETIME, HANDLE,
        INVALID_HANDLE_VALUE, NO_ERROR, STATUS_INFO_LENGTH_MISMATCH, UNICODE_STRING, WAIT_OBJECT_0,
        WAIT_TIMEOUT,
    };
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, MIB_TCPROW_OWNER_PID, MIB_TCPTABLE_OWNER_PID,
        TCP_TABLE_OWNER_PID_LISTENER,
    };
    use windows_sys::Win32::Networking::WinSock::AF_INET;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::StationsAndDesktops::{
        GetProcessWindowStation, GetUserObjectInformationW, UOI_FLAGS, USEROBJECTFLAGS,
    };
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, QueryFullProcessImageNameW, TerminateProcess,
        WaitForSingleObject,
    };

    use super::{ProcessIdentity, split_command_line};

    pub(super) const AVAILABLE: bool = false;

    /// One owned kernel handle, closed exactly once.
    struct Handle(HANDLE);

    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: the handle was returned open by Windows and is owned here.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    /// `pid` opened for `access`; `Ok(None)` when no process has that id. A
    /// process this account may not open is an error, never "gone".
    fn open(pid: u32, access: u32) -> io::Result<Option<Handle>> {
        // SAFETY: value arguments only; the handle returned is owned below.
        let handle = unsafe { OpenProcess(access, 0, pid) };
        if handle.is_null() {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER.cast_signed()) {
                Ok(None)
            } else {
                Err(error)
            };
        }
        Ok(Some(Handle(handle)))
    }

    /// Whether the process behind `handle` has exited. Its object outlives it
    /// for as long as anyone holds a handle, so this asks the object.
    fn exited(handle: &Handle) -> io::Result<bool> {
        // SAFETY: a live handle opened with `SYNCHRONIZE`; a zero timeout polls.
        match unsafe { WaitForSingleObject(handle.0, 0) } {
            WAIT_TIMEOUT => Ok(false),
            WAIT_OBJECT_0 => Ok(true),
            _ => Err(io::Error::last_os_error()),
        }
    }

    pub(super) fn process_identity(pid: u32) -> io::Result<Option<ProcessIdentity>> {
        let Some(process) = open(pid, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE)?
        else {
            return Ok(None);
        };
        if exited(&process)? {
            return Ok(None);
        }
        let mut times = [FILETIME::default(); 4];
        let [creation, exit, kernel, user] = &mut times;
        // SAFETY: a live handle and four writable `FILETIME`s.
        if unsafe { GetProcessTimes(process.0, creation, exit, kernel, user) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let created =
            (u64::from(times[0].dwHighDateTime) << 32) | u64::from(times[0].dwLowDateTime);
        Ok(Some(ProcessIdentity {
            pid,
            start_time: Some(created.to_string()),
            process_group: None,
            argv: command_line(&process)
                .map(|line| split_command_line(&line))
                .unwrap_or_default(),
        }))
    }

    /// The command line `process` was created with.
    fn command_line(process: &Handle) -> io::Result<String> {
        let mut needed = 0_u32;
        // `u64` storage keeps the leading `UNICODE_STRING` aligned.
        let mut storage = vec![0_u64; 256];
        loop {
            let length =
                u32::try_from(storage.len() * size_of::<u64>()).map_err(io::Error::other)?;
            // SAFETY: `storage` is writable for exactly `length` bytes.
            let status = unsafe {
                NtQueryInformationProcess(
                    process.0,
                    ProcessCommandLineInformation,
                    storage.as_mut_ptr().cast(),
                    length,
                    &raw mut needed,
                )
            };
            if status == STATUS_INFO_LENGTH_MISMATCH && needed > length {
                storage = vec![0_u64; (needed as usize).div_ceil(size_of::<u64>())];
                continue;
            }
            if status < 0 {
                return Err(io::Error::other(format!(
                    "NtQueryInformationProcess(command line) failed: {status:#x}"
                )));
            }
            break;
        }
        // SAFETY: on success the buffer begins with the `UNICODE_STRING`.
        let header = unsafe { ptr::read(storage.as_ptr().cast::<UNICODE_STRING>()) };
        let units = usize::from(header.Length) / 2;
        if units == 0 || header.Buffer.is_null() {
            return Ok(String::new());
        }
        let base = storage.as_ptr() as usize;
        let start = header.Buffer as usize;
        let end = start
            .checked_add(units * 2)
            .ok_or_else(|| io::Error::other("overflow"))?;
        if start < base + size_of::<UNICODE_STRING>() || end > base + storage.len() * 8 {
            return Err(io::Error::other(
                "the command line does not lie inside the buffer it was copied to",
            ));
        }
        // SAFETY: the range was just proven to lie inside `storage`.
        let text = unsafe { std::slice::from_raw_parts(header.Buffer, units) };
        Ok(String::from_utf16_lossy(text))
    }

    /// Every entry of one process-table snapshot.
    fn process_table() -> io::Result<Vec<PROCESSENTRY32W>> {
        // SAFETY: value arguments only; the snapshot handle is owned below.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let snapshot = Handle(snapshot);
        let mut entries = Vec::new();
        let mut entry = PROCESSENTRY32W {
            dwSize: u32::try_from(size_of::<PROCESSENTRY32W>()).map_err(io::Error::other)?,
            ..PROCESSENTRY32W::default()
        };
        // SAFETY: a live snapshot handle and an entry whose `dwSize` is set.
        let mut more = unsafe { Process32FirstW(snapshot.0, &raw mut entry) } != 0;
        while more {
            entries.push(entry);
            // SAFETY: as above.
            more = unsafe { Process32NextW(snapshot.0, &raw mut entry) } != 0;
        }
        Ok(entries)
    }

    pub(super) fn live_pids() -> io::Result<Vec<u32>> {
        let mut pids: Vec<u32> = process_table()?
            .iter()
            .map(|entry| entry.th32ProcessID)
            .collect();
        pids.sort_unstable();
        Ok(pids)
    }

    pub(super) fn parent_pid(pid: u32) -> io::Result<Option<u32>> {
        Ok(process_table()?
            .iter()
            .find(|entry| entry.th32ProcessID == pid)
            .map(|entry| entry.th32ParentProcessID))
    }

    pub(super) fn loopback_listener_owner(port: u16) -> io::Result<Option<u32>> {
        // `u32` storage matches the table's alignment.
        let mut storage = vec![0_u32; 1024];
        for _ in 0..8 {
            let mut length =
                u32::try_from(storage.len() * size_of::<u32>()).map_err(io::Error::other)?;
            // SAFETY: `storage` is writable for exactly `length` bytes.
            let result = unsafe {
                GetExtendedTcpTable(
                    storage.as_mut_ptr().cast(),
                    &raw mut length,
                    0,
                    u32::from(AF_INET),
                    TCP_TABLE_OWNER_PID_LISTENER,
                    0,
                )
            };
            if result == ERROR_INSUFFICIENT_BUFFER {
                storage = vec![0_u32; (length as usize).div_ceil(size_of::<u32>()) + 64];
                continue;
            }
            if result != NO_ERROR {
                return Err(io::Error::from_raw_os_error(result.cast_signed()));
            }
            // SAFETY: on success the buffer begins with the table's entry count.
            let count = unsafe { (*storage.as_ptr().cast::<MIB_TCPTABLE_OWNER_PID>()).dwNumEntries }
                as usize;
            let rows_offset = std::mem::offset_of!(MIB_TCPTABLE_OWNER_PID, table);
            if rows_offset + count * size_of::<MIB_TCPROW_OWNER_PID>()
                > storage.len() * size_of::<u32>()
            {
                return Err(io::Error::other("the TCP table overruns its buffer"));
            }
            for index in 0..count {
                // SAFETY: the row lies inside `storage`, as checked above.
                let row = unsafe {
                    ptr::read(
                        storage
                            .as_ptr()
                            .cast::<u8>()
                            .add(rows_offset + index * size_of::<MIB_TCPROW_OWNER_PID>())
                            .cast::<MIB_TCPROW_OWNER_PID>(),
                    )
                };
                // Both fields hold network byte order in their first bytes.
                let address = Ipv4Addr::from(row.dwLocalAddr.to_ne_bytes());
                let [high, low, ..] = row.dwLocalPort.to_ne_bytes();
                if address == Ipv4Addr::LOCALHOST && u16::from_be_bytes([high, low]) == port {
                    return Ok(Some(row.dwOwningPid));
                }
            }
            return Ok(None);
        }
        Err(io::Error::other(
            "the TCP table kept growing while it was read",
        ))
    }

    pub(super) fn executable(pid: u32) -> io::Result<Option<PathBuf>> {
        let Some(process) = open(pid, PROCESS_QUERY_LIMITED_INFORMATION)? else {
            return Ok(None);
        };
        let mut buffer = vec![0_u16; 32_768];
        let mut length = u32::try_from(buffer.len()).map_err(io::Error::other)?;
        // SAFETY: a live handle and `length` writable UTF-16 code units.
        let ok = unsafe {
            QueryFullProcessImageNameW(
                process.0,
                PROCESS_NAME_WIN32,
                buffer.as_mut_ptr(),
                &raw mut length,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        buffer.truncate(length as usize);
        Ok(Some(PathBuf::from(OsString::from_wide(&buffer))))
    }

    pub(super) fn kill_pid(pid: u32) -> io::Result<()> {
        let Some(process) = open(pid, PROCESS_TERMINATE | PROCESS_SYNCHRONIZE)? else {
            return Ok(());
        };
        // SAFETY: a live handle opened with `PROCESS_TERMINATE`.
        if unsafe { TerminateProcess(process.0, 1) } == 0 {
            let error = io::Error::last_os_error();
            // Terminating a process that already exited is refused; it is gone.
            return if exited(&process)? {
                Ok(())
            } else {
                Err(error)
            };
        }
        Ok(())
    }

    pub(super) fn kill_process_group(_pgid: u32) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Windows has no process groups; a Job Object ends a process tree",
        ))
    }

    pub(super) fn window_station_is_visible() -> io::Result<bool> {
        /// `winuser.h`: the station has visible display surfaces.
        const WSF_VISIBLE: u32 = 0x0001;
        // SAFETY: no arguments; the handle returned is the process's own
        // station, which must not be closed.
        let station = unsafe { GetProcessWindowStation() };
        if station.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut flags = USEROBJECTFLAGS::default();
        let mut needed = 0_u32;
        // SAFETY: a live station handle and a writable `USEROBJECTFLAGS` of
        // exactly the size passed.
        let ok = unsafe {
            GetUserObjectInformationW(
                station,
                UOI_FLAGS,
                (&raw mut flags).cast(),
                u32::try_from(size_of::<USEROBJECTFLAGS>()).map_err(io::Error::other)?,
                &raw mut needed,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(flags.dwFlags & WSF_VISIBLE != 0)
    }

    pub(super) fn holds_socket_inode(_pid: u32, _inode: u64) -> io::Result<bool> {
        Err(unread("another process's handles"))
    }

    pub(super) fn loopback_listener_inode(_port: u16) -> io::Result<Option<u64>> {
        Err(unread("socket inodes, which Windows does not have"))
    }

    pub(super) fn environment_carries(
        _pid: u32,
        _variable: &str,
        _nonce: &str,
    ) -> io::Result<bool> {
        Err(unread("another process's environment"))
    }

    fn unread(what: &str) -> io::Error {
        io::Error::new(
            io::ErrorKind::Unsupported,
            format!("this build does not read {what} on Windows, so nothing is inferred from it"),
        )
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod imp {
    use std::io;
    use std::path::PathBuf;

    use super::ProcessIdentity;

    pub(super) const AVAILABLE: bool = false;

    fn unavailable<T>() -> io::Result<T> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "live process identity observation is not implemented for this platform, so no \
             disposition may be inferred from it",
        ))
    }

    pub(super) fn process_identity(_pid: u32) -> io::Result<Option<ProcessIdentity>> {
        unavailable()
    }

    pub(super) fn live_pids() -> io::Result<Vec<u32>> {
        unavailable()
    }

    pub(super) fn holds_socket_inode(_pid: u32, _inode: u64) -> io::Result<bool> {
        unavailable()
    }

    pub(super) fn loopback_listener_inode(_port: u16) -> io::Result<Option<u64>> {
        unavailable()
    }

    pub(super) fn environment_carries(
        _pid: u32,
        _variable: &str,
        _nonce: &str,
    ) -> io::Result<bool> {
        unavailable()
    }

    #[cfg(unix)]
    pub(super) fn socket_identity(_socket: std::os::fd::BorrowedFd<'_>) -> io::Result<u64> {
        unavailable()
    }

    pub(super) fn executable(_pid: u32) -> io::Result<Option<PathBuf>> {
        unavailable()
    }

    pub(super) fn kill_pid(_pid: u32) -> io::Result<()> {
        unavailable()
    }

    pub(super) fn kill_process_group(_pgid: u32) -> io::Result<()> {
        unavailable()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_start_time_is_never_a_match() {
        let observed = ProcessIdentity {
            pid: 42,
            start_time: None,
            process_group: Some(42),
            argv: Vec::new(),
        };
        assert!(
            !observed.matches_recorded(42, Some("991122"), Some(42)),
            "a process whose start time cannot be observed is never trusted as the recorded one"
        );
        let recorded_without_start = ProcessIdentity {
            pid: 42,
            start_time: Some("991122".to_owned()),
            process_group: Some(42),
            argv: Vec::new(),
        };
        assert!(!recorded_without_start.matches_recorded(42, None, Some(42)));
    }

    #[test]
    fn pid_and_pgid_reuse_without_a_matching_start_time_is_rejected() {
        let observed = ProcessIdentity {
            pid: 4242,
            start_time: Some("777".to_owned()),
            process_group: Some(4242),
            argv: Vec::new(),
        };
        assert!(!observed.matches_recorded(4242, Some("991122"), Some(4242)));
        assert!(observed.matches_recorded(4242, Some("777"), Some(4242)));
        assert!(
            !observed.matches_recorded(4242, Some("777"), Some(1)),
            "the process group must corroborate the PID/start-time pair"
        );
    }

    #[test]
    fn the_barrier_marker_is_what_proves_user_code_has_not_started() {
        // The argument vector `gate::barrier_argv` gives the barrier.
        let barrier_argv = vec![
            "godot-mcp-supervisor".to_owned(),
            "barrier".to_owned(),
            BARRIER_ARGV_MARKER.to_owned(),
            "--gate".to_owned(),
            "/tmp/gate".to_owned(),
            "--".to_owned(),
            "godot".to_owned(),
        ];
        let gated = ProcessIdentity {
            pid: 1,
            start_time: Some("1".to_owned()),
            process_group: Some(1),
            argv: barrier_argv.clone(),
        };
        assert!(gated.is_gated_barrier());
        let running = ProcessIdentity {
            argv: vec!["godot".to_owned(), "--headless".to_owned()],
            ..gated.clone()
        };
        assert!(!running.is_gated_barrier());
        // A wrapper that starts the barrier carries its arguments too, further
        // along: it is not the barrier.
        let mut wrapper_argv = vec![
            "bwrap".to_owned(),
            "--unshare-pid".to_owned(),
            "--".to_owned(),
        ];
        wrapper_argv.extend(barrier_argv);
        let wrapper = ProcessIdentity {
            argv: wrapper_argv,
            ..gated
        };
        assert!(!wrapper.is_gated_barrier());
    }

    #[test]
    fn the_argument_area_yields_the_argument_vector_and_the_environment() {
        // The shape `KERN_PROCARGS2` answers with: the argument count, the
        // executable path, the NULs that align what follows, the arguments, the
        // environment, an empty string, then the kernel's own `key=value` strings.
        let mut area = 3_i32.to_ne_bytes().to_vec();
        area.extend_from_slice(b"/opt/godot-mcp/godot-mcp-supervisor\0\0\0\0");
        area.extend_from_slice(b"godot-mcp-supervisor\0barrier\0\0");
        area.extend_from_slice(b"HOME=/Users/qa\0GODOT_MCP_LAUNCH_NONCE=ab12\0\0");
        area.extend_from_slice(
            b"executable_path=/opt/godot-mcp/godot-mcp-supervisor\0ptr_munge=\0",
        );
        let parsed = parse_argument_area(&area).expect("a well-formed area");
        assert_eq!(parsed.argv, ["godot-mcp-supervisor", "barrier", ""]);
        assert!(parsed.carries("GODOT_MCP_LAUNCH_NONCE", "ab12"));
        assert!(parsed.carries("HOME", "/Users/qa"));
        assert!(!parsed.carries("GODOT_MCP_LAUNCH_NONCE", "ab1"));
        assert!(!parsed.carries("GODOT_MCP_LAUNCH_NONCE", "ab123"));
        assert!(
            !parsed.carries("executable_path", "/opt/godot-mcp/godot-mcp-supervisor"),
            "the kernel's own strings after the environment are not part of it"
        );
    }

    #[test]
    fn a_short_or_malformed_argument_area_is_never_read_as_a_process() {
        assert!(parse_argument_area(&[]).is_none());
        assert!(parse_argument_area(&[1, 0]).is_none());
        assert!(parse_argument_area(&(-1_i32).to_ne_bytes()).is_none());
        // It names two arguments and carries one.
        let mut area = 2_i32.to_ne_bytes().to_vec();
        area.extend_from_slice(b"/bin/sleep\0\0sleep\0");
        assert!(parse_argument_area(&area).is_none());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_loopback_listener_is_attributed_to_the_process_holding_it() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let port = listener.local_addr().expect("address").port();
        let inode = loopback_listener_inode(port)
            .expect("the socket table is readable")
            .expect("the listener is found");
        assert!(holds_socket_inode(std::process::id(), inode).expect("descriptors readable"));
        drop(listener);
        // A child that another test starts at this moment holds a duplicate of
        // every descriptor until it executes its program, the listener's
        // included, so the socket can outlive the drop by that much.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while loopback_listener_inode(port).expect("readable").is_some() {
            assert!(
                std::time::Instant::now() < deadline,
                "the listener outlived its last descriptor"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_socket_is_attributed_only_to_the_processes_holding_it() {
        use std::os::fd::AsFd as _;

        let (ours, theirs) = std::os::unix::net::UnixStream::pair().expect("socket pair");
        let identity = socket_identity(theirs.as_fd()).expect("identifiable");
        assert_ne!(
            identity,
            socket_identity(ours.as_fd()).expect("identifiable"),
            "each end of a pair is its own socket"
        );
        let pid = std::process::id();
        assert!(holds_socket_inode(pid, identity).expect("inspectable"));
        drop(theirs);
        assert!(
            !holds_socket_inode(pid, identity).expect("inspectable"),
            "a closed end is held by nobody"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_process_executable_is_the_binary_it_runs() {
        let running = executable(std::process::id())
            .expect("observable")
            .expect("this process is alive");
        assert_eq!(
            std::fs::canonicalize(running).expect("resolvable"),
            std::fs::canonicalize(std::env::current_exe().expect("current executable"))
                .expect("resolvable")
        );
        assert_eq!(
            executable(i32::MAX.cast_unsigned()).expect("observable"),
            None
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_child_is_observed_with_its_arguments_and_group_until_it_exits() {
        // The environment is observed on the bundled supervisor instead
        // (`tests/process_observations.rs`): macOS withholds the environment of
        // its own signed system binaries, such as this `/bin/sleep`.
        use std::time::{Duration, Instant};

        let mut child = std::process::Command::new("/bin/sleep")
            .arg("37")
            .spawn()
            .expect("spawn /bin/sleep");
        let pid = child.id();
        let deadline = Instant::now() + Duration::from_secs(10);
        // Until it has replaced its image, a child still describes this binary.
        let observed = loop {
            let observed = process_identity(pid)
                .expect("observable")
                .expect("the child is alive");
            if observed.argv.last().map(String::as_str) == Some("37") {
                break observed;
            }
            assert!(
                Instant::now() < deadline,
                "the child never ran: {observed:?}"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        let own = process_identity(std::process::id())
            .expect("observable")
            .expect("this process is alive");
        assert!(observed.start_time.is_some());
        assert_eq!(observed.process_group, own.process_group);
        assert!(live_pids().expect("enumerable").contains(&pid));
        kill_pid(pid).expect("signalled");
        // Killed and not yet reaped, the child is a zombie: it is reported gone.
        while process_identity(pid).expect("observable").is_some() {
            assert!(
                Instant::now() < deadline,
                "a killed child is still reported alive"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!child.wait().expect("reaped").success());
    }

    /// The barrier marker is found in a Windows process's command line only if
    /// it is split the way the C runtime split it for that process's own
    /// `argv`, quotes and backslashes included.
    #[test]
    fn a_windows_command_line_splits_as_the_c_runtime_reads_it() {
        let split = |line: &str| split_command_line(line);
        assert_eq!(
            split(
                r#""C:\Program Files\godot-mcp\godot-mcp-supervisor.exe" barrier --godot-mcp-barrier --gate "C:\state\launch 1\gate" -- C:\godot\Godot_v4.7.1-stable_win64_console.exe --headless"#
            ),
            [
                r"C:\Program Files\godot-mcp\godot-mcp-supervisor.exe",
                "barrier",
                BARRIER_ARGV_MARKER,
                "--gate",
                r"C:\state\launch 1\gate",
                "--",
                r"C:\godot\Godot_v4.7.1-stable_win64_console.exe",
                "--headless",
            ]
        );
        // The program name takes no escapes; the arguments do.
        assert_eq!(split(r#"C:\a\b.exe x"#), [r"C:\a\b.exe", "x"]);
        assert_eq!(split(r#"p a\\"b c" d"#), ["p", r"a\b c", "d"]);
        assert_eq!(split(r#"p a\\\"b c"#), ["p", r#"a\"b"#, "c"]);
        assert_eq!(split(r#"p a\\b"#), ["p", r"a\\b"]);
        assert_eq!(split(r#"p "a""b" c"#), ["p", r#"a"b"#, "c"]);
        assert_eq!(split("p \t a\t\tb "), ["p", "a", "b"]);
        assert_eq!(split(r#"p """#), ["p", ""]);
        assert_eq!(split(""), Vec::<String>::new());
    }

    /// The lineage a Windows launch builds: the barrier (10) runs Godot's
    /// console executable (20), which runs the windowed engine (30) that listens.
    #[test]
    fn a_windows_listener_belongs_to_the_launch_whose_barrier_it_descends_from() {
        let table = |entries: &'static [(u32, Option<u32>, u64)]| {
            move |pid: u32| {
                entries
                    .iter()
                    .find(|(id, _, _)| *id == pid)
                    .map(|(_, parent, created)| (*parent, *created))
            }
        };
        let launch = table(&[(10, Some(4), 100), (20, Some(10), 110), (30, Some(20), 120)]);
        assert_eq!(
            launched_program_in(&launch, 30, 10, "100"),
            Some(20),
            "the program the barrier ran is the console executable"
        );
        assert_eq!(launched_program_in(&launch, 20, 10, "100"), Some(20));
        assert_eq!(
            launched_program_in(&launch, 30, 10, "101"),
            None,
            "a barrier with another creation time is not the recorded one"
        );
        assert_eq!(
            launched_program_in(&launch, 10, 10, "100"),
            None,
            "the barrier itself never listens"
        );
        assert_eq!(
            launched_program_in(&launch, 30, 4, "1"),
            None,
            "three levels is not the shape"
        );

        let direct = table(&[(10, Some(4), 100), (30, Some(10), 120)]);
        assert_eq!(launched_program_in(&direct, 30, 10, "100"), Some(30));

        // 20 exited and its id now names a process created after its "child".
        let recycled = table(&[(10, Some(4), 100), (20, Some(10), 500), (30, Some(20), 120)]);
        assert_eq!(launched_program_in(&recycled, 30, 10, "100"), None);
        let orphaned = table(&[(10, Some(4), 100), (30, Some(20), 120)]);
        assert_eq!(launched_program_in(&orphaned, 30, 10, "100"), None);
        let unrelated = table(&[(10, Some(4), 100), (50, Some(40), 120), (40, Some(4), 90)]);
        assert_eq!(launched_program_in(&unrelated, 50, 10, "100"), None);
    }

    #[cfg(windows)]
    #[test]
    fn a_windows_process_is_observed_with_its_creation_time_and_arguments_until_it_exits() {
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};

        let own = process_identity(std::process::id())
            .expect("observable")
            .expect("this process is alive");
        assert!(own.start_time.is_some());
        assert_eq!(own.process_group, None, "Windows has no process groups");
        assert!(own.matches_recorded(own.pid, own.start_time.as_deref(), None));

        let mut child = Command::new("ping")
            .args(["-n", "120", "127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ping");
        let pid = child.id();
        let observed = process_identity(pid)
            .expect("observable")
            .expect("the child is alive");
        assert!(
            observed
                .argv
                .ends_with(&["-n".to_owned(), "120".to_owned(), "127.0.0.1".to_owned()]),
            "{:?}",
            observed.argv
        );
        assert!(
            observed.start_time >= own.start_time,
            "{observed:?} {own:?}"
        );
        assert_eq!(
            parent_pid(pid).expect("observable"),
            Some(std::process::id())
        );
        assert!(live_pids().expect("enumerable").contains(&pid));
        let image = executable(pid)
            .expect("observable")
            .expect("the child is alive");
        assert!(
            image
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.eq_ignore_ascii_case("ping.exe")),
            "{}",
            image.display()
        );

        kill_pid(pid).expect("terminated");
        let deadline = Instant::now() + Duration::from_secs(10);
        // The handle `child` holds keeps the process object, not the process.
        while process_identity(pid).expect("observable").is_some() {
            assert!(
                Instant::now() < deadline,
                "a terminated child is still reported alive"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!child.wait().expect("waited").success());
        kill_pid(pid).expect("terminating a process that is gone is no error");
        assert_eq!(process_identity(0xFFFF_FFFC).expect("observable"), None);
    }

    #[cfg(windows)]
    #[test]
    fn a_windows_loopback_listener_is_attributed_to_the_process_that_owns_it() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let port = listener.local_addr().expect("address").port();
        assert_eq!(
            loopback_listener_owner(port).expect("the TCP table is readable"),
            Some(std::process::id())
        );
        drop(listener);
        assert_eq!(loopback_listener_owner(port).expect("readable"), None);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn this_process_is_observable_with_a_start_time() {
        let pid = std::process::id();
        let observed = process_identity(pid)
            .expect("procfs is readable")
            .expect("this process is alive");
        assert_eq!(observed.pid, pid);
        assert!(observed.start_time.is_some());
        assert!(observed.process_group.is_some());
        assert!(observed.matches_recorded(
            pid,
            observed.start_time.as_deref(),
            observed.process_group
        ));
    }
}
