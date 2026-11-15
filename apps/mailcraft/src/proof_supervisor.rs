// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Supervises one private, loopback-only LanguageTool server.
//!
//! This module is deliberately std-only. It owns exactly one Java child and the private runtime
//! directory it creates for that child. It does not perform HTTP health or warm-up; the caller does
//! that after a `BOUND <port>` event and before exposing the endpoint.
//!
//! Contract points implemented here:
//! - the caller's verifier approves the installed directory before a runtime directory is created
//!   or Java is spawned;
//! - Java is an absolute path, every argument is passed separately (never through a shell), stdin
//!   is null, and only a small environment whitelist survives;
//! - the startup marker is accepted only as a complete bounded line ending in ` Server started`;
//!   both output streams are drained continuously and never retained or logged;
//! - a port is chosen on 127.0.0.1. Binding it here is only a reservation with an unavoidable
//!   handoff race; the marker plus a live owned child are the actual proof of ownership. An
//!   occupied port therefore fails without adopting or touching the other listener;
//! - EOF or `STOP` on the private parent pipe kills and reaps only this child, including during
//!   startup, and only then reports `STOPPED`.
//!
//! Windows null files and hidden-window creation are compile-time guarded but have not been
//! exercised on Windows. Unsupported JVM flags make Java exit without the marker, which is
//! reported as a failure rather than silently dropped.

use std::ffi::OsString;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub const EVENT_BOUND: &str = "BOUND";
pub const EVENT_STOPPED: &str = "STOPPED";
pub const EVENT_FAILED: &str = "FAILED";

/// The complete line suffix the pinned LanguageTool 6.6 server writes only after a successful bind.
pub const SERVER_STARTED_MARKER: &str = " Server started";

const SERVER_JAR: &str = "languagetool-server.jar";
const SERVER_MAIN: &str = "org.languagetool.server.HTTPServer";
const CONFIG_FILE: &str = "server.properties";
const RUNTIME_PREFIX: &str = "mailcraft-proof";
const MAX_STARTUP_LINE: usize = 4096;
const MAX_CONTROL_LINE: usize = 64;
const READ_CHUNK: usize = 8192;
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const MAX_RUNTIME_DIR_ATTEMPTS: u32 = 32;

const CONFIG_TEXT: &str = "\
maxTextLength=16384
maxTextHardLength=16384
maxCheckTimeMillis=5000
maxCheckThreads=1
maxWorkQueueSize=2
cacheSize=0
pipelineCaching=false
";

static NEXT_RUNTIME_ID: AtomicU64 = AtomicU64::new(0);

/// Static failure kinds. No draft text, command line, JVM output, or filesystem path is included.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    InvalidConfig,
    VerificationRefused,
    InvalidInstallation,
    PortUnavailable,
    RuntimeDirectoryFailed,
    SpawnFailed,
    StartupTimedOut,
    StartupOutputOverflow,
    ChildExited,
    EventPipeClosed,
    Io,
}

#[cfg(test)]
type CommandHook = Box<dyn Fn(&mut Command, u16) -> Vec<(String, String)> + Send + Sync>;

/// What to run and how long to wait for the pinned server's startup marker.
pub struct Config {
    java: PathBuf,
    install_dir: PathBuf,
    startup_timeout: Duration,
    /// Shared with the private lifetime monitor (or any other owner-approved cancellation). A
    /// set flag means the caller no longer wants a server: the supervisor stops before it verifies,
    /// creates a runtime directory, or spawns a child.
    stop: Arc<AtomicBool>,
    #[cfg(test)]
    test_port: Option<u16>,
    #[cfg(test)]
    test_command: Option<CommandHook>,
    #[cfg(test)]
    test_runtime_base: Option<PathBuf>,
}

impl Config {
    pub fn new(java: impl Into<PathBuf>, install_dir: impl Into<PathBuf>) -> Self {
        Self {
            java: java.into(),
            install_dir: install_dir.into(),
            startup_timeout: Duration::from_secs(20),
            stop: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            test_port: None,
            #[cfg(test)]
            test_command: None,
            #[cfg(test)]
            test_runtime_base: None,
        }
    }

    #[cfg(test)]
    pub fn with_startup_timeout(mut self, timeout: Duration) -> Self {
        self.startup_timeout = timeout;
        self
    }

