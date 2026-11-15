// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Tests for the managed proof-server supervisor.
//!
//! The fake child is this test binary itself, re-run with private arguments. No shell script is
//! involved, so every start, marker, overflow, stop and EOF below travels through real pipes and a
//! real child process. `parent_eof_*` closes the only writer end, which is what an abrupt host
//! process exit does to the private pipe.

use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use super::proof_supervisor::{Config, EVENT_BOUND, EVENT_FAILED, Failure, supervise};

const FAKE_ENV: &str = "MAILCRAFT_PROOF_TEST_FAKE_CHILD";
const FAKE_PIDFILE: &str = "MAILCRAFT_PROOF_TEST_PIDFILE";
const FAKE_EXACT_TEST: &str = "proof_supervisor_tests::fake_child_entry";

static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

// ---------------------------------------------------------------------------------------------
// Temporary directories and paths (tests only)
// ---------------------------------------------------------------------------------------------

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "mailcraft-proof-supervisor-{}-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed),
            label
        ));
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn install_dir(&self) -> PathBuf {
        let dir = self.path.join("install");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("languagetool-server.jar"), b"test jar").unwrap();
        dir
    }

    fn runtime_base(&self) -> PathBuf {
        let dir = self.path.join("runtime");
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn assert_empty_dir(path: &Path) {
    let names: Vec<String> = fs::read_dir(path).unwrap().map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert!(names.is_empty(), "runtime directory is not empty: {names:?}");
}

fn wait_for_file(path: &Path, timeout: Duration) -> bool {
    wait_until(timeout, || path.is_file())
}

fn wait_until(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
    let started = Instant::now();
    loop {
        if done() {
            return true;
        }
        if started.elapsed() >= timeout {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn read_pid(path: &Path) -> u32 {
    fs::read_to_string(path).unwrap().trim().parse().unwrap_or_else(|_| panic!("pid file {path:?} does not hold a pid"))
}

#[cfg(target_os = "linux")]
fn process_alive(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).is_dir()
}

#[cfg(not(target_os = "linux"))]
fn process_alive(_pid: u32) -> bool {
    // There is no std-only liveness probe on this platform, so reap assertions are Linux evidence
    // only. Windows and macOS supervisor behavior is not claimed as exercised.
    true
}

fn assert_alive(pid: u32) {
    assert!(process_alive(pid), "child {pid} is not alive");
}

fn assert_reaped(pid: u32) {
    assert!(!process_alive(pid), "child {pid} still exists after teardown");
}

// ---------------------------------------------------------------------------------------------
// Parent control: a controllable byte channel, plus real OS pipes below
// ---------------------------------------------------------------------------------------------

struct ChannelReader {
    rx: mpsc::Receiver<Vec<u8>>,
    pending: Vec<u8>,
    pos: usize,
}

impl Read for ChannelReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            if self.pos < self.pending.len() {
                let n = (self.pending.len() - self.pos).min(buf.len());
                buf[..n].copy_from_slice(&self.pending[self.pos..self.pos + n]);
                self.pos += n;
                return Ok(n);
            }
            match self.rx.recv() {
                Ok(next) => {
                    self.pending = next;
                    self.pos = 0;
                    if self.pending.is_empty() {
                        continue;
                    }
                }
                Err(_) => return Ok(0),
            }
        }
    }
}

fn control_channel() -> (ChannelReader, mpsc::Sender<Vec<u8>>) {
    let (tx, rx) = mpsc::channel();
    (ChannelReader { rx, pending: Vec::new(), pos: 0 }, tx)
}

// ---------------------------------------------------------------------------------------------
// Events sink
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Default)]
struct SharedEvents(Arc<Mutex<Vec<u8>>>);

