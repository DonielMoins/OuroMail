// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Tests for bounded Java 17+ discovery.
//!
//! Every probe here starts a real child process over real pipes. The "Java" is a synthetic fixture:
//! an executable file whose shebang is its own business and whose body prints the exact bytes under
//! test. `proof_java` itself starts that file directly with `-version`; no shell is involved on the
//! production side, so the reply shapes, the oversized reply, the hung reply and the descendant that
//! keeps the inherited pipes open are all deterministic. Fixture tests are `unix`-only; the parser,
//! planner, environment and redaction tests run everywhere.
//!
//! Nothing here downloads anything, writes outside a temporary directory, or starts a LanguageTool
//! server: the only process that ever runs a JVM is whichever `java` the fixture points at.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use mailcraft_ui_egui::proof_server::Java;

use super::proof_java::{
    Discovery, Inputs, MAX_CANDIDATES, MAX_DIRECTORY_ENTRIES, MAX_DIRECTORY_READ, MAX_OUTSTANDING_READERS, MAX_PATH_ENTRIES, MAX_RETAINED_PER_STREAM,
    MINIMUM_JAVA, PROBE_TIMEOUT, ProbeOutcome, ReadStatus, acceptable_directory, acquire_reader_slot, bounded_scan_names, discover,
    discover_cancellable, discover_with, discover_with_control, drain_stream, inputs_from, outstanding_reader_slots, parse_major, plan, probe,
    probe_guarded, probe_within, scan_root,
};
use super::proof_supervisor::environment;

// ---------------------------------------------------------------------------------------------
// Temporary directories (tests only)
// ---------------------------------------------------------------------------------------------

static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("mailcraft-proof-java-{}-{}-{}", std::process::id(), NEXT_TEMP.fetch_add(1, Ordering::Relaxed), label));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Fixture tests hold this while they write an executable file and start it.
///
/// Another thread's `fork` inherits a file descriptor that is still open for writing, and the kernel
/// then refuses to execute that same file (`ETXTBSY`). That is a property of this test binary, which
/// both writes and spawns from many threads at once; production never writes an executable, so the
/// production code has no retry and this lock exists only to keep the fixtures deterministic.
static FIXTURE_LOCK: Mutex<()> = Mutex::new(());

fn fixture_guard() -> MutexGuard<'static, ()> {
    FIXTURE_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------------------------------
// Synthetic java fixtures
// ---------------------------------------------------------------------------------------------

/// What a modern JDK (`java -version` on stdout) writes.
#[cfg(unix)]
const MODERN: &str = r#"printf 'openjdk version "21.0.1" 2023-10-17\nOpenJDK Runtime Environment (build 21.0.1+12-29)\n'"#;
/// What a JDK 8 writes (the reply goes to stderr).
#[cfg(unix)]
const LEGACY: &str = r#"printf 'java version "1.8.0_392"\nOpenJDK Runtime Environment (build 1.8.0_392-8u392-ga-1~22.04-b08)\n' >&2"#;

/// Writes an executable script at `path`, creating its parent, and returns the path.
#[cfg(unix)]
fn executable_at(path: &Path, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    path.to_path_buf()
}

/// A directory that looks like a `JAVA_HOME`: `<root>/bin/java` answers with `body`.
#[cfg(unix)]
fn java_home_with(root: &Path, name: &str, body: &str) -> PathBuf {
    let home = root.join(name);
    executable_at(&home.join("bin").join("java"), body);
    home
}

/// A plain directory whose `java` answers with `body`.
#[cfg(unix)]
fn directory_with_java(root: &Path, name: &str, body: &str) -> PathBuf {
    let directory = root.join(name);
    executable_at(&directory.join("java"), body);
    directory
}

#[cfg(unix)]
fn only_home(home: Option<PathBuf>) -> Inputs {
    Inputs { java_home: home, path_directories: Vec::new(), standard_directories: Vec::new() }
}

// ---------------------------------------------------------------------------------------------
// Bounds and the bounded version parser (no process involved)
// ---------------------------------------------------------------------------------------------

#[test]
fn the_documented_bounds_are_the_ones_used() {
    assert_eq!(MINIMUM_JAVA, 17);
    assert_eq!(PROBE_TIMEOUT, Duration::from_secs(2));
    const { assert!(MAX_RETAINED_PER_STREAM * 2 <= 4096) };
    const { assert!(MAX_CANDIDATES >= 1) };
    const { assert!(MAX_PATH_ENTRIES >= 1) };
    const { assert!(MAX_DIRECTORY_ENTRIES <= MAX_PATH_ENTRIES) };
}

#[test]
fn parse_reads_modern_and_legacy_version_lines() {
    assert_eq!(parse_major(b"openjdk version \"21.0.1\" 2023-10-17\n"), Some(21));
    assert_eq!(parse_major(b"openjdk version \"17\" 2021-09-14\n"), Some(17));
    assert_eq!(parse_major(b"openjdk version \"17-ea\" 2021-09-14\n"), Some(17));
    assert_eq!(parse_major(b"openjdk version \"17+35\"\n"), Some(17));
    assert_eq!(parse_major(b"java version \"1.8.0_392\"\nOpenJDK Runtime Environment\n"), Some(8));
    assert_eq!(parse_major(b"java version \"1.0.2\"\n"), Some(0));
    assert_eq!(parse_major(b"java version \"1.7.0_80\"\n"), Some(7));
    assert_eq!(parse_major(b"openjdk version \"9.0.4\"\n"), Some(9));
    assert_eq!(parse_major(b"openjdk version \"27.0.1.0.3\" 2026-10-24\n"), Some(27));
}

#[test]
fn parse_accepts_complete_well_formed_version_tokens() {
    for (token, major) in [
        (b"17-ea".as_slice(), 17),
        (b"17+35".as_slice(), 17),
        (b"17.0.1-rc1".as_slice(), 17),
        (b"1.8.0_392".as_slice(), 8),
        (b"27.0.1.0.3".as_slice(), 27),
        (b"17.0.1+12".as_slice(), 17),
        (b"21-ea+35".as_slice(), 21),
    ] {
        let reply = [b"openjdk version \"".as_slice(), token, b"\" 2026-11-12\n".as_slice()].concat();
        assert_eq!(parse_major(&reply), Some(major), "a well-formed token was refused: {:?}", String::from_utf8_lossy(token));
    }
}

