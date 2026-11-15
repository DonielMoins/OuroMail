// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! The private native proofing helper: the only part of MailCraft that finds Java and owns the
//! LanguageTool server process.
//!
//! The desktop app starts this same executable with [`crate::proof_protocol::Mode::arguments`] and
//! a private stdin pipe that the app holds for the helper's whole life. This module's contract:
//!
//! - [`run`] is the entry point and runs before logging, the mailbox or single-instance handling.
//!   A terminal stdin is refused: the helper never reads the owner's keyboard.
//! - A lifetime monitor is installed on that private pipe before any installation is read or any
//!   Java candidate is probed. It is the only reader of process stdin, and it sets one shared stop
//!   flag on end of file, on any control input (the host's only command is `STOP`, so anything else
//!   is malformed) and on a read error. Everything below polls that flag, so an abruptly closed pipe
//!   (the application died) ends the work instead of leaving it running.
//! - Probe mode runs only the bounded, cancellable discovery and reports one fixed
//!   `PROBE <status>` line. Serve mode accepts no Java executable or JAR from its arguments: it
//!   reads only the pinned `mailcraft-net` locator's verified installation, re-verifies that same
//!   directory inside the supervisor before a child is spawned, discovers Java with the stop flag,
//!   and hands the supervisor a read adapter that returns end of file once the flag is set. There
//!   is never a second reader of process stdin.
//! - The supervisor owns, kills and reaps the Java child; this module returns only after the
//!   supervisor has done so. It therefore never knowingly exits while a child it owns is alive.
//!   Success means the supervisor stopped the server; a failed start is reported with a fixed
//!   exit status and a `FAILED` event.
//!
//! What is guaranteed and what is not: the supported lifetime guarantee is the death of the
//! *application*, which closes the private pipe. An external `SIGKILL` of the helper itself runs no
//! cleanup code at all on any platform, so this module does not claim it can kill Java in that
//! case. Only the supervisor's own child is ever signalled; no other process or listener is
//! touched.
//!
//! Paths, launcher names, raw control input, Java output and command lines never appear in an
//! event, an exit status, an error or `Debug` output. Windows null files, hidden-window creation
//! and process creation are compile-time guarded but have only been exercised on Linux.

use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use mailcraft_net::languagetool_install::{self, InstallError, Installed};
use mailcraft_ui_egui::proof_server::Java;

use crate::proof_java::{self, Discovery};
use crate::proof_protocol::{
    EXIT_INTEGRITY, EXIT_INVOCATION, EXIT_JAVA_MISSING, EXIT_JAVA_TOO_OLD, EXIT_NOT_INSTALLED, EXIT_TIMEOUT, Event, Mode, write_event,
};
use crate::proof_supervisor::{self, Config, Failure};

/// One read from the private lifetime pipe.
const CONTROL_CHUNK: usize = 128;

/// How often the supervisor's control adapter asks the stop flag.
const STOP_POLL: Duration = Duration::from_millis(10);

/// Any failure without a fixed protocol status. The host maps every other code to `StartFailed`.
const EXIT_START_FAILED: i32 = 1;

/// Run one helper invocation and return the process exit status.
///
/// The caller must have already dispatched on [`crate::proof_protocol::Mode`] before any logging,
/// mailbox or single-instance startup, and must exit the process with the returned status. A
/// normal stop (the application closed the private pipe) is `0`; a terminal stdin returns
/// `EXIT_INVOCATION`; installation, discovery and startup failures return the fixed protocol
/// statuses defined by [`crate::proof_protocol`].
pub fn run(mode: Mode) -> i32 {
    let terminal = io::stdin().is_terminal();
    run_with(mode, terminal, io::stdin(), io::stdout(), &Native)
}

/// The composition seam: production passes the real stdin, stdout and [`Native`] engine. Tests in
/// this crate pass a real pipe, a capturing writer and a scripted engine, so the lifetime monitor,
/// the cancellation gates and the exit mapping are exercised as the process runs them. Nothing
/// here accepts a command, a Java path or a JAR: [`Native`] is the only production engine and it
/// can only call the pinned locator, bounded discovery and the fixed supervisor.
pub(crate) fn run_with<R, W, E>(mode: Mode, terminal: bool, lifetime: R, events: W, engine: &E) -> i32
where
    R: Read + Send + 'static,
    W: Write,
    E: Engine,
{
    if terminal {
        return EXIT_INVOCATION;
    }
    let stop = Arc::new(AtomicBool::new(false));
    if Lifetime::start(lifetime, Arc::clone(&stop)).is_err() {
        return EXIT_START_FAILED;
    }
    let mut events = events;
    match mode {
        Mode::Probe => probe(engine, &stop, &mut events),
        Mode::Serve { base } => serve(&base, engine, &stop, &mut events),
    }
}