impl Write for SharedEvents {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl SharedEvents {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

fn bound_port(text: &str) -> Option<u16> {
    text.lines().find_map(|line| line.strip_prefix(EVENT_BOUND).and_then(|rest| rest.strip_prefix(' ')).and_then(|port| port.parse().ok()))
}

fn wait_for_bound(events: &SharedEvents, timeout: Duration) -> Option<u16> {
    let bound = wait_until(timeout, || bound_port(&events.text()).is_some());
    if bound { bound_port(&events.text()) } else { None }
}

// ---------------------------------------------------------------------------------------------
// Fake child: this very test binary, chosen by a private environment variable
// ---------------------------------------------------------------------------------------------

fn fake_command(mode: &str, pidfile: &Path) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .arg("--exact")
        .arg(FAKE_EXACT_TEST)
        .arg("--nocapture")
        .arg("--test-threads=1")
        .env(FAKE_ENV, mode)
        .env(FAKE_PIDFILE, pidfile)
        .stdin(Stdio::null());
    command
}

struct FakeConfig {
    config: Config,
    pidfile: PathBuf,
    args_seen: Arc<Mutex<Vec<String>>>,
    env_seen: Arc<Mutex<Vec<(String, String)>>>,
    server_properties_seen: Arc<Mutex<Option<String>>>,
    modes_seen: Arc<Mutex<Option<(u32, u32)>>>,
}

fn fake_config(root: &TempRoot, install: &Path, runtime_base: &Path, mode: &'static str, timeout: Duration) -> FakeConfig {
    let pidfile = root.path().join(format!("fake-{mode}.pid"));
    let args_seen = Arc::new(Mutex::new(Vec::new()));
    let env_seen = Arc::new(Mutex::new(Vec::new()));
    let server_properties_seen = Arc::new(Mutex::new(None));
    let modes_seen = Arc::new(Mutex::new(None));
    let hook_args = Arc::clone(&args_seen);
    let hook_env = Arc::clone(&env_seen);
    let hook_properties = Arc::clone(&server_properties_seen);
    let hook_modes = Arc::clone(&modes_seen);
    let hook_pidfile = pidfile.clone();

    let config = Config::new(std::env::current_exe().unwrap(), install)
        .with_startup_timeout(timeout)
        .with_test_runtime_base(runtime_base.to_path_buf())
        .with_test_command(move |command, _port| {
            let args: Vec<String> = command.get_args().map(|arg| arg.to_string_lossy().into_owned()).collect();
            if let Some(index) = args.iter().position(|arg| arg == "--config")
                && let Some(path) = args.get(index + 1)
            {
                *hook_properties.lock().unwrap() = fs::read_to_string(path).ok();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let file_mode = fs::metadata(path).ok().map(|metadata| metadata.permissions().mode() & 0o777);
                    let directory_mode = Path::new(path)
                        .parent()
                        .and_then(|directory| fs::metadata(directory).ok())
                        .map(|metadata| metadata.permissions().mode() & 0o777);
                    if let (Some(file_mode), Some(directory_mode)) = (file_mode, directory_mode) {
                        *hook_modes.lock().unwrap() = Some((file_mode, directory_mode));
                    }
                }
            }
            *hook_args.lock().unwrap() = args;
            *hook_env.lock().unwrap() = command
                .get_envs()
                .map(|(key, value)| (key.to_string_lossy().into_owned(), value.map(|value| value.to_string_lossy().into_owned()).unwrap_or_default()))
                .collect();
            *command = fake_command(mode, &hook_pidfile);
            vec![(FAKE_ENV.to_string(), mode.to_string()), (FAKE_PIDFILE.to_string(), hook_pidfile.to_string_lossy().into_owned())]
        });

    FakeConfig { config, pidfile, args_seen, env_seen, server_properties_seen, modes_seen }
}

