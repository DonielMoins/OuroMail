// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Tests for the native managed proofing host (#954).
//!
//! The host's worker lifecycle is exercised with real threads, real operating-system pipes and
//! real `/bin/sh` helper processes (the `unix` tests), so the lifetime pipe, event parsing, child
//! reaping and cancellation ordering are the ones the program runs. The installer and readiness
//! coordinator are injected mocks: no test downloads the pinned archive, starts Java, binds a
//! fixed port or contacts the LanguageTool server on `127.0.0.1:18081`. The real installer and
//! the real readiness exchange are compiled in the production paths and are measured separately.

use std::collections::VecDeque;
use std::io;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use mailcraft_model::proof::{Language, MAX_ANSWER};
use mailcraft_net::languagetool::CheckOutcome;
use mailcraft_net::languagetool_install::{InstallError, Installed, Progress};
use mailcraft_ui_egui::proof_server::{Action, Issue, Java, ManagedServer, Phase, Status};

use super::proof_host::{
    Config, Helper, Installer, LaunchGate, Launcher, NativeReadiness, Readiness, Spawn, acquire_event_slot, event_slots_in_use, reset_event_pool,
    set_event_slot_limit, start, warm_outcomes_ok,
};
use super::proof_protocol::Mode;

// ---------------------------------------------------------------------------------------------
// Small fixtures: a temporary directory, helper scripts, and the injected backends.
// ---------------------------------------------------------------------------------------------

const WAIT: Duration = Duration::from_secs(8);

fn wake() -> Arc<dyn Fn() + Send + Sync> {
    Arc::new(|| {})
}

fn base_dir() -> PathBuf {
    std::env::temp_dir().join("mailcraft-proof-host-fixture")
}

fn installed() -> Installed {
    Installed { directory: base_dir().join("LanguageTool-6.6"), version: "6.6".to_string() }
}

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!("mailcraft-proof-host-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn script(&self, name: &str, body: &str) -> PathBuf {
        let path = self.path.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        path
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A launcher that must never be asked for a command.
#[derive(Default)]
struct CountingLauncher {
    calls: AtomicUsize,
}

impl Launcher for CountingLauncher {
    fn command(&self, _mode: &Mode) -> io::Result<Command> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(io::Error::other("no helper in a passive test"))
    }
}

/// A launcher that runs a script per protocol mode.
struct ScriptLauncher {
    probe: PathBuf,
    serve: PathBuf,
    probe_calls: AtomicUsize,
    serve_calls: AtomicUsize,
}

impl ScriptLauncher {
    fn new(probe: PathBuf, serve: PathBuf) -> Self {
        Self { probe, serve, probe_calls: AtomicUsize::new(0), serve_calls: AtomicUsize::new(0) }
    }
}

impl Launcher for ScriptLauncher {
    fn command(&self, mode: &Mode) -> io::Result<Command> {
        match mode {
            Mode::Probe => {
                self.probe_calls.fetch_add(1, Ordering::SeqCst);
                Ok(Command::new(&self.probe))
            }
            Mode::Serve { .. } => {
                self.serve_calls.fetch_add(1, Ordering::SeqCst);
                Ok(Command::new(&self.serve))
            }
        }
    }
}

/// A launcher that hands out one Serve fixture per call, in order. One host can then own an old
/// instance and a newer one whose output differs, without rewriting a script under a running shell.
struct SequencedLauncher {
    probe: PathBuf,
    serves: Mutex<VecDeque<PathBuf>>,
    probe_calls: AtomicUsize,
    serve_calls: AtomicUsize,
}

impl SequencedLauncher {
    fn new(probe: PathBuf, serves: impl IntoIterator<Item = PathBuf>) -> Self {
        Self { probe, serves: Mutex::new(serves.into_iter().collect()), probe_calls: AtomicUsize::new(0), serve_calls: AtomicUsize::new(0) }
    }
}

impl Launcher for SequencedLauncher {
    fn command(&self, mode: &Mode) -> io::Result<Command> {
        match mode {
            Mode::Probe => {
                self.probe_calls.fetch_add(1, Ordering::SeqCst);
                Ok(Command::new(&self.probe))
            }
            Mode::Serve { .. } => {
                self.serve_calls.fetch_add(1, Ordering::SeqCst);
                match self.serves.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).pop_front() {
                    Some(path) => Ok(Command::new(path)),
                    None => Err(io::Error::other("no Serve fixture left")),
                }
            }
        }
    }
}

/// A launcher that blocks while it builds either command, so a cancellation can be delivered after
/// the pre-launch check but before any helper is spawned.
struct GatedLauncher {
    probe: PathBuf,
    serve: PathBuf,
    entered: Mutex<Option<mpsc::Sender<()>>>,
    gate: Mutex<Option<Receiver<()>>>,
    probe_calls: AtomicUsize,
    serve_calls: AtomicUsize,
}

impl GatedLauncher {
    fn new(probe: PathBuf, serve: PathBuf) -> Self {
        Self { probe, serve, entered: Mutex::new(None), gate: Mutex::new(None), probe_calls: AtomicUsize::new(0), serve_calls: AtomicUsize::new(0) }
    }

    fn block(&self) {
        if let Some(entered) = self.entered.lock().unwrap().take() {
            let _ = entered.send(());
        }
        if let Some(gate) = self.gate.lock().unwrap().take() {
            let _ = gate.recv();
        }
    }
}

impl Launcher for GatedLauncher {
    fn command(&self, mode: &Mode) -> io::Result<Command> {
        match mode {
            Mode::Probe => {
                self.probe_calls.fetch_add(1, Ordering::SeqCst);
                self.block();
                Ok(Command::new(&self.probe))
            }
            Mode::Serve { .. } => {
                self.serve_calls.fetch_add(1, Ordering::SeqCst);
                self.block();
                Ok(Command::new(&self.serve))
            }
        }
    }
}

#[derive(Default)]
struct FakeInstall {
    locate_calls: AtomicUsize,
    install_calls: AtomicUsize,
    locate_result: Mutex<Option<Installed>>,
    locate_error: Mutex<Option<InstallError>>,
    locate_gate: Mutex<Option<Receiver<()>>>,
    locate_panics: AtomicBool,
    install_result: Mutex<Option<Result<Installed, InstallError>>>,
    install_gate: Mutex<Option<Receiver<()>>>,
    progress: Mutex<Vec<Progress>>,
    cancel_seen: AtomicBool,
    /// Test-only: a real installer that finishes its own promotion (or reports a confirmed
    /// failure) just as the owner cancels must still state that outcome truthfully.
    ignore_cancel: AtomicBool,
}

impl FakeInstall {
    fn with_installed(installed: Installed) -> Self {
        let fake = Self::default();
        *fake.locate_result.lock().unwrap() = Some(installed);
        fake
    }
}

impl Installer for FakeInstall {
    fn locate(&self, _base: &Path) -> Result<Option<Installed>, InstallError> {
        self.locate_calls.fetch_add(1, Ordering::SeqCst);
        if self.locate_panics.swap(false, Ordering::SeqCst) {
            // A backend that ignores its contract: the host must turn this into a fixed failure.
            std::panic::panic_any("injected installer panic");
        }
        if let Some(gate) = self.locate_gate.lock().unwrap().take() {
            let _ = gate.recv();
        }
        if let Some(error) = *self.locate_error.lock().unwrap() {
            return Err(error);
        }
        Ok(self.locate_result.lock().unwrap().clone())
    }

    fn install(&self, _base: &Path, cancel: &AtomicBool, progress: &dyn Fn(Progress)) -> Result<Installed, InstallError> {
        self.install_calls.fetch_add(1, Ordering::SeqCst);
        for report in self.progress.lock().unwrap().iter().copied() {
            progress(report);
        }
        if let Some(gate) = self.install_gate.lock().unwrap().take() {
            loop {
                if cancel.load(Ordering::SeqCst) {
                    self.cancel_seen.store(true, Ordering::SeqCst);
                    if !self.ignore_cancel.load(Ordering::SeqCst) {
                        return Err(InstallError::Cancelled);
                    }
                }
                match gate.recv_timeout(Duration::from_millis(5)) {
                    Ok(()) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        }
        if cancel.load(Ordering::SeqCst) {
            self.cancel_seen.store(true, Ordering::SeqCst);
        }
        if cancel.load(Ordering::SeqCst) && !self.ignore_cancel.load(Ordering::SeqCst) {
            return Err(InstallError::Cancelled);
        }
        match self.install_result.lock().unwrap().clone() {
            Some(result) => result,
            None => Err(InstallError::Io),
        }
    }
}

struct FakeReady {
    calls: AtomicUsize,
    issue: Option<Issue>,
    endpoints: Mutex<Vec<String>>,
    helper_pid_file: Option<PathBuf>,
    helper_was_alive: AtomicBool,
    /// Test-only: told once, before the exchange, that readiness has been asked.
    entered: Mutex<Option<mpsc::Sender<()>>>,
    /// Test-only: held open until the test releases it, so a hostile tail can arrive *during* warm-up.
    gate: Mutex<Option<Receiver<()>>>,
}

impl FakeReady {
    fn new(issue: Option<Issue>) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            issue,
            endpoints: Mutex::new(Vec::new()),
            helper_pid_file: None,
            helper_was_alive: AtomicBool::new(false),
            entered: Mutex::new(None),
            gate: Mutex::new(None),
        }
    }

    fn watching(issue: Option<Issue>, helper_pid_file: PathBuf) -> Self {
        let mut ready = Self::new(issue);
        ready.helper_pid_file = Some(helper_pid_file);
        ready
    }
}

impl Readiness for FakeReady {
    fn warm(&self, endpoint: &str, _deadline: Instant, cancelled: &dyn Fn() -> bool) -> Result<(), Issue> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.endpoints.lock().unwrap().push(endpoint.to_string());
        if let Some(path) = &self.helper_pid_file {
            self.helper_was_alive.store(process_is_alive(path), Ordering::SeqCst);
        }
        if let Some(entered) = self.entered.lock().unwrap().take() {
            let _ = entered.send(());
        }
        if let Some(gate) = self.gate.lock().unwrap().take() {
            let _ = gate.recv();
        }
        thread::sleep(Duration::from_millis(10));
        if cancelled() {
            return Err(Issue::Cancelled);
        }
        match self.issue {
            Some(issue) => Err(issue),
            None => Ok(()),
        }
    }
}

fn real_spawn(name: &str, body: Box<dyn FnOnce() + Send + 'static>) -> io::Result<thread::JoinHandle<()>> {
    thread::Builder::new().name(name.to_string()).spawn(body)
}

fn spawn() -> Spawn {
    Arc::new(real_spawn)
}

fn config(base: PathBuf, launcher: Arc<dyn Launcher>, installer: Arc<dyn Installer>, readiness: Arc<dyn Readiness>) -> Config {
    Config::testing(base, wake(), launcher, installer, readiness, spawn())
}

/// Whether a PID is still known to the OS. `has_proc` is the platform's answer: Linux keeps
/// `/proc/<pid>` (a zombie is still listed), so its directory is authoritative; elsewhere the
/// external `kill -0` is asked, which sends no signal. Injected so both branches are exercised on
/// one machine instead of only the branch the build happens to have.
fn pid_is_alive(pid: u32, has_proc: bool, proc_entry: impl Fn(u32) -> bool, kill_zero: impl Fn(u32) -> bool) -> bool {
    if has_proc { proc_entry(pid) } else { kill_zero(pid) }
}

fn process_is_alive(pid_file: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(pid_file) else { return false };
    let Ok(pid) = text.trim().parse::<u32>() else { return false };
    pid_is_alive(
        pid,
        cfg!(target_os = "linux"),
        |pid| Path::new(&format!("/proc/{pid}")).exists(),
        |pid| {
            Command::new("kill")
                .arg("-0")
                .arg(pid.to_string())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|status| status.success())
                .unwrap_or(false)
        },
    )
}

