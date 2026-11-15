// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Bounded discovery of a Java 17+ runtime for the owner-operated local proofing server.
//!
//! This module is deliberately std-only, like [`crate::proof_supervisor`]: it depends on nothing but
//! [`Java`] and `std`, and keeps no state of the owner's. Its only shared state is the process-wide
//! count of output-reader permits described below; every probe otherwise starts from nothing.
//!
//! Contract points implemented here:
//! - [`discover_cancellable`] is blocking. It reads directories and starts short-lived child processes, so the
//!   host calls it from its background worker on an owner's explicit Check or Start action, never
//!   from `status()`, a constructor, the window thread, or at module load. Nothing runs on import.
//! - [`discover_cancellable`] is the same pass with the owner's stop flag: the flag is read before
//!   the search, between candidates and inside every probe, so a stop ends the pass within one
//!   [`POLL_INTERVAL`] instead of at the next deadline, and only the probe's own child is ever
//!   signalled. A cancelled pass hands back no launcher. The flag is owned by the caller (the
//!   host's helper) and only ever read here: nothing in this module creates, clears or waits on it.
//!   What happens to a probe when the *application* dies abruptly is the helper's contract, not
//!   this module's, and is not established by these tests.
//! - Nothing here downloads, installs, creates a directory, writes a file, opens a socket, or asks
//!   the network anything. A missing Java is reported, never fixed.
//! - Candidates are considered in a fixed order: the `JAVA_HOME` hint, bounded `PATH` entries, then
//!   bounded platform standard locations. The `PATH` bound counts the entries actually traversed,
//!   before any is refused; a directory scan counts every result it reads, including failures, so a
//!   long run of unusable entries cannot push the scan past its bound. Relative and current-directory
//!   `PATH` entries are refused, duplicate candidates are collapsed, and a candidate must be an
//!   absolute path to an executable regular file.
//! - The probe starts the candidate itself with `-version` as one separate argument, a null stdin,
//!   the shared sanitized environment ([`environment`]), and no shell of any kind. At most 4 KiB of
//!   its output is retained, it must finish within [`PROBE_TIMEOUT`], and only the child this probe
//!   started is ever killed and reaped. A single deadline, taken before the child is started, covers
//!   process creation, waiting and reading the reply; there is no read grace after it. Reader
//!   threads are detached, so a descendant that keeps the inherited pipes open can never make the
//!   probe wait indefinitely. Their number is bounded process-wide by
//!   [`MAX_OUTSTANDING_READERS`]: a probe is refused before any child is started when no reader slot
//!   is free, so repeated probes against a descendant that holds the pipes can never accumulate
//!   threads without limit. One pass is therefore bounded by [`MAX_CANDIDATES`] probes of
//!   [`PROBE_TIMEOUT`] each, which is the worst case a background worker has to be willing to spend;
//!   a stop flag shortens it, it never lengthens a probe.
//! - Both the modern (`openjdk version "21.0.1"`) and the legacy (`java version "1.8.0_392"`) reply
//!   are parsed by a bounded parser; malformed, truncated, oversized, non-zero or silent replies
//!   fail closed. A reply is only trusted when both streams ended cleanly within the retention
//!   bound, the parsing and the exit status agree. A read error or output beyond the bound is never
//!   turned into a version.
//! - Failures carry no detail: no path, version text, raw output, or command line is returned,
//!   logged, or formatted. [`Discovery`]'s `Debug` output is redacted.
//!
//! Cross-platform limits: the `PATH` and standard-location layout is written for Unix and Windows
//! but has only been exercised on Linux. On Windows the child is created with `CREATE_NO_WINDOW` so
//! a probe never flashes a console, and that flag is compile-time guarded but not exercised here.
//! Process creation and the final `wait` after a kill are operating-system calls: the deadline
//! covers when this code stops waiting for them, but a pathologically slow kernel could still make
//! either call take longer than [`PROBE_TIMEOUT`]. No wait is unbounded on purpose.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use mailcraft_ui_egui::proof_server::Java;