#[test]
fn parse_refuses_a_version_token_with_a_malformed_suffix() {
    for token in [
        b"17garbage".as_slice(),
        b"17.0.1garbage".as_slice(),
        b"17.0.1-".as_slice(),
        b"17.0.1+".as_slice(),
        b"17.0.1_".as_slice(),
        b"17..0".as_slice(),
        b"17.".as_slice(),
        b"17.0.1 rc1".as_slice(),
        b"17.0.1-rc 1".as_slice(),
        b"17\x017".as_slice(),
        b"17.0.1-\x07".as_slice(),
        b"17.0.1\xff".as_slice(),
    ] {
        let reply = [b"openjdk version \"".as_slice(), token, b"\"\n".as_slice()].concat();
        assert_eq!(parse_major(&reply), None, "a malformed token was accepted: {:?}", String::from_utf8_lossy(token));
    }
}

#[test]
fn parse_fails_closed_on_malformed_replies() {
    assert_eq!(parse_major(b""), None);
    assert_eq!(parse_major(b"openjdk version 21.0.1\n"), None, "an unquoted version must not be guessed at");
    assert_eq!(parse_major(b"openjdk version \"21.0.1\n"), None, "an unterminated quote must be refused");
    assert_eq!(parse_major(b"openjdk version \"\"\n"), None);
    assert_eq!(parse_major(b"openjdk version \"snapshot\"\n"), None);
    assert_eq!(parse_major(b"openjdk version \".17\"\n"), None);
    assert_eq!(parse_major(b"openjdk version \"17.0.1-rc1\" trailing \"9.0\"\n"), Some(17));
    let oversized = [b"openjdk version \"".as_slice(), &[b'9'; 200], b"\"".as_slice()].concat();
    assert_eq!(parse_major(&oversized), None, "a version token beyond the bound must be refused");
    assert_eq!(parse_major(&[0xff, 0xfe, b'v', b'e', b'r', b's', b'i', b'o', b'n', b' ']), None);
}

#[test]
fn drain_stream_reports_a_read_error_instead_of_discarding_it() {
    struct FailsAfterOneRead {
        sent: bool,
    }
    impl Read for FailsAfterOneRead {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.sent {
                return Err(io::Error::other("synthetic read failure"));
            }
            self.sent = true;
            let reply = b"openjdk version \"21.0.1\"\n";
            if let Some(target) = buffer.get_mut(..reply.len()) {
                target.copy_from_slice(reply);
            }
            Ok(reply.len())
        }
    }
    let (bytes, status) = drain_stream(FailsAfterOneRead { sent: false });
    assert_eq!(bytes, b"openjdk version \"21.0.1\"\n");
    assert_eq!(status, ReadStatus::Errored, "a failed read was reported as a complete stream");
}

#[test]
fn drain_stream_marks_output_beyond_the_bound_as_overflowed() {
    struct Finite {
        remaining: usize,
    }
    impl Read for Finite {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.remaining == 0 {
                return Ok(0);
            }
            let count = self.remaining.min(buffer.len());
            if let Some(target) = buffer.get_mut(..count) {
                for byte in target.iter_mut() {
                    *byte = b'x';
                }
            }
            self.remaining -= count;
            Ok(count)
        }
    }
    let (bytes, status) = drain_stream(Finite { remaining: MAX_RETAINED_PER_STREAM.saturating_add(1000) });
    assert_eq!(bytes.len(), MAX_RETAINED_PER_STREAM, "only the retained prefix may be kept");
    assert_eq!(status, ReadStatus::Overflowed, "bytes beyond the bound were silently dropped");

    let (bytes, status) = drain_stream(b"openjdk version \"21.0.1\"\n".as_slice());
    assert_eq!(bytes, b"openjdk version \"21.0.1\"\n");
    assert_eq!(status, ReadStatus::Complete);
}

// ---------------------------------------------------------------------------------------------
// Candidate planning
// ---------------------------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn plan_puts_the_java_home_hint_first_then_path_then_standard_locations() {
    let _fixture = fixture_guard();
    let root = TempRoot::new("order");
    let home = java_home_with(root.path(), "hint", MODERN);
    let on_path = directory_with_java(root.path(), "path", MODERN);
    let standard = directory_with_java(root.path(), "standard", MODERN);
    let inputs = Inputs { java_home: Some(home.clone()), path_directories: vec![on_path.clone()], standard_directories: vec![standard.clone()] };
    let candidates = plan(&inputs);
    assert_eq!(candidates, vec![home.join("bin").join("java"), on_path.join("java"), standard.join("java")]);
}

#[cfg(unix)]
#[test]
fn plan_collapses_duplicate_candidates() {
    let _fixture = fixture_guard();
    let root = TempRoot::new("duplicates");
    let home = java_home_with(root.path(), "home", MODERN);
    let inputs = Inputs {
        java_home: Some(home.clone()),
        path_directories: vec![home.join("bin"), home.join("bin")],
        standard_directories: vec![home.join("bin")],
    };
    assert_eq!(plan(&inputs), vec![home.join("bin").join("java")]);
}

#[test]
fn directories_that_mean_here_are_refused() {
    assert!(!acceptable_directory(Path::new("bin")));
    assert!(!acceptable_directory(Path::new("./bin")));
    assert!(!acceptable_directory(Path::new(".")));
    assert!(!acceptable_directory(Path::new("")));
    assert!(!acceptable_directory(Path::new("/usr/../bin")));
    let current = std::env::current_dir().unwrap();
    assert!(!acceptable_directory(&current), "the working directory is never searched");
    #[cfg(unix)]
    assert!(acceptable_directory(Path::new("/usr/bin")));
    let root = TempRoot::new("acceptable");
    assert!(acceptable_directory(root.path()));
}