fn wait_for(host: &Arc<dyn ManagedServer>, limit: Duration, what: &str, mut done: impl FnMut(&Status) -> bool) -> Status {
    let started = Instant::now();
    loop {
        let status = host.status();
        if done(&status) {
            return status;
        }
        assert!(started.elapsed() < limit, "timed out waiting for {what}; last status {status:?}");
        thread::sleep(Duration::from_millis(5));
    }
}

fn wait_phase(host: &Arc<dyn ManagedServer>, phase: Phase) -> Status {
    wait_for(host, WAIT, &format!("phase {phase:?}"), |status| status.phase == phase)
}

fn wait_gone(pid_file: &Path) {
    let started = Instant::now();
    while process_is_alive(pid_file) {
        assert!(started.elapsed() < WAIT, "the helper process outlived its cleanup");
        thread::sleep(Duration::from_millis(10));
    }
}

// ---------------------------------------------------------------------------------------------
// Passive construction and explicit, read-only inspection.
// ---------------------------------------------------------------------------------------------

#[test]
fn unavailable_without_a_base_or_with_a_relative_one() {
    let _fixture = host_fixture();
    for base in [None, Some(PathBuf::from("relative/directory"))] {
        let host = super::proof_host::service(base, wake());
        let before = host.status();
        assert_eq!(before.phase, Phase::Unavailable);
        assert_eq!(before.issue, Some(Issue::Unavailable));
        assert_eq!(host.request(Action::Check), Err(Issue::Unavailable));
        assert_eq!(host.request(Action::Download), Err(Issue::Unavailable));
        assert_eq!(host.status(), before);
    }
}

#[test]
fn constructor_and_status_are_memory_only() {
    let _fixture = host_fixture();
    let launcher = Arc::new(CountingLauncher::default());
    let installer = Arc::new(FakeInstall::default());
    let readiness = Arc::new(FakeReady::new(None));
    let host = start(config(base_dir(), launcher.clone(), installer.clone(), readiness.clone()));
    let before = host.status();
    assert_eq!(before.phase, Phase::Unchecked);
    thread::sleep(Duration::from_millis(50));
    assert_eq!(launcher.calls.load(Ordering::SeqCst), 0, "no helper may be started before an owner action");
    assert_eq!(installer.locate_calls.load(Ordering::SeqCst), 0);
    assert_eq!(installer.install_calls.load(Ordering::SeqCst), 0);
    assert_eq!(readiness.calls.load(Ordering::SeqCst), 0);
    for _ in 0..16 {
        assert_eq!(host.status(), before, "status may never discover, download or start anything");
    }
}

/// The fixture's liveness answer must be the platform's: `/proc` where it exists, the external
/// `kill -0` (which sends no signal) elsewhere. The injected seam lets both be exercised here.
#[test]
fn pid_liveness_uses_proc_where_it_exists_and_kill_zero_elsewhere() {
    let _fixture = host_fixture();
    // Linux: `/proc/<pid>` is authoritative, and a listed zombie is still a process.
    assert!(pid_is_alive(4242, true, |pid| pid == 4242, |_| false));
    assert!(!pid_is_alive(4243, true, |_| false, |pid| pid == 4243), "where /proc exists its answer is the one used");
    // macOS and the other Unix systems have no /proc: the portable answer must be the external one.
    assert!(pid_is_alive(4244, false, |_| false, |pid| pid == 4244), "without /proc the portable answer must be used");
    assert!(!pid_is_alive(4245, false, |pid| pid == 4245, |_| false), "without /proc no /proc answer may be invented");
    // A pid file that names no process is never alive.
    let root = TempRoot::new("pid-liveness");
    let missing = root.path.join("missing.pid");
    assert!(!process_is_alive(&missing));
    let junk = root.path.join("junk.pid");
    std::fs::write(&junk, b"not a pid\n").unwrap();
    assert!(!process_is_alive(&junk));
    #[cfg(unix)]
    {
        let own = root.path.join("own.pid");
        std::fs::write(&own, std::process::id().to_string()).unwrap();
        assert!(process_is_alive(&own), "this test's own process is observably alive");
    }
}

#[cfg(unix)]
#[test]
fn check_is_explicit_and_read_only() {
    let _fixture = host_fixture();
    let root = TempRoot::new("check");
    let launcher = Arc::new(ScriptLauncher::new(root.script("probe.sh", "printf 'PROBE MISSING\\n'"), root.script("serve.sh", "exit 7")));
    let installer = Arc::new(FakeInstall::default());
    let readiness = Arc::new(FakeReady::new(None));
    let host = start(config(root.path.clone(), launcher.clone(), installer.clone(), readiness.clone()));
    assert_eq!(host.status().phase, Phase::Unchecked);
    host.request(Action::Check).unwrap();
    let status = wait_phase(&host, Phase::NotInstalled);
    assert_eq!(status.java, Java::Missing);
    assert_eq!(installer.locate_calls.load(Ordering::SeqCst), 1);
    assert_eq!(installer.install_calls.load(Ordering::SeqCst), 0, "Check never downloads");
    assert_eq!(readiness.calls.load(Ordering::SeqCst), 0, "Check never starts a server");
    assert_eq!(launcher.probe_calls.load(Ordering::SeqCst), 1);
    assert_eq!(launcher.serve_calls.load(Ordering::SeqCst), 0, "Check never starts the server helper");
    assert!(status.endpoint.is_none());
}

/// A Check that cannot read the installation (an unknown, not an absence) and is then cancelled
/// learns nothing new about absence: the cached verified version and Java fact survive, the
/// operation ends through the ordinary cancellation path, and a later Check still works.
#[cfg(unix)]
#[test]
fn a_cancelled_check_that_could_not_verify_keeps_the_verified_facts() {
    let _fixture = host_fixture();
    let root = TempRoot::new("cancel-locate-error");
    let launcher = Arc::new(ScriptLauncher::new(root.script("probe.sh", "printf 'PROBE AVAILABLE\\n'"), root.script("serve.sh", "exit 7")));
    let installer = Arc::new(FakeInstall::with_installed(installed()));
    let host = start(config(root.path.clone(), launcher, installer.clone(), Arc::new(FakeReady::new(None))));
    host.request(Action::Check).unwrap();
    let before = wait_phase(&host, Phase::Installed);
    assert_eq!(before.java, Java::Available);
    assert_eq!(before.version, "6.6");

    // The next Check cannot read the installation at all; cancel it while `locate` is blocked.
    *installer.locate_error.lock().unwrap() = Some(InstallError::Io);
    let (release, gate) = mpsc::channel();
    installer.locate_gate.lock().unwrap().replace(gate);
    host.request(Action::Check).unwrap();
    wait_phase(&host, Phase::Checking);
    assert_eq!(host.request(Action::Cancel), Ok(()));
    let _ = release.send(());
    let cancelled = wait_for(&host, WAIT, "the cancelled check", |status| {
        matches!(status.phase, Phase::Stopped | Phase::Cancelled | Phase::Failed | Phase::NotInstalled)
    });
    assert_eq!(cancelled.phase, Phase::Stopped, "a cancelled check over a verified install stays startable");
    assert_eq!(cancelled.issue, Some(Issue::Cancelled));
    assert_eq!(cancelled.java, Java::Available, "an unreadable installation says nothing about Java");
    assert_eq!(cancelled.version, "6.6", "an unknown locate never erases a verified version");

    // The cached facts are still usable: a later Check completes normally.
    *installer.locate_error.lock().unwrap() = None;
    host.request(Action::Check).unwrap();
    let recovered = wait_phase(&host, Phase::Installed);
    assert_eq!(recovered.version, "6.6");
    assert_eq!(recovered.issue, None);
}

/// Cancellation does not turn confirmed corruption into a merely unreadable installation. Check
/// and Download must both invalidate a previously verified distribution on an integrity failure.
#[cfg(unix)]
#[test]
fn a_confirmed_integrity_failure_is_not_relabelled_as_a_cancelled_check() {
    let _fixture = host_fixture();
    let root = TempRoot::new("check-integrity-cancel");
    let launcher = Arc::new(ScriptLauncher::new(root.script("probe.sh", "printf 'PROBE AVAILABLE\\n'"), root.script("serve.sh", "exit 7")));
    let installer = Arc::new(FakeInstall::with_installed(installed()));
    let host = start(config(root.path.clone(), launcher.clone(), installer.clone(), Arc::new(FakeReady::new(None))));
    host.request(Action::Check).unwrap();
    let before = wait_phase(&host, Phase::Installed);
    assert_eq!(before.version, "6.6");
    assert_eq!(before.java, Java::Available);

    *installer.locate_error.lock().unwrap() = Some(InstallError::Integrity);
    let (release, gate) = mpsc::channel();
    installer.locate_gate.lock().unwrap().replace(gate);
    host.request(Action::Check).unwrap();
    wait_phase(&host, Phase::Checking);
    assert_eq!(host.request(Action::Cancel), Ok(()));
    let _ = release.send(());
    let failed = wait_for(&host, WAIT, "the cancelled check's confirmed integrity failure", |status| {
        matches!(status.phase, Phase::Failed | Phase::Stopped | Phase::Cancelled)
    });
    assert_eq!(failed.phase, Phase::Failed, "confirmed corruption is a failure, not a retained installation");
    assert_eq!(failed.issue, Some(Issue::IntegrityFailed));
    assert!(failed.version.is_empty(), "the old version is no longer verified");
    assert!(failed.endpoint.is_none());
    assert_eq!(failed.epoch, 0);
    assert_eq!(host.request(Action::Start), Err(Issue::NotInstalled));
    assert_eq!(launcher.probe_calls.load(Ordering::SeqCst), 1);
    assert_eq!(launcher.serve_calls.load(Ordering::SeqCst), 0);
}

#[cfg(unix)]
#[test]
fn rejected_requests_admit_nothing_and_replace_nothing() {
    let _fixture = host_fixture();
    let root = TempRoot::new("reject");
    let launcher = Arc::new(ScriptLauncher::new(root.script("probe.sh", "printf 'PROBE MISSING\\n'"), root.script("serve.sh", "exit 7")));
    let installer = Arc::new(FakeInstall::default());
    let (release, gate) = mpsc::channel();
    installer.locate_gate.lock().unwrap().replace(gate);
    let host = start(config(root.path.clone(), launcher.clone(), installer.clone(), Arc::new(FakeReady::new(None))));
    host.request(Action::Check).unwrap();
    wait_for(&host, WAIT, "the blocked check", |status| status.phase == Phase::Checking);
    let before = host.status();
    assert_eq!(host.request(Action::Download), Err(Issue::Busy));
    assert_eq!(host.request(Action::Start), Err(Issue::Busy));
    assert_eq!(host.request(Action::Check), Err(Issue::Busy));
    assert_eq!(host.status(), before, "a rejected request changes nothing");
    let _ = release.send(());
    wait_phase(&host, Phase::NotInstalled);
    assert_eq!(installer.install_calls.load(Ordering::SeqCst), 0, "no rejected Download may run later");
    assert_eq!(launcher.serve_calls.load(Ordering::SeqCst), 0, "no rejected Start may run later");
}

// ---------------------------------------------------------------------------------------------
// Download: progress, no epoch movement, and never an implicit start.
// ---------------------------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn download_progress_keeps_the_epoch_and_never_starts_the_server() {
    let _fixture = host_fixture();
    let root = TempRoot::new("download");
    let launcher = Arc::new(ScriptLauncher::new(root.script("probe.sh", "printf 'PROBE MISSING\\n'"), root.script("serve.sh", "exit 7")));
    let installer = Arc::new(FakeInstall::default());
    let (release, gate) = mpsc::channel();
    *installer.install_gate.lock().unwrap() = Some(gate);
    *installer.install_result.lock().unwrap() = Some(Ok(installed()));
    *installer.progress.lock().unwrap() = vec![Progress::Download { written: 50, total: 100 }, Progress::Extract { files: 16, total: 100 }];
    let host = start(config(root.path.clone(), launcher.clone(), installer.clone(), Arc::new(FakeReady::new(None))));
    host.request(Action::Check).unwrap();
    wait_phase(&host, Phase::NotInstalled);

    host.request(Action::Download).unwrap();
    let progress = wait_for(&host, WAIT, "download progress", |status| status.total_bytes == 100);
    assert_eq!(progress.epoch, 0, "progress is not a new running instance");
    assert_eq!(progress.downloaded_bytes, 16);
    assert!(matches!(progress.phase, Phase::Downloading | Phase::Extracting));
    assert!(progress.endpoint.is_none());
    assert_eq!(launcher.serve_calls.load(Ordering::SeqCst), 0, "Download never starts the server helper");

    let _ = release.send(());
    let done = wait_phase(&host, Phase::Installed);
    assert_eq!(done.epoch, 0, "completing a download publishes no endpoint");
    assert_eq!(done.version, "6.6");
    assert!(done.endpoint.is_none());
    assert_eq!(installer.install_calls.load(Ordering::SeqCst), 1);
    assert_eq!(launcher.serve_calls.load(Ordering::SeqCst), 0);
}