use crate::proof_supervisor::environment;

/// The oldest Java the pinned LanguageTool server is supported on.
pub(crate) const MINIMUM_JAVA: u32 = 17;

/// The whole probe of one candidate, including reading its reply, must finish within this bound.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Per-stream retention bound, so at most 4 KiB of a candidate's output is ever kept.
pub(crate) const MAX_RETAINED_PER_STREAM: usize = 2048;

/// `PATH` entries considered, which bounds the directories read for one discovery pass.
pub(crate) const MAX_PATH_ENTRIES: usize = 32;

/// A version-manager directory listing is bounded to this many names before its entries are used.
pub(crate) const MAX_DIRECTORY_ENTRIES: usize = 16;

/// The hard cap on directory entries read at all, so one huge directory cannot flood memory.
pub(crate) const MAX_DIRECTORY_READ: usize = 256;

/// The total number of distinct candidates one pass may probe.
pub(crate) const MAX_CANDIDATES: usize = 24;

/// A version token inside a `version "..."` line is refused beyond this length.
const MAX_VERSION_TOKEN: usize = 64;

/// The most probe output readers that may be alive at once, process-wide.
///
/// A reader lives until its pipe reaches end of file. A descendant that inherits the pipes can keep
/// one alive indefinitely, so without a cap each repeated probe would add two threads that can never
/// be joined. Slots are taken before a child is started, so a saturated process refuses probes
/// (without running anything) instead of growing without bound. They are released when the reader
/// thread ends, which is exactly when its pipe closed.
pub(crate) const MAX_OUTSTANDING_READERS: usize = 32;

const POLL_INTERVAL: Duration = Duration::from_millis(10);

static READER_SLOTS: Mutex<usize> = Mutex::new(MAX_OUTSTANDING_READERS);

fn reader_slots() -> MutexGuard<'static, usize> {
    READER_SLOTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// One process-wide output-reader permit; dropping it returns the permit.
pub(crate) struct ReaderSlot {
    _private: (),
}

impl Drop for ReaderSlot {
    fn drop(&mut self) {
        let mut available = reader_slots();
        *available = available.saturating_add(1).min(MAX_OUTSTANDING_READERS);
    }
}

/// Takes one reader permit without waiting, or reports that the process is saturated.
pub(crate) fn acquire_reader_slot() -> Option<ReaderSlot> {
    let mut available = reader_slots();
    *available = available.checked_sub(1)?;
    Some(ReaderSlot { _private: () })
}

/// Readers currently alive, held or blocked. Tests use it to observe admission and release.
#[cfg(test)]
pub(crate) fn outstanding_reader_slots() -> usize {
    MAX_OUTSTANDING_READERS.saturating_sub(*reader_slots())
}

#[cfg(windows)]
const JAVA_EXECUTABLE: &str = "java.exe";
#[cfg(not(windows))]
const JAVA_EXECUTABLE: &str = "java";

/// What one explicit discovery pass found.
///
/// `executable` is set only when `status` is [`Java::Available`], so a caller can never be handed a
/// path it would be wrong to start. Nothing here is logged.
#[derive(Clone, PartialEq, Eq)]
pub struct Discovery {
    /// The launcher that answered for Java 17 or newer, as an absolute path.
    pub executable: Option<PathBuf>,
    pub status: Java,
}

/// Counts only: the path is redacted, so a log line or a panic message cannot leak the owner's
/// directory layout.
impl fmt::Debug for Discovery {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let executable = if self.executable.is_some() { "Some(<redacted>)" } else { "None" };
        write!(formatter, "Discovery {{ status: {:?}, executable: {} }}", self.status, executable)
    }
}

/// The inputs of one pass. Production builds them from the host's own environment; tests supply
/// their own so no real Java is needed.
#[derive(Default)]
pub(crate) struct Inputs {
    /// A validated absolute `JAVA_HOME`; its `bin` directory is tried first.
    pub(crate) java_home: Option<PathBuf>,
    /// Already-split `PATH` entries.
    pub(crate) path_directories: Vec<PathBuf>,
    /// Platform standard directories, already expanded to the directory that holds the launcher.
    pub(crate) standard_directories: Vec<PathBuf>,
}