fn write_line(text: &str) {
    let mut out = io::stdout();
    let _ = out.write_all(text.as_bytes());
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

fn marker_lines() {
    write_line("2026-11-13 00:34:50 +0000 Setting up thread pool with 1 threads");
    write_line("2026-11-13 00:34:51 +0000 Starting LanguageTool 6.6 server on http://localhost:0...");
    write_line("2026-11-13 00:34:51 +0000 Server started");
}

fn hang_forever() -> ! {
    loop {
        thread::sleep(Duration::from_millis(25));
    }
}

/// The one real test body the fake child runs. Without the private variable it is a no-op pass.
#[test]
fn fake_child_entry() {
    let mode = match std::env::var(FAKE_ENV) {
        Ok(mode) => mode,
        Err(_) => return,
    };
    if let Ok(pidfile) = std::env::var(FAKE_PIDFILE) {
        let _ = fs::write(pidfile, std::process::id().to_string());
    }
    match mode.as_str() {
        "silent" => hang_forever(),
        "marker" => {
            marker_lines();
            hang_forever()
        }
        "marker_then_exit" => {
            marker_lines();
            thread::sleep(Duration::from_millis(300));
            std::process::exit(7)
        }
        "noise" => {
            let mut out = io::stdout();
            let chunk = vec![b'x'; 8192];
            for _ in 0..16 {
                let _ = out.write_all(&chunk);
            }
            let _ = out.flush();
            hang_forever()
        }
        "fail" => {
            write_line("Exception in thread \"main\" java.lang.RuntimeException: could not start");
            std::process::exit(1)
        }
        "orphan" => {
            // A grandchild keeps the inherited stdout pipe and prints the marker after the direct
            // child has exited. A live owned child is required, so this must never bind.
            let exe = std::env::current_exe().unwrap();
            let _ = Command::new(exe)
                .arg("--exact")
                .arg(FAKE_EXACT_TEST)
                .arg("--nocapture")
                .arg("--test-threads=1")
                .env(FAKE_ENV, "delayed_marker")
                .env_remove(FAKE_PIDFILE)
                .stdin(Stdio::null())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .spawn();
            std::process::exit(0)
        }
        "delayed_marker" => {
            thread::sleep(Duration::from_millis(300));
            marker_lines();
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------------------------
// Verification and configuration
// ---------------------------------------------------------------------------------------------

#[test]
fn verification_refusal_has_no_process_or_directory_side_effects() {
    let root = TempRoot::new("refused");
    let install = root.install_dir();
    let runtime = root.runtime_base();
    let verifier_calls = Arc::new(AtomicUsize::new(0));
    let spawn_calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(None));

    let calls = Arc::clone(&verifier_calls);
    let seen_path = Arc::clone(&seen);
    let install_for_check = install.clone();
    let spawns = Arc::clone(&spawn_calls);
    let config = Config::new(std::env::current_exe().unwrap(), &install)
        .with_startup_timeout(Duration::from_secs(2))
        .with_test_runtime_base(runtime.clone())
        .with_test_command(move |_command, _port| {
            spawns.fetch_add(1, Ordering::SeqCst);
            Vec::new()
        });

    let (control, _sender) = control_channel();
    let events = SharedEvents::default();
    let result = supervise(
        config,
        move |dir: &Path| {
            calls.fetch_add(1, Ordering::SeqCst);
            *seen_path.lock().unwrap() = Some(dir.to_path_buf());
            Err(())
        },
        control,
        events.clone(),
    );

    assert_eq!(result, Err(Failure::VerificationRefused));
    assert_eq!(verifier_calls.load(Ordering::SeqCst), 1);
    assert_eq!(*seen.lock().unwrap(), Some(install_for_check));
    assert_eq!(spawn_calls.load(Ordering::SeqCst), 0);
    assert_eq!(events.text(), "FAILED\n");
    assert_empty_dir(&runtime);
}

#[test]
fn relative_java_path_is_rejected_before_verification() {
    let root = TempRoot::new("relative-java");
    let install = root.install_dir();
    let runtime = root.runtime_base();
    let verifier_calls = Arc::new(AtomicUsize::new(0));
    let calls = Arc::clone(&verifier_calls);
    let config = Config::new("java", &install).with_test_runtime_base(runtime.clone());

    let (control, _sender) = control_channel();
    let events = SharedEvents::default();
    let result = supervise(
        config,
        move |_dir: &Path| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
        control,
        events.clone(),
    );

    assert_eq!(result, Err(Failure::InvalidConfig));
    assert_eq!(verifier_calls.load(Ordering::SeqCst), 0);
    assert_eq!(events.text(), "FAILED\n");
    assert_empty_dir(&runtime);
}

#[test]
fn relative_install_directory_is_rejected() {
    let root = TempRoot::new("relative-install");
    let install = root.install_dir();
    let runtime = root.runtime_base();
    let config = Config::new(std::env::current_exe().unwrap(), "relative-install").with_test_runtime_base(runtime.clone());
    let (control, _sender) = control_channel();
    let events = SharedEvents::default();
    let result = supervise(config, |_dir: &Path| Ok(()), control, events.clone());
    assert_eq!(result, Err(Failure::InvalidConfig));
    assert_eq!(events.text(), "FAILED\n");
    assert_empty_dir(&runtime);
    assert!(install.is_dir());
}

#[test]
fn missing_server_jar_is_refused_before_spawn_or_runtime_directory() {
    let root = TempRoot::new("missing-jar");
    let install = root.path().join("install-without-jar");
    fs::create_dir_all(&install).unwrap();
    let runtime = root.runtime_base();
    let spawn_calls = Arc::new(AtomicUsize::new(0));
    let spawns = Arc::clone(&spawn_calls);
    let config =
        Config::new(std::env::current_exe().unwrap(), &install).with_test_runtime_base(runtime.clone()).with_test_command(move |_command, _port| {
            spawns.fetch_add(1, Ordering::SeqCst);
            Vec::new()
        });
    let (control, _sender) = control_channel();
    let events = SharedEvents::default();
    let result = supervise(config, |_dir: &Path| Ok(()), control, events.clone());
    assert_eq!(result, Err(Failure::InvalidInstallation));
    assert_eq!(spawn_calls.load(Ordering::SeqCst), 0);
    assert_eq!(events.text(), "FAILED\n");
    assert_empty_dir(&runtime);
}

#[test]
fn launch_arguments_are_separate_private_and_do_not_inherit_jvm_injection() {
    let root = TempRoot::new("arguments");
    let install = root.install_dir();
    let runtime = root.runtime_base();
    let fake = fake_config(&root, &install, &runtime, "silent", Duration::from_secs(2));
    let args_seen = Arc::clone(&fake.args_seen);
    let env_seen = Arc::clone(&fake.env_seen);
    let properties_seen = Arc::clone(&fake.server_properties_seen);
    let modes_seen = Arc::clone(&fake.modes_seen);

    let (control, sender) = control_channel();
    drop(sender); // controller gone before startup completes: clean stop
    let events = SharedEvents::default();
    let result = supervise(fake.config, |_dir: &Path| Ok(()), control, events.clone());
    assert_eq!(result, Ok(()));
    assert_eq!(events.text(), "STOPPED\n");

    let args = args_seen.lock().unwrap().clone();
    assert_eq!(args.first().map(String::as_str), Some("-Xmx512m"));
    assert!(args.iter().any(|arg| arg == "-Djava.net.preferIPv4Stack=true"));
    assert!(args.iter().any(|arg| arg == "-Djava.awt.headless=true"));
    assert!(args.iter().any(|arg| arg == "-XX:-HeapDumpOnOutOfMemoryError"));
    #[cfg(unix)]
    assert!(args.iter().any(|arg| arg == "-XX:ErrorFile=/dev/null"));
    assert!(args.iter().any(|arg| arg.starts_with("-Djava.io.tmpdir=") && arg.contains("runtime")));
    assert!(args.iter().any(|arg| arg == "-cp"));
    assert!(args.iter().any(|arg| arg.ends_with("languagetool-server.jar")));
    assert!(args.iter().any(|arg| arg == "org.languagetool.server.HTTPServer"));
    assert!(args.iter().any(|arg| arg == "--config"));
    assert!(args.iter().any(|arg| arg.ends_with("server.properties")));
    assert!(args.iter().any(|arg| arg == "--port"));
    assert!(args.iter().any(|arg| arg.parse::<u16>().is_ok()));
    for forbidden in ["--public", "--verbose", "--allow-origin"] {
        assert!(!args.iter().any(|arg| arg == forbidden), "forbidden argument {forbidden} was passed");
    }
    assert!(!args.iter().any(|arg| arg.contains(' ')), "arguments must be passed separately: {args:?}");

    let properties = properties_seen.lock().unwrap().clone().unwrap_or_default();
    for expected in [
        "maxTextLength=16384",
        "maxTextHardLength=16384",
        "maxCheckTimeMillis=5000",
        "maxCheckThreads=1",
        "maxWorkQueueSize=2",
        "cacheSize=0",
        "pipelineCaching=false",
    ] {
        assert!(properties.contains(expected), "server properties are missing {expected:?}: {properties:?}");
    }
    #[cfg(unix)]
    {
        let (file_mode, directory_mode) = (*modes_seen.lock().unwrap()).unwrap_or((0, 0));
        assert_eq!(file_mode & 0o777, 0o600, "server properties are not private");
        assert_eq!(directory_mode & 0o777, 0o700, "runtime directory is not private");
    }
    #[cfg(not(unix))]
    let _ = &modes_seen;

    let env = env_seen.lock().unwrap().clone();
    for injection in ["JAVA_TOOL_OPTIONS", "_JAVA_OPTIONS", "JDK_JAVA_OPTIONS", "CLASSPATH", "JAVA_OPTIONS"] {
        assert!(!env.iter().any(|(key, _)| key == injection), "{injection} survived the environment whitelist: {env:?}");
    }
    assert!(
        env.iter().any(|(key, value)| key == "TMPDIR" && value.contains("runtime")),
        "Java temporary directory is not confined to the private runtime directory: {env:?}"
    );
    assert_empty_dir(&runtime);
}

// ---------------------------------------------------------------------------------------------
// Startup: timeout, bounded output, marker, liveness
// ---------------------------------------------------------------------------------------------

#[test]
fn missing_java_executable_fails_and_leaves_no_runtime_directory() {
    let root = TempRoot::new("missing-java");
    let install = root.install_dir();
    let runtime = root.runtime_base();
    let config =
        Config::new("/definitely/not/a/java/binary", &install).with_startup_timeout(Duration::from_secs(2)).with_test_runtime_base(runtime.clone());
    let (control, _sender) = control_channel();
    let events = SharedEvents::default();
    let result = supervise(config, |_dir: &Path| Ok(()), control, events.clone());
    assert_eq!(result, Err(Failure::SpawnFailed));
    assert_eq!(events.text(), "FAILED\n");
    assert_empty_dir(&runtime);
}

#[test]
fn child_exit_after_bound_is_reported_as_failure_not_stopped() {
    let root = TempRoot::new("post-bound-exit");
    let install = root.install_dir();
    let runtime = root.runtime_base();
    let fake = fake_config(&root, &install, &runtime, "marker_then_exit", Duration::from_secs(10));
    let pidfile = fake.pidfile.clone();
    let (control, _sender) = control_channel();
    let events = SharedEvents::default();

    let handle = {
        let events = events.clone();
        thread::spawn(move || supervise(fake.config, |_dir: &Path| Ok(()), control, events))
    };
    assert!(wait_for_bound(&events, Duration::from_secs(10)).is_some());
    assert_eq!(handle.join().unwrap(), Err(Failure::ChildExited));

    let text = events.text();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{text:?}");
    assert!(lines.first().is_some_and(|line| line.starts_with("BOUND ")), "{text:?}");
    assert_eq!(lines.get(1), Some(&EVENT_FAILED), "{text:?}");
    assert!(!text.contains("STOPPED"), "{text:?}");
    assert!(wait_for_file(&pidfile, Duration::from_secs(2)));
    assert_reaped(read_pid(&pidfile));
    assert_empty_dir(&runtime);
}

#[test]
fn startup_timeout_kills_and_reaps_the_child() {
    let root = TempRoot::new("timeout");
    let install = root.install_dir();
    let runtime = root.runtime_base();
    let fake = fake_config(&root, &install, &runtime, "silent", Duration::from_millis(1200));
    let pidfile = fake.pidfile.clone();
    let (control, _sender) = control_channel();
    let events = SharedEvents::default();
    let started = Instant::now();
    let result = supervise(fake.config, |_dir: &Path| Ok(()), control, events.clone());
    let elapsed = started.elapsed();

    assert_eq!(result, Err(Failure::StartupTimedOut));
    assert_eq!(events.text(), "FAILED\n");
    assert!(elapsed >= Duration::from_millis(1200), "returned too early");
    assert!(elapsed < Duration::from_secs(10), "timeout was not bounded");
    assert!(wait_for_file(&pidfile, Duration::from_secs(1)));
    assert_reaped(read_pid(&pidfile));
    assert_empty_dir(&runtime);
}

#[test]
fn oversized_startup_output_fails_without_accumulating_it() {
    let root = TempRoot::new("noise");
    let install = root.install_dir();
    let runtime = root.runtime_base();
    let fake = fake_config(&root, &install, &runtime, "noise", Duration::from_secs(10));
    let pidfile = fake.pidfile.clone();
    let (control, _sender) = control_channel();
    let events = SharedEvents::default();
    let started = Instant::now();
    let result = supervise(fake.config, |_dir: &Path| Ok(()), control, events.clone());
    let elapsed = started.elapsed();

    assert_eq!(result, Err(Failure::StartupOutputOverflow));
    assert_eq!(events.text(), "FAILED\n");
    assert!(elapsed < Duration::from_secs(5), "oversized output was not bounded");
    assert!(wait_for_file(&pidfile, Duration::from_secs(1)));
    assert_reaped(read_pid(&pidfile));
    assert_empty_dir(&runtime);
}

#[test]
fn marker_without_a_live_owned_child_is_not_bound() {
    let root = TempRoot::new("orphan");
    let install = root.install_dir();
    let runtime = root.runtime_base();
    let fake = fake_config(&root, &install, &runtime, "orphan", Duration::from_secs(5));
    let pidfile = fake.pidfile.clone();
    let (control, _sender) = control_channel();
    let events = SharedEvents::default();
    let result = supervise(fake.config, |_dir: &Path| Ok(()), control, events.clone());

    assert_eq!(result, Err(Failure::ChildExited));
    assert_eq!(events.text(), "FAILED\n");
    assert!(wait_for_file(&pidfile, Duration::from_secs(2)));
    assert_reaped(read_pid(&pidfile));
    assert_empty_dir(&runtime);
}

#[test]
fn bound_is_reported_only_after_the_marker_and_while_the_child_lives() {
    let root = TempRoot::new("bound");
    let install = root.install_dir();
    let runtime = root.runtime_base();
    let fake = fake_config(&root, &install, &runtime, "marker", Duration::from_secs(10));
    let pidfile = fake.pidfile.clone();
    let (control, sender) = control_channel();
    let events = SharedEvents::default();

    let handle = {
        let events = events.clone();
        thread::spawn(move || supervise(fake.config, |_dir: &Path| Ok(()), control, events))
    };

    let port = wait_for_bound(&events, Duration::from_secs(10)).expect("BOUND was not reported");
    assert!(port > 0);
    assert!(wait_for_file(&pidfile, Duration::from_secs(2)));
    let pid = read_pid(&pidfile);
    assert_alive(pid);

    sender.send(b"STOP\n".to_vec()).unwrap();
    assert_eq!(handle.join().unwrap(), Ok(()));
    let text = events.text();
    assert_eq!(text.matches(EVENT_BOUND).count(), 1, "{text:?}");
    assert!(text.ends_with("STOPPED\n"), "{text:?}");
    assert!(!text.contains(EVENT_FAILED), "{text:?}");
    assert_reaped(pid);
    assert_empty_dir(&runtime);
}

// ---------------------------------------------------------------------------------------------
// Parent teardown: real pipe EOF during startup and after BOUND
// ---------------------------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn parent_eof_during_startup_kills_and_reaps() {
    use std::os::unix::net::UnixStream;

    let root = TempRoot::new("eof-startup");
    let install = root.install_dir();
    let runtime = root.runtime_base();
    let fake = fake_config(&root, &install, &runtime, "silent", Duration::from_secs(30));
    let pidfile = fake.pidfile.clone();
    let (control, writer) = UnixStream::pair().unwrap();
    let events = SharedEvents::default();
    let started = Instant::now();

    let handle = {
        let events = events.clone();
        thread::spawn(move || supervise(fake.config, |_dir: &Path| Ok(()), control, events))
    };
    // Let the fake child reach its entry point, then close the only write end. It never prints a
    // marker, so this still happens during startup.
    assert!(wait_for_file(&pidfile, Duration::from_secs(5)));
    // The controller process exiting closes this, its only write end, without a Stop command.
    drop(writer);

    assert_eq!(handle.join().unwrap(), Ok(()));
    assert!(started.elapsed() < Duration::from_secs(10), "EOF during startup did not stop promptly");
    assert_eq!(events.text(), "STOPPED\n");
    assert!(wait_for_file(&pidfile, Duration::from_secs(2)));
    assert_reaped(read_pid(&pidfile));
    assert_empty_dir(&runtime);
}

#[cfg(unix)]
#[test]
fn parent_eof_after_bound_kills_and_reaps() {
    use std::os::unix::net::UnixStream;

    let root = TempRoot::new("eof-bound");
    let install = root.install_dir();
    let runtime = root.runtime_base();
    let fake = fake_config(&root, &install, &runtime, "marker", Duration::from_secs(10));
    let pidfile = fake.pidfile.clone();
    let (control, writer) = UnixStream::pair().unwrap();
    let (events_read, events_write) = UnixStream::pair().unwrap();
    events_read.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let mut reader = BufReader::new(events_read);

    let handle = thread::spawn(move || supervise(fake.config, |_dir: &Path| Ok(()), control, events_write));

    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let port = bound_port(&line).expect("BOUND line");

    let pid = {
        assert!(wait_for_file(&pidfile, Duration::from_secs(2)));
        read_pid(&pidfile)
    };
    assert_alive(pid);
    // The socket was reserved but the fake child does not listen; BOUND is about the child and
    // its marker, not about an unrelated HTTP responder.
    assert!(port > 0);

    drop(writer);
    assert_eq!(handle.join().unwrap(), Ok(()));

    let mut stopped = String::new();
    reader.read_line(&mut stopped).unwrap();
    assert_eq!(stopped, "STOPPED\n");
    assert_reaped(pid);
    assert_empty_dir(&runtime);
}

#[cfg(unix)]
#[test]
fn parent_stop_command_over_real_pipe_kills_and_reaps() {
    use std::os::unix::net::UnixStream;

    let root = TempRoot::new("stop-pipe");
    let install = root.install_dir();
    let runtime = root.runtime_base();
    let fake = fake_config(&root, &install, &runtime, "marker", Duration::from_secs(10));
    let pidfile = fake.pidfile.clone();
    let (control, mut writer) = UnixStream::pair().unwrap();
    let (events_read, events_write) = UnixStream::pair().unwrap();
    events_read.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let mut reader = BufReader::new(events_read);

    let handle = thread::spawn(move || supervise(fake.config, |_dir: &Path| Ok(()), control, events_write));

    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(bound_port(&line).is_some(), "no BOUND: {line:?}");
    assert!(wait_for_file(&pidfile, Duration::from_secs(2)));
    let pid = read_pid(&pidfile);
    assert_alive(pid);

    // CRLF is deliberately used: a Windows host writes the same line ending.
    writer.write_all(b"STOP\r\n").unwrap();
    assert_eq!(handle.join().unwrap(), Ok(()));
    let mut stopped = String::new();
    reader.read_line(&mut stopped).unwrap();
    assert_eq!(stopped, "STOPPED\n");
    assert_reaped(pid);
    assert_empty_dir(&runtime);
}

// ---------------------------------------------------------------------------------------------
// Cancellation admission: a known stop never verifies, creates, or spawns
// ---------------------------------------------------------------------------------------------

/// A stop already visible before the verifier runs does no work at all: no verification, no
/// runtime directory and no command hook. `STOPPED` is the only event.
#[test]
fn known_stop_before_verification_does_no_work() {
    let root = TempRoot::new("stop-before-verify");
    let install = root.install_dir();
    let runtime = root.runtime_base();
    let stop = Arc::new(AtomicBool::new(true));
    let verifier_calls = Arc::new(AtomicUsize::new(0));
    let hook_calls = Arc::new(AtomicUsize::new(0));

    let calls = Arc::clone(&verifier_calls);
    let hooks = Arc::clone(&hook_calls);
    let config = Config::new(std::env::current_exe().unwrap(), &install)
        .with_startup_timeout(Duration::from_secs(2))
        .with_test_runtime_base(runtime.clone())
        .with_stop(Arc::clone(&stop))
        .with_test_command(move |_command, _port| {
            hooks.fetch_add(1, Ordering::SeqCst);
            Vec::new()
        });

    let (control, _sender) = control_channel();
    let events = SharedEvents::default();
    let result = supervise(
        config,
        move |_dir: &Path| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
        control,
        events.clone(),
    );

    assert_eq!(result, Ok(()));
    assert_eq!(events.text(), "STOPPED\n");
    assert_eq!(verifier_calls.load(Ordering::SeqCst), 0, "a known stop must not verify");
    assert_eq!(hook_calls.load(Ordering::SeqCst), 0, "a known stop must not reach the command hook");
    assert_empty_dir(&runtime);
}

/// A stop that becomes visible while the verifier is running is honoured after it returns: no
/// runtime directory, no command/prepare hook and no child. The unspawnable launcher means an
/// attempted spawn would have produced `FAILED`; `Ok` is proof it was never attempted.
#[test]
fn known_stop_during_verification_never_admits_a_child() {
    let root = TempRoot::new("stop-during-verify");
    let install = root.install_dir();
    let runtime = root.runtime_base();
    let stop = Arc::new(AtomicBool::new(false));
    let verifier_calls = Arc::new(AtomicUsize::new(0));
    let hook_calls = Arc::new(AtomicUsize::new(0));

    let calls = Arc::clone(&verifier_calls);
    let verifier_stop = Arc::clone(&stop);
    let hooks = Arc::clone(&hook_calls);
    let config = Config::new("/definitely/not/a/java/binary", &install)
        .with_startup_timeout(Duration::from_secs(2))
        .with_test_runtime_base(runtime.clone())
        .with_stop(Arc::clone(&stop))
        .with_test_command(move |_command, _port| {
            hooks.fetch_add(1, Ordering::SeqCst);
            Vec::new()
        });

    let (control, _sender) = control_channel();
    let events = SharedEvents::default();
    let result = supervise(
        config,
        move |_dir: &Path| {
            calls.fetch_add(1, Ordering::SeqCst);
            verifier_stop.store(true, Ordering::SeqCst);
            Ok(())
        },
        control,
        events.clone(),
    );

    assert_eq!(result, Ok(()));
    assert_eq!(events.text(), "STOPPED\n");
    assert_eq!(verifier_calls.load(Ordering::SeqCst), 1);
    assert_eq!(hook_calls.load(Ordering::SeqCst), 0, "no command may be prepared after a known stop");
    assert_empty_dir(&runtime);
}

/// A verifier that refuses *because* the helper was cancelled must report a normal stop, never an
/// integrity refusal (`exit 3` in the helper). No `FAILED` line is written.
#[test]
fn cancellation_during_verification_is_stopped_not_an_integrity_refusal() {
    let root = TempRoot::new("cancel-during-verify");
    let install = root.install_dir();
    let runtime = root.runtime_base();
    let stop = Arc::new(AtomicBool::new(false));
    let verifier_calls = Arc::new(AtomicUsize::new(0));
    let hook_calls = Arc::new(AtomicUsize::new(0));

    let calls = Arc::clone(&verifier_calls);
    let verifier_stop = Arc::clone(&stop);
    let hooks = Arc::clone(&hook_calls);
    let config = Config::new("/definitely/not/a/java/binary", &install)
        .with_startup_timeout(Duration::from_secs(2))
        .with_test_runtime_base(runtime.clone())
        .with_stop(Arc::clone(&stop))
        .with_test_command(move |_command, _port| {
            hooks.fetch_add(1, Ordering::SeqCst);
            Vec::new()
        });

    let (control, _sender) = control_channel();
    let events = SharedEvents::default();
    let result = supervise(
        config,
        move |_dir: &Path| {
            calls.fetch_add(1, Ordering::SeqCst);
            verifier_stop.store(true, Ordering::SeqCst);
            Err(())
        },
        control,
        events.clone(),
    );

    assert_eq!(result, Ok(()), "cancellation is a normal stop, not a verification refusal");
    assert_eq!(events.text(), "STOPPED\n");
    assert!(!events.text().contains(EVENT_FAILED));
    assert_eq!(verifier_calls.load(Ordering::SeqCst), 1);
    assert_eq!(hook_calls.load(Ordering::SeqCst), 0);
    assert_empty_dir(&runtime);
}

/// The last synchronous gate sits immediately before the child would be created. The command hook
/// runs, sets the stop flag, and the unspawnable launcher proves `spawn` was never called: the
/// result is still `Ok` with `STOPPED`, not `SpawnFailed`.
#[test]
fn stop_set_at_the_command_hook_blocks_the_final_spawn() {
    let root = TempRoot::new("stop-at-hook");
    let install = root.install_dir();
    let runtime = root.runtime_base();
    let stop = Arc::new(AtomicBool::new(false));
    let hook_calls = Arc::new(AtomicUsize::new(0));
    let verifier_calls = Arc::new(AtomicUsize::new(0));

    let hooks = Arc::clone(&hook_calls);
    let hook_stop = Arc::clone(&stop);
    let calls = Arc::clone(&verifier_calls);
    let config = Config::new("/definitely/not/a/java/binary", &install)
        .with_startup_timeout(Duration::from_secs(2))
        .with_test_runtime_base(runtime.clone())
        .with_stop(Arc::clone(&stop))
        .with_test_command(move |_command, _port| {
            hooks.fetch_add(1, Ordering::SeqCst);
            hook_stop.store(true, Ordering::SeqCst);
            Vec::new()
        });

    let (control, _sender) = control_channel();
    let events = SharedEvents::default();
    let result = supervise(
        config,
        move |_dir: &Path| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
        control,
        events.clone(),
    );

    assert_eq!(result, Ok(()));
    assert_eq!(events.text(), "STOPPED\n");
    assert!(!events.text().contains(EVENT_FAILED));
    assert_eq!(verifier_calls.load(Ordering::SeqCst), 1, "the verifier ran before the hook");
    assert_eq!(hook_calls.load(Ordering::SeqCst), 1, "the command hook was entered");
    assert_empty_dir(&runtime);
}

// ---------------------------------------------------------------------------------------------
// Port collision and ownership
// ---------------------------------------------------------------------------------------------

#[test]
fn port_collision_is_not_adopted_and_the_occupier_survives() {
    let root = TempRoot::new("collision");
    let install = root.install_dir();
    let runtime = root.runtime_base();
    let occupier = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = occupier.local_addr().unwrap().port();

    let fake = fake_config(&root, &install, &runtime, "fail", Duration::from_secs(10));
    let fake = FakeConfig { config: fake.config.with_test_port(port), ..fake };
    let pidfile = fake.pidfile.clone();
    let (control, _sender) = control_channel();
    let events = SharedEvents::default();
    let result = supervise(fake.config, |_dir: &Path| Ok(()), control, events.clone());

    assert_eq!(result, Err(Failure::ChildExited));
    assert_eq!(events.text(), "FAILED\n");
    assert!(wait_for_file(&pidfile, Duration::from_secs(2)));
    assert_reaped(read_pid(&pidfile));

    let connected = TcpStream::connect_timeout(&format!("127.0.0.1:{port}").parse().unwrap(), Duration::from_secs(2));
    assert!(connected.is_ok(), "the occupier stopped accepting connections");
    assert_empty_dir(&runtime);
}

#[test]
fn stopping_the_managed_child_leaves_an_unrelated_child_running() {
    let root = TempRoot::new("sibling");
    let install = root.install_dir();
    let runtime = root.runtime_base();
    let sibling_pidfile = root.path().join("sibling.pid");
    let mut sibling = fake_command("silent", &sibling_pidfile).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    assert!(wait_for_file(&sibling_pidfile, Duration::from_secs(10)));
    let sibling_pid = read_pid(&sibling_pidfile);
    assert_alive(sibling_pid);

    let fake = fake_config(&root, &install, &runtime, "marker", Duration::from_secs(10));
    let pidfile = fake.pidfile.clone();
    let (control, sender) = control_channel();
    let events = SharedEvents::default();
    let handle = {
        let events = events.clone();
        thread::spawn(move || supervise(fake.config, |_dir: &Path| Ok(()), control, events))
    };
    assert!(wait_for_bound(&events, Duration::from_secs(10)).is_some());
    sender.send(b"STOP\n".to_vec()).unwrap();
    assert_eq!(handle.join().unwrap(), Ok(()));

    assert_reaped(read_pid(&pidfile));
    assert_alive(sibling_pid);
    assert!(sibling.try_wait().unwrap().is_none());
    let _ = sibling.kill();
    let _ = sibling.wait();
}

// ---------------------------------------------------------------------------------------------
// Real verified fixture, when present. No download, no existing server touched.
// ---------------------------------------------------------------------------------------------

fn real_fixture() -> Option<(PathBuf, PathBuf)> {
    let java = PathBuf::from("/usr/bin/java");
    let install = PathBuf::from("/tmp/lt/LanguageTool-6.6");
    if java.is_file() && install.join("languagetool-server.jar").is_file() { Some((java, install)) } else { None }
}

#[cfg(unix)]
#[test]
fn real_fixture_reports_bound_serves_languages_and_stops_on_parent_eof() {
    use std::os::unix::net::UnixStream;

    let Some((java, install)) = real_fixture() else {
        eprintln!("skipped: /tmp/lt/LanguageTool-6.6 or /usr/bin/java is not present");
        return;
    };
    let root = TempRoot::new("real");
    let runtime = root.runtime_base();
    let (control, writer) = UnixStream::pair().unwrap();
    let (events_read, events_write) = UnixStream::pair().unwrap();
    events_read.set_read_timeout(Some(Duration::from_secs(45))).unwrap();
    let mut reader = BufReader::new(events_read);

    let config = Config::new(java, install).with_startup_timeout(Duration::from_secs(30)).with_test_runtime_base(runtime.clone());
    let handle = thread::spawn(move || supervise(config, |_dir: &Path| Ok(()), control, events_write));

    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let port = bound_port(&line).expect("real LanguageTool did not report BOUND");
    assert!(port > 0);

    let addr = format!("127.0.0.1:{port}").parse().unwrap();
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(10)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    stream.write_all(b"GET /v2/languages HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").unwrap();
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    assert!(
        response.starts_with("HTTP/1.1 200") || response.starts_with("HTTP/1.0 200"),
        "unexpected health response: {:?}",
        response.chars().take(200).collect::<String>()
    );
    assert!(response.contains("languages") || response.contains("language"), "health response did not describe languages");

    drop(writer);
    assert_eq!(handle.join().unwrap(), Ok(()));
    let mut stopped = String::new();
    reader.read_line(&mut stopped).unwrap();
    assert_eq!(stopped, "STOPPED\n");

    let mut released = false;
    for _ in 0..100 {
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            released = true;
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    assert!(released, "the managed port is still held after STOPPED");
    assert_empty_dir(&runtime);
}

#[cfg(unix)]
#[test]
fn real_fixture_refuses_an_occupied_port_without_adopting_it() {
    use std::os::unix::net::UnixStream;

    let Some((java, install)) = real_fixture() else {
        eprintln!("skipped: /tmp/lt/LanguageTool-6.6 or /usr/bin/java is not present");
        return;
    };
    let root = TempRoot::new("real-collision");
    let runtime = root.runtime_base();
    let occupier = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = occupier.local_addr().unwrap().port();
    let (control, writer) = UnixStream::pair().unwrap();
    let events = SharedEvents::default();
    let config =
        Config::new(java, install).with_startup_timeout(Duration::from_secs(30)).with_test_runtime_base(runtime.clone()).with_test_port(port);

    let result = supervise(config, |_dir: &Path| Ok(()), control, events.clone());
    drop(writer);
    assert_eq!(result, Err(Failure::ChildExited));
    assert_eq!(events.text(), "FAILED\n");
    let connected = TcpStream::connect_timeout(&format!("127.0.0.1:{port}").parse().unwrap(), Duration::from_secs(2));
    assert!(connected.is_ok(), "the unrelated occupier was stopped");
    assert_empty_dir(&runtime);
}