#[cfg(unix)]
#[test]
fn plan_skips_relative_and_current_directory_entries() {
    let _fixture = fixture_guard();
    let root = TempRoot::new("excluded");
    let valid = directory_with_java(root.path(), "valid", MODERN);
    let current = std::env::current_dir().unwrap();
    let inputs = Inputs {
        java_home: None,
        path_directories: vec![PathBuf::from("."), current, PathBuf::from("/tmp/../tmp"), PathBuf::from("valid")],
        standard_directories: vec![valid.clone()],
    };
    assert_eq!(plan(&inputs), vec![valid.join("java")]);
}

#[cfg(unix)]
#[test]
fn plan_ignores_path_entries_beyond_the_bound() {
    let _fixture = fixture_guard();
    let root = TempRoot::new("path-cap");
    let mut entries: Vec<PathBuf> = (0..MAX_PATH_ENTRIES).map(|index| root.path().join(format!("empty-{index}"))).collect();
    // The only launcher sits just past the bound, so it must not be reached.
    entries.push(directory_with_java(root.path(), "beyond", MODERN));
    let inputs = Inputs { java_home: None, path_directories: entries, standard_directories: Vec::new() };
    assert!(plan(&inputs).is_empty());
}

#[cfg(unix)]
#[test]
fn plan_stops_at_the_candidate_bound() {
    let _fixture = fixture_guard();
    let root = TempRoot::new("candidate-cap");
    let directories: Vec<PathBuf> =
        (0..MAX_CANDIDATES.saturating_add(3)).map(|index| directory_with_java(root.path(), &format!("jdk-{index}"), MODERN)).collect();
    let inputs = Inputs { java_home: None, path_directories: directories, standard_directories: Vec::new() };
    assert_eq!(plan(&inputs).len(), MAX_CANDIDATES);
}

#[cfg(unix)]
#[test]
fn plan_skips_plain_files_without_execute_permission_and_directories_named_java() {
    let _fixture = fixture_guard();
    use std::os::unix::fs::PermissionsExt;
    let root = TempRoot::new("not-executable");
    let plain = root.path().join("plain");
    fs::create_dir_all(&plain).unwrap();
    let file = plain.join("java");
    fs::write(&file, "not really java").unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    let nested = root.path().join("nested");
    fs::create_dir_all(nested.join("java")).unwrap();
    let valid = directory_with_java(root.path(), "valid", MODERN);

    let inputs = Inputs { java_home: None, path_directories: vec![plain, nested, valid.clone()], standard_directories: Vec::new() };
    assert_eq!(plan(&inputs), vec![valid.join("java")]);
}

#[cfg(unix)]
#[test]
fn scan_root_is_sorted_absolute_and_bounded() {
    let _fixture = fixture_guard();
    let root = TempRoot::new("scan");
    let jvm = root.path().join("jvm");
    fs::create_dir_all(&jvm).unwrap();
    for index in 0..MAX_DIRECTORY_ENTRIES.saturating_add(4) {
        fs::create_dir_all(jvm.join(format!("jdk-{index:02}"))).unwrap();
    }
    let scanned = scan_root(&jvm, Path::new("bin"));
    assert_eq!(scanned.len(), MAX_DIRECTORY_ENTRIES);
    let mut sorted = scanned.clone();
    sorted.sort();
    assert_eq!(scanned, sorted, "a scan is reproducible");
    assert!(scanned.iter().all(|path| path.is_absolute() && path.ends_with("bin")));
    assert_eq!(scanned.first(), Some(&jvm.join("jdk-00").join("bin")), "a scan starts at the first sorted name");
    assert_eq!(scanned.last(), Some(&jvm.join(format!("jdk-{:02}", MAX_DIRECTORY_ENTRIES - 1)).join("bin")));
    assert!(scan_root(&root.path().join("absent"), Path::new("bin")).is_empty());
}

#[test]
fn path_inputs_count_every_traversed_entry_before_filtering() {
    // The first 32 entries are relative and therefore refused. A usable absolute directory just past
    // the bound must stay unvisited: the bound is on traversal, not on how many survive filtering.
    let entries: Vec<PathBuf> =
        (0..MAX_PATH_ENTRIES).map(|index| PathBuf::from(format!("relative-{index}"))).chain(std::iter::once(PathBuf::from("/usr/bin"))).collect();
    let path = std::env::join_paths(entries.iter()).unwrap();
    let inputs = inputs_from(None, Some(path.as_os_str()), Vec::new());
    assert!(inputs.path_directories.is_empty(), "traversal continued past the bound: {:?}", inputs.path_directories);
}

#[test]
fn path_inputs_accept_usable_entries_inside_the_bound() {
    let root = TempRoot::new("inputs-bound");
    let usable = root.path().to_path_buf();
    let mut entries: Vec<PathBuf> = (0..MAX_PATH_ENTRIES - 1).map(|index| PathBuf::from(format!("relative-{index}"))).collect();
    entries.push(usable.clone());
    let path = std::env::join_paths(entries.iter()).unwrap();
    let inputs = inputs_from(None, Some(path.as_os_str()), Vec::new());
    assert_eq!(inputs.path_directories, vec![usable]);
}

#[test]
fn directory_scan_counts_failed_results_toward_the_read_bound() {
    // 256 results that cannot be named fill the budget on their own: a name after them is never read.
    let results = (0..MAX_DIRECTORY_READ)
        .map(|_| Err(io::Error::new(io::ErrorKind::PermissionDenied, "synthetic failure")))
        .chain(std::iter::once(Ok(OsString::from("jdk-late"))));
    assert!(bounded_scan_names(results).is_empty(), "a failed entry did not consume a read slot");
}