/// Discover a Java 17+ launcher, blocking until the bounded search and probes are done.
///
/// Call this from a host background worker on an explicit owner action. It may read directories and
/// start several short-lived children, so it must never run on the window thread, inside `status()`,
/// a constructor, or at module load. It never downloads or installs anything, never writes anywhere,
/// and never starts a server.
#[cfg(test)]
pub fn discover() -> Discovery {
    discover_with(&host_inputs())
}

/// Discover a Java 17+ launcher, giving up as soon as `stop` is set.
///
/// A bounded pass that keeps ownership of each probe, with the caller's stop flag. The flag is read
/// before the search, between candidates and inside every probe, so a stop ends the pass within one
/// poll of the probe in flight and no candidate after it is started; a child that is running when
/// the stop arrives is killed and reaped by the probe that started it, and nothing else is signalled.
///
/// A cancelled pass hands back no launcher and reports [`Java::Unknown`], or [`Java::TooOld`] when a
/// launcher had already answered as too old. It never reports [`Java::Available`]: it cannot claim a
/// usable launcher was verified, and it may not have searched at all. `Discovery` has no cancelled
/// status of its own; the host owns the flag, so it reports its own cancellation, and a caller can
/// also read the flag after this returns to tell "found nothing" from "was stopped".
pub fn discover_cancellable(stop: &AtomicBool) -> Discovery {
    discover_with_control(&host_inputs(), stop)
}

fn host_inputs() -> Inputs {
    let java_home = std::env::var_os("JAVA_HOME");
    let path = std::env::var_os("PATH");
    inputs_from(java_home.as_deref(), path.as_deref(), standard_directories())
}

/// Builds one pass's inputs from a raw `JAVA_HOME` and a raw, unsplit `PATH`.
///
/// Kept separate from the environment so tests can exercise the traversal bound without touching
/// global state. The `PATH` bound counts the entries actually traversed, before any is refused: a
/// long run of unusable entries cannot let a directory beyond the bound be searched.
pub(crate) fn inputs_from(java_home: Option<&OsStr>, path: Option<&OsStr>, standard_directories: Vec<PathBuf>) -> Inputs {
    let java_home = java_home.map(PathBuf::from).filter(|home| home.is_absolute());
    let mut path_directories = Vec::new();
    if let Some(path) = path {
        for directory in std::env::split_paths(path).take(MAX_PATH_ENTRIES) {
            if acceptable_directory(&directory) {
                path_directories.push(directory);
            }
        }
    }
    Inputs { java_home, path_directories, standard_directories }
}

/// The platform locations a Java runtime is normally installed in, with every scan bounded.
#[cfg(unix)]
fn standard_directories() -> Vec<PathBuf> {
    const FIXED: &[&str] = &[
        "/usr/bin",
        "/bin",
        "/usr/local/bin",
        "/usr/local/opt/openjdk/bin",
        "/opt/homebrew/opt/openjdk/bin",
        "/usr/local/opt/openjdk@17/bin",
        "/opt/homebrew/opt/openjdk@17/bin",
        "/usr/local/opt/openjdk@21/bin",
        "/opt/homebrew/opt/openjdk@21/bin",
    ];
    let mut directories: Vec<PathBuf> = FIXED.iter().map(PathBuf::from).collect();
    for root in ["/usr/lib/jvm", "/opt/java"] {
        directories.extend(scan_root(Path::new(root), Path::new("bin")));
    }
    #[cfg(target_os = "macos")]
    directories.extend(scan_root(Path::new("/Library/Java/JavaVirtualMachines"), Path::new("Contents/Home/bin")));
    directories
}