    /// Share a private cancellation flag with the supervisor.
    ///
    /// The helper passes the same flag its lifetime monitor sets on end of file or malformed
    /// control input. Every gate reads it synchronously, and a set flag is a normal stop: no
    /// verification refusal, no child and no `FAILED`. A caller that never sets it gets exactly
    /// the previous behavior.
    pub fn with_stop(mut self, stop: Arc<AtomicBool>) -> Self {
        self.stop = stop;
        self
    }

    /// Test-only: force a port so the collision path can be exercised. Never used in production.
    #[cfg(test)]
    pub(crate) fn with_test_port(mut self, port: u16) -> Self {
        self.test_port = Some(port);
        self
    }

    /// Test-only: replace the command after the normal arguments and environment are prepared.
    /// The returned pairs are applied after the whitelist, so a test binary can select its mode.
    #[cfg(test)]
    pub(crate) fn with_test_command(mut self, hook: impl Fn(&mut Command, u16) -> Vec<(String, String)> + Send + Sync + 'static) -> Self {
        self.test_command = Some(Box::new(hook));
        self
    }

    /// Test-only: keep runtime directories inside the test's own temporary root.
    #[cfg(test)]
    pub(crate) fn with_test_runtime_base(mut self, base: PathBuf) -> Self {
        self.test_runtime_base = Some(base);
        self
    }
}

/// Run until the parent pipe says stop (or closes), or until the owned child exits.
///
/// The private control pipe accepts the single line `STOP\n`; every other line is bounded and
/// discarded, and EOF is a stop too. On success the only event written is `STOPPED`: that covers a
/// stop known at any of the admission gates (including one shared through [`Config::with_stop`]) as
/// well as the parent pipe's `STOP` or EOF. `BOUND <port>` is written only after the marker and
/// while the owned child is still alive. A failed start writes `FAILED` and returns the static
/// `Failure`.
pub fn supervise<C, W>(config: Config, verifier: impl FnOnce(&Path) -> Result<(), ()>, parent_control: C, events: W) -> Result<(), Failure>
where
    C: Read + Send + 'static,
    W: Write,
{
    let mut events = events;
    match run(config, verifier, parent_control, &mut events) {
        Ok(()) => {
            // Every normal return from `run` is a stop; there is no other success path.
            let _ = write_event(&mut events, EVENT_STOPPED);
            Ok(())
        }
        Err(failure) => {
            let _ = write_event(&mut events, EVENT_FAILED);
            Err(failure)
        }
    }
}