#[cfg(unix)]
#[test]
fn cancelling_a_download_holds_cancelling_until_the_installer_stops() {
    let _fixture = host_fixture();
    let root = TempRoot::new("download-cancel");
    let launcher = Arc::new(ScriptLauncher::new(root.script("probe.sh", "printf 'PROBE AVAILABLE\\n'"), root.script("serve.sh", "exit 7")));
    let installer = Arc::new(FakeInstall::default());
    let (release, gate) = mpsc::channel();
    *installer.install_gate.lock().unwrap() = Some(gate);
    *installer.install_result.lock().unwrap() = Some(Ok(installed()));
    let host = start(config(root.path.clone(), launcher, installer.clone(), Arc::new(FakeReady::new(None))));
    host.request(Action::Check).unwrap();
    wait_phase(&host, Phase::NotInstalled);
    host.request(Action::Download).unwrap();
    wait_for(&host, WAIT, "the installer to be busy", |status| status.phase == Phase::Downloading);
    assert_eq!(host.request(Action::Cancel), Ok(()));
    let cancelling = host.status();
    assert_eq!(cancelling.phase, Phase::Cancelling);
    assert_eq!(host.request(Action::Check), Err(Issue::Busy), "a rejected request cannot clear cancellation");
    assert_eq!(host.status(), cancelling);
    let _ = release.send(());
    let done =
        wait_for(&host, WAIT, "cancelled download", |status| !matches!(status.phase, Phase::Downloading | Phase::Extracting | Phase::Cancelling));
    assert_eq!(done.issue, Some(Issue::Cancelled));
    assert!(installer.cancel_seen.load(Ordering::SeqCst), "the real cancellation flag is what the installer sees");
    assert!(done.endpoint.is_none());
}

/// A Download is not a Check: after it promotes an installation, a Java fact may be recorded only
/// when this Download's own probe confirmed it. A malformed or failed probe leaves `Unknown`, so a
/// final `Installed` cannot be started until a fresh Check.
#[cfg(unix)]
#[test]
fn a_download_with_an_unconfirmed_probe_records_no_java_fact() {
    let _fixture = host_fixture();
    for (label, bad_probe) in [("malformed", "printf 'GARBAGE\\n'"), ("nonzero", "printf 'PROBE AVAILABLE\\n'\nexit 7")] {
        let root = TempRoot::new(&format!("download-probe-{label}"));
        let launcher = Arc::new(ScriptLauncher::new(root.script("probe.sh", "printf 'PROBE AVAILABLE\\n'"), root.script("serve.sh", "exit 7")));
        let installer = Arc::new(FakeInstall::default());
        *installer.install_result.lock().unwrap() = Some(Ok(installed()));
        let host = start(config(root.path.clone(), launcher.clone(), installer.clone(), Arc::new(FakeReady::new(None))));
        // A previous Check found no installation but did see Java.
        host.request(Action::Check).unwrap();
        let checked = wait_phase(&host, Phase::NotInstalled);
        assert_eq!(checked.java, Java::Available, "{label}");
        // The Download's own probe is refused; the promotion is still the new fact.
        root.script("probe.sh", bad_probe);
        host.request(Action::Download).unwrap();
        let promoted = wait_phase(&host, Phase::Installed);
        assert_eq!(promoted.version, "6.6", "{label}");
        assert_eq!(promoted.java, Java::Unknown, "{label}: an unconfirmed probe never reuses an older Java fact");
        assert!(promoted.endpoint.is_none(), "{label}");
        assert_eq!(host.request(Action::Start), Err(Issue::JavaMissing), "{label}: Start needs a confirmed Java");
        // The promoted installation is now what `locate` sees; a fresh Check confirms Java.
        *installer.locate_result.lock().unwrap() = Some(installed());
        root.script("probe.sh", "printf 'PROBE AVAILABLE\\n'");
        host.request(Action::Check).unwrap();
        let rechecked = wait_phase(&host, Phase::Installed);
        assert_eq!(rechecked.java, Java::Available, "{label}");
        assert_eq!(rechecked.version, "6.6", "{label}");
    }
}

/// A promotion that completed while the owner's cancel was pending is a real installation: it is
/// kept, verified and with its version, but the Download never confirmed Java, so `Unknown` is the
/// only truthful fact until a fresh Check.
#[cfg(unix)]
#[test]
fn a_download_promoted_while_cancelled_keeps_the_installation_but_no_java_fact() {
    let _fixture = host_fixture();
    let root = TempRoot::new("download-promote-cancel");
    let launcher = Arc::new(ScriptLauncher::new(root.script("probe.sh", "printf 'PROBE AVAILABLE\\n'"), root.script("serve.sh", "exit 7")));
    let installer = Arc::new(FakeInstall::default());
    let (release, gate) = mpsc::channel();
    *installer.install_gate.lock().unwrap() = Some(gate);
    *installer.install_result.lock().unwrap() = Some(Ok(installed()));
    installer.ignore_cancel.store(true, Ordering::SeqCst);
    let host = start(config(root.path.clone(), launcher, installer.clone(), Arc::new(FakeReady::new(None))));
    host.request(Action::Check).unwrap();
    let checked = wait_phase(&host, Phase::NotInstalled);
    assert_eq!(checked.java, Java::Available);

    host.request(Action::Download).unwrap();
    wait_for(&host, WAIT, "the installer to be busy", |status| status.phase == Phase::Downloading);
    assert_eq!(host.request(Action::Cancel), Ok(()));
    assert_eq!(host.status().phase, Phase::Cancelling);
    let _ = release.send(());
    let promoted = wait_for(&host, WAIT, "the completed promotion", |status| matches!(status.phase, Phase::Installed | Phase::Cancelled));
    assert_eq!(promoted.phase, Phase::Installed, "the promotion really happened and must not be reported as a refusal");
    assert_eq!(promoted.version, "6.6");
    assert_eq!(promoted.issue, Some(Issue::Cancelled));
    assert_eq!(promoted.java, Java::Unknown, "an unprobed promotion invents no Java fact");
    assert_eq!(host.request(Action::Start), Err(Issue::JavaMissing));
    // The verified installation is still there: one Check makes it startable.
    *installer.locate_result.lock().unwrap() = Some(installed());
    host.request(Action::Check).unwrap();
    let rechecked = wait_phase(&host, Phase::Installed);
    assert_eq!(rechecked.java, Java::Available);
    assert_eq!(rechecked.version, "6.6");
}

/// A confirmed integrity failure is a fact about the installation, not an unknown cancelled step:
/// it must be reported as such even while the owner's cancel is pending, and nothing may stay
/// verified afterwards.
#[cfg(unix)]
#[test]
fn a_confirmed_integrity_failure_is_not_relabelled_as_a_cancelled_download() {
    let _fixture = host_fixture();
    let root = TempRoot::new("download-integrity-cancel");
    let launcher = Arc::new(ScriptLauncher::new(root.script("probe.sh", "printf 'PROBE AVAILABLE\\n'"), root.script("serve.sh", "exit 7")));
    let installer = Arc::new(FakeInstall::default());
    let (release, gate) = mpsc::channel();
    *installer.install_gate.lock().unwrap() = Some(gate);
    *installer.install_result.lock().unwrap() = Some(Err(InstallError::Integrity));
    installer.ignore_cancel.store(true, Ordering::SeqCst);
    let host = start(config(root.path.clone(), launcher, installer, Arc::new(FakeReady::new(None))));
    host.request(Action::Check).unwrap();
    wait_phase(&host, Phase::NotInstalled);
    host.request(Action::Download).unwrap();
    wait_for(&host, WAIT, "the installer to be busy", |status| status.phase == Phase::Downloading);
    assert_eq!(host.request(Action::Cancel), Ok(()));
    let _ = release.send(());
    let failed = wait_for(&host, WAIT, "the confirmed integrity failure", |status| matches!(status.phase, Phase::Failed | Phase::Cancelled));
    assert_eq!(failed.phase, Phase::Failed, "a confirmed integrity failure is a fact, not a cancellation");
    assert_eq!(failed.issue, Some(Issue::IntegrityFailed));
    assert_eq!(failed.version, "", "nothing may stay verified after an integrity failure");
    assert!(failed.endpoint.is_none());
    assert_eq!(host.request(Action::Start), Err(Issue::NotInstalled), "an invalidated installation cannot start");
}

// ---------------------------------------------------------------------------------------------
// Start, Stop, Cancel and a helper that dies on its own.
// ---------------------------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn stop_revokes_the_endpoint_before_the_helper_has_exited() {
    let _fixture = host_fixture();
    let root = TempRoot::new("stop");
    let pid_file = root.path.join("helper.pid");
    let launcher = Arc::new(ScriptLauncher::new(
        root.script("probe.sh", "printf 'PROBE AVAILABLE\\n'"),
        root.script("serve.sh", &format!("echo $$ > '{}'\nprintf 'BOUND 42401\\n'\ncat >/dev/null\nsleep 0.3\nexit 0", pid_file.display())),
    ));
    let installer = Arc::new(FakeInstall::with_installed(installed()));
    let host = start(config(root.path.clone(), launcher, installer, Arc::new(FakeReady::new(None))));
    host.request(Action::Check).unwrap();
    let installed_status = wait_phase(&host, Phase::Installed);
    assert_eq!(installed_status.java, Java::Available);
    host.request(Action::Start).unwrap();
    let running = wait_phase(&host, Phase::Running);
    assert_eq!(running.endpoint.as_deref(), Some("http://127.0.0.1:42401"));
    assert_eq!(running.epoch, 1);

    assert_eq!(host.request(Action::Stop), Ok(()));
    let stopping = host.status();
    assert_eq!(stopping.phase, Phase::Stopping);
    assert!(stopping.endpoint.is_none(), "Stop revokes the endpoint before any async cleanup");
    assert_eq!(stopping.epoch, 2, "revoking a running instance is one epoch move");
    assert!(process_is_alive(&pid_file), "the owned helper is still alive while cleanup is pending");
    let stopped = wait_phase(&host, Phase::Stopped);
    assert!(stopped.endpoint.is_none());
    wait_gone(&pid_file);
}

#[cfg(unix)]
#[test]
fn cancel_during_start_cannot_publish_late_and_stays_pending_until_reaped() {
    let _fixture = host_fixture();
    let root = TempRoot::new("cancel-start");
    let pid_file = root.path.join("helper.pid");
    let launcher = Arc::new(ScriptLauncher::new(
        root.script("probe.sh", "printf 'PROBE AVAILABLE\\n'"),
        root.script(
            "serve.sh",
            &format!("echo $$ > '{}'\nsleep 0.25\nprintf 'BOUND 42402\\n'\ncat >/dev/null\nsleep 0.25\nexit 0", pid_file.display()),
        ),
    ));
    let installer = Arc::new(FakeInstall::with_installed(installed()));
    let readiness = Arc::new(FakeReady::new(None));
    let host = start(config(root.path.clone(), launcher, installer, readiness.clone()));
    host.request(Action::Check).unwrap();
    wait_phase(&host, Phase::Installed);
    host.request(Action::Start).unwrap();
    wait_phase(&host, Phase::Starting);
    // This test is about a helper that already exists: wait for the fixture itself, so the
    // cancellation is not the earlier, helper-free case (which the gate tests own).
    wait_for(&host, WAIT, "the Serve helper to start", |_| process_is_alive(&pid_file));
    assert_eq!(host.request(Action::Cancel), Ok(()));
    let cancelling = host.status();
    assert_eq!(cancelling.phase, Phase::Cancelling);
    assert!(cancelling.endpoint.is_none());
    let started = Instant::now();
    while started.elapsed() < Duration::from_millis(150) {
        let status = host.status();
        assert_ne!(status.phase, Phase::Running, "a cancelled start never publishes");
        assert!(status.endpoint.is_none());
        assert_eq!(status.phase, Phase::Cancelling, "cleanup is still pending until the helper is reaped");
        thread::sleep(Duration::from_millis(10));
    }
    let stopped = wait_phase(&host, Phase::Stopped);
    assert_eq!(stopped.issue, Some(Issue::Cancelled));
    assert_eq!(stopped.version, "6.6", "a verified installation survives a cancelled start");
    assert!(stopped.endpoint.is_none());
    assert_eq!(readiness.calls.load(Ordering::SeqCst), 0, "readiness is never reached after cancellation");
    wait_gone(&pid_file);
}