#[cfg(windows)]
fn standard_directories() -> Vec<PathBuf> {
    let mut directories = Vec::new();
    for variable in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
        let Some(base) = std::env::var_os(variable).map(PathBuf::from).filter(|base| base.is_absolute()) else {
            continue;
        };
        for relative in ["Java", "Eclipse Adoptium", "Amazon Corretto", "Microsoft", "Zulu", "BellSoft\\LibericaJDK"] {
            directories.extend(scan_root(&base.join(relative), Path::new("bin")));
        }
    }
    directories
}

/// Lists `root` up to a hard read cap, sorts the names so a scan is reproducible, and returns at most
/// [`MAX_DIRECTORY_ENTRIES`] candidates of the form `root/<name>/<suffix>`.
pub(crate) fn scan_root(root: &Path, suffix: &Path) -> Vec<PathBuf> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    let names = bounded_scan_names(entries.map(|entry| entry.map(|entry| entry.file_name())));
    names.into_iter().map(|name| root.join(name).join(suffix)).collect()
}

/// The bounded, sorted names of a directory scan.
///
/// The read cap counts every result, including the ones that failed: an entry that could not be
/// named still consumes one slot, so a directory that keeps returning errors can never make the
/// scan traverse more than [`MAX_DIRECTORY_READ`] results. Only named entries are returned, and at
/// most [`MAX_DIRECTORY_ENTRIES`] of them.
pub(crate) fn bounded_scan_names(results: impl Iterator<Item = io::Result<OsString>>) -> Vec<OsString> {
    let mut names: Vec<OsString> = Vec::new();
    // `take` comes first on purpose: a failed result still consumes one of the read slots.
    for name in results.take(MAX_DIRECTORY_READ).flatten() {
        names.push(name);
    }
    names.sort();
    names.truncate(MAX_DIRECTORY_ENTRIES);
    names
}

/// A directory may only be searched when it is an absolute path that cannot mean "here".
pub(crate) fn acceptable_directory(directory: &Path) -> bool {
    if !directory.is_absolute() {
        return false;
    }
    for component in directory.components() {
        if matches!(component, Component::ParentDir | Component::CurDir) {
            return false;
        }
    }
    // An absolute path equal to the working directory is the current directory by another name.
    match std::env::current_dir() {
        Ok(current) => current != directory,
        Err(_) => true,
    }
}

/// A candidate must be an absolute path to an executable regular file; nothing is followed further.
fn usable_executable(path: &Path) -> bool {
    if !path.is_absolute() {
        return false;
    }
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(_) => return false,
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return false;
        }
    }
    true
}

/// The bounded, ordered, de-duplicated candidate list one pass probes.
pub(crate) fn plan(inputs: &Inputs) -> Vec<PathBuf> {
    let mut directories: Vec<PathBuf> = Vec::new();
    if let Some(home) = inputs.java_home.as_ref().filter(|home| home.is_absolute()) {
        directories.push(home.join("bin"));
    }
    for directory in inputs.path_directories.iter().take(MAX_PATH_ENTRIES).chain(inputs.standard_directories.iter().take(MAX_PATH_ENTRIES)) {
        if acceptable_directory(directory) {
            directories.push(directory.clone());
        }
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    for directory in directories {
        if candidates.len() >= MAX_CANDIDATES {
            break;
        }
        let candidate = directory.join(JAVA_EXECUTABLE);
        if candidates.contains(&candidate) || !usable_executable(&candidate) {
            continue;
        }
        candidates.push(candidate);
    }
    candidates
}

/// The fixed outcome of one bounded probe. No detail travels back from the child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProbeOutcome {
    /// A well-formed reply, carrying the major release it reported.
    Version { major: u32 },
    /// The candidate ran and exited successfully but its reply could not be parsed within bounds,
    /// including when a stream produced more output than the retention bound.
    Malformed,
    /// The candidate could not be started at all.
    SpawnFailed,
    /// The probe did not finish within its bound; the owned child was killed and reaped.
    TimedOut,
    /// The probe ended without a complete, successful reply.
    Failed,
    /// No output-reader capacity was free, so the candidate was not started.
    Busy,
    /// The pass was cancelled. Either nothing was started because the stop was already set, or the
    /// child this probe started was killed and reaped; no other process was touched.
    Stopped,
    /// The candidate was not an absolute path, so nothing was started.
    Refused,
}