fn run<C, W>(config: Config, verifier: impl FnOnce(&Path) -> Result<(), ()>, parent_control: C, events: &mut W) -> Result<(), Failure>
where
    C: Read + Send + 'static,
    W: Write,
{
    validate(&config)?;

    // A stop already known before any work leaves no trace at all.
    if stopped(&config) {
        return Ok(());
    }
    // Verification must precede the runtime directory and the child. A refusal leaves no trace,
    // but a cancellation observed while verification ran is a stop, never an integrity refusal.
    if verifier(&config.install_dir).is_err() {
        if stopped(&config) {
            return Ok(());
        }
        return Err(Failure::VerificationRefused);
    }
    if stopped(&config) {
        return Ok(());
    }
    if !config.install_dir.join(SERVER_JAR).is_file() {
        return Err(Failure::InvalidInstallation);
    }
    // The last gate before reserving a port and creating the private runtime directory.
    if stopped(&config) {
        return Ok(());
    }

    let port = choose_port(&config)?;
    let runtime = RuntimeDirectory::create(&config)?;
    let mut child = match spawn_child(&config, port, &runtime)? {
        Spawned::Child(child) => child,
        Spawned::Stopped => return Ok(()),
    };

    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            terminate(&mut child);
            return Err(Failure::SpawnFailed);
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            terminate(&mut child);
            return Err(Failure::SpawnFailed);
        }
    };

    let (signals, receiver) = mpsc::channel();
    let control_signals = signals.clone();
    let _control_thread = match spawn_named("mailcraft-proof-control", move || {
        monitor_control(parent_control, control_signals);
    }) {
        Ok(handle) => handle,
        Err(failure) => {
            terminate(&mut child);
            return Err(failure);
        }
    };
    let stdout_signals = signals.clone();
    let _stdout_thread = match spawn_named("mailcraft-proof-stdout", move || {
        monitor_stdout(stdout, stdout_signals);
    }) {
        Ok(handle) => handle,
        Err(failure) => {
            terminate(&mut child);
            return Err(failure);
        }
    };
    let _stderr_thread = match spawn_named("mailcraft-proof-stderr", move || {
        drain_quietly(stderr);
    }) {
        Ok(handle) => handle,
        Err(failure) => {
            terminate(&mut child);
            return Err(failure);
        }
    };
    drop(signals);

    let started = Instant::now();
    loop {
        if started.elapsed() >= config.startup_timeout {
            terminate(&mut child);
            return Err(Failure::StartupTimedOut);
        }
        let wait = config.startup_timeout.saturating_sub(started.elapsed()).min(POLL_INTERVAL);
        match receiver.recv_timeout(wait) {
            Ok(Signal::Marker) => match child.try_wait() {
                Ok(Some(_)) => {
                    terminate(&mut child);
                    return Err(Failure::ChildExited);
                }
                Ok(None) => {
                    if control_stop_pending(&receiver) {
                        terminate(&mut child);
                        return Ok(());
                    }
                    if write_event(events, &format!("{EVENT_BOUND} {port}")).is_err() {
                        terminate(&mut child);
                        return Err(Failure::EventPipeClosed);
                    }
                    break;
                }
                Err(_) => {
                    terminate(&mut child);
                    return Err(Failure::Io);
                }
            },
            Ok(Signal::StartupOutputOverflow) => {
                terminate(&mut child);
                return Err(Failure::StartupOutputOverflow);
            }
            Ok(Signal::ControlStop) => {
                terminate(&mut child);
                return Ok(());
            }
            Ok(Signal::StdoutEof) => {}
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                // Both monitor threads are gone and no stop signal was queued: the control side
                // cannot be reached any more, so this is treated as a stop.
                terminate(&mut child);
                return Ok(());
            }
        }
        match child.try_wait() {
            Ok(Some(_)) => {
                terminate(&mut child);
                return Err(Failure::ChildExited);
            }
            Ok(None) => {}
            Err(_) => {
                terminate(&mut child);
                return Err(Failure::Io);
            }
        }
    }

    loop {
        match receiver.recv_timeout(POLL_INTERVAL) {
            Ok(Signal::ControlStop) | Err(RecvTimeoutError::Disconnected) => {
                terminate(&mut child);
                return Ok(());
            }
            Ok(_) | Err(RecvTimeoutError::Timeout) => {}
        }
        match child.try_wait() {
            Ok(Some(_)) => {
                terminate(&mut child);
                return Err(Failure::ChildExited);
            }
            Ok(None) => {}
            Err(_) => {
                terminate(&mut child);
                return Err(Failure::Io);
            }
        }
    }
}

fn validate(config: &Config) -> Result<(), Failure> {
    if !config.java.is_absolute() || !config.install_dir.is_absolute() || config.startup_timeout.is_zero() {
        return Err(Failure::InvalidConfig);
    }
    Ok(())
}

fn stopped(config: &Config) -> bool {
    config.stop.load(Ordering::SeqCst)
}

fn choose_port(config: &Config) -> Result<u16, Failure> {
    #[cfg(test)]
    if let Some(port) = config.test_port {
        return Ok(port);
    }
    #[cfg(not(test))]
    let _ = config;
    // This is only a reservation. The listener is closed before Java starts, so an unrelated
    // process can still take the port; the startup marker plus a live child decide ownership.
    let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|_| Failure::PortUnavailable)?;
    let port = listener.local_addr().map_err(|_| Failure::PortUnavailable)?.port();
    drop(listener);
    if port == 0 {
        return Err(Failure::PortUnavailable);
    }
    Ok(port)
}

enum Spawned {
    Child(Child),
    Stopped,
}

fn spawn_child(config: &Config, port: u16, runtime: &RuntimeDirectory) -> Result<Spawned, Failure> {
    let mut command = Command::new(&config.java);
    command.args(launch_arguments(config, port, runtime.path()));
    apply_environment(&mut command, runtime.path());
    configure_io(&mut command, &config.install_dir);

    #[cfg(test)]
    let test_environment = match &config.test_command {
        Some(hook) => hook(&mut command, port),
        None => Vec::new(),
    };
    #[cfg(not(test))]
    let test_environment: Vec<(String, String)> = Vec::new();

    // Re-apply after a test hook, which may have replaced the whole command.
    apply_environment(&mut command, runtime.path());
    configure_io(&mut command, &config.install_dir);
    for (key, value) in test_environment {
        command.env(key, value);
    }

    // The last synchronous gate: after the test hook has had every chance to change the command
    // and immediately before the child would be created.
    if stopped(config) {
        return Ok(Spawned::Stopped);
    }
    command.spawn().map(Spawned::Child).map_err(|_| Failure::SpawnFailed)
}