/// What the helper needs from the outside world. Implemented only by [`Native`] in production; the
/// test module implements it over real pipes, scripted answers and the real supervisor.
pub(crate) trait Engine {
    /// The bounded Java 17+ discovery, giving up within one poll once `stop` is set.
    fn discover(&self, stop: &AtomicBool) -> Discovery;
    /// The pinned installation under `base`, checked whole. Read-only.
    fn locate(&self, base: &Path) -> Result<Option<Installed>, InstallError>;
    /// Re-verify that `directory` is still the pinned installation.
    fn verify(&self, base: &Path, directory: &Path) -> Result<(), ()>;
    /// Run the supervisor exactly once with the helper's shared stop flag. It owns, kills and
    /// reaps its child, and returns only after the child is gone or was never started. `control`
    /// is the lifetime adapter; `stop` is the same flag the lifetime monitor sets.
    fn supervise<V>(
        &self,
        java: PathBuf,
        install: PathBuf,
        stop: Arc<AtomicBool>,
        verifier: V,
        control: Box<dyn Read + Send>,
        events: &mut dyn Write,
    ) -> Result<(), Failure>
    where
        V: FnOnce(&Path) -> Result<(), ()>;
}

/// The production engine: the real pinned locator, the real bounded discovery and the real
/// supervisor. No argument, environment variable or control message can replace any of them.
pub(crate) struct Native;

impl Engine for Native {
    fn discover(&self, stop: &AtomicBool) -> Discovery {
        proof_java::discover_cancellable(stop)
    }

    fn locate(&self, base: &Path) -> Result<Option<Installed>, InstallError> {
        languagetool_install::locate(base)
    }

    fn verify(&self, base: &Path, directory: &Path) -> Result<(), ()> {
        match languagetool_install::locate(base) {
            Ok(Some(installed)) if installed.directory == directory => Ok(()),
            // A missing, changed or unreadable installation is a refusal, never a silent pass.
            _ => Err(()),
        }
    }

    fn supervise<V>(
        &self,
        java: PathBuf,
        install: PathBuf,
        stop: Arc<AtomicBool>,
        verifier: V,
        control: Box<dyn Read + Send>,
        events: &mut dyn Write,
    ) -> Result<(), Failure>
    where
        V: FnOnce(&Path) -> Result<(), ()>,
    {
        proof_supervisor::supervise(Config::new(java, install).with_stop(stop), verifier, control, events)
    }
}

/// The helper's lifetime: one detached thread reading the private pipe, and the shared stop flag.
struct Lifetime;

impl Lifetime {
    /// Install the monitor before anything else runs. The handle is detached on purpose: the
    /// helper's work ends independently of the pipe, and the process exit releases the blocked
    /// read. Nothing waits on this thread.
    fn start<R: Read + Send + 'static>(reader: R, stop: Arc<AtomicBool>) -> Result<(), ()> {
        thread::Builder::new().name("mailcraft-proof-lifetime".to_string()).spawn(move || monitor(reader, stop)).map(|_handle| ()).map_err(|_| ())
    }
}