/// Whether the caller has asked this pass to stop.
///
/// `Relaxed` is enough: the flag guards no data of ours, it only asks this code to stop, and a stop
/// seen one poll later is still inside the documented bound. Nothing here ever writes the flag.
fn stopped(stop: Option<&AtomicBool>) -> bool {
    stop.is_some_and(|stop| stop.load(Ordering::Relaxed))
}

/// Probes one absolute candidate within [`PROBE_TIMEOUT`].
pub(crate) fn probe(program: &Path) -> ProbeOutcome {
    probe_within(program, PROBE_TIMEOUT)
}

/// Probes one candidate, bounded by `timeout`. Only a child this call started is ever signalled.
///
/// The deadline is taken before the child is started, so `timeout` covers process creation, waiting
/// and reading the reply together. There is no read grace after it: when the deadline passes while a
/// descendant still holds a pipe, the probe reports what it has and the detached reader (and its
/// permit) stays with the pipe until it closes.
pub(crate) fn probe_within(program: &Path, timeout: Duration) -> ProbeOutcome {
    probe_control(program, timeout, None)
}

/// Probes one candidate, bounded by `timeout` and by `stop`.
///
/// The same bound and the same ownership as [`probe_within`], with the caller's stop flag read at the
/// four points where a stop can still save work: before the deadline is taken and the reader slots
/// are admitted, again immediately before the spawn call, on every wait poll, and between
/// reply-collection waits (so no single wait is longer than [`POLL_INTERVAL`]).
/// A stop kills and reaps exactly the child this call started, and reports [`ProbeOutcome::Stopped`];
/// no other candidate, and no other process, is touched. A stop is never longer than the deadline and
/// can only shorten the probe.
pub(crate) fn probe_guarded(program: &Path, timeout: Duration, stop: &AtomicBool) -> ProbeOutcome {
    probe_control(program, timeout, Some(stop))
}