fn launch_arguments(config: &Config, port: u16, runtime: &Path) -> Vec<OsString> {
    let mut arguments = Vec::with_capacity(14);
    arguments.push(OsString::from("-Xmx512m"));
    arguments.push(OsString::from("-Djava.net.preferIPv4Stack=true"));
    arguments.push(OsString::from("-Djava.awt.headless=true"));
    arguments.push(OsString::from("-XX:-HeapDumpOnOutOfMemoryError"));
    #[cfg(windows)]
    arguments.push(OsString::from("-XX:ErrorFile=NUL"));
    #[cfg(not(windows))]
    arguments.push(OsString::from("-XX:ErrorFile=/dev/null"));

    let mut temporary_directory = OsString::from("-Djava.io.tmpdir=");
    temporary_directory.push(runtime);
    arguments.push(temporary_directory);

    arguments.push(OsString::from("-cp"));
    arguments.push(config.install_dir.join(SERVER_JAR).into_os_string());
    arguments.push(OsString::from(SERVER_MAIN));
    arguments.push(OsString::from("--config"));
    arguments.push(runtime.join(CONFIG_FILE).into_os_string());
    arguments.push(OsString::from("--port"));
    arguments.push(OsString::from(port.to_string()));
    arguments
}

/// Applies the one sanitized environment every Java child of this program gets.
///
/// The environment is cleared and only a small whitelist survives, then the options that let an
/// ambient shell or desktop session inject JVM arguments are removed. Shared with the discovery
/// probe in `proof_java` so a `java -version` and the supervised server can never disagree about
/// which variables they see.
pub(crate) fn environment(command: &mut Command) {
    command.env_clear();
    #[cfg(unix)]
    const KEEP: &[&str] = &["PATH", "LANG", "LC_ALL", "LC_CTYPE", "TZ"];
    #[cfg(windows)]
    const KEEP: &[&str] = &["PATH", "PATHEXT", "SystemRoot", "SystemDrive", "WINDIR", "COMSPEC"];
    for key in KEEP {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    // env_clear already removed these; the explicit removals keep that guarantee visible.
    for key in ["JAVA_TOOL_OPTIONS", "_JAVA_OPTIONS", "JDK_JAVA_OPTIONS", "JAVA_OPTIONS", "CLASSPATH"] {
        command.env_remove(key);
    }
}

fn apply_environment(command: &mut Command, runtime: &Path) {
    environment(command);
    // The private runtime directory stays the child's only writable temporary location.
    #[cfg(unix)]
    command.env("TMPDIR", runtime);
    #[cfg(windows)]
    {
        command.env("TEMP", runtime);
        command.env("TMP", runtime);
    }
}

fn configure_io(command: &mut Command, install_dir: &Path) {
    command.current_dir(install_dir).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        // CREATE_NO_WINDOW: the server must never flash a console window. Untested on Windows.
        use std::os::windows::process::CommandExt;
        let _ = command.creation_flags(0x0800_0000);
    }
}

fn spawn_named(name: &str, work: impl FnOnce() + Send + 'static) -> Result<JoinHandle<()>, Failure> {
    thread::Builder::new().name(name.to_string()).spawn(work).map_err(|_| Failure::Io)
}

fn terminate(child: &mut Child) {
    let already_exited = matches!(child.try_wait(), Ok(Some(_)));
    if !already_exited {
        let _ = child.kill();
    }
    let _ = child.wait();
}

fn control_stop_pending(receiver: &mpsc::Receiver<Signal>) -> bool {
    while let Ok(signal) = receiver.try_recv() {
        if matches!(signal, Signal::ControlStop) {
            return true;
        }
    }
    false
}

fn write_event<W: Write>(events: &mut W, code: &str) -> Result<(), Failure> {
    events.write_all(code.as_bytes()).and_then(|()| events.write_all(b"\n")).and_then(|()| events.flush()).map_err(|_| Failure::EventPipeClosed)
}

#[derive(Clone, Copy, Debug)]
enum Signal {
    ControlStop,
    Marker,
    StartupOutputOverflow,
    StdoutEof,
}