/// End of file, any control input or a read error stops the helper at once.
///
/// The only line the application ever writes is `STOP`, and its other action is closing the pipe;
/// there is no other valid command. Waiting for a complete line (or a length cap) would let one to
/// sixty-three unknown bytes without a newline stall cancellation, so any successful read stops
/// immediately. Input is never inspected or retained beyond the fact that bytes arrived, so no
/// private control text can reach an error or a log. A read error is a stop too: a pipe that cannot
/// be read is not a lifeline.
fn monitor<R: Read>(mut reader: R, stop: Arc<AtomicBool>) {
    let mut buffer = [0u8; CONTROL_CHUNK];
    loop {
        match reader.read(&mut buffer) {
            Ok(_) => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    stop.store(true, Ordering::SeqCst);
}

/// The supervisor's private control pipe. It is never process stdin: it waits on the lifetime
/// flag and then reports end of file, which is exactly the supervisor's existing stop signal.
struct StopAware {
    stop: Arc<AtomicBool>,
    ended: bool,
}

impl Read for StopAware {
    fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
        if self.ended {
            return Ok(0);
        }
        while !self.stop.load(Ordering::SeqCst) {
            thread::sleep(STOP_POLL);
        }
        self.ended = true;
        Ok(0)
    }
}

/// Probe mode: one cancellable discovery pass and one fixed event.
fn probe<E: Engine, W: Write>(engine: &E, stop: &Arc<AtomicBool>, events: &mut W) -> i32 {
    if stop.load(Ordering::SeqCst) {
        return stopped(events);
    }
    let discovery = engine.discover(stop.as_ref());
    // A pass that was stopped never claims a usable launcher, whatever it happened to find.
    if stop.load(Ordering::SeqCst) {
        return stopped(events);
    }
    match write_event(Event::Probe(discovery.status), events) {
        Ok(()) => 0,
        Err(_) => EXIT_START_FAILED,
    }
}

/// Serve mode: pinned installation, cancellable discovery, then the supervisor.
fn serve<E: Engine, W: Write>(base: &Path, engine: &E, stop: &Arc<AtomicBool>, events: &mut W) -> i32 {
    if stop.load(Ordering::SeqCst) {
        return stopped(events);
    }
    // Only the pinned locator's answer is used. Nothing here is taken from the invocation.
    let found = match engine.locate(base) {
        Ok(found) => found,
        Err(InstallError::Integrity) => return failed(events, EXIT_INTEGRITY),
        Err(_) => return failed(events, EXIT_START_FAILED),
    };
    // `locate` hashes the whole installation and can take seconds: a stop that arrived during it
    // must end the helper before Java discovery or any child.
    if stop.load(Ordering::SeqCst) {
        return stopped(events);
    }
    let Some(installed) = found else {
        return failed(events, EXIT_NOT_INSTALLED);
    };
    let discovery = engine.discover(stop.as_ref());
    if stop.load(Ordering::SeqCst) {
        return stopped(events);
    }
    let java = match (discovery.status, discovery.executable) {
        (Java::Available, Some(java)) => java,
        (Java::Missing, _) => return failed(events, EXIT_JAVA_MISSING),
        (Java::TooOld, _) => return failed(events, EXIT_JAVA_TOO_OLD),
        _ => return failed(events, EXIT_START_FAILED),
    };
    // Last gate before the supervisor: a cancelled helper starts no child at all.
    if stop.load(Ordering::SeqCst) {
        return stopped(events);
    }
    let install = installed.directory;
    let expected = install.clone();
    let base = base.to_path_buf();
    let verifier = move |directory: &Path| {
        if directory != expected.as_path() {
            return Err(());
        }
        engine.verify(&base, directory)
    };
    let control = Box::new(StopAware { stop: Arc::clone(stop), ended: false });
    match engine.supervise(java, install, Arc::clone(stop), verifier, control, events) {
        Ok(()) => 0,
        Err(failure) => exit_for(failure),
    }
}

/// A normal stop writes `STOPPED`; failing to write it is not a start failure worth a different
/// exit status, because the host maps any non-zero code to a fixed issue.
fn stopped<W: Write>(events: &mut W) -> i32 {
    match write_event(Event::Stopped, events) {
        Ok(()) => 0,
        Err(_) => EXIT_START_FAILED,
    }
}

/// A refusal before the supervisor: report `FAILED`, then the fixed status for the reason.
fn failed<W: Write>(events: &mut W, code: i32) -> i32 {
    let _ = write_event(Event::Failed, events);
    code
}

/// Map the supervisor's static failure to the protocol's fixed exit status.
fn exit_for(failure: Failure) -> i32 {
    match failure {
        Failure::VerificationRefused => EXIT_INTEGRITY,
        Failure::InvalidInstallation => EXIT_NOT_INSTALLED,
        Failure::StartupTimedOut => EXIT_TIMEOUT,
        _ => EXIT_START_FAILED,
    }
}
