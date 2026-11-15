// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Tests for the private native proofing helper.
//!
//! Every lifetime pipe below is a real operating-system pipe, every supervisor child is a real
//! `/bin/sh` process, and the Java fixtures are real executables. The helper is started on its own
//! thread with an injected engine (the production entry point's only seam), so the lifetime
//! monitor, the cancellation checks, the exit statuses and the event bytes are exercised exactly
//! as the process would run them. Nothing here contacts a LanguageTool server, binds a fixed port
//! or writes outside a temporary directory.
//!
//! The `unix`-only tests use `/bin/sh` fixtures; the pipe, monitor, status and locate tests run
//! everywhere. Windows child handling remains compile-time guarded and unexercised here.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use mailcraft_net::languagetool_install::{self, InstallError, Installed};
use mailcraft_ui_egui::proof_server::Java;

use super::proof_helper::{Engine, Native, run_with};
use super::proof_java::{self, Discovery, Inputs};
use super::proof_protocol::{EXIT_INTEGRITY, EXIT_INVOCATION, EXIT_JAVA_MISSING, EXIT_JAVA_TOO_OLD, EXIT_NOT_INSTALLED, EXIT_TIMEOUT, Mode};
use super::proof_supervisor::{Config, Failure, supervise};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

// ---------------------------------------------------------------------------------------------
// Temporary directories, real pipes, shared event bytes
// ---------------------------------------------------------------------------------------------

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("mailcraft-proof-helper-{}-{}-{label}", std::process::id(), NEXT_TEMP.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn install(&self) -> PathBuf {
        let directory = self.path.join("install");
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("languagetool-server.jar"), b"test jar").unwrap();
        directory
    }

    fn runtime(&self) -> PathBuf {
        let directory = self.path.join("runtime");
        fs::create_dir_all(&directory).unwrap();
        directory
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[derive(Clone, Default)]
struct SharedEvents(Arc<Mutex<Vec<u8>>>);

impl Write for SharedEvents {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl SharedEvents {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }

    fn is_empty(&self) -> bool {
        self.0.lock().unwrap().is_empty()
    }
}

fn pipe() -> (io::PipeReader, io::PipeWriter) {
    io::pipe().unwrap()
}

fn spawn_run<R, E>(mode: Mode, terminal: bool, lifetime: R, events: SharedEvents, engine: E) -> mpsc::Receiver<i32>
where
    R: Read + Send + 'static,
    E: Engine + Send + 'static,
{
    let (sender, receiver) = mpsc::channel();
    thread::Builder::new()
        .name("mailcraft-proof-helper-test".to_string())
        .spawn(move || {
            let code = run_with(mode, terminal, lifetime, events, &engine);
            let _ = sender.send(code);
        })
        .unwrap();
    receiver
}

fn finish(receiver: mpsc::Receiver<i32>) -> i32 {
    receiver.recv_timeout(Duration::from_secs(30)).expect("the helper did not finish")
}

fn wait_for_stop(stop: &AtomicBool, limit: Duration) -> bool {
    let started = Instant::now();
    while started.elapsed() < limit {
        if stop.load(Ordering::SeqCst) {
            return true;
        }
        thread::sleep(Duration::from_millis(2));
    }
    false
}

fn wait_until(limit: Duration, mut ready: impl FnMut() -> bool) -> bool {
    let started = Instant::now();
    while started.elapsed() < limit {
        if ready() {
            return true;
        }
        thread::sleep(Duration::from_millis(2));
    }
    false
}

#[cfg(unix)]
fn write_executable(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    fs::write(path, body).unwrap();
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

#[cfg(unix)]
fn write_script(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
}

#[cfg(unix)]
fn read_pid(path: &Path) -> Option<u32> {
    let text = fs::read_to_string(path).ok()?;
    text.trim().parse().ok()
}

#[cfg(target_os = "linux")]
fn process_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(all(unix, not(target_os = "linux")))]
fn process_alive(pid: u32) -> bool {
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(unix)]
fn shell_script(prefix: &str, pidfile: &Path) -> String {
    format!("#!/bin/sh\n{prefix}echo $$ > '{}'\nexec sleep 30\n", pidfile.display())
}

// ---------------------------------------------------------------------------------------------
// Engines (test-only implementations of the helper's private seam)
// ---------------------------------------------------------------------------------------------

fn unknown() -> Discovery {
    Discovery { executable: None, status: Java::Unknown }
}

fn available(java: PathBuf) -> Discovery {
    Discovery { executable: Some(java), status: Java::Available }
}

/// A probe engine that answers immediately and counts how it was used.
struct FixedProbe {
    discovery: Discovery,
    discover_calls: AtomicUsize,
}

impl FixedProbe {
    fn new(discovery: Discovery) -> Self {
        Self { discovery, discover_calls: AtomicUsize::new(0) }
    }
}

impl Engine for FixedProbe {
    fn discover(&self, _stop: &AtomicBool) -> Discovery {
        self.discover_calls.fetch_add(1, Ordering::SeqCst);
        self.discovery.clone()
    }

    fn locate(&self, _base: &Path) -> Result<Option<Installed>, InstallError> {
        Ok(None)
    }

    fn verify(&self, _base: &Path, _directory: &Path) -> Result<(), ()> {
        Err(())
    }

    fn supervise<V: FnOnce(&Path) -> Result<(), ()>>(
        &self,
        _java: PathBuf,
        _install: PathBuf,
        _stop: Arc<AtomicBool>,
        _verifier: V,
        _control: Box<dyn Read + Send>,
        _events: &mut dyn Write,
    ) -> Result<(), Failure> {
        Err(Failure::SpawnFailed)
    }
}

/// A probe engine whose discovery waits for the helper's stop flag, so control input and EOF can
/// be observed as cancellation. It records whether it was ever allowed to start work.
struct BlockingProbe {
    entered: AtomicBool,
    saw_stop: AtomicBool,
    spawn_attempts: AtomicUsize,
}

impl BlockingProbe {
    fn new() -> Self {
        Self { entered: AtomicBool::new(false), saw_stop: AtomicBool::new(false), spawn_attempts: AtomicUsize::new(0) }
    }
}

impl Engine for BlockingProbe {
    fn discover(&self, stop: &AtomicBool) -> Discovery {
        self.entered.store(true, Ordering::SeqCst);
        let stopped = wait_for_stop(stop, Duration::from_secs(10));
        self.saw_stop.store(stopped, Ordering::SeqCst);
        if !stopped {
            self.spawn_attempts.fetch_add(1, Ordering::SeqCst);
        }
        unknown()
    }

    fn locate(&self, _base: &Path) -> Result<Option<Installed>, InstallError> {
        Ok(None)
    }

    fn verify(&self, _base: &Path, _directory: &Path) -> Result<(), ()> {
        Err(())
    }

    fn supervise<V: FnOnce(&Path) -> Result<(), ()>>(
        &self,
        _java: PathBuf,
        _install: PathBuf,
        _stop: Arc<AtomicBool>,
        _verifier: V,
        _control: Box<dyn Read + Send>,
        _events: &mut dyn Write,
    ) -> Result<(), Failure> {
        self.spawn_attempts.fetch_add(1, Ordering::SeqCst);
        Err(Failure::SpawnFailed)
    }
}

/// The real bounded Java discovery, over a fixture directory. `wait_for_stop_first` proves that a
/// cancellation observed before discovery starts never spawns the fixture.
struct RealProbe {
    directory: PathBuf,
    wait_for_stop_first: bool,
}

impl Engine for RealProbe {
    fn discover(&self, stop: &AtomicBool) -> Discovery {
        if self.wait_for_stop_first {
            let _ = wait_for_stop(stop, Duration::from_secs(10));
        }
        let inputs = Inputs { java_home: None, path_directories: vec![self.directory.clone()], standard_directories: Vec::new() };
        proof_java::discover_with_control(&inputs, stop)
    }

    fn locate(&self, _base: &Path) -> Result<Option<Installed>, InstallError> {
        Ok(None)
    }

    fn verify(&self, _base: &Path, _directory: &Path) -> Result<(), ()> {
        Err(())
    }

    fn supervise<V: FnOnce(&Path) -> Result<(), ()>>(
        &self,
        _java: PathBuf,
        _install: PathBuf,
        _stop: Arc<AtomicBool>,
        _verifier: V,
        _control: Box<dyn Read + Send>,
        _events: &mut dyn Write,
    ) -> Result<(), Failure> {
        Err(Failure::SpawnFailed)
    }
}

struct SupervisorPlan {
    java: PathBuf,
    script: PathBuf,
    runtime: PathBuf,
    timeout: Duration,
}

/// A serve engine whose locate/discover/verify answers are scripted, and which either delegates to
/// the real supervisor with a real `/bin/sh` child or returns a fixed supervisor failure.
struct ServeEngine {
    locate_value: Result<Option<Installed>, InstallError>,
    discovery: Discovery,
    verify_result: Result<(), ()>,
    locate_delay: Duration,
    plan: Option<SupervisorPlan>,
    fixed_failure: Failure,
    locate_calls: AtomicUsize,
    discover_calls: AtomicUsize,
    verify_calls: AtomicUsize,
    supervise_calls: AtomicUsize,
    recorded_java: Mutex<Option<PathBuf>>,
    recorded_install: Mutex<Option<PathBuf>>,
    /// The stop flag the supervisor was handed, so a held verifier can watch the helper's own
    /// cancellation instead of sleeping.
    stop_seen: Mutex<Option<Arc<AtomicBool>>>,
    verify_entered: AtomicBool,
    verify_waits_for_stop: bool,
    verify_saw_stop: AtomicBool,
    /// Counts entries to the supervisor's command/prepare hook: the last point before `spawn`.
    command_admissions: Arc<AtomicUsize>,
}

impl ServeEngine {
    fn new(locate_value: Result<Option<Installed>, InstallError>, discovery: Discovery) -> Self {
        Self {
            locate_value,
            discovery,
            verify_result: Ok(()),
            locate_delay: Duration::ZERO,
            plan: None,
            fixed_failure: Failure::StartupTimedOut,
            locate_calls: AtomicUsize::new(0),
            discover_calls: AtomicUsize::new(0),
            verify_calls: AtomicUsize::new(0),
            supervise_calls: AtomicUsize::new(0),
            recorded_java: Mutex::new(None),
            recorded_install: Mutex::new(None),
            stop_seen: Mutex::new(None),
            verify_entered: AtomicBool::new(false),
            verify_waits_for_stop: false,
            verify_saw_stop: AtomicBool::new(false),
            command_admissions: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn with_verify(mut self, result: Result<(), ()>) -> Self {
        self.verify_result = result;
        self
    }

    fn with_locate_delay(mut self, delay: Duration) -> Self {
        self.locate_delay = delay;
        self
    }

    fn with_supervisor(mut self, plan: SupervisorPlan) -> Self {
        self.plan = Some(plan);
        self
    }

    /// Hold the verifier until the helper's stop flag is set, so EOF can be observed exactly at
    /// the boundary the re-verification gate protects.
    fn with_verify_waits_for_stop(mut self) -> Self {
        self.verify_waits_for_stop = true;
        self
    }
}

impl Engine for ServeEngine {
    fn discover(&self, _stop: &AtomicBool) -> Discovery {
        self.discover_calls.fetch_add(1, Ordering::SeqCst);
        self.discovery.clone()
    }

    fn locate(&self, _base: &Path) -> Result<Option<Installed>, InstallError> {
        self.locate_calls.fetch_add(1, Ordering::SeqCst);
        if !self.locate_delay.is_zero() {
            thread::sleep(self.locate_delay);
        }
        self.locate_value.clone()
    }

    fn verify(&self, _base: &Path, _directory: &Path) -> Result<(), ()> {
        self.verify_calls.fetch_add(1, Ordering::SeqCst);
        self.verify_entered.store(true, Ordering::SeqCst);
        if self.verify_waits_for_stop {
            let stop = self.stop_seen.lock().unwrap().clone();
            let observed = stop.is_some_and(|stop| wait_for_stop(stop.as_ref(), Duration::from_secs(10)));
            self.verify_saw_stop.store(observed, Ordering::SeqCst);
        }
        self.verify_result
    }

    fn supervise<V: FnOnce(&Path) -> Result<(), ()>>(
        &self,
        java: PathBuf,
        install: PathBuf,
        stop: Arc<AtomicBool>,
        verifier: V,
        control: Box<dyn Read + Send>,
        events: &mut dyn Write,
    ) -> Result<(), Failure> {
        self.supervise_calls.fetch_add(1, Ordering::SeqCst);
        *self.recorded_java.lock().unwrap() = Some(java);
        *self.recorded_install.lock().unwrap() = Some(install.clone());
        *self.stop_seen.lock().unwrap() = Some(Arc::clone(&stop));
        match &self.plan {
            Some(plan) => {
                let script = plan.script.clone();
                let admissions = Arc::clone(&self.command_admissions);
                let config = Config::new(&plan.java, &install)
                    .with_startup_timeout(plan.timeout)
                    .with_test_runtime_base(plan.runtime.clone())
                    .with_stop(stop)
                    .with_test_command(move |command, _port| {
                        admissions.fetch_add(1, Ordering::SeqCst);
                        let mut replacement = Command::new("/bin/sh");
                        replacement.arg(&script);
                        *command = replacement;
                        Vec::new()
                    });
                supervise(config, verifier, control, events)
            }
            None => Err(self.fixed_failure),
        }
    }
}

fn installed(path: &Path) -> Installed {
    Installed { directory: path.to_path_buf(), version: "6.6".to_string() }
}

// ---------------------------------------------------------------------------------------------
// Invocation and probe
// ---------------------------------------------------------------------------------------------

#[test]
fn terminal_stdin_is_refused_before_any_work() {
    let engine = Arc::new(FixedProbe::new(unknown()));
    let events = SharedEvents::default();
    let (_reader, writer) = pipe();
    let code = finish(spawn_run(Mode::Probe, true, io::empty(), events.clone(), ArcEngine(engine.clone())));
    assert_eq!(code, EXIT_INVOCATION, "a terminal stdin must be refused");
    assert!(events.is_empty(), "a refused invocation writes no event");
    assert_eq!(engine.discover_calls.load(Ordering::SeqCst), 0, "a refused invocation does no work");
    drop(writer);
}

#[test]
fn probe_reports_available_and_exit_zero() {
    let engine = FixedProbe::new(available(PathBuf::from("/usr/bin/java")));
    let events = SharedEvents::default();
    let (reader, writer) = pipe();
    let code = finish(spawn_run(Mode::Probe, false, reader, events.clone(), engine));
    assert_eq!(code, 0);
    assert_eq!(events.text(), "PROBE AVAILABLE\n");
    drop(writer);
}

#[test]
fn probe_reports_missing_too_old_and_unknown_as_fixed_events() {
    for (discovery, expected) in [
        (Discovery { executable: None, status: Java::Missing }, "PROBE MISSING\n"),
        (Discovery { executable: None, status: Java::TooOld }, "PROBE TOOOLD\n"),
        (unknown(), "PROBE UNKNOWN\n"),
    ] {
        let engine = FixedProbe::new(discovery);
        let events = SharedEvents::default();
        let (reader, writer) = pipe();
        let code = finish(spawn_run(Mode::Probe, false, reader, events.clone(), engine));
        assert_eq!(code, 0);
        assert_eq!(events.text(), expected);
        drop(writer);
    }
}

#[test]
fn probe_stop_line_cancels_and_writes_stopped() {
    let engine = Arc::new(BlockingProbe::new());
    let events = SharedEvents::default();
    let (reader, mut writer) = pipe();
    let receiver = spawn_run(Mode::Probe, false, reader, events.clone(), ArcEngine(engine.clone()));
    assert!(wait_until(Duration::from_secs(5), || engine.entered.load(Ordering::SeqCst)));
    writer.write_all(b"STOP\n").unwrap();
    writer.flush().unwrap();
    assert_eq!(finish(receiver), 0);
    assert!(engine.saw_stop.load(Ordering::SeqCst), "STOP must set the one stop flag");
    assert_eq!(engine.spawn_attempts.load(Ordering::SeqCst), 0, "a cancelled probe starts nothing");
    assert_eq!(events.text(), "STOPPED\n");
    drop(writer);
}

#[test]
fn probe_malformed_control_input_fails_closed() {
    let engine = Arc::new(BlockingProbe::new());
    let events = SharedEvents::default();
    let (reader, mut writer) = pipe();
    let receiver = spawn_run(Mode::Probe, false, reader, events.clone(), ArcEngine(engine.clone()));
    assert!(wait_until(Duration::from_secs(5), || engine.entered.load(Ordering::SeqCst)));
    writer.write_all(b"KEEPALIVE\n").unwrap();
    writer.flush().unwrap();
    assert_eq!(finish(receiver), 0);
    assert!(engine.saw_stop.load(Ordering::SeqCst), "unknown control input is a stop");
    assert_eq!(engine.spawn_attempts.load(Ordering::SeqCst), 0);
    assert_eq!(events.text(), "STOPPED\n");
    drop(writer);
}

#[test]
fn probe_overlong_control_input_fails_closed_without_a_newline() {
    let engine = Arc::new(BlockingProbe::new());
    let events = SharedEvents::default();
    let (reader, mut writer) = pipe();
    let receiver = spawn_run(Mode::Probe, false, reader, events.clone(), ArcEngine(engine.clone()));
    assert!(wait_until(Duration::from_secs(5), || engine.entered.load(Ordering::SeqCst)));
    writer.write_all(&[b'A'; 4096]).unwrap();
    writer.flush().unwrap();
    assert_eq!(finish(receiver), 0);
    assert!(engine.saw_stop.load(Ordering::SeqCst), "an overlong private line is a stop");
    assert_eq!(engine.spawn_attempts.load(Ordering::SeqCst), 0);
    assert_eq!(events.text(), "STOPPED\n");
    drop(writer);
}

#[test]
fn probe_single_unknown_byte_without_a_newline_stops_immediately() {
    let engine = Arc::new(BlockingProbe::new());
    let events = SharedEvents::default();
    let (reader, mut writer) = pipe();
    let receiver = spawn_run(Mode::Probe, false, reader, events.clone(), ArcEngine(engine.clone()));
    assert!(wait_until(Duration::from_secs(5), || engine.entered.load(Ordering::SeqCst)));
    // One byte without a newline. The private host only ever writes `STOP` or closes the pipe, so
    // any data is a stop; waiting for a line boundary would stall cancellation.
    writer.write_all(b"X").unwrap();
    writer.flush().unwrap();
    assert_eq!(finish(receiver), 0);
    assert!(engine.saw_stop.load(Ordering::SeqCst), "any private control byte is a stop");
    assert_eq!(engine.spawn_attempts.load(Ordering::SeqCst), 0);
    assert_eq!(events.text(), "STOPPED\n");
    drop(writer);
}

/// The monitor is started before discovery; a pipe that is already at EOF means discovery never
/// starts a child at all.
#[cfg(unix)]
#[test]
fn probe_eof_before_discovery_starts_no_child() {
    let root = TempRoot::new("probe-eof-before");
    let bin = root.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let pidfile = root.path().join("java.pid");
    write_executable(&bin.join("java"), &shell_script("", &pidfile));
    let engine = RealProbe { directory: bin, wait_for_stop_first: true };
    let events = SharedEvents::default();
    let (reader, writer) = pipe();
    drop(writer);
    let code = finish(spawn_run(Mode::Probe, false, reader, events.clone(), engine));
    assert_eq!(code, 0);
    assert_eq!(events.text(), "STOPPED\n");
    assert!(!pidfile.exists(), "no candidate process may be started after cancellation");
}

/// EOF while the real discovery is probing kills and reaps exactly the child that probe started.
#[cfg(unix)]
#[test]
fn probe_eof_during_probe_kills_and_reaps_its_own_child() {
    let root = TempRoot::new("probe-eof-during");
    let bin = root.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let pidfile = root.path().join("java.pid");
    write_executable(&bin.join("java"), &shell_script("", &pidfile));
    let engine = RealProbe { directory: bin, wait_for_stop_first: false };
    let events = SharedEvents::default();
    let (reader, writer) = pipe();
    let receiver = spawn_run(Mode::Probe, false, reader, events.clone(), engine);
    assert!(wait_until(Duration::from_secs(5), || pidfile.exists()), "the fixture Java never started");
    let pid = read_pid(&pidfile).expect("the fixture writes its own pid");
    assert!(process_alive(pid));
    let started = Instant::now();
    drop(writer);
    assert_eq!(finish(receiver), 0);
    assert!(started.elapsed() < proof_java::PROBE_TIMEOUT, "cancellation must end the probe, not its deadline");
    assert_eq!(events.text(), "STOPPED\n");
    assert!(!process_alive(pid), "the probe must kill and reap its own child");
}

// ---------------------------------------------------------------------------------------------
// Serve: pinned installation and Java discovery gates
// ---------------------------------------------------------------------------------------------

/// The production `Native` engine's locate is the pinned `mailcraft-net` locator. An empty base
/// has no verified 6.6 installation, so the helper refuses with `not installed` and never asks
/// for Java.
#[test]
fn serve_missing_pinned_install_refuses_before_java() {
    let root = TempRoot::new("serve-missing-install");
    let engine = Arc::new(ServeEngine::new(languagetool_install::locate(root.path()), available(PathBuf::from("/usr/bin/java"))));
    let events = SharedEvents::default();
    let (reader, writer) = pipe();
    let code = finish(spawn_run(Mode::Serve { base: root.path().to_path_buf() }, false, reader, events.clone(), ArcEngine(engine.clone())));
    assert_eq!(code, EXIT_NOT_INSTALLED);
    assert_eq!(events.text(), "FAILED\n");
    assert_eq!(engine.discover_calls.load(Ordering::SeqCst), 0, "Java must not be discovered without a verified install");
    assert_eq!(engine.supervise_calls.load(Ordering::SeqCst), 0);
    drop(writer);
}

/// A `.6.6` entry that is not a verified installation is `integrity`, not `not installed`, and is
/// refused before any Java discovery.
#[test]
fn serve_broken_install_layout_is_integrity_before_java() {
    let root = TempRoot::new("serve-broken-install");
    let version_dir = root.path().join("languagetool").join("6.6");
    fs::create_dir_all(&version_dir).unwrap();
    let engine = Arc::new(ServeEngine::new(languagetool_install::locate(root.path()), available(PathBuf::from("/usr/bin/java"))));
    let events = SharedEvents::default();
    let (reader, writer) = pipe();
    let code = finish(spawn_run(Mode::Serve { base: root.path().to_path_buf() }, false, reader, events.clone(), ArcEngine(engine.clone())));
    assert_eq!(code, EXIT_INTEGRITY);
    assert_eq!(events.text(), "FAILED\n");
    assert_eq!(engine.discover_calls.load(Ordering::SeqCst), 0, "Java must not be discovered for a broken installation");
    assert_eq!(engine.supervise_calls.load(Ordering::SeqCst), 0);
    drop(writer);
}

#[test]
fn serve_integrity_error_refuses_before_java() {
    let root = TempRoot::new("serve-integrity");
    let engine = Arc::new(ServeEngine::new(Err(InstallError::Integrity), available(PathBuf::from("/usr/bin/java"))));
    let events = SharedEvents::default();
    let (reader, writer) = pipe();
    let code = finish(spawn_run(Mode::Serve { base: root.path().to_path_buf() }, false, reader, events.clone(), ArcEngine(engine.clone())));
    assert_eq!(code, EXIT_INTEGRITY);
    assert_eq!(events.text(), "FAILED\n");
    assert_eq!(engine.discover_calls.load(Ordering::SeqCst), 0);
    assert_eq!(engine.supervise_calls.load(Ordering::SeqCst), 0);
    drop(writer);
}

#[test]
fn serve_locate_io_error_is_start_failed() {
    let root = TempRoot::new("serve-io");
    let engine = Arc::new(ServeEngine::new(Err(InstallError::Io), available(PathBuf::from("/usr/bin/java"))));
    let events = SharedEvents::default();
    let (reader, writer) = pipe();
    let code = finish(spawn_run(Mode::Serve { base: root.path().to_path_buf() }, false, reader, events.clone(), ArcEngine(engine.clone())));
    assert_eq!(code, 1);
    assert_eq!(events.text(), "FAILED\n");
    assert_eq!(engine.discover_calls.load(Ordering::SeqCst), 0);
    drop(writer);
}

#[test]
fn serve_missing_and_too_old_java_use_fixed_exits() {
    for (status, expected) in [(Java::Missing, EXIT_JAVA_MISSING), (Java::TooOld, EXIT_JAVA_TOO_OLD)] {
        let root = TempRoot::new("serve-java");
        let install = root.install();
        let engine = Arc::new(ServeEngine::new(Ok(Some(installed(&install))), Discovery { executable: None, status }));
        let events = SharedEvents::default();
        let (reader, writer) = pipe();
        let code = finish(spawn_run(Mode::Serve { base: root.path().to_path_buf() }, false, reader, events.clone(), ArcEngine(engine.clone())));
        assert_eq!(code, expected);
        assert_eq!(events.text(), "FAILED\n");
        assert_eq!(engine.supervise_calls.load(Ordering::SeqCst), 0, "Java discovery gating precedes the supervisor");
        drop(writer);
    }
}

#[test]
fn serve_unknown_java_is_start_failed() {
    let root = TempRoot::new("serve-unknown");
    let install = root.install();
    let engine = ServeEngine::new(Ok(Some(installed(&install))), unknown());
    let events = SharedEvents::default();
    let (reader, writer) = pipe();
    let code = finish(spawn_run(Mode::Serve { base: root.path().to_path_buf() }, false, reader, events.clone(), engine));
    assert_eq!(code, 1);
    assert_eq!(events.text(), "FAILED\n");
    drop(writer);
}

/// A stop that arrives while locate is reading the installation is seen after the disk work and
/// before Java discovery; no child can be started.
#[test]
fn serve_cancelled_after_disk_work_never_discovers() {
    let root = TempRoot::new("serve-cancel-disk");
    let install = root.install();
    let engine = Arc::new(
        ServeEngine::new(Ok(Some(installed(&install))), available(PathBuf::from("/usr/bin/java"))).with_locate_delay(Duration::from_millis(400)),
    );
    let events = SharedEvents::default();
    let (reader, writer) = pipe();
    let receiver = spawn_run(Mode::Serve { base: root.path().to_path_buf() }, false, reader, events.clone(), ArcEngine(engine.clone()));
    assert!(wait_until(Duration::from_secs(5), || engine.locate_calls.load(Ordering::SeqCst) == 1));
    drop(writer);
    assert_eq!(finish(receiver), 0);
    assert_eq!(engine.discover_calls.load(Ordering::SeqCst), 0, "Java must not be discovered after cancellation");
    assert_eq!(engine.supervise_calls.load(Ordering::SeqCst), 0, "no supervisor may start after cancellation");
    assert_eq!(events.text(), "STOPPED\n");
}

/// A stop that is already set, or becomes set while `locate` is reading the installation, is
/// honoured no later than the gate right after `locate`: no Java discovery and no supervisor.
/// This does not claim the end of file was always observed before `locate` was entered; the
/// `locate` delay makes the post-locate gate the deterministic boundary asserted here.
#[test]
fn serve_cancelled_around_locate_never_discovers_or_supervises() {
    let root = TempRoot::new("serve-cancel-early");
    let install = root.install();
    let engine = Arc::new(
        ServeEngine::new(Ok(Some(installed(&install))), available(PathBuf::from("/usr/bin/java"))).with_locate_delay(Duration::from_millis(250)),
    );
    let events = SharedEvents::default();
    let (reader, writer) = pipe();
    drop(writer);
    let code = finish(spawn_run(Mode::Serve { base: root.path().to_path_buf() }, false, reader, events.clone(), ArcEngine(engine.clone())));
    assert_eq!(code, 0);
    assert_eq!(engine.discover_calls.load(Ordering::SeqCst), 0, "the post-locate gate must precede Java discovery");
    assert_eq!(engine.supervise_calls.load(Ordering::SeqCst), 0);
    assert_eq!(events.text(), "STOPPED\n");
}

#[test]
fn serve_supervisor_timeout_maps_to_exit_nine() {
    let root = TempRoot::new("serve-timeout");
    let install = root.install();
    let engine = ServeEngine::new(Ok(Some(installed(&install))), available(PathBuf::from("/usr/bin/java")));
    let events = SharedEvents::default();
    let (reader, writer) = pipe();
    let code = finish(spawn_run(Mode::Serve { base: root.path().to_path_buf() }, false, reader, events.clone(), engine));
    assert_eq!(code, EXIT_TIMEOUT);
    drop(writer);
}

// ---------------------------------------------------------------------------------------------
// Serve: real supervisor child over real pipes
// ---------------------------------------------------------------------------------------------

#[cfg(unix)]
fn serve_plan(root: &TempRoot, script: &Path) -> SupervisorPlan {
    SupervisorPlan { java: script.to_path_buf(), script: script.to_path_buf(), runtime: root.runtime(), timeout: Duration::from_secs(5) }
}

#[cfg(unix)]
fn serve_with_child(label: &str, body: &str) -> (TempRoot, ServeEngine, PathBuf) {
    let root = TempRoot::new(label);
    let install = root.install();
    let pidfile = root.path().join("java.pid");
    let script = root.path().join("server.sh");
    write_script(&script, &shell_script(body, &pidfile));
    let plan = serve_plan(&root, &script);
    let engine = ServeEngine::new(Ok(Some(installed(&install))), available(script.clone())).with_supervisor(plan);
    (root, engine, pidfile)
}

/// The helper passes exactly the pinned directory and the discovered launcher to the supervisor;
/// the supervisor re-verifies that directory before the child is spawned.
#[cfg(unix)]
#[test]
fn serve_passes_pinned_directory_and_java_to_supervisor() {
    let (root, engine, pidfile) = serve_with_child("serve-paths", "echo ' Server started'\n");
    let engine = Arc::new(engine);
    let events = SharedEvents::default();
    let (reader, writer) = pipe();
    let receiver = spawn_run(Mode::Serve { base: root.path().to_path_buf() }, false, reader, events.clone(), ArcEngine(engine.clone()));
    assert!(wait_until(Duration::from_secs(5), || events.text().contains("BOUND ")));
    assert_eq!(engine.verify_calls.load(Ordering::SeqCst), 1, "the pinned directory is re-verified before spawn");
    assert_eq!(*engine.recorded_install.lock().unwrap(), Some(root.install()));
    assert_eq!(*engine.recorded_java.lock().unwrap(), Some(root.path().join("server.sh")));
    drop(writer);
    assert_eq!(finish(receiver), 0);
    let pid = read_pid(&pidfile).expect("the child ran");
    assert!(!process_alive(pid));
}

/// A verifier refusal happens before any runtime directory or child: the supervisor reports it and
/// the helper maps it to `integrity`. The command-hook admission counter is the deterministic proof
/// that the last pre-spawn boundary was never reached.
#[cfg(unix)]
#[test]
fn serve_verification_refusal_happens_before_spawn() {
    let (root, engine, pidfile) = serve_with_child("serve-verify-refused", "");
    let engine = Arc::new(engine.with_verify(Err(())));
    let events = SharedEvents::default();
    let (reader, writer) = pipe();
    let code = finish(spawn_run(Mode::Serve { base: root.path().to_path_buf() }, false, reader, events.clone(), ArcEngine(engine.clone())));
    assert_eq!(code, EXIT_INTEGRITY);
    assert_eq!(engine.verify_calls.load(Ordering::SeqCst), 1);
    assert_eq!(engine.command_admissions.load(Ordering::SeqCst), 0, "verification refusal must precede the command hook");
    assert!(!pidfile.exists(), "verification refusal must precede the child");
    drop(writer);
}

/// EOF arrives while the supervisor's re-verification is still reading the installation. The
/// helper's stop flag is the one the supervisor was handed; the verifier observes it and returns,
/// and the supervisor's post-verifier gate stops the run before the command hook or a child.
#[cfg(unix)]
#[test]
fn serve_eof_while_verification_is_held_stops_without_spawn_or_failure() {
    let (root, engine, pidfile) = serve_with_child("serve-eof-verify", "");
    let engine = Arc::new(engine.with_verify_waits_for_stop());
    let events = SharedEvents::default();
    let (reader, writer) = pipe();
    let receiver = spawn_run(Mode::Serve { base: root.path().to_path_buf() }, false, reader, events.clone(), ArcEngine(engine.clone()));
    assert!(wait_until(Duration::from_secs(5), || engine.verify_entered.load(Ordering::SeqCst)), "the re-verification never started");
    drop(writer);
    assert_eq!(finish(receiver), 0);
    assert!(engine.verify_saw_stop.load(Ordering::SeqCst), "the verifier must observe the helper's own stop flag");
    assert_eq!(engine.command_admissions.load(Ordering::SeqCst), 0, "a known stop must not reach the command hook");
    assert!(!pidfile.exists(), "no child may be started after cancellation during verification");
    let text = events.text();
    assert_eq!(text, "STOPPED\n");
    assert!(!text.contains("FAILED"), "cancellation must not be reported as a start failure: {text:?}");
}

/// EOF during startup: the owned child never printed the marker and is killed and reaped.
#[cfg(unix)]
#[test]
fn serve_eof_during_startup_stops_owned_child() {
    let (root, engine, pidfile) = serve_with_child("serve-eof-startup", "");
    let events = SharedEvents::default();
    let (reader, writer) = pipe();
    let receiver = spawn_run(Mode::Serve { base: root.path().to_path_buf() }, false, reader, events.clone(), engine);
    assert!(wait_until(Duration::from_secs(5), || pidfile.exists()), "the child never started");
    let pid = read_pid(&pidfile).expect("the child writes its pid");
    assert!(process_alive(pid));
    drop(writer);
    assert_eq!(finish(receiver), 0);
    assert_eq!(events.text(), "STOPPED\n");
    assert!(!process_alive(pid), "the supervisor must kill and reap its child during startup");
}

/// EOF while running: BOUND was reported, then the same owned child is killed and reaped.
#[cfg(unix)]
#[test]
fn serve_eof_while_running_stops_owned_child() {
    let (root, engine, pidfile) = serve_with_child("serve-eof-running", "echo ' Server started'\n");
    let events = SharedEvents::default();
    let (reader, writer) = pipe();
    let receiver = spawn_run(Mode::Serve { base: root.path().to_path_buf() }, false, reader, events.clone(), engine);
    assert!(wait_until(Duration::from_secs(5), || events.text().contains("BOUND ") && pidfile.exists()), "the child never reported BOUND");
    let pid = read_pid(&pidfile).expect("the child writes its pid");
    assert!(process_alive(pid));
    drop(writer);
    assert_eq!(finish(receiver), 0);
    let text = events.text();
    assert!(text.starts_with("BOUND "), "a bound port is reported before the stop: {text:?}");
    assert!(text.ends_with("STOPPED\n"), "the stop is reported as STOPPED: {text:?}");
    assert!(!process_alive(pid), "the supervisor must kill and reap its running child");
}

// ---------------------------------------------------------------------------------------------
// The production locator, directly
// ---------------------------------------------------------------------------------------------

#[test]
fn real_locator_finds_no_install_under_an_empty_base() {
    let root = TempRoot::new("locate-empty");
    assert_eq!(Native.locate(root.path()).unwrap(), None);
}

#[test]
fn real_locator_calls_an_unverified_tree_integrity() {
    let root = TempRoot::new("locate-broken");
    fs::create_dir_all(root.path().join("languagetool").join("6.6")).unwrap();
    assert_eq!(Native.locate(root.path()).unwrap_err(), InstallError::Integrity);
}

// ---------------------------------------------------------------------------------------------
// Arc<Engine> plumbing (the helper only needs `&E`, but threads need an owned handle)
// ---------------------------------------------------------------------------------------------

struct ArcEngine<T>(Arc<T>);

impl<T: Engine> Engine for ArcEngine<T> {
    fn discover(&self, stop: &AtomicBool) -> Discovery {
        self.0.discover(stop)
    }

    fn locate(&self, base: &Path) -> Result<Option<Installed>, InstallError> {
        self.0.locate(base)
    }

    fn verify(&self, base: &Path, directory: &Path) -> Result<(), ()> {
        self.0.verify(base, directory)
    }

    fn supervise<V: FnOnce(&Path) -> Result<(), ()>>(
        &self,
        java: PathBuf,
        install: PathBuf,
        stop: Arc<AtomicBool>,
        verifier: V,
        control: Box<dyn Read + Send>,
        events: &mut dyn Write,
    ) -> Result<(), Failure> {
        self.0.supervise(java, install, stop, verifier, control, events)
    }
}