/// A Start the owner has already cancelled must not even ask the launcher for a Serve command, let
/// alone spawn a helper. The gate parks the worker at the top of `run_start` so the ordering is
/// deterministic; the terminal path is the ordinary cancelled one, with the verified facts kept.
#[cfg(unix)]
#[test]
fn a_start_cancelled_before_the_worker_launches_never_asks_for_a_serve_helper() {
    let _fixture = host_fixture();
    let root = TempRoot::new("cancel-pre-launch");
    let marker = root.path.join("serve.started");
    let launcher = Arc::new(ScriptLauncher::new(
        root.script("probe.sh", "printf 'PROBE AVAILABLE\\n'"),
        root.script("serve.sh", &format!("touch '{}'\nprintf 'BOUND 42520\\n'\ncat >/dev/null\nexit 0", marker.display())),
    ));
    let readiness = Arc::new(FakeReady::new(None));
    let gate = LaunchGate::new();
    let host = start(
        config(root.path.clone(), launcher.clone(), Arc::new(FakeInstall::with_installed(installed())), readiness.clone())
            .with_launch_gate(gate.clone()),
    );
    host.request(Action::Check).unwrap();
    wait_phase(&host, Phase::Installed);
    gate.arm();
    host.request(Action::Start).unwrap();
    assert!(gate.wait_parked(WAIT), "the worker reached run_start");
    assert_eq!(host.status().phase, Phase::Starting, "the owner's Start is admitted but nothing is launched yet");
    assert_eq!(host.request(Action::Cancel), Ok(()));
    assert_eq!(host.status().phase, Phase::Cancelling);
    gate.open();
    let stopped = wait_phase(&host, Phase::Stopped);
    assert_eq!(stopped.issue, Some(Issue::Cancelled));
    assert_eq!(stopped.version, "6.6", "the verified installation is untouched");
    assert!(stopped.endpoint.is_none());
    assert_eq!(launcher.serve_calls.load(Ordering::SeqCst), 0, "a cancelled Start never even builds the Serve command");
    assert!(!marker.exists(), "no Serve helper was spawned");
    assert_eq!(event_slots_in_use(), 0, "no event-reader slot was admitted for a helper");
    assert_eq!(readiness.calls.load(Ordering::SeqCst), 0);
}

/// The same guarantee after the command has been built: a cancellation known before `Helper::spawn`
/// must still leave no helper process and no admitted reader behind.
#[cfg(unix)]
#[test]
fn a_start_cancelled_while_the_launcher_builds_its_command_spawns_no_helper() {
    let _fixture = host_fixture();
    let root = TempRoot::new("cancel-in-launcher");
    let marker = root.path.join("serve.started");
    let launcher = Arc::new(GatedLauncher::new(
        root.script("probe.sh", "printf 'PROBE AVAILABLE\\n'"),
        root.script("serve.sh", &format!("touch '{}'\nprintf 'BOUND 42521\\n'\ncat >/dev/null\nexit 0", marker.display())),
    ));
    let (entered, entered_rx) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let readiness = Arc::new(FakeReady::new(None));
    let host = start(config(root.path.clone(), launcher.clone(), Arc::new(FakeInstall::with_installed(installed())), readiness.clone()));
    // The Check's own probe runs before the gate is set: only the Serve command is held.
    host.request(Action::Check).unwrap();
    wait_phase(&host, Phase::Installed);
    launcher.entered.lock().unwrap().replace(entered);
    launcher.gate.lock().unwrap().replace(gate);
    host.request(Action::Start).unwrap();
    entered_rx.recv_timeout(WAIT).expect("the launcher is asked for the Serve command");
    assert_eq!(host.request(Action::Cancel), Ok(()));
    assert_eq!(host.status().phase, Phase::Cancelling);
    let _ = release.send(());
    let stopped = wait_phase(&host, Phase::Stopped);
    assert_eq!(stopped.issue, Some(Issue::Cancelled));
    assert_eq!(stopped.version, "6.6");
    assert_eq!(launcher.serve_calls.load(Ordering::SeqCst), 1, "the command was built before the second check");
    assert!(!marker.exists(), "no helper may be spawned once cancellation is known");
    assert_eq!(event_slots_in_use(), 0, "no event-reader slot was admitted for a helper");
    assert_eq!(readiness.calls.load(Ordering::SeqCst), 0);
}

/// The same gate on the Probe seam: a Download whose probe command was being built when the owner
/// cancelled spawns no probe helper, and the promotion it already completed stays a real fact.
#[cfg(unix)]
#[test]
fn a_download_cancelled_while_the_probe_command_is_built_spawns_no_probe_helper() {
    let _fixture = host_fixture();
    let root = TempRoot::new("cancel-probe-launcher");
    let marker = root.path.join("probe.started");
    let launcher = Arc::new(GatedLauncher::new(
        root.script("probe.sh", &format!("touch '{}'\nprintf 'PROBE AVAILABLE\\n'\nexit 0", marker.display())),
        root.script("serve.sh", "exit 7"),
    ));
    let installer = Arc::new(FakeInstall::default());
    *installer.install_result.lock().unwrap() = Some(Ok(installed()));
    let host = start(config(root.path.clone(), launcher.clone(), installer, Arc::new(FakeReady::new(None))));
    // The first Check probes without a gate; only the Download's own probe is held.
    host.request(Action::Check).unwrap();
    let checked = wait_phase(&host, Phase::NotInstalled);
    assert_eq!(checked.java, Java::Available);
    // That Check ran the same fixture, so start the witness from a clean slate.
    std::fs::remove_file(&marker).unwrap();
    let (entered, entered_rx) = mpsc::channel();
    launcher.entered.lock().unwrap().replace(entered);
    let (release, gate) = mpsc::channel();
    launcher.gate.lock().unwrap().replace(gate);
    host.request(Action::Download).unwrap();
    entered_rx.recv_timeout(WAIT).expect("the Download asks for its Probe command");
    assert_eq!(host.request(Action::Cancel), Ok(()));
    assert_eq!(host.status().phase, Phase::Cancelling);
    let _ = release.send(());
    let done = wait_for(&host, WAIT, "the cancelled probe", |status| matches!(status.phase, Phase::Installed | Phase::Cancelled | Phase::Failed));
    assert!(!marker.exists(), "no Probe helper may be spawned once cancellation is known");
    assert_eq!(launcher.probe_calls.load(Ordering::SeqCst), 2, "the command was built before the second check");
    assert_eq!(done.phase, Phase::Installed, "the completed promotion is still a real fact");
    assert_eq!(done.version, "6.6");
    assert_eq!(done.java, Java::Unknown, "the probe never ran, so it confirms no Java fact");
    assert_eq!(done.issue, Some(Issue::Cancelled));
    let started = Instant::now();
    while event_slots_in_use() != 0 {
        assert!(started.elapsed() < WAIT, "a helper reader still holds a slot");
        thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(unix)]
#[test]
fn child_death_while_running_clears_the_endpoint() {
    let _fixture = host_fixture();
    let root = TempRoot::new("death");
    let launcher = Arc::new(ScriptLauncher::new(
        root.script("probe.sh", "printf 'PROBE AVAILABLE\\n'"),
        root.script("serve.sh", "printf 'BOUND 42403\\n'\nsleep 0.25\nexit 3"),
    ));
    let host = start(config(root.path.clone(), launcher, Arc::new(FakeInstall::with_installed(installed())), Arc::new(FakeReady::new(None))));
    host.request(Action::Check).unwrap();
    wait_phase(&host, Phase::Installed);
    host.request(Action::Start).unwrap();
    let running = wait_phase(&host, Phase::Running);
    assert_eq!(running.epoch, 1);
    let failed = wait_phase(&host, Phase::Failed);
    assert!(failed.endpoint.is_none(), "the endpoint dies with its owned helper");
    assert_eq!(failed.epoch, 2);
    assert_eq!(failed.issue, Some(Issue::IntegrityFailed), "exit 3 is the pinned integrity failure");
}

#[cfg(unix)]
#[test]
fn a_bound_marker_without_a_live_helper_is_not_readiness() {
    let _fixture = host_fixture();
    let root = TempRoot::new("dead-bound");
    let launcher = Arc::new(ScriptLauncher::new(
        root.script("probe.sh", "printf 'PROBE AVAILABLE\\n'"),
        root.script("serve.sh", "printf 'BOUND 42404\\n'\nexit 0"),
    ));
    let readiness = Arc::new(FakeReady::new(None));
    let host = start(config(root.path.clone(), launcher, Arc::new(FakeInstall::with_installed(installed())), readiness.clone()));
    host.request(Action::Check).unwrap();
    wait_phase(&host, Phase::Installed);
    host.request(Action::Start).unwrap();
    let started = Instant::now();
    loop {
        let status = host.status();
        assert!(status.endpoint.is_none(), "a dead helper's BOUND never publishes");
        if status.phase == Phase::Failed {
            break;
        }
        assert!(started.elapsed() < WAIT, "timed out waiting for the dead helper to be refused");
        thread::sleep(Duration::from_millis(5));
    }
    assert!(readiness.calls.load(Ordering::SeqCst) <= 1, "readiness is never trusted for a dead child");
}

#[cfg(unix)]
#[test]
fn warmup_is_required_before_publishing_and_requires_both_languages() {
    let _fixture = host_fixture();
    let checked = |count: usize| CheckOutcome { diagnostics: Vec::new(), checked_segments: count, skipped_segments: 0, partial: false };
    assert!(warm_outcomes_ok(&checked(1), &checked(1)));
    assert!(!warm_outcomes_ok(&checked(0), &checked(1)), "English alone is not readiness");
    assert!(!warm_outcomes_ok(&checked(1), &checked(0)), "French alone is not readiness");
    assert!(!warm_outcomes_ok(&checked(0), &checked(0)));

    let root = TempRoot::new("warmup");
    let pid_file = root.path.join("helper.pid");
    let launcher = Arc::new(ScriptLauncher::new(
        root.script("probe.sh", "printf 'PROBE AVAILABLE\\n'"),
        root.script("serve.sh", &format!("echo $$ > '{}'\nprintf 'BOUND 42405\\n'\ncat >/dev/null\nexit 0", pid_file.display())),
    ));
    let readiness = Arc::new(FakeReady::watching(Some(Issue::StartFailed), pid_file.clone()));
    let host = start(config(root.path.clone(), launcher, Arc::new(FakeInstall::with_installed(installed())), readiness.clone()));
    host.request(Action::Check).unwrap();
    wait_phase(&host, Phase::Installed);
    host.request(Action::Start).unwrap();
    let failed = wait_phase(&host, Phase::Failed);
    assert_eq!(failed.issue, Some(Issue::StartFailed));
    assert!(failed.endpoint.is_none(), "BOUND alone never publishes");
    assert_eq!(readiness.calls.load(Ordering::SeqCst), 1);
    assert!(readiness.helper_was_alive.load(Ordering::SeqCst), "the readiness exchange is only asked while the owned helper is alive");
    let endpoint = readiness.endpoints.lock().unwrap().first().cloned().unwrap_or_default();
    assert_eq!(endpoint, "http://127.0.0.1:42405");
    wait_gone(&pid_file);
}

#[cfg(unix)]
#[test]
fn drop_returns_within_its_bound_and_closes_the_lifetime_pipe() {
    let _fixture = host_fixture();
    let root = TempRoot::new("drop");
    let pid_file = root.path.join("helper.pid");
    let launcher = Arc::new(ScriptLauncher::new(
        root.script("probe.sh", "printf 'PROBE AVAILABLE\\n'"),
        root.script("serve.sh", &format!("echo $$ > '{}'\nprintf 'BOUND 42406\\n'\ncat >/dev/null\nsleep 0.2\nexit 0", pid_file.display())),
    ));
    let host = start(config(root.path.clone(), launcher, Arc::new(FakeInstall::with_installed(installed())), Arc::new(FakeReady::new(None))));
    host.request(Action::Check).unwrap();
    wait_phase(&host, Phase::Installed);
    host.request(Action::Start).unwrap();
    wait_phase(&host, Phase::Running);
    let started = Instant::now();
    drop(host);
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_millis(700), "dropping the service must not wait for the helper, took {elapsed:?}");
    wait_gone(&pid_file);
}