/// The whole probe of one candidate, with an optional cancellation flag.
fn probe_control(program: &Path, timeout: Duration, stop: Option<&AtomicBool>) -> ProbeOutcome {
    if !program.is_absolute() || timeout.is_zero() {
        return ProbeOutcome::Refused;
    }
    if stopped(stop) {
        return ProbeOutcome::Stopped;
    }
    let Some(deadline) = Instant::now().checked_add(timeout) else {
        return ProbeOutcome::Failed;
    };
    // Admission comes before the child, so a saturated process never starts a candidate whose output
    // it could not read. A permit travels into its reader thread and comes back when that thread ends.
    let Some(stdout_slot) = acquire_reader_slot() else {
        return ProbeOutcome::Busy;
    };
    let Some(stderr_slot) = acquire_reader_slot() else {
        return ProbeOutcome::Busy;
    };
    let mut command = Command::new(program);
    command.arg("-version");
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    // The same whitelist and JVM-option removals the supervised server gets.
    environment(&mut command);
    #[cfg(windows)]
    {
        // CREATE_NO_WINDOW: a probe must never flash a console. Untested on Windows.
        use std::os::windows::process::CommandExt;
        let _ = command.creation_flags(0x0800_0000);
    }

    // Checked again as late as it can be: a pass cancelled while this candidate was being prepared
    // starts nothing at all.
    if stopped(stop) {
        return ProbeOutcome::Stopped;
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => return ProbeOutcome::SpawnFailed,
    };

    // Both streams are drained continuously by detached threads: a child that writes a lot is never
    // blocked, and a descendant holding the pipes can never make this call wait for a join.
    let (sender, receiver) = mpsc::channel();
    let mut expected = 0usize;
    match child.stdout.take() {
        Some(stdout) => {
            if spawn_reader(stdout, Stream::Stdout, sender.clone(), stdout_slot) {
                expected += 1;
            } else {
                kill_and_reap(&mut child);
                return ProbeOutcome::Failed;
            }
        }
        None => drop(stdout_slot),
    }
    match child.stderr.take() {
        Some(stderr) => {
            if spawn_reader(stderr, Stream::Stderr, sender.clone(), stderr_slot) {
                expected += 1;
            } else {
                kill_and_reap(&mut child);
                return ProbeOutcome::Failed;
            }
        }
        None => drop(stderr_slot),
    }
    drop(sender);

    let mut exit = None;
    let mut timed_out = false;
    let mut cancelled = false;
    loop {
        // Every wait poll asks first, so a stop kills this probe's own child straight away instead of
        // at its deadline; the next candidate is then never started at all.
        if stopped(stop) {
            cancelled = true;
            break;
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                exit = Some(status);
                break;
            }
            Ok(None) => {}
            Err(_) => {
                kill_and_reap(&mut child);
                return ProbeOutcome::Failed;
            }
        }
        if Instant::now() >= deadline {
            timed_out = true;
            break;
        }
        thread::sleep(POLL_INTERVAL);
    }
    if cancelled {
        kill_and_reap(&mut child);
        return ProbeOutcome::Stopped;
    }
    if timed_out {
        kill_and_reap(&mut child);
        return ProbeOutcome::TimedOut;
    }

    let mut stdout_report: Option<(Vec<u8>, ReadStatus)> = None;
    let mut stderr_report: Option<(Vec<u8>, ReadStatus)> = None;
    let mut received = 0usize;
    let mut cancelled = false;
    while received < expected {
        if stopped(stop) {
            cancelled = true;
            break;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        // One wait is never longer than a poll, so a stop is noticed while the reply is collected
        // rather than at the deadline. The deadline itself is unchanged.
        let slice = remaining.min(POLL_INTERVAL);
        match receiver.recv_timeout(slice) {
            Ok((Stream::Stdout, bytes, status)) => {
                stdout_report = Some((bytes, status));
                received += 1;
            }
            Ok((Stream::Stderr, bytes, status)) => {
                stderr_report = Some((bytes, status));
                received += 1;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    if cancelled {
        // The child has already exited when this loop runs, so this signals nothing unless it is
        // still alive; either way exactly this probe's child is reaped.
        kill_and_reap(&mut child);
        return ProbeOutcome::Stopped;
    }
    if expected == 0 || received != expected {
        return ProbeOutcome::Failed;
    }
    let Some((stdout, stdout_status)) = stdout_report else {
        return ProbeOutcome::Failed;
    };
    let Some((stderr, stderr_status)) = stderr_report else {
        return ProbeOutcome::Failed;
    };
    // A stream that failed or overflowed is not a reply that can be trusted, even when the bytes
    // retained from its beginning happen to contain a well-formed version line.
    if matches!(stdout_status, ReadStatus::Errored) || matches!(stderr_status, ReadStatus::Errored) {
        return ProbeOutcome::Failed;
    }
    if matches!(stdout_status, ReadStatus::Overflowed) || matches!(stderr_status, ReadStatus::Overflowed) {
        return ProbeOutcome::Malformed;
    }
    let Some(status) = exit else {
        return ProbeOutcome::Failed;
    };
    if !status.success() {
        return ProbeOutcome::Failed;
    }
    match parse_major(&stdout).or_else(|| parse_major(&stderr)) {
        Some(major) => ProbeOutcome::Version { major },
        None => ProbeOutcome::Malformed,
    }
}

#[derive(Clone, Copy)]
enum Stream {
    Stdout,
    Stderr,
}

/// How one drained pipe ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReadStatus {
    /// The pipe reached end of file within the retention bound.
    Complete,
    /// More bytes arrived than [`MAX_RETAINED_PER_STREAM`]; they were drained and discarded.
    Overflowed,
    /// The read failed; the bytes after the failure are unknown.
    Errored,
}

/// Drains one pipe to end of file, retaining at most [`MAX_RETAINED_PER_STREAM`] bytes.
///
/// Reading continues after the bound so the child is never blocked, but the fact that more arrived
/// is reported instead of silently dropped: an oversized or failed stream can never be mistaken for
/// a complete reply.
pub(crate) fn drain_stream<R: Read>(mut reader: R) -> (Vec<u8>, ReadStatus) {
    let mut retained: Vec<u8> = Vec::with_capacity(256);
    let mut buffer = [0u8; 512];
    let mut overflowed = false;
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                let room = MAX_RETAINED_PER_STREAM.saturating_sub(retained.len());
                if count > room {
                    overflowed = true;
                }
                let keep = room.min(count);
                if let Some(bytes) = buffer.get(..keep) {
                    retained.extend_from_slice(bytes);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return (retained, ReadStatus::Errored),
        }
    }
    let status = if overflowed { ReadStatus::Overflowed } else { ReadStatus::Complete };
    (retained, status)
}

/// Drains one pipe to end of file on a detached thread, taking its reader permit with it.
///
/// The handle is dropped, which detaches the thread on purpose: if a descendant keeps the inherited
/// pipe open, this thread stays blocked in `read` and must not be joined. It holds one
/// [`MAX_OUTSTANDING_READERS`] permit until it ends, so blocked readers are bounded process-wide.
fn spawn_reader<R: Read + Send + 'static>(reader: R, stream: Stream, sender: Sender<(Stream, Vec<u8>, ReadStatus)>, slot: ReaderSlot) -> bool {
    thread::Builder::new()
        .name("mailcraft-proof-java-probe".to_string())
        .spawn(move || {
            // The permit is released when this closure (and so the thread) ends.
            let _slot = slot;
            let (retained, status) = drain_stream(reader);
            let _ = sender.send((stream, retained, status));
        })
        .is_ok()
}