#[test]
fn directory_scan_returns_at_most_the_directory_bound_of_sorted_names() {
    let mut results: Vec<io::Result<OsString>> =
        (0..MAX_DIRECTORY_ENTRIES).rev().map(|index| Ok(OsString::from(format!("jdk-{index:02}")))).collect();
    results.push(Err(io::Error::new(io::ErrorKind::PermissionDenied, "synthetic failure")));
    results.push(Ok(OsString::from("jdk-late")));
    let names = bounded_scan_names(results.into_iter());
    assert_eq!(names.len(), MAX_DIRECTORY_ENTRIES);
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "the bounded scan is sorted");
    assert_eq!(names.first(), Some(&OsString::from("jdk-00")));
    assert!(!names.contains(&OsString::from("jdk-late")), "a name past the read bound was used");
}

#[cfg(unix)]
#[test]
fn probe_reads_a_modern_reply_from_stdout() {
    let _fixture = fixture_guard();
    let root = TempRoot::new("probe-modern");
    let java = executable_at(&root.path().join("java"), MODERN);
    assert_eq!(probe(&java), ProbeOutcome::Version { major: 21 });
}

#[cfg(unix)]
#[test]
fn probe_reads_the_legacy_one_dot_eight_reply_from_stderr_as_major_eight() {
    let _fixture = fixture_guard();
    let root = TempRoot::new("probe-legacy");
    let java = executable_at(&root.path().join("java"), LEGACY);
    assert_eq!(probe(&java), ProbeOutcome::Version { major: 8 });
}