// ---------------------------------------------------------------------------------------------
// Protocol hostility: a bad frame is refused whole, never repaired into a later good one.
// ---------------------------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn malformed_partial_and_oversized_helper_output_is_refused() {
    let _fixture = host_fixture();
    let cases =
        [("malformed", "printf 'GARBAGE\\nPROBE AVAILABLE\\n'"), ("partial", "printf 'PROBE AVAILABLE'"), ("oversized", "printf '%70s\\n' X")];
    for (name, body) in cases {
        let root = TempRoot::new(&format!("hostile-{name}"));
        let launcher = Arc::new(ScriptLauncher::new(root.script("probe.sh", body), root.script("serve.sh", "exit 7")));
        let host = start(config(root.path.clone(), launcher, Arc::new(FakeInstall::with_installed(installed())), Arc::new(FakeReady::new(None))));
        host.request(Action::Check).unwrap();
        let status = wait_for(&host, WAIT, "a refused probe", |status| matches!(status.phase, Phase::Failed | Phase::Installed));
        assert_eq!(status.phase, Phase::Failed, "{name} helper output must not be accepted");
        assert_eq!(status.issue, Some(Issue::StartFailed), "{name}");
        assert!(status.endpoint.is_none(), "{name}");
    }
}

#[cfg(unix)]
#[test]
fn a_malformed_bound_frame_never_publishes() {
    let _fixture = host_fixture();
    let root = TempRoot::new("bad-bound");
    let launcher = Arc::new(ScriptLauncher::new(
        root.script("probe.sh", "printf 'PROBE AVAILABLE\\n'"),
        root.script("serve.sh", "printf 'BOUND not-a-port\\nGARBAGE\\nBOUND 42407\\n'\ncat >/dev/null\nexit 0"),
    ));
    let readiness = Arc::new(FakeReady::new(None));
    let host = start(config(root.path.clone(), launcher, Arc::new(FakeInstall::with_installed(installed())), readiness.clone()));
    host.request(Action::Check).unwrap();
    wait_phase(&host, Phase::Installed);
    host.request(Action::Start).unwrap();
    let status = wait_phase(&host, Phase::Failed);
    assert_eq!(status.issue, Some(Issue::StartFailed));
    assert!(status.endpoint.is_none());
    assert_eq!(readiness.calls.load(Ordering::SeqCst), 0);
}

/// A reader's protocol violation moves an owned `Starting` instance out of publication range, but
/// it is not the owner's `Stop`. A warm-up that then answers must fail the start, never be
/// described as an ordinary stopped instance: no endpoint, epoch 0, `Failed`/`StartFailed`.
#[cfg(unix)]
#[test]
fn a_protocol_violation_during_held_warmup_is_a_start_failure_not_a_stop() {
    let _fixture = host_fixture();
    let root = TempRoot::new("bad-tail-warm");
    let pid_file = root.path.join("serve.pid");
    let launcher = Arc::new(ScriptLauncher::new(
        root.script("probe.sh", "printf 'PROBE AVAILABLE\\n'"),
        // A valid BOUND starts the readiness exchange; the malformed frames arrive while it is held.
        root.script(
            "serve.sh",
            &format!("echo $$ > '{}'\nprintf 'BOUND 42503\\n'\nsleep 0.3\nprintf 'BOUND not-a-port\\n'\ncat >/dev/null\nexit 0", pid_file.display()),
        ),
    ));
    let readiness = Arc::new(FakeReady::new(None));
    let (entered, entered_rx) = mpsc::channel();
    readiness.entered.lock().unwrap().replace(entered);
    let (release, gate) = mpsc::channel();
    readiness.gate.lock().unwrap().replace(gate);
    let host = start(config(root.path.clone(), launcher, Arc::new(FakeInstall::with_installed(installed())), readiness.clone()));
    host.request(Action::Check).unwrap();
    wait_phase(&host, Phase::Installed);
    host.request(Action::Start).unwrap();
    entered_rx.recv_timeout(WAIT).expect("readiness is asked only after a valid BOUND");
    let revoked =
        wait_for(&host, WAIT, "the malformed tail to revoke the endpoint", |status| matches!(status.phase, Phase::Stopping | Phase::Failed));
    assert_eq!(revoked.phase, Phase::Stopping, "the owned helper is still being cleaned up");
    assert_eq!(revoked.epoch, 0, "a revoked Starting instance never published");
    assert!(revoked.endpoint.is_none());
    let _ = release.send(());
    let failed = wait_phase(&host, Phase::Failed);
    assert_eq!(failed.issue, Some(Issue::StartFailed), "the helper broke the protocol; this is not the owner's stop");
    assert_eq!(failed.epoch, 0, "the endpoint was never published");
    assert!(failed.endpoint.is_none());
    wait_gone(&pid_file);
}

#[cfg(unix)]
#[test]
fn a_worker_that_cannot_start_restores_a_failed_status_and_admits_no_work() {
    let _fixture = host_fixture();
    let root = TempRoot::new("spawn-fail");
    // A slow probe keeps the second, admitted operation observably in `Checking`.
    let launcher = Arc::new(ScriptLauncher::new(root.script("probe.sh", "sleep 0.3\nprintf 'PROBE MISSING\\n'"), root.script("serve.sh", "exit 7")));
    let installer = Arc::new(FakeInstall::default());
    let fail_once = Arc::new(AtomicBool::new(true));
    let spare = fail_once.clone();
    let spawn: Spawn =
        Arc::new(
            move |name, body| {
                if spare.swap(false, Ordering::SeqCst) { Err(io::Error::other("worker could not start")) } else { real_spawn(name, body) }
            },
        );
    let config = Config::testing(root.path.clone(), wake(), launcher, installer.clone(), Arc::new(FakeReady::new(None)), spawn);
    let host = start(config);
    assert_eq!(host.request(Action::Check), Err(Issue::StartFailed));
    let failed = host.status();
    assert_eq!(failed.phase, Phase::Failed);
    assert_eq!(failed.issue, Some(Issue::StartFailed));
    assert!(failed.endpoint.is_none());
    assert_eq!(installer.locate_calls.load(Ordering::SeqCst), 0, "the failed attempt did no work");
    // The failed attempt left no active phase and no queued operation: a later Check is admitted.
    assert_eq!(host.request(Action::Check), Ok(()));
    wait_phase(&host, Phase::Checking);
    wait_phase(&host, Phase::NotInstalled);
    assert_eq!(installer.locate_calls.load(Ordering::SeqCst), 1, "the later operation really ran");
}

// ---------------------------------------------------------------------------------------------
// Root class 1: a Probe helper is never killed. Closing its lifetime pipe and waiting for the
// owned process to be reaped is the only cleanup; `Installed`/`Failed` is claimed only after it.
// ---------------------------------------------------------------------------------------------

/// A bounded hold: the fixture keeps running (and owns its own fake "Java") until the sentinel
/// exists or the bounded loop ends. It never outlives the test by more than a couple of seconds.
fn hold(body: &str, sentinel: &Path, ticks: u32) -> String {
    format!("{body}\ni=0\nwhile [ ! -e '{}' ]; do\n  i=$((i+1))\n  [ \"$i\" -ge {ticks} ] && break\n  sleep 0.05\ndone", sentinel.display())
}

#[cfg(unix)]
#[test]
fn probe_cleanup_never_kills_the_helper_and_waits_past_the_old_two_second_deadline() {
    let _fixture = host_fixture();
    // The old cleanup killed the short Probe helper after two seconds, which could orphan the Java
    // process that probe owns. The helper here ignores the closed pipe for 2.4s: longer than the
    // deadline the old code killed at, so a kill shows up as an early terminal phase and dead PID.
    let root = TempRoot::new("probe-no-kill");
    let pid_file = root.path.join("probe.pid");
    let sentinel = root.path.join("release");
    let script = hold(&format!("echo $$ > '{}'\nprintf 'PROBE AVAILABLE\\n'", pid_file.display()), &sentinel, 48);
    let launcher = Arc::new(ScriptLauncher::new(root.script("probe.sh", &script), root.script("serve.sh", "exit 7")));
    let host = start(config(root.path.clone(), launcher, Arc::new(FakeInstall::default()), Arc::new(FakeReady::new(None))));
    host.request(Action::Check).unwrap();
    wait_for(&host, WAIT, "the probe helper to start", |_| process_is_alive(&pid_file));
    let started = Instant::now();
    // Past the old 2s kill: the operation is still owned and the helper is still alive.
    while started.elapsed() < Duration::from_millis(2300) {
        let status = host.status();
        assert!(matches!(status.phase, Phase::Checking | Phase::Installed), "no force-kill may end the probe early: {status:?}");
        assert!(process_is_alive(&pid_file), "the probe helper must not be killed by the host");
        thread::sleep(Duration::from_millis(25));
    }
    assert!(matches!(host.status().phase, Phase::Checking | Phase::Installed), "still pending before the helper exits on its own");
    std::fs::write(&sentinel, b"go").unwrap();
    let done = wait_for(&host, WAIT, "the probe helper to be reaped", |status| matches!(status.phase, Phase::Installed | Phase::NotInstalled));
    assert_eq!(done.java, Java::Available, "the owned probe completed and its Java fact is recorded");
    wait_gone(&pid_file);
    assert!(started.elapsed() >= Duration::from_millis(2300), "the operation waited for the owned helper: {started:?}");
}

#[cfg(unix)]
#[test]
fn stopping_a_named_instance_never_kills_its_helper() {
    let _fixture = host_fixture();
    let root = TempRoot::new("stop-no-kill");
    let pid_file = root.path.join("serve.pid");
    let sentinel = root.path.join("release");
    let serve = hold(&format!("echo $$ > '{}'\nprintf 'BOUND 42501\\n'", pid_file.display()), &sentinel, 48);
    let launcher = Arc::new(ScriptLauncher::new(root.script("probe.sh", "printf 'PROBE AVAILABLE\\n'"), root.script("serve.sh", &serve)));
    let host = start(config(root.path.clone(), launcher, Arc::new(FakeInstall::with_installed(installed())), Arc::new(FakeReady::new(None))));
    host.request(Action::Check).unwrap();
    wait_phase(&host, Phase::Installed);
    host.request(Action::Start).unwrap();
    wait_phase(&host, Phase::Running);
    assert_eq!(host.request(Action::Stop), Ok(()));
    let stopping = host.status();
    assert_eq!(stopping.phase, Phase::Stopping);
    assert!(stopping.endpoint.is_none());
    // Two hundred milliseconds beyond the old forced cutoff: the owned helper is still ours.
    thread::sleep(Duration::from_millis(2300));
    assert!(process_is_alive(&pid_file), "the Serve helper must be reaped, never killed");
    assert_eq!(host.status().phase, Phase::Stopping, "cleanup stays pending until the helper is actually reaped");
    std::fs::write(&sentinel, b"go").unwrap();
    let stopped = wait_phase(&host, Phase::Stopped);
    assert_eq!(stopped.issue, None);
    assert!(stopped.endpoint.is_none());
    wait_gone(&pid_file);
}