/// Reads a bounded private control line. EOF and `STOP` both mean stop; anything else is discarded.
fn monitor_control<R: Read>(mut reader: R, signals: Sender<Signal>) {
    let mut line: Vec<u8> = Vec::with_capacity(MAX_CONTROL_LINE);
    let mut discarding = false;
    let mut buffer = [0u8; 128];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => {
                let _ = signals.send(Signal::ControlStop);
                return;
            }
            Ok(count) => {
                let chunk = match buffer.get(..count) {
                    Some(chunk) => chunk,
                    None => continue,
                };
                for byte in chunk {
                    if *byte == b'\n' {
                        let without_cr = line.len().saturating_sub(1);
                        let text = if line.last() == Some(&b'\r') { line.get(..without_cr).unwrap_or(&[]) } else { line.as_slice() };
                        if text == b"STOP".as_slice() {
                            let _ = signals.send(Signal::ControlStop);
                            return;
                        }
                        line.clear();
                        discarding = false;
                    } else if !discarding {
                        if line.len() >= MAX_CONTROL_LINE {
                            discarding = true;
                            line.clear();
                        } else {
                            line.push(*byte);
                        }
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => {
                let _ = signals.send(Signal::ControlStop);
                return;
            }
        }
    }
}

/// Finds the startup marker in complete, bounded lines, then drains and discards everything else.
fn monitor_stdout<R: Read>(mut reader: R, signals: Sender<Signal>) {
    let mut line: Vec<u8> = Vec::with_capacity(64);
    let mut discarding = false;
    let mut marker_seen = false;
    let mut buffer = [0u8; READ_CHUNK];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => {
                let _ = signals.send(Signal::StdoutEof);
                return;
            }
            Ok(count) => {
                let chunk = match buffer.get(..count) {
                    Some(chunk) => chunk,
                    None => continue,
                };
                for byte in chunk {
                    if marker_seen {
                        continue;
                    }
                    if *byte == b'\n' {
                        let without_cr = line.len().saturating_sub(1);
                        let text = if line.last() == Some(&b'\r') { line.get(..without_cr).unwrap_or(&[]) } else { line.as_slice() };
                        if text.ends_with(SERVER_STARTED_MARKER.as_bytes()) {
                            marker_seen = true;
                            let _ = signals.send(Signal::Marker);
                        }
                        line.clear();
                        discarding = false;
                    } else if !discarding {
                        if line.len() >= MAX_STARTUP_LINE {
                            discarding = true;
                            line.clear();
                            let _ = signals.send(Signal::StartupOutputOverflow);
                        } else {
                            line.push(*byte);
                        }
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => {
                let _ = signals.send(Signal::StdoutEof);
                return;
            }
        }
    }
}

/// Reads stderr in fixed chunks and keeps nothing.
fn drain_quietly<R: Read>(mut reader: R) {
    let mut buffer = [0u8; READ_CHUNK];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return,
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        }
    }
}

/// A uniquely named, private directory containing only this child's server properties.
struct RuntimeDirectory {
    path: PathBuf,
    config_path: PathBuf,
}

impl RuntimeDirectory {
    fn create(config: &Config) -> Result<Self, Failure> {
        let base = runtime_base(config);
        for _ in 0..MAX_RUNTIME_DIR_ATTEMPTS {
            let id = NEXT_RUNTIME_ID.fetch_add(1, Ordering::Relaxed);
            let path = base.join(format!("{RUNTIME_PREFIX}-{}-{id}", std::process::id()));
            match create_private_directory(&path) {
                Ok(()) => {
                    let config_path = path.join(CONFIG_FILE);
                    if write_private_config(&config_path).is_ok() {
                        return Ok(Self { path, config_path });
                    }
                    let _ = fs::remove_file(&config_path);
                    let _ = fs::remove_dir(&path);
                    return Err(Failure::RuntimeDirectoryFailed);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(_) => return Err(Failure::RuntimeDirectoryFailed),
            }
        }
        Err(Failure::RuntimeDirectoryFailed)
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for RuntimeDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.config_path);
        // Remove only a real directory we created; never follow a replaced symlink.
        match fs::symlink_metadata(&self.path) {
            Ok(metadata) if metadata.file_type().is_dir() => {
                let _ = fs::remove_dir_all(&self.path);
            }
            _ => {}
        }
    }
}

fn runtime_base(config: &Config) -> PathBuf {
    #[cfg(test)]
    if let Some(base) = &config.test_runtime_base {
        return base.clone();
    }
    #[cfg(not(test))]
    let _ = config;
    std::env::temp_dir()
}

fn create_private_directory(path: &Path) -> io::Result<()> {
    let mut builder = DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

fn write_private_config(path: &Path) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(CONFIG_TEXT.as_bytes())?;
    file.flush()
}