#[cfg(unix)]
#[test]
fn probe_accepts_a_bare_and_an_early_access_major() {
    let _fixture = fixture_guard();
    let root = TempRoot::new("probe-shapes");
    let bare = executable_at(&root.path().join("bare").join("java"), r#"printf 'openjdk version "17" 2021-09-14\n'"#);
    let early = executable_at(&root.path().join("early").join("java"), r#"printf 'openjdk version "17-ea" 2021-09-14\n'"#);
    assert_eq!(probe(&bare), ProbeOutcome::Version { major: 17 });
    assert_eq!(probe(&early), ProbeOutcome::Version { major: 17 });
}

#[cfg(unix)]
#[test]
fn probe_fails_closed_on_a_malformed_reply() {
    let _fixture = fixture_guard();
    let root = TempRoot::new("probe-malformed");
    let java = executable_at(&root.path().join("java"), r#"printf 'openjdk version 21.0.1\n'"#);
    assert_eq!(probe(&java), ProbeOutcome::Malformed);
}

#[cfg(unix)]
#[test]
fn probe_fails_closed_on_a_silent_success() {
    let _fixture = fixture_guard();
    let root = TempRoot::new("probe-silent");
    let java = executable_at(&root.path().join("java"), "exit 0");
    assert_eq!(probe(&java), ProbeOutcome::Malformed);
}

#[cfg(unix)]
#[test]
fn probe_fails_closed_on_a_non_zero_exit() {
    let _fixture = fixture_guard();
    let root = TempRoot::new("probe-nonzero");
    let java = executable_at(&root.path().join("java"), r#"printf 'openjdk version "21.0.1"\n'; exit 3"#);
    assert_eq!(probe(&java), ProbeOutcome::Failed);
    assert_eq!(probe_within(&java, Duration::from_millis(500)), ProbeOutcome::Failed);
}

#[test]
fn probe_refuses_a_relative_program_without_starting_anything() {
    assert_eq!(probe(Path::new("java")), ProbeOutcome::Refused);
    assert_eq!(probe(Path::new("")), ProbeOutcome::Refused);
    assert_eq!(probe_within(Path::new("/usr/bin/java"), Duration::ZERO), ProbeOutcome::Refused);
}

#[cfg(unix)]
#[test]
fn probe_stops_reading_at_the_retained_bound_without_blocking_the_child() {
    let _fixture = fixture_guard();
    let root = TempRoot::new("probe-huge");
    // 64 KiB first: the version line that follows is past the retained bound and must not be parsed.
    let java = executable_at(&root.path().join("java"), r#"printf '%65536s' '' | tr ' ' x; printf '\nopenjdk version "21.0.1"\n'"#);
    let started = Instant::now();
    assert_eq!(probe(&java), ProbeOutcome::Malformed);
    assert!(started.elapsed() < Duration::from_secs(2), "a chatty child must not stall the probe");
}

#[cfg(unix)]
#[test]
fn probe_rejects_a_version_line_followed_by_an_overflow_on_stdout() {
    let _fixture = fixture_guard();
    let root = TempRoot::new("probe-huge-after-stdout");
    // The version line is complete and correct, but the stream as a whole was not: more arrived than
    // the retained bound, so the reply is not one the probe can trust or parse in full.
    let java = executable_at(&root.path().join("java"), r#"printf 'openjdk version "21.0.1"\n'; printf '%65536s' '' | tr ' ' x"#);
    assert_eq!(probe(&java), ProbeOutcome::Malformed);
}

#[cfg(unix)]
#[test]
fn probe_rejects_a_version_line_followed_by_an_overflow_on_stderr() {
    let _fixture = fixture_guard();
    let root = TempRoot::new("probe-huge-after-stderr");
    let java = executable_at(&root.path().join("java"), r#"printf 'openjdk version "21.0.1"\n'; printf '%65536s' '' | tr ' ' x >&2"#);
    assert_eq!(probe(&java), ProbeOutcome::Malformed);
}

#[cfg(unix)]
#[test]
fn probe_times_out_at_its_bound_and_reaps_the_owned_child() {
    let _fixture = fixture_guard();
    let root = TempRoot::new("probe-hang");
    let pidfile = root.path().join("java.pid");
    let java = executable_at(&root.path().join("java"), &format!("echo $$ > '{}'; exec sleep 60", pidfile.display()));
    let started = Instant::now();
    assert_eq!(probe(&java), ProbeOutcome::TimedOut);
    let elapsed = started.elapsed();
    assert!(elapsed >= PROBE_TIMEOUT, "the probe must give the child its full bound: {elapsed:?}");
    assert!(elapsed < PROBE_TIMEOUT + Duration::from_secs(5), "the probe must not wait indefinitely: {elapsed:?}");

    let pid = fs::read_to_string(&pidfile).unwrap();
    let pid = pid.trim().to_string();
    assert!(!pid.is_empty());
    #[cfg(target_os = "linux")]
    assert!(!Path::new("/proc").join(&pid).exists(), "the killed child was not reaped: pid {pid}");
}

#[cfg(unix)]
#[test]
fn probe_does_not_wait_for_a_descendant_that_holds_the_pipes() {
    let _fixture = fixture_guard();
    let root = TempRoot::new("probe-descendant");
    let java = executable_at(&root.path().join("java"), "sleep 3 & exec sleep 60");
    let started = Instant::now();
    // A short bound: the killed child's grandchild keeps stdout open for seconds afterwards.
    assert_eq!(probe_within(&java, Duration::from_millis(400)), ProbeOutcome::TimedOut);
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_millis(2500), "the probe waited for a descendant: {elapsed:?}");
}

#[cfg(unix)]
#[test]
fn probe_collection_shares_the_single_timeout_deadline() {
    let _fixture = fixture_guard();
    let root = TempRoot::new("probe-single-deadline");
    // The child answers at once and exits, but a descendant keeps stdout open. The probe has to wait
    // for the stream until its one deadline and then stop: the old code added a fixed 250 ms read
    // grace after the deadline, so the whole probe outlived its documented bound.
    let java = executable_at(&root.path().join("java"), r#"sleep 5 & printf 'openjdk version "21.0.1"\n'; exit 0"#);
    let timeout = Duration::from_millis(600);
    let started = Instant::now();
    assert_eq!(probe_within(&java, timeout), ProbeOutcome::Failed);
    let elapsed = started.elapsed();
    assert!(elapsed >= timeout, "the probe returned before its deadline: {elapsed:?}");
    assert!(elapsed < timeout + Duration::from_millis(150), "the probe added a read grace after its deadline: {elapsed:?} for a {timeout:?} bound");
}

#[cfg(unix)]
#[test]
fn saturated_reader_admission_refuses_before_spawning_and_recovers() {
    let _fixture = fixture_guard();
    let root = TempRoot::new("admission-saturated");
    let pidfile = root.path().join("java.pid");
    // This candidate would leave a marker if it were ever started.
    let java = executable_at(&root.path().join("java"), &format!("echo $$ > '{}'; printf 'openjdk version \"21.0.1\"\\n'", pidfile.display()));
    let mut held = Vec::new();
    while let Some(slot) = acquire_reader_slot() {
        held.push(slot);
    }
    assert!(held.len() <= MAX_OUTSTANDING_READERS, "more permits were issued than the bound allows");
    assert_eq!(outstanding_reader_slots(), MAX_OUTSTANDING_READERS, "the pool was over-subscribed");
    assert_eq!(probe(&java), ProbeOutcome::Busy);
    assert!(!pidfile.exists(), "a refused probe started the candidate anyway");
    drop(held);
    assert_eq!(probe(&java), ProbeOutcome::Version { major: 21 }, "capacity did not come back when permits were released");
}

#[cfg(unix)]
#[test]
fn a_blocked_reader_holds_its_permit_until_the_pipe_closes() {
    let _fixture = fixture_guard();
    let root = TempRoot::new("admission-release");
    let pidfile = root.path().join("child.pid");
    // The fixture's own descendant inherits stdout and keeps it open after the child exits.
    let java = executable_at(&root.path().join("java"), &format!("sleep 60 & echo $! > '{}'; exit 0", pidfile.display()));
    let before = outstanding_reader_slots();
    assert_eq!(probe_within(&java, Duration::from_millis(300)), ProbeOutcome::Failed);
    assert!(outstanding_reader_slots() > before, "a reader blocked on the inherited pipe did not hold its permit");

    // Clean up the fixture-owned descendant, exactly as the task allows. Its pipe then closes and the
    // blocked reader (and its permit) must come back without any join.
    let pid = fs::read_to_string(&pidfile).unwrap();
    let pid = pid.trim().to_string();
    assert!(!pid.is_empty());
    let _ = Command::new("kill").arg("-KILL").arg(&pid).status();
    let deadline = Instant::now() + Duration::from_secs(5);
    while outstanding_reader_slots() > before && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(outstanding_reader_slots() <= before, "the permit was not released when the pipe closed");
}

#[test]
fn probe_fails_closed_when_the_program_cannot_be_started() {
    let _fixture = fixture_guard();
    let root = TempRoot::new("probe-absent");
    assert_eq!(probe(&root.path().join("missing").join("java")), ProbeOutcome::SpawnFailed);
}

// ---------------------------------------------------------------------------------------------
// The shared sanitized environment
// ---------------------------------------------------------------------------------------------

#[test]
fn environment_keeps_only_the_whitelist_and_removes_jvm_injection() {
    let mut command = Command::new("/nonexistent/program");
    command.env("JAVA_TOOL_OPTIONS", "-javaagent:/tmp/agent.jar");
    command.env("_JAVA_OPTIONS", "-Xmx8g");
    command.env("JDK_JAVA_OPTIONS", "-Dsun.java.command=x");
    command.env("JAVA_OPTIONS", "-Xms1g");
    command.env("CLASSPATH", "/tmp/hostile.jar");
    command.env("MAILCRAFT_HOSTILE_MARKER", "1");
    environment(&mut command);

    let seen: Vec<String> = command.get_envs().map(|(key, _)| key.to_string_lossy().into_owned()).collect();
    for injection in ["JAVA_TOOL_OPTIONS", "_JAVA_OPTIONS", "JDK_JAVA_OPTIONS", "JAVA_OPTIONS", "CLASSPATH"] {
        assert!(!seen.iter().any(|key| key == injection), "{injection} survived the whitelist: {seen:?}");
    }
    assert!(!seen.iter().any(|key| key == "MAILCRAFT_HOSTILE_MARKER"), "an unrelated variable survived: {seen:?}");
    assert!(!seen.iter().any(|key| key == "TMPDIR"), "the shared helper does not choose a temporary directory");

    #[cfg(unix)]
    let allowed: &[&str] = &["PATH", "LANG", "LC_ALL", "LC_CTYPE", "TZ"];
    #[cfg(windows)]
    let allowed: &[&str] = &["PATH", "PATHEXT", "SystemRoot", "SystemDrive", "WINDIR", "COMSPEC"];
    for key in &seen {
        assert!(allowed.contains(&key.as_str()), "unexpected variable in the sanitized environment: {key}");
    }
    #[cfg(unix)]
    assert!(seen.iter().any(|key| key == "PATH"), "the whitelist must keep PATH");
}

#[cfg(unix)]
#[test]
fn the_probed_child_sees_only_the_sanitized_environment() {
    let _fixture = fixture_guard();
    let root = TempRoot::new("probe-env");
    let dump = root.path().join("env.txt");
    let java = executable_at(&root.path().join("java"), &format!(r#"env > '{}'; printf 'openjdk version "21.0.1"\n'"#, dump.display()));
    assert_eq!(probe(&java), ProbeOutcome::Version { major: 21 });
    let text = fs::read_to_string(&dump).unwrap();
    let keys: Vec<&str> = text.lines().filter_map(|line| line.split_once('=').map(|(key, _)| key)).collect();
    for forbidden in [
        "JAVA_TOOL_OPTIONS",
        "_JAVA_OPTIONS",
        "JDK_JAVA_OPTIONS",
        "JAVA_OPTIONS",
        "CLASSPATH",
        "HOME",
        "USER",
        "TMPDIR",
        "TEMP",
        "TMP",
        "MAILCRAFT_HOSTILE_MARKER",
    ] {
        assert!(!keys.contains(&forbidden), "{forbidden} reached the probe: {keys:?}");
    }
    assert!(keys.contains(&"PATH"), "the whitelisted PATH did not reach the probe: {keys:?}");
}

// ---------------------------------------------------------------------------------------------
// Discovery results
// ---------------------------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn discover_reports_available_with_the_launcher_it_probed() {
    let _fixture = fixture_guard();
    let root = TempRoot::new("discover-available");
    let home = java_home_with(root.path(), "jdk", MODERN);
    let discovery = discover_with(&only_home(Some(home.clone())));
    assert_eq!(discovery.status, Java::Available);
    assert_eq!(discovery.executable, Some(home.join("bin").join("java")));
}

/// The host entry point itself: no injected inputs, the real `JAVA_HOME`, `PATH` and platform
/// locations of this machine, with the invariant that a path is only ever handed back when it is
/// the launcher that answered.
#[test]
fn discover_on_this_host_reports_a_status_and_only_a_usable_path() {
    let _fixture = fixture_guard();
    let discovery = discover();
    match discovery.status {
        Java::Available => {
            let executable = discovery.executable.as_ref().expect("an available Java must name its launcher");
            assert!(executable.is_absolute(), "a launcher must be an absolute path");
            assert!(executable.is_file());
        }
        Java::TooOld | Java::Missing | Java::Unknown => {
            assert_eq!(discovery.executable, None, "only an available Java may name a launcher");
        }
    }
}

#[test]
fn discover_reports_missing_when_no_launcher_exists() {
    let discovery = discover_with(&Inputs::default());
    assert_eq!(discovery.status, Java::Missing);
    assert_eq!(discovery.executable, None);
}

#[cfg(unix)]
#[test]
fn discover_reports_too_old_without_handing_back_a_path() {
    let _fixture = fixture_guard();
    let root = TempRoot::new("discover-old");
    let home = java_home_with(root.path(), "jdk8", LEGACY);
    let discovery = discover_with(&only_home(Some(home)));
    assert_eq!(discovery.status, Java::TooOld);
    assert_eq!(discovery.executable, None, "a launcher that cannot run the server is never returned");
}

#[cfg(unix)]
#[test]
fn discover_continues_past_an_old_launcher_to_a_usable_one() {
    let _fixture = fixture_guard();
    let root = TempRoot::new("discover-continue");
    let old = directory_with_java(root.path(), "old", LEGACY);
    let usable = directory_with_java(root.path(), "usable", MODERN);
    let inputs = Inputs { java_home: None, path_directories: vec![old, usable.clone()], standard_directories: Vec::new() };
    let discovery = discover_with(&inputs);
    assert_eq!(discovery.status, Java::Available);
    assert_eq!(discovery.executable, Some(usable.join("java")));
}

#[cfg(unix)]
#[test]
fn discover_reports_unknown_when_a_launcher_does_not_answer_usefully() {
    let _fixture = fixture_guard();
    let root = TempRoot::new("discover-unknown");
    let malformed = java_home_with(root.path(), "odd", r#"printf 'openjdk version 21.0.1\n'"#);
    let discovery = discover_with(&only_home(Some(malformed)));
    assert_eq!(discovery.status, Java::Unknown);
    assert_eq!(discovery.executable, None);
}

#[cfg(unix)]
#[test]
fn discover_reports_unknown_when_a_launcher_cannot_start() {
    let _fixture = fixture_guard();
    let root = TempRoot::new("discover-unstartable");
    let home = root.path().join("broken");
    executable_at(&home.join("bin").join("java"), "exit 0");
    // Replace the body so the shebang names an interpreter that does not exist.
    fs::write(home.join("bin").join("java"), "#!/nonexistent/interpreter\n").unwrap();
    let discovery = discover_with(&only_home(Some(home)));
    assert_eq!(discovery.status, Java::Unknown);
    assert_eq!(discovery.executable, None);
}

#[test]
fn discovery_debug_is_redacted() {
    let discovery = Discovery { executable: Some(PathBuf::from("/home/someone/jdk-21/bin/java")), status: Java::Available };
    let text = format!("{discovery:?}");
    assert!(text.contains("Available"));
    assert!(text.contains("<redacted>"));
    assert!(!text.contains("jdk-21"), "a path leaked through Debug: {text}");
    assert!(!text.contains("someone"), "a path leaked through Debug: {text}");

    let missing = Discovery { executable: None, status: Java::Missing };
    assert_eq!(format!("{missing:?}"), "Discovery { status: Missing, executable: None }");
}

#[cfg(unix)]
#[test]
fn a_real_java_on_this_host_is_available_or_reported_as_too_old() {
    let _fixture = fixture_guard();
    let candidate = Path::new("/usr/bin/java");
    if !candidate.is_file() {
        return;
    }
    let outcome = probe(candidate);
    let major = match outcome {
        ProbeOutcome::Version { major } => major,
        other => panic!("the real java did not answer with a version: {other:?}"),
    };
    assert!(major >= 1);
    let inputs = Inputs { java_home: None, path_directories: Vec::new(), standard_directories: vec![PathBuf::from("/usr/bin")] };
    let discovery = discover_with(&inputs);
    if major >= MINIMUM_JAVA {
        assert_eq!(discovery.status, Java::Available);
        assert_eq!(discovery.executable.as_deref(), Some(candidate));
    } else {
        assert_eq!(discovery.status, Java::TooOld);
        assert_eq!(discovery.executable, None);
    }
}

// ---------------------------------------------------------------------------------------------
// Cancellation: the owner's stop reaches the owned child, and nothing else
// ---------------------------------------------------------------------------------------------
//
// These tests drive the guarded entry points directly with a flag they set themselves. They cover
// the flag-to-owned-child behaviour only: the flag belongs to the host's helper, and nothing here
// starts a helper, so application lifetime (an abrupt parent exit killing a probe) is NOT tested
// by this file.

/// Waits, bounded, for a fixture to create `path`; used to know the owned child is really running
/// before the test cancels the probe that started it.
#[cfg(unix)]
fn wait_for_path(path: &Path, within: Duration) -> bool {
    let deadline = Instant::now().checked_add(within);
    let deadline = match deadline {
        Some(deadline) => deadline,
        None => return path.exists(),
    };
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    path.exists()
}

/// The pid a fixture wrote, asserting the file really holds one.
#[cfg(unix)]
fn written_pid(path: &Path) -> String {
    let text = fs::read_to_string(path).unwrap();
    let pid = text.trim().to_string();
    assert!(!pid.is_empty(), "the fixture did not write its pid");
    pid
}

#[cfg(target_os = "linux")]
fn is_reaped(pid: &str) -> bool {
    !Path::new("/proc").join(pid).exists()
}

#[cfg(unix)]
#[test]
fn a_pass_cancelled_before_it_begins_starts_no_child() {
    let _fixture = fixture_guard();
    let root = TempRoot::new("cancel-before-spawn");
    let probed_marker = root.path().join("probed");
    let java =
        executable_at(&root.path().join("java"), &format!("echo probed > '{}'; printf 'openjdk version \"21.0.1\"\\n'", probed_marker.display()));
    let searched_marker = root.path().join("searched");
    let home =
        java_home_with(root.path(), "jdk", &format!("echo searched > '{}'; printf 'openjdk version \"21.0.1\"\\n'", searched_marker.display()));
    let stop = AtomicBool::new(true);

    let started = Instant::now();
    assert_eq!(probe_guarded(&java, PROBE_TIMEOUT, &stop), ProbeOutcome::Stopped, "a cancelled probe must report that it was stopped");
    assert!(started.elapsed() < Duration::from_millis(500), "a cancelled probe waited: {:?}", started.elapsed());
    // The marker alone cannot prove nothing was started: a child killed the instant it is spawned may
    // not have run its first command yet. A candidate that cannot be spawned is deterministic instead:
    // reaching the spawn call at all would report SpawnFailed, so Stopped proves the flag is read
    // before any child is created.
    let absent = root.path().join("absent").join("java");
    assert_eq!(probe_guarded(&absent, PROBE_TIMEOUT, &stop), ProbeOutcome::Stopped, "a cancelled probe reached the spawn call");
    assert!(!probed_marker.exists(), "a cancelled probe started the candidate anyway");

    // A whole pass, cancelled before it begins, reads nothing into a child and never claims a
    // launcher it did not verify.
    let discovery = discover_with_control(&only_home(Some(home)), &stop);
    assert_eq!(discovery.status, Java::Unknown);
    assert_eq!(discovery.executable, None, "a cancelled pass handed back a launcher");
    assert!(!searched_marker.exists(), "a cancelled pass started a candidate");

    // The host entry point takes the same flag: cancelled before the search, it starts nothing and
    // reports no launcher, whatever this machine happens to have installed.
    let host = discover_cancellable(&stop);
    assert_eq!(host.executable, None);
    assert_ne!(host.status, Java::Available);
}

#[cfg(unix)]
#[test]
fn a_stop_during_a_probe_kills_and_reaps_the_owned_child() {
    let _fixture = fixture_guard();
    let root = TempRoot::new("cancel-mid-probe");
    let pidfile = root.path().join("java.pid");
    let java = executable_at(&root.path().join("java"), &format!("echo $$ > '{}'; exec sleep 60", pidfile.display()));
    let stop = AtomicBool::new(false);

    let (outcome, elapsed) = std::thread::scope(|scope| {
        let handle = scope.spawn(|| {
            let started = Instant::now();
            (probe_guarded(&java, PROBE_TIMEOUT, &stop), started.elapsed())
        });
        assert!(wait_for_path(&pidfile, Duration::from_secs(5)), "the fixture child never started");
        stop.store(true, Ordering::SeqCst);
        handle.join().unwrap()
    });

    assert_eq!(outcome, ProbeOutcome::Stopped, "a stopped probe did not say so");
    assert!(elapsed < PROBE_TIMEOUT, "the stop was only noticed at the deadline: {elapsed:?}");
    #[cfg(target_os = "linux")]
    assert!(is_reaped(&written_pid(&pidfile)), "the stopped probe did not reap its own child");
}

#[cfg(unix)]
#[test]
fn a_stop_ends_the_pass_before_the_next_candidate() {
    let _fixture = fixture_guard();
    let root = TempRoot::new("cancel-between-candidates");
    let first_pid = root.path().join("first.pid");
    let first = directory_with_java(root.path(), "first", &format!("echo $$ > '{}'; exec sleep 60", first_pid.display()));
    let second_marker = root.path().join("second-started");
    let second =
        directory_with_java(root.path(), "second", &format!("echo started > '{}'; printf 'openjdk version \"21.0.1\"\\n'", second_marker.display()));
    let inputs = Inputs { java_home: None, path_directories: vec![first, second], standard_directories: Vec::new() };
    let stop = AtomicBool::new(false);

    let (discovery, elapsed) = std::thread::scope(|scope| {
        let handle = scope.spawn(|| {
            let started = Instant::now();
            (discover_with_control(&inputs, &stop), started.elapsed())
        });
        assert!(wait_for_path(&first_pid, Duration::from_secs(5)), "the first candidate never started");
        stop.store(true, Ordering::SeqCst);
        handle.join().unwrap()
    });

    assert_eq!(discovery.executable, None);
    assert_eq!(discovery.status, Java::Unknown, "a cancelled pass claimed a version it did not see");
    assert!(elapsed < PROBE_TIMEOUT, "the pass spent another full probe bound after the stop: {elapsed:?}");
    // Corroboration only: a child killed the moment it is spawned may not have run its first command,
    // so this cannot prove that a later candidate was never attempted; the probe-level test proves
    // that deterministically with a candidate that could not be spawned at all.
    assert!(!second_marker.exists(), "a candidate after the cancelled one ran");
    #[cfg(target_os = "linux")]
    assert!(is_reaped(&written_pid(&first_pid)), "the cancelled candidate's child was not reaped");
}

#[cfg(unix)]
#[test]
fn a_stop_during_reply_collection_returns_before_the_deadline() {
    let _fixture = fixture_guard();
    let root = TempRoot::new("cancel-during-collection");
    let descendant = root.path().join("descendant.pid");
    // The child answers at once and exits; its descendant keeps stdout open, so the probe is in the
    // reply-collection phase when the stop arrives. One wait is never longer than a poll, so the
    // stop must be seen there instead of at the deadline.
    let java = executable_at(
        &root.path().join("java"),
        &format!("sleep 60 & echo $! > '{}'; printf 'openjdk version \"21.0.1\"\\n'; exit 0", descendant.display()),
    );
    let timeout = PROBE_TIMEOUT;
    let stop = AtomicBool::new(false);
    let before = outstanding_reader_slots();

    let (outcome, elapsed) = std::thread::scope(|scope| {
        let handle = scope.spawn(|| {
            let started = Instant::now();
            (probe_guarded(&java, timeout, &stop), started.elapsed())
        });
        // Long enough that the child has certainly exited and its reply was read.
        std::thread::sleep(Duration::from_millis(200));
        stop.store(true, Ordering::SeqCst);
        handle.join().unwrap()
    });

    assert_eq!(outcome, ProbeOutcome::Stopped, "a stop during reply collection was not honoured");
    assert!(elapsed >= Duration::from_millis(200));
    assert!(elapsed < Duration::from_millis(900), "the stop was only noticed at the deadline: {elapsed:?}");

    // Clean up the fixture-owned descendant, then prove the readers and their permits came back
    // once the inherited pipe closed: cancellation may not cost reader capacity.
    let _ = Command::new("kill").arg("-KILL").arg(written_pid(&descendant)).status();
    let deadline = Instant::now() + Duration::from_secs(5);
    while outstanding_reader_slots() > before && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(outstanding_reader_slots() <= before, "reader capacity was not recovered when the pipe closed");
}

#[cfg(unix)]
#[test]
fn a_cancelled_probe_returns_its_reader_capacity() {
    let _fixture = fixture_guard();
    let root = TempRoot::new("cancel-capacity");
    let pidfile = root.path().join("java.pid");
    // No descendant: killing the owned child closes both pipes, so both permits must come back.
    let java = executable_at(&root.path().join("java"), &format!("echo $$ > '{}'; exec sleep 60", pidfile.display()));
    let stop = AtomicBool::new(false);
    let before = outstanding_reader_slots();

    let outcome = std::thread::scope(|scope| {
        let handle = scope.spawn(|| probe_guarded(&java, PROBE_TIMEOUT, &stop));
        assert!(wait_for_path(&pidfile, Duration::from_secs(5)), "the fixture child never started");
        stop.store(true, Ordering::SeqCst);
        handle.join().unwrap()
    });
    assert_eq!(outcome, ProbeOutcome::Stopped);

    let deadline = Instant::now() + Duration::from_secs(5);
    while outstanding_reader_slots() > before && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(outstanding_reader_slots() <= before, "a stopped probe held reader capacity it no longer needed");

    // Capacity that came back is capacity another probe can use.
    let modern = executable_at(&root.path().join("later").join("java"), MODERN);
    assert_eq!(probe(&modern), ProbeOutcome::Version { major: 21 });
}

#[cfg(unix)]
#[test]
fn the_guarded_probe_keeps_the_documented_deadline() {
    let _fixture = fixture_guard();
    let root = TempRoot::new("cancel-deadline");
    let hang = executable_at(&root.path().join("hang").join("java"), "exec sleep 60");
    let modern = executable_at(&root.path().join("modern").join("java"), MODERN);
    // A flag that is never set changes nothing: the bound is still the whole probe's deadline, and
    // the reply of a candidate that answers in time is still reported.
    let stop = AtomicBool::new(false);
    let timeout = Duration::from_millis(400);
    let started = Instant::now();
    assert_eq!(probe_guarded(&hang, timeout, &stop), ProbeOutcome::TimedOut);
    let elapsed = started.elapsed();
    assert!(elapsed >= timeout, "a guarded probe returned before its deadline: {elapsed:?}");
    assert!(elapsed < timeout + Duration::from_secs(2), "a guarded probe outlived its deadline: {elapsed:?}");
    assert_eq!(probe_guarded(&modern, PROBE_TIMEOUT, &stop), ProbeOutcome::Version { major: 21 });
    assert_eq!(probe_guarded(Path::new("java"), PROBE_TIMEOUT, &stop), ProbeOutcome::Refused);
}