// ---------------------------------------------------------------------------------------------
// Root class 2: a failure revokes the endpoint first, under the state lock, and stays non-terminal
// while the failed helper is still being cleaned up.
// ---------------------------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn a_running_protocol_violation_revokes_the_endpoint_before_cleanup_and_holds_admission() {
    let _fixture = host_fixture();
    let root = TempRoot::new("fail-revoke");
    let pid_file = root.path.join("serve.pid");
    let sentinel = root.path.join("release");
    let serve = hold(&format!("echo $$ > '{}'\nprintf 'BOUND 42502\\n'\nsleep 0.25\nprintf 'GARBAGE\\n'", pid_file.display()), &sentinel, 80);
    let launcher = Arc::new(ScriptLauncher::new(root.script("probe.sh", "printf 'PROBE AVAILABLE\\n'"), root.script("serve.sh", &serve)));
    let host = start(config(root.path.clone(), launcher, Arc::new(FakeInstall::with_installed(installed())), Arc::new(FakeReady::new(None))));
    host.request(Action::Check).unwrap();
    wait_phase(&host, Phase::Installed);
    host.request(Action::Start).unwrap();
    let running = wait_phase(&host, Phase::Running);
    assert_eq!(running.endpoint.as_deref(), Some("http://127.0.0.1:42502"));
    // The bad frame revokes the endpoint at once, under the same lock as the stop request.
    let revoked =
        wait_for(&host, WAIT, "the endpoint to be revoked by the failure", |status| matches!(status.phase, Phase::Stopping | Phase::Failed));
    assert!(revoked.endpoint.is_none(), "a failed helper never keeps its endpoint");
    if revoked.phase == Phase::Stopping {
        assert!(process_is_alive(&pid_file), "the helper is still alive while cleanup is pending");
        assert_eq!(host.request(Action::Start), Err(Issue::Busy), "a newly failed instance cannot be replaced before cleanup");
        assert_eq!(host.request(Action::Check), Err(Issue::Busy));
    }
    std::fs::write(&sentinel, b"go").unwrap();
    let failed = wait_phase(&host, Phase::Failed);
    assert!(failed.endpoint.is_none());
    assert_eq!(failed.issue, Some(Issue::StartFailed));
    wait_gone(&pid_file);
    // Only after the owned helper is reaped may fresh work be admitted.
    assert_ne!(host.request(Action::Check), Err(Issue::Busy), "the reaped failure no longer holds admission");
}

#[cfg(unix)]
#[test]
fn a_failed_serve_exit_code_is_reported_truthfully_after_the_helper_is_reaped() {
    let _fixture = host_fixture();
    // A `FAILED` event is a fixed "no" but not the reason; the owned process's exit code is. The
    // helper holds briefly so the host must reap before it may finish.
    for (code, expected_issue, expected_java) in [
        // An integrity failure invalidates the installation but says nothing new about Java, so
        // the last verified fact is kept, never invented.
        (3, Issue::IntegrityFailed, Java::Available),
        // The pinned installation is gone; the earlier successful probe still says Java is present.
        (4, Issue::NotInstalled, Java::Available),
        (5, Issue::JavaMissing, Java::Missing),
        (6, Issue::JavaTooOld, Java::TooOld),
    ] {
        let root = TempRoot::new(&format!("serve-exit-{code}"));
        let launcher = Arc::new(ScriptLauncher::new(
            root.script("probe.sh", "printf 'PROBE AVAILABLE\\n'"),
            root.script("serve.sh", &format!("printf 'FAILED\\n'\nsleep 0.05\nexit {code}")),
        ));
        let host = start(config(root.path.clone(), launcher, Arc::new(FakeInstall::with_installed(installed())), Arc::new(FakeReady::new(None))));
        host.request(Action::Check).unwrap();
        wait_phase(&host, Phase::Installed);
        host.request(Action::Start).unwrap();
        let failed = wait_phase(&host, Phase::Failed);
        assert_eq!(failed.issue, Some(expected_issue), "exit {code} keeps its own meaning");
        assert_eq!(failed.java, expected_java, "exit {code} keeps its own Java fact");
        assert!(failed.endpoint.is_none());
    }
}

// ---------------------------------------------------------------------------------------------
// Root class 3: a Probe is accepted only when it is one Probe event, a clean natural EOF and a
// successful owned exit. A valid prefix plus a hostile tail is refused whole.
// ---------------------------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn a_probe_with_a_nonzero_exit_is_refused_after_a_clean_line() {
    let _fixture = host_fixture();
    let root = TempRoot::new("probe-exit7");
    let pid_file = root.path.join("probe.pid");
    let launcher = Arc::new(ScriptLauncher::new(
        root.script("probe.sh", &format!("echo $$ > '{}'\nprintf 'PROBE AVAILABLE\\n'\nexit 7", pid_file.display())),
        root.script("serve.sh", "exit 7"),
    ));
    let host = start(config(root.path.clone(), launcher, Arc::new(FakeInstall::with_installed(installed())), Arc::new(FakeReady::new(None))));
    host.request(Action::Check).unwrap();
    let failed = wait_for(&host, WAIT, "a nonzero probe exit to be refused", |status| status.phase == Phase::Failed);
    assert_eq!(failed.issue, Some(Issue::StartFailed), "a clean line with a failed process is not a verified probe");
    assert!(failed.endpoint.is_none());
    assert_eq!(failed.java, Java::Unknown, "an unproven helper never invents a Java fact");
    wait_gone(&pid_file);
}

#[cfg(unix)]
#[test]
fn a_probe_prefix_with_a_hostile_tail_is_refused_whole() {
    let _fixture = host_fixture();
    // One valid Probe event followed by an oversized frame: the whole helper is not the helper.
    let root = TempRoot::new("probe-tail");
    let launcher = Arc::new(ScriptLauncher::new(
        root.script("probe.sh", "printf 'PROBE AVAILABLE\\n'\nsleep 0.02\nprintf '%70s\\n' X\nexit 0"),
        root.script("serve.sh", "exit 7"),
    ));
    let host = start(config(root.path.clone(), launcher, Arc::new(FakeInstall::with_installed(installed())), Arc::new(FakeReady::new(None))));
    host.request(Action::Check).unwrap();
    let failed = wait_for(&host, WAIT, "a hostile tail to be refused", |status| status.phase == Phase::Failed);
    assert_eq!(failed.issue, Some(Issue::StartFailed));
    assert!(failed.endpoint.is_none());
}

#[cfg(unix)]
#[test]
fn a_fast_zero_exit_after_a_valid_probe_is_not_a_start_failure() {
    let _fixture = host_fixture();
    // The child can exit before the reader has queued its last frame. That race must still yield
    // one accepted Probe, never a `StartFailed` that discards a truthful answer.
    for attempt in 0..16 {
        let root = TempRoot::new(&format!("probe-fast-{attempt}"));
        let launcher =
            Arc::new(ScriptLauncher::new(root.script("probe.sh", "printf 'PROBE AVAILABLE\\n'\nexit 0"), root.script("serve.sh", "exit 7")));
        let host = start(config(root.path.clone(), launcher, Arc::new(FakeInstall::default()), Arc::new(FakeReady::new(None))));
        host.request(Action::Check).unwrap();
        let done = wait_for(&host, WAIT, "the fast probe", |status| matches!(status.phase, Phase::Installed | Phase::NotInstalled | Phase::Failed));
        assert_eq!(done.phase, Phase::NotInstalled, "attempt {attempt}: a clean zero exit publishes no failed check");
        assert_eq!(done.java, Java::Available);
        assert_eq!(done.issue, None);
        assert!(done.endpoint.is_none());
    }
}

#[cfg(unix)]
#[test]
fn a_probe_plus_a_second_event_is_refused_whole() {
    let _fixture = host_fixture();
    let root = TempRoot::new("probe-extra");
    let launcher =
        Arc::new(ScriptLauncher::new(root.script("probe.sh", "printf 'PROBE AVAILABLE\\nPROBE MISSING\\n'"), root.script("serve.sh", "exit 7")));
    let host = start(config(root.path.clone(), launcher, Arc::new(FakeInstall::default()), Arc::new(FakeReady::new(None))));
    host.request(Action::Check).unwrap();
    let failed = wait_for(&host, WAIT, "extra probe frames to be refused", |status| status.phase == Phase::Failed);
    assert_eq!(failed.issue, Some(Issue::StartFailed));
    assert!(failed.endpoint.is_none());
}

// ---------------------------------------------------------------------------------------------
// Root class 4: a revoked generation is not a global revocation. A reaped helper's detached reader
// can still deliver a hostile tail later; that callback belongs to the operation that admitted it
// and must never move a newer owned instance.
// ---------------------------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn a_stale_detached_reader_cannot_revoke_a_newer_running_instance() {
    let _fixture = host_fixture();
    let root = TempRoot::new("stale-revoke");
    let old_pid = root.path.join("old.pid");
    let new_pid = root.path.join("new.pid");
    let release = root.path.join("release");
    // The old Serve helper binds, leaves a background descendant holding the write end of its
    // stdout and exits on Stop. The descendant's malformed tail arrives only after a newer
    // instance is running.
    let old_serve = root.script(
        "serve-old.sh",
        &format!(
            "echo $$ > '{}'\nprintf 'BOUND 42601\\n'\n( while [ ! -e '{}' ]; do sleep 0.02; done; printf 'GARBAGE\\n' ) &\ncat >/dev/null\nexit 0",
            old_pid.display(),
            release.display()
        ),
    );
    let new_serve = root.script("serve-new.sh", &format!("echo $$ > '{}'\nprintf 'BOUND 42602\\n'\ncat >/dev/null\nexit 0", new_pid.display()));
    let launcher = Arc::new(SequencedLauncher::new(root.script("probe.sh", "printf 'PROBE AVAILABLE\\n'"), [old_serve, new_serve]));
    let host = start(config(root.path.clone(), launcher, Arc::new(FakeInstall::with_installed(installed())), Arc::new(FakeReady::new(None))));
    host.request(Action::Check).unwrap();
    wait_phase(&host, Phase::Installed);

    host.request(Action::Start).unwrap();
    let first = wait_phase(&host, Phase::Running);
    assert_eq!(first.endpoint.as_deref(), Some("http://127.0.0.1:42601"));
    assert_eq!(first.epoch, 1);
    assert!(process_is_alive(&old_pid));

    // Stopping the old instance reaps its own process, but its descendant keeps the old stdout open.
    assert_eq!(host.request(Action::Stop), Ok(()));
    let stopped = wait_phase(&host, Phase::Stopped);
    assert!(stopped.endpoint.is_none());
    assert_eq!(stopped.epoch, 2);
    assert_eq!(event_slots_in_use(), 1, "the old helper's reader is detached on its descendant's pipe");

    host.request(Action::Start).unwrap();
    let second = wait_phase(&host, Phase::Running);
    assert_eq!(second.endpoint.as_deref(), Some("http://127.0.0.1:42602"));
    assert_eq!(second.epoch, 3);
    assert_eq!(event_slots_in_use(), 2, "the stale detached reader and the live instance's reader");
    assert!(process_is_alive(&new_pid));

    // The old descendant's malformed tail reaches the old reader while a newer instance is Running.
    std::fs::write(&release, b"go").unwrap();
    let started = Instant::now();
    while event_slots_in_use() != 1 {
        assert!(started.elapsed() < WAIT, "the stale reader never released its slot");
        thread::sleep(Duration::from_millis(5));
    }
    let after = host.status();
    assert_eq!(after.phase, Phase::Running, "a stale reader never stops a newer instance: {after:?}");
    assert_eq!(after.endpoint.as_deref(), Some("http://127.0.0.1:42602"));
    assert_eq!(after.epoch, 3, "a stale reader moves no epoch");
    assert!(process_is_alive(&new_pid), "the new owned helper is still alive");

    // The newer instance is still fully owned: an ordinary Stop ends it.
    assert_eq!(host.request(Action::Stop), Ok(()));
    let ended = wait_phase(&host, Phase::Stopped);
    assert_eq!(ended.issue, None);
    assert!(ended.endpoint.is_none());
    wait_gone(&new_pid);
}