/// Kills and reaps exactly the child this probe started, and nothing else.
fn kill_and_reap(child: &mut Child) {
    if !matches!(child.try_wait(), Ok(Some(_))) {
        let _ = child.kill();
    }
    let _ = child.wait();
}

/// Extracts the major release from a bounded `version "..."` line.
pub(crate) fn parse_major(bytes: &[u8]) -> Option<u32> {
    const NEEDLE: &[u8] = b"version \"";
    let start = find(bytes, NEEDLE)?.checked_add(NEEDLE.len())?;
    let rest = bytes.get(start..)?;
    let end = rest.iter().position(|byte| *byte == b'"')?;
    if end == 0 || end > MAX_VERSION_TOKEN {
        return None;
    }
    major_from_token(rest.get(..end)?)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|window| window == needle)
}

/// Reads a complete, well-formed version token: dotted numeric components, then an optional suffix.
///
/// The forms a supported JVM actually reports are accepted: `21.0.1`, `17`, `17-ea`, `17+35`,
/// `17.0.1-rc1`, `21-ea+35`, the legacy `1.8.0_392` and `27.0.1.0.3`. Everything else is refused:
/// trailing junk such as `17garbage`, a component without digits, an empty or interrupted suffix, or
/// any byte outside ASCII digits and the suffix alphabet. A token is never read past its quote.
fn major_from_token(token: &[u8]) -> Option<u32> {
    if token.is_empty() || token.len() > MAX_VERSION_TOKEN {
        return None;
    }
    let mut components: Vec<u32> = Vec::new();
    let mut index = 0usize;
    loop {
        let (value, next) = version_component(token, index)?;
        components.push(value);
        index = next;
        match token.get(index) {
            None => break,
            Some(b'.') => {
                if !matches!(token.get(index + 1), Some(byte) if byte.is_ascii_digit()) {
                    return None;
                }
                index += 1;
            }
            Some(b'-') | Some(b'+') | Some(b'_') => {
                index += 1;
                validate_version_suffix(token.get(index..)?)?;
                break;
            }
            Some(_) => return None,
        }
    }
    let leading = *components.first()?;
    if leading == 1 && components.len() > 1 {
        // Legacy `1.<major>...`: the release is the number after the leading one.
        components.get(1).copied()
    } else {
        Some(leading)
    }
}