// ---------------------------------------------------------------------------------------------
// Root class 5: an I/O error from `try_wait` is not proof of exit. The child stays owned, no
// unknown status is cached, and cleanup keeps waiting for the positive status the OS gives later.
// ---------------------------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn an_unverified_wait_error_keeps_the_child_owned_until_its_real_exit() {
    let _fixture = host_fixture();
    let root = TempRoot::new("wait-error");
    let script = root.script("idle.sh", "printf 'BOUND 42610\\n'\nsleep 0.2\nexit 0");
    let mut command = Command::new(&script);
    command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    let mut helper = Helper::spawn(command, None).expect("the helper fixture starts");

    // The injected result stands in for a real `try_wait` failure: it proves nothing about the child.
    helper.inject_wait_error_once();
    assert_eq!(helper.exited(), None, "an I/O error never reports an exit");
    assert!(helper.owns_child(), "the child is retained until a positive exit is observed");
    helper.inject_wait_error_once();
    assert!(!helper.alive(), "unverified liveness is never ownership proof");
    assert!(helper.owns_child(), "an unverified poll still retains the child");

    // Cleanup observes the child's own exit, zero, instead of claiming an unknown reaped status.
    assert_eq!(helper.reap(), Some(Some(0)));
    assert!(!helper.owns_child());
}

// ---------------------------------------------------------------------------------------------
// Root class 6: bounded, process-wide event-reader admission. A reader blocked on a descendant's
// pipe must never be joined, and new work is refused rather than unboundedly threaded.
//
// Every test in this module takes `HOST_FIXTURE_LOCK`, because every one of them either reads the
// one process-wide pool or spawns a helper whose reader takes a slot from it. That keeps the
// module's own tests from racing each other under plain parallel `libtest` (the rest of the
// workspace still runs in parallel); only the explicit pool arithmetic below additionally assumes
// that no reader from an earlier test is still holding a slot.
// ---------------------------------------------------------------------------------------------

#[test]
fn the_event_reader_pool_refuses_before_spawning() {
    let _fixture = host_fixture();
    let _cap = PoolCap::lower(1);
    let first = acquire_event_slot();
    assert!(first.is_some(), "one free slot is admitted");
    assert!(acquire_event_slot().is_none(), "a saturated reader pool admits no more helpers");
    drop(first);
    assert!(acquire_event_slot().is_some(), "released slots are reusable");
}

#[cfg(unix)]
#[test]
fn a_full_reader_pool_fails_the_operation_without_starting_a_helper() {
    let _fixture = host_fixture();
    let _cap = PoolCap::lower(1);
    let held = acquire_event_slot().expect("the one test slot");
    let root = TempRoot::new("pool-full");
    let marker = root.path.join("probe.started");
    let launcher = Arc::new(ScriptLauncher::new(
        root.script("probe.sh", &format!("touch '{}'\nprintf 'PROBE AVAILABLE\\n'", marker.display())),
        root.script("serve.sh", "exit 7"),
    ));
    let host = start(config(root.path.clone(), launcher, Arc::new(FakeInstall::default()), Arc::new(FakeReady::new(None))));
    host.request(Action::Check).unwrap();
    let failed =
        wait_for(&host, WAIT, "the refused operation", |status| matches!(status.phase, Phase::Failed | Phase::Installed | Phase::NotInstalled));
    assert_eq!(failed.phase, Phase::Failed, "no reader permit means no operation");
    assert_eq!(failed.issue, Some(Issue::StartFailed));
    assert!(!marker.exists(), "no helper may be spawned without admitted event-reader capacity");
    drop(held);
    host.request(Action::Check).unwrap();
    let recovered = wait_for(&host, WAIT, "the next operation with capacity", |status| {
        matches!(status.phase, Phase::Installed | Phase::NotInstalled | Phase::Failed)
    });
    assert_eq!(recovered.phase, Phase::NotInstalled, "capacity is reusable after the earlier refusal");
}

#[cfg(unix)]
#[test]
fn a_descendant_holding_stdout_never_hangs_the_worker_and_only_holds_its_own_slot() {
    let _fixture = host_fixture();
    let _cap = PoolCap::lower(2);
    let root = TempRoot::new("descendant");
    let script = root.script("probe.sh", "sleep 2 &\nprintf 'PROBE AVAILABLE\\n'\nexit 0");
    let mut command = Command::new(&script);
    command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    let started = Instant::now();
    let mut helper = Helper::spawn(command, None).expect("the helper fixture starts");
    // The direct child is reaped at once, but a descendant still holds the write end of stdout.
    helper.reap();
    assert!(started.elapsed() < Duration::from_millis(1500), "reaping never joins a reader its descendant keeps blocked");
    let slots = event_slots_in_use();
    assert_eq!(slots, 1, "a blocked detached reader keeps exactly its own permit while the descendant holds the pipe");
    assert!(acquire_event_slot().is_some(), "capacity stays bounded and available while one reader is detached");
    drop(helper);
    thread::sleep(Duration::from_millis(2600));
    assert_eq!(event_slots_in_use(), 0, "the detached slot is released when the descendant ends");
    reset_event_pool();
}

#[cfg(unix)]
#[test]
fn a_readiness_panic_after_binding_fails_without_leaking_a_running_endpoint() {
    let _fixture = host_fixture();
    let root = TempRoot::new("panic-ready");
    let pid_file = root.path.join("serve.pid");
    let launcher = Arc::new(ScriptLauncher::new(
        root.script("probe.sh", "printf 'PROBE AVAILABLE\\n'"),
        root.script("serve.sh", &format!("echo $$ > '{}'\nprintf 'BOUND 42510\\n'\ncat >/dev/null\nexit 0", pid_file.display())),
    ));
    struct PanicReady;
    impl Readiness for PanicReady {
        fn warm(&self, _endpoint: &str, _deadline: Instant, _cancelled: &dyn Fn() -> bool) -> Result<(), Issue> {
            std::panic::panic_any("injected readiness panic");
        }
    }
    let host = start(config(root.path.clone(), launcher, Arc::new(FakeInstall::with_installed(installed())), Arc::new(PanicReady)));
    host.request(Action::Check).unwrap();
    wait_phase(&host, Phase::Installed);
    host.request(Action::Start).unwrap();
    let failed = wait_for(&host, WAIT, "the panic to become a fixed failure", |status| status.phase == Phase::Failed);
    assert_eq!(failed.issue, Some(Issue::StartFailed));
    assert!(failed.endpoint.is_none(), "a panicking readiness exchange may never leave an endpoint");
    wait_gone(&pid_file);
    // The owned helper was reaped, so a new admitted operation may run.
    assert_eq!(host.request(Action::Check), Ok(()));
    wait_for(&host, WAIT, "fresh work after the panic", |status| matches!(status.phase, Phase::Installed | Phase::NotInstalled));
}

#[cfg(unix)]
#[test]
fn an_installer_panic_before_any_child_is_a_fixed_failure() {
    let _fixture = host_fixture();
    let root = TempRoot::new("panic-install");
    let launcher = Arc::new(ScriptLauncher::new(root.script("probe.sh", "printf 'PROBE AVAILABLE\\n'"), root.script("serve.sh", "exit 7")));
    let installer = Arc::new(FakeInstall::default());
    installer.locate_panics.store(true, Ordering::SeqCst);
    let host = start(config(root.path.clone(), launcher.clone(), installer.clone(), Arc::new(FakeReady::new(None))));
    host.request(Action::Check).unwrap();
    let failed = wait_for(&host, WAIT, "the installer panic", |status| status.phase == Phase::Failed);
    assert_eq!(failed.issue, Some(Issue::StartFailed));
    assert!(failed.endpoint.is_none());
    assert_eq!(launcher.probe_calls.load(Ordering::SeqCst), 0, "no helper is started after a failed locate");
    // A later operation still runs.
    host.request(Action::Check).unwrap();
    wait_phase(&host, Phase::NotInstalled);
}

#[cfg(unix)]
#[test]
fn an_unexpected_clean_exit_while_serving_is_never_reported_as_running() {
    let _fixture = host_fixture();
    let root = TempRoot::new("clean-exit");
    let launcher = Arc::new(ScriptLauncher::new(
        root.script("probe.sh", "printf 'PROBE AVAILABLE\\n'"),
        root.script("serve.sh", "printf 'BOUND 42511\\n'\nsleep 0.3\nexit 0"),
    ));
    let host = start(config(root.path.clone(), launcher, Arc::new(FakeInstall::with_installed(installed())), Arc::new(FakeReady::new(None))));
    host.request(Action::Check).unwrap();
    wait_phase(&host, Phase::Installed);
    host.request(Action::Start).unwrap();
    let running = wait_phase(&host, Phase::Running);
    assert!(running.endpoint.is_some());
    let failed = wait_phase(&host, Phase::Failed);
    assert!(failed.endpoint.is_none(), "the endpoint dies with its owned helper, even on a zero exit");
}

/// The one gate for this module's tests. Every host test takes it for its whole body: each either
/// changes the process-wide reader cap or spawns a helper whose reader takes a slot, so letting two
/// run at once makes a global count depend on an unrelated test. Other modules' tests are unaffected
/// and still run in parallel.
static HOST_FIXTURE_LOCK: Mutex<()> = Mutex::new(());

fn host_fixture() -> std::sync::MutexGuard<'static, ()> {
    HOST_FIXTURE_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Wait until no reader from an earlier test still holds a slot, so exclusive pool arithmetic starts
/// from a pool this test owns alone. Bounded: a stuck reader fails the test instead of hanging the run.
fn quiescent_pool() {
    let started = Instant::now();
    while event_slots_in_use() != 0 {
        assert!(started.elapsed() < WAIT, "an earlier helper's reader still holds an event slot");
        thread::sleep(Duration::from_millis(5));
    }
}

/// One test's lowered cap on the one process-wide reader pool. The production cap is restored when
/// the test ends, including on a panic, so no later test inherits it.
struct PoolCap;

impl PoolCap {
    fn lower(limit: usize) -> Self {
        quiescent_pool();
        set_event_slot_limit(limit);
        PoolCap
    }
}

impl Drop for PoolCap {
    fn drop(&mut self) {
        reset_event_pool();
    }
}
// ---------------------------------------------------------------------------------------------
// The real readiness exchange, over loopback TCP, against a stand-in grammar server.
//
// `NativeReadiness` is the exchange the program itself runs on a start; the rest of this module
// tests it through the injected `Readiness` trait. These tests ask the real one, with a stand-in
// that speaks the `/v2/languages` and `/v2/check` shapes LanguageTool 6.6 writes (the same JSON
// the `mailcraft-net` tests read). Nothing here starts Java, installs anything or contacts a real
// server: the only traffic is between this process and a listener it made on this machine.
// ---------------------------------------------------------------------------------------------

/// What a server that could not finish in its own check limit answers, as the real supervisor
/// does: a 500 whose body is the server's own words, never any of the owner's text.
const COLD_REFUSAL: &str = "Error: Checking took longer than 5.0 seconds, which is this server\u{2019}s limit. Please make sure you have selected the proper language or consider submitting a shorter text.";

/// The language list a LanguageTool 6.6 server answers `/v2/languages` with.
const READY_LANGUAGES: &str = r#"[{"name":"English (US)","code":"en-US","longCode":"en-US"},{"name":"French","code":"fr","longCode":"fr-FR"}]"#;

/// One whole `POST /v2/check` answer, as LanguageTool 6.6 writes it, with no matches: the shape
/// the `languagetool-rust` crate's `Response` type defines, which is what a warm-up only needs.
fn warm_answer(code: &str, name: &str, detected: &str, confidence: f64) -> String {
    format!(
        r#"{{"software":{{"name":"LanguageTool","version":"6.6","buildDate":"2025-03-27 20:50:25 +0100","apiVersion":1,"premium":false,"premiumHint":"","status":""}},"warnings":{{"incompleteResults":false}},"language":{{"name":"{name}","code":"{code}","detectedLanguage":{{"name":"{name}","code":"{detected}","confidence":{confidence},"source":"ngram"}}}},"matches":[]}}"#
    )
}