/// One run of one to six ASCII digits, and the index just past it.
fn version_component(token: &[u8], start: usize) -> Option<(u32, usize)> {
    let mut value: u32 = 0;
    let mut index = start;
    let mut digits = 0usize;
    while let Some(byte) = token.get(index) {
        if !byte.is_ascii_digit() {
            break;
        }
        if digits >= 6 {
            return None;
        }
        let digit = u32::from(byte.checked_sub(b'0')?);
        value = value.checked_mul(10)?.checked_add(digit)?;
        digits += 1;
        index += 1;
    }
    if digits == 0 { None } else { Some((value, index)) }
}

/// One or more non-empty ASCII alphanumeric segments joined by `-`, `+` or `_`.
///
/// At most sixteen bytes may sit in one segment. Nothing else is allowed, so control characters,
/// spaces, non-ASCII bytes and trailing separators make the whole token malformed.
fn validate_version_suffix(suffix: &[u8]) -> Option<()> {
    if suffix.is_empty() {
        return None;
    }
    let mut segment_len = 0usize;
    for byte in suffix {
        if byte.is_ascii_alphanumeric() {
            segment_len = segment_len.checked_add(1)?;
            if segment_len > 16 {
                return None;
            }
        } else if matches!(byte, b'-' | b'+' | b'_') {
            if segment_len == 0 {
                return None;
            }
            segment_len = 0;
        } else {
            return None;
        }
    }
    if segment_len == 0 { None } else { Some(()) }
}

/// One discovery pass over the given inputs.
#[cfg(test)]
pub(crate) fn discover_with(inputs: &Inputs) -> Discovery {
    discover_control(inputs, None)
}

/// One discovery pass that stops as soon as `stop` is set.
pub(crate) fn discover_with_control(inputs: &Inputs, stop: &AtomicBool) -> Discovery {
    discover_control(inputs, Some(stop))
}

/// The pass itself: bounded candidate plan, bounded probes in order, and a stop checked before the
/// search, before each candidate and by each probe while it runs.
fn discover_control(inputs: &Inputs, stop: Option<&AtomicBool>) -> Discovery {
    let mut too_old = false;
    let mut probed = false;
    let mut cancelled = stopped(stop);
    if !cancelled {
        for candidate in plan(inputs) {
            // Between candidates: a cancelled pass starts nothing further, so the candidate after the
            // one being probed at the moment of the stop is never reached. This is the first of two
            // checks — each probe also refuses to start a child once the flag is set.
            if stopped(stop) {
                cancelled = true;
                break;
            }
            probed = true;
            let outcome = match stop {
                Some(stop) => probe_guarded(&candidate, PROBE_TIMEOUT, stop),
                None => probe(&candidate),
            };
            match outcome {
                ProbeOutcome::Version { major } if major >= MINIMUM_JAVA => {
                    return Discovery { executable: Some(candidate), status: Java::Available };
                }
                ProbeOutcome::Version { .. } => too_old = true,
                ProbeOutcome::Stopped => {
                    cancelled = true;
                    break;
                }
                ProbeOutcome::Malformed
                | ProbeOutcome::SpawnFailed
                | ProbeOutcome::TimedOut
                | ProbeOutcome::Failed
                | ProbeOutcome::Busy
                | ProbeOutcome::Refused => {}
            }
        }
    }
    let status = if too_old {
        Java::TooOld
    } else if cancelled || probed {
        // Either the search was stopped before it found a usable launcher, or a launcher exists and
        // none answered usefully: neither may be reported as Missing, which claims there is none.
        Java::Unknown
    } else {
        Java::Missing
    };
    Discovery { executable: None, status }
}