/// One HTTP answer with `body`, as a plain server writes it.
fn warm_http(status: &str, body: &str) -> Vec<u8> {
    format!("HTTP/1.1 {status}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
        .into_bytes()
}

/// The French answer that proves the model was loaded and the sentence checked.
fn french_checked() -> String {
    warm_answer("fr-CA", "French (Canada)", "fr", 0.99)
}

/// The English answer that proves the model was loaded and the sentence checked.
fn english_checked() -> String {
    warm_answer("en-CA", "English (Canada)", "en", 0.99)
}

/// Which of the two synthetic checks a request is. The sentences are this module's own fixed
/// readiness text — never the owner's — so a piece of one names the language it was sent for.
fn warm_language(request: &str) -> Option<Language> {
    if request.contains("Cette+phrase") {
        Some(Language::FrCa)
    } else if request.contains("This+sentence") {
        Some(Language::EnCa)
    } else {
        None
    }
}

/// A stand-in grammar server on this machine: it answers each request with bytes of the test's
/// choosing and keeps every request it was sent, so a test can count attempts and compare them.
struct FakeGrammar {
    port: u16,
    halt: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<String>>>,
}

impl FakeGrammar {
    fn start(answer: impl Fn(&str) -> Vec<u8> + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a free port on this machine");
        let port = listener.local_addr().expect("the port it took").port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let kept = Arc::clone(&requests);
        let halt = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&halt);
        let answer = Arc::new(answer);
        thread::spawn(move || {
            for stream in listener.incoming() {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                let Ok(mut stream) = stream else { continue };
                let Some(request) = warm_read_request(&mut stream) else { continue };
                kept.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push(request.clone());
                let _ = stream.write_all(&answer(&request));
                let _ = stream.flush();
            }
        });
        Self { port, halt, requests }
    }

    fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn asked(&self) -> usize {
        self.requests.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).len()
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }
}

impl Drop for FakeGrammar {
    fn drop(&mut self) {
        // Wake the listener's `accept` with a connection it refuses to serve, so the thread ends
        // with the fixture instead of holding a port for the rest of the run.
        self.halt.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

/// One whole HTTP/1.1 request from `stream`: its line, its headers, the blank line and its body.
fn warm_read_request(stream: &mut TcpStream) -> Option<String> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut request = String::new();
    let mut length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse().ok()?;
        }
        request.push_str(&line);
        if line == "\r\n" {
            break;
        }
        if request.len() > 16 * 1024 {
            return None;
        }
    }
    let mut body = vec![0u8; length.min(64 * 1024)];
    reader.read_exact(&mut body).ok()?;
    request.push_str(&String::from_utf8_lossy(&body));
    Some(request)
}

/// A grammar server that answers the readiness list, both synthetic checks, and refuses whatever
/// else, with the French check following `french`.
fn grammar(french: impl Fn(usize) -> Vec<u8> + Send + Sync + 'static) -> (FakeGrammar, Arc<AtomicUsize>) {
    let attempts = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&attempts);
    let fake = FakeGrammar::start(move |request| {
        if request.starts_with("GET /v2/languages ") {
            return warm_http("200 OK", READY_LANGUAGES);
        }
        match warm_language(request) {
            Some(Language::FrCa) => french(counted.fetch_add(1, Ordering::SeqCst)),
            Some(Language::EnCa) => warm_http("200 OK", &english_checked()),
            _ => warm_http("400 Bad Request", "no"),
        }
    });
    (fake, attempts)
}

/// The budget one of these exchanges is given: as for a real start, and never shorter than the
/// per-request patience, so a stand-in that answers at once is never timed out.
fn warm_deadline() -> Instant {
    Instant::now() + Duration::from_secs(20)
}

#[test]
fn a_cold_french_model_is_asked_once_more_and_the_exchange_succeeds() {
    // The proven production defect on a cold start: the French model is still loading, so the
    // server answers the first French check with its own 500 after its 5 s limit. The very same
    // synthetic check, asked again, is answered — and the exchange must succeed, not fail a
    // start the server is ready for.
    let (fake, french) =
        grammar(|attempt| if attempt == 0 { warm_http("500 Internal Server Error", COLD_REFUSAL) } else { warm_http("200 OK", &french_checked()) });
    let outcome = NativeReadiness.warm(&fake.endpoint(), warm_deadline(), &|| false);
    assert_eq!(outcome, Ok(()), "one more identical French check brings a cold model up");
    assert_eq!(french.load(Ordering::SeqCst), 2, "exactly one retry, never more");
    let requests = fake.requests();
    assert_eq!(requests.len(), 4, "the list, English once, and French twice: {requests:?}");
    assert!(requests.first().is_some_and(|request| request.starts_with("GET /v2/languages ")));
    assert_eq!(warm_language(&requests[2]), Some(Language::FrCa), "the third request is the French check");
    // The retry is the same fixed synthetic sentence, byte for byte: no draft text, nothing new.
    let first = requests[2].split_once("\r\n\r\n").map(|(_, body)| body.to_string());
    let second = requests[3].split_once("\r\n\r\n").map(|(_, body)| body.to_string());
    assert!(first.is_some() && first == second, "the retry replays the same synthetic check");
}

#[test]
fn a_french_check_that_keeps_failing_is_asked_exactly_twice() {
    let (fake, french) = grammar(|_| warm_http("500 Internal Server Error", COLD_REFUSAL));
    let outcome = NativeReadiness.warm(&fake.endpoint(), warm_deadline(), &|| false);
    assert_eq!(outcome, Err(Issue::StartFailed), "a server that keeps refusing is a failed start");
    assert_eq!(french.load(Ordering::SeqCst), 2, "two attempts and no more: there is no retry loop");
    assert_eq!(fake.asked(), 4, "the list, English once, French twice");
}

#[test]
fn the_english_model_is_brought_up_by_one_more_check_as_well() {
    let english = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&english);
    let fake = FakeGrammar::start(move |request| {
        if request.starts_with("GET /v2/languages ") {
            return warm_http("200 OK", READY_LANGUAGES);
        }
        match warm_language(request) {
            Some(Language::EnCa) => {
                if counted.fetch_add(1, Ordering::SeqCst) == 0 {
                    warm_http("500 Internal Server Error", COLD_REFUSAL)
                } else {
                    warm_http("200 OK", &english_checked())
                }
            }
            Some(Language::FrCa) => warm_http("200 OK", &french_checked()),
            _ => warm_http("400 Bad Request", "no"),
        }
    });
    let outcome = NativeReadiness.warm(&fake.endpoint(), warm_deadline(), &|| false);
    assert_eq!(outcome, Ok(()));
    assert_eq!(english.load(Ordering::SeqCst), 2, "each language gets at most one more check");
    assert_eq!(fake.asked(), 4);
}

#[test]
fn an_answer_that_is_not_a_check_is_never_asked_again() {
    // A server that answers rubbish has answered: only its own refusal (`Failed`) is worth one
    // more identical synthetic check.
    let (fake, french) = grammar(|_| warm_http("200 OK", "[]"));
    let outcome = NativeReadiness.warm(&fake.endpoint(), warm_deadline(), &|| false);
    assert_eq!(outcome, Err(Issue::StartFailed));
    assert_eq!(french.load(Ordering::SeqCst), 1, "a garbled answer is not retried");
}

#[test]
fn an_answer_too_long_to_read_is_never_asked_again() {
    let huge = format!("{{\"matches\":[{}]}}", "\"a\"".repeat(MAX_ANSWER / 4));
    let (fake, french) = grammar(move |_| warm_http("200 OK", &huge));
    let outcome = NativeReadiness.warm(&fake.endpoint(), warm_deadline(), &|| false);
    assert_eq!(outcome, Err(Issue::StartFailed));
    assert_eq!(french.load(Ordering::SeqCst), 1, "an answer over the limit is not retried");
}

#[test]
fn a_cancellation_between_the_two_attempts_stops_the_second() {
    let cancelled = Arc::new(AtomicBool::new(false));
    let stopped = Arc::clone(&cancelled);
    let (fake, french) = grammar(move |_| {
        // The owner stops the start while the first French check is failing.
        stopped.store(true, Ordering::SeqCst);
        warm_http("500 Internal Server Error", COLD_REFUSAL)
    });
    let outcome = NativeReadiness.warm(&fake.endpoint(), warm_deadline(), &|| cancelled.load(Ordering::SeqCst));
    assert_eq!(outcome, Err(Issue::Cancelled), "a cancelled start says so, not that it failed");
    assert_eq!(french.load(Ordering::SeqCst), 1, "no check is replayed for an owner who stopped");
    assert_eq!(fake.asked(), 3, "the list, English once, French once");
}

#[test]
fn a_budget_spent_by_the_first_attempt_stops_the_second() {
    // The one check that fails spends the whole budget: the retry has no time left, so it is
    // never sent. The stand-in waits far longer than the budget, so the client's own patience
    // (ten times short of the wait) is what ends the attempt.
    let (fake, french) = grammar(|_| {
        thread::sleep(Duration::from_millis(1000));
        warm_http("500 Internal Server Error", COLD_REFUSAL)
    });
    let deadline = Instant::now() + Duration::from_millis(100);
    let outcome = NativeReadiness.warm(&fake.endpoint(), deadline, &|| false);
    assert_eq!(outcome, Err(Issue::StartTimedOut), "a spent budget is a timeout");
    assert_eq!(french.load(Ordering::SeqCst), 1, "no attempt is made past the deadline");
}

#[test]
fn an_answer_that_checked_nothing_is_a_failure_and_is_not_asked_again() {
    // The retry exists for a server's own refusal, not for an answer that proves neither model
    // checked its sentence: the positive condition on both languages is unchanged.
    let (fake, french) = grammar(|_| warm_http("200 OK", &warm_answer("fr-CA", "French (Canada)", "fr", 0.5)));
    let outcome = NativeReadiness.warm(&fake.endpoint(), warm_deadline(), &|| false);
    assert_eq!(outcome, Err(Issue::StartFailed), "an unchecked French sentence is never readiness");
    assert_eq!(french.load(Ordering::SeqCst), 1, "an answer that is no proof is not retried");
}

#[test]
fn the_exchange_speaks_only_to_a_literal_loopback_address() {
    for refused in ["http://192.0.2.7:8081", "https://127.0.0.1:8081", "http://127.0.0.1:0", "", "   "] {
        assert_eq!(NativeReadiness.warm(refused, warm_deadline(), &|| false), Err(Issue::StartFailed), "{refused}");
    }
}

/// The same cold French start through the host itself: a `BOUND` from the owned helper whose port
/// is the stand-in server's, the real exchange, and the endpoint published only once both
/// synthetic checks were actually answered.
#[cfg(unix)]
#[test]
fn a_cold_french_start_publishes_its_endpoint_after_one_more_synthetic_check() {
    let _fixture = host_fixture();
    let root = TempRoot::new("cold-french-start");
    let (fake, french) =
        grammar(|attempt| if attempt == 0 { warm_http("500 Internal Server Error", COLD_REFUSAL) } else { warm_http("200 OK", &french_checked()) });
    let serve = root.script("serve.sh", &format!("printf 'BOUND {}\\n'\ncat >/dev/null\nexit 0", fake.port));
    let launcher = Arc::new(ScriptLauncher::new(root.script("probe.sh", "printf 'PROBE AVAILABLE\\n'"), serve));
    let host = start(config(root.path.clone(), launcher, Arc::new(FakeInstall::with_installed(installed())), Arc::new(NativeReadiness)));
    host.request(Action::Check).unwrap();
    wait_phase(&host, Phase::Installed);
    host.request(Action::Start).unwrap();
    let running = wait_phase(&host, Phase::Running);
    assert_eq!(running.endpoint.as_deref(), Some(fake.endpoint().as_str()), "the literal endpoint is published");
    assert_eq!(french.load(Ordering::SeqCst), 2, "the cold French model was asked exactly once more");
    assert!(running.issue.is_none());
}
