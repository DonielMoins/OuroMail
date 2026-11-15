// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! The native managed proofing host (#954): the owner's explicit, bounded control of one pinned
//! LanguageTool installation and one private helper process.
//!
//! This module implements `proof_server::ManagedServer` against the shared admission/generation
//! state in [`crate::proof_host_state`] and the fixed helper protocol in
//! [`crate::proof_protocol`]. It is deliberately asynchronous: `status` is a cached snapshot,
//! `request` admits at most one operation under a short lock, and every file, probe, installer,
//! helper and HTTP exchange runs on one lazily made worker.
//!
//! Contract points implemented here:
//! - **Passive construction.** A `None` or relative base is `Unavailable`. A valid base starts no
//!   thread, touches no file, starts no process and opens no socket until the owner's first
//!   admitted `Check`, `Download` or `Start`; `status` never does any of them.
//! - **One bounded worker.** The first admitted operation makes one worker thread and one bounded
//!   one-slot queue. Later requests are admitted by the shared state or refused without queueing
//!   anything. `Cancel`/`Stop` only set a flag and revoke the endpoint under the same short lock;
//!   they never write a pipe or wait for a process on the caller's thread.
//! - **The helper is the only process.** `Check` runs `locate` and a short `Probe` helper;
//!   `Download` runs the pinned installer, reports progress and never starts the server; `Start`
//!   runs one `Serve` helper, accepts `BOUND` only while that owned child is alive, then asks the
//!   real readiness exchange (a bodyless language-list request and one synthetic English and one
//!   synthetic French check, each with at least one actually checked segment) before publishing the
//!   literal loopback endpoint. A check the server itself refuses (its own 500 while a cold model
//!   loads) is asked once more — never more than twice, and only for that refusal — so that a cold
//!   start is not a failed one. No Java path, JAR, address or command line crosses the protocol. A
//!   `Start` whose cancellation is already known before the helper is launched (or while the
//!   launcher builds its command) spawns no helper at all and finishes through the ordinary
//!   cancelled/stopped path, keeping every verified fact.
//! - **Honest cleanup.** A helper is never killed to make a status look finished. `Stop`/`Cancel`
//!   close the helper's private lifetime pipe and revoke the endpoint under the short state lock;
//!   the host then stays in `Stopping`/`Cancelling` until the owned helper has actually been
//!   reaped. The same is true of the short `Probe` helper, which owns its own Java discovery and
//!   must be allowed to end by itself. A failure likewise revokes the endpoint before any cleanup
//!   and becomes terminal `Failed` only after the owned helper is gone. Dropping the service closes
//!   admission and wakes the worker within a fixed bound; abrupt application death closes the
//!   operating-system pipe, which is the helper's own stop signal.
//! - **Hostile output is refused whole.** Every frame is read by `proof_protocol::read_event`
//!   (at most 64 bytes), a bounded total event count and a bounded channel. A malformed, partial
//!   or oversized frame ends the operation; a later valid prefix is never accepted. A `Probe` is
//!   accepted only when it is exactly one `PROBE` event, a clean natural end of file and a
//!   successful owned exit status.
//! - **Bounded readers.** Event readers are admitted from one process-wide, bounded pool before a
//!   child is spawned, so repeated or hostile operations can never accumulate threads. A reader
//!   whose child exited while a descendant still holds the write end of its stdout is detached
//!   with its permit, never joined; the permit is released when that pipe finally closes.
//! - **No secrets, drafts or paths in errors.** Every failure is a fixed [`Issue`], every
//!   `Debug` rendering is redacted, and nothing here logs.

use std::fmt;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, TryLockError, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use mailcraft_model::proof::Language;
use mailcraft_net::languagetool::{self, CheckOutcome, LanguageToolError};
use mailcraft_net::languagetool_install::{self, InstallError, Installed, Progress};
use mailcraft_ui_egui::proof_server::{Action, Issue, Java, ManagedServer, Phase, Status};

use crate::proof_host_state::State;
use crate::proof_protocol::{self, Event, Mode};

/// How many frames one helper may send before it is treated as not the helper.
const MAX_HELPER_EVENTS: usize = 16;
/// How many frames may wait between the reader and the worker.
const EVENT_CHANNEL: usize = 8;
/// How many helper event readers may exist at once, for the whole process. A bounded pool makes
/// repeated operations and a hostile descendant holding a pipe unable to accumulate threads.
const EVENT_SLOT_CAP: usize = 8;
/// One explicit `Check`, `Download` or `Start` has this long in total.
const START_BUDGET: Duration = Duration::from_secs(60);
/// A short `Probe` helper has this long to answer.
const PROBE_BUDGET: Duration = Duration::from_secs(60);
/// The largest wait between cancellation/liveness checks.
const STARTUP_POLL: Duration = Duration::from_millis(50);
/// The running supervisor's liveness tick.
const RUN_POLL: Duration = Duration::from_millis(25);
/// The cleanup reaping tick.
const REAP_POLL: Duration = Duration::from_millis(25);
/// How long `Drop` waits for the worker to acknowledge shutdown.
const DROP_WAIT: Duration = Duration::from_millis(100);
/// Never wait longer than the grammar adapter's own patience for one readiness exchange.
const WARM_PATIENCE: Duration = Duration::from_secs(8);
/// How many times one synthetic readiness check may be asked. A grammar model that is still
/// loading answers the first check for its language with the server's own refusal (its HTTP 500
/// after the check limit), and the identical check asked once more is answered. Two attempts and
/// no more: a server that keeps refusing is a real failure, not a slow start.
const WARM_ATTEMPTS: usize = 2;
/// A fixed synthetic sentence, never draft text, for the English readiness check.
const ENGLISH_WARMUP: &str = "This sentence is only a readiness check.";
/// A fixed synthetic sentence, never draft text, for the French readiness check.
const FRENCH_WARMUP: &str = "Cette phrase sert seulement à vérifier le serveur.";

/// Called from a worker thread after a meaningful status change.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

/// The process-wide limit for live helper event readers. A test may lower it; production never
/// raises it above its fixed cap.
static EVENT_SLOT_LIMIT: Mutex<usize> = Mutex::new(EVENT_SLOT_CAP);
/// Readers whose thread is alive right now. Each owns one admitted slot until it sees end of file.
static EVENT_SLOTS: Mutex<usize> = Mutex::new(0);

/// One admitted event-reader slot. Dropping it returns the slot to the process-wide pool.
pub struct SlotHold;

impl Drop for SlotHold {
    fn drop(&mut self) {
        let mut slots = EVENT_SLOTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        *slots = slots.saturating_sub(1);
    }
}

/// Try to admit one event reader. `None` means the bounded pool is full: no child may be spawned.
pub fn acquire_event_slot() -> Option<SlotHold> {
    let limit = *EVENT_SLOT_LIMIT.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut slots = EVENT_SLOTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if *slots >= limit {
        return None;
    }
    *slots += 1;
    Some(SlotHold)
}

/// Lower the process-wide reader cap. Tests use a small value; production never calls this.
#[cfg(test)]
pub fn set_event_slot_limit(limit: usize) {
    let limit = limit.max(1);
    *EVENT_SLOT_LIMIT.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = limit;
}

/// Restore the production cap. Test-only, so one pool test cannot starve the others.
#[cfg(test)]
pub fn reset_event_pool() {
    *EVENT_SLOT_LIMIT.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = EVENT_SLOT_CAP;
}

/// How many event readers are alive right now.
#[cfg(test)]
pub fn event_slots_in_use() -> usize {
    *EVENT_SLOTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------------------------------
// Injected seams. Production uses the real installer, the real grammar adapter and this binary.
// ---------------------------------------------------------------------------------------------

/// Builds the private helper command. The real one names only `current_exe` and the fixed protocol.
pub(crate) trait Launcher: Send + Sync {
    fn command(&self, mode: &Mode) -> io::Result<Command>;
}

/// The pinned installer and verifier. Production forwards to `mailcraft_net`.
pub(crate) trait Installer: Send + Sync {
    fn locate(&self, base: &Path) -> Result<Option<Installed>, InstallError>;
    fn install(&self, base: &Path, cancel: &AtomicBool, progress: &dyn Fn(Progress)) -> Result<Installed, InstallError>;
}

/// The real health/warm-up exchange, measured against one literal loopback endpoint.
pub(crate) trait Readiness: Send + Sync {
    fn warm(&self, endpoint: &str, deadline: Instant, cancelled: &dyn Fn() -> bool) -> Result<(), Issue>;
}

/// Makes the one worker thread. A function object so tests can make it fail.
pub(crate) type Spawn = Arc<dyn Fn(&str, Box<dyn FnOnce() + Send + 'static>) -> io::Result<JoinHandle<()>> + Send + Sync>;

fn spawn_thread(name: &str, body: Box<dyn FnOnce() + Send + 'static>) -> io::Result<JoinHandle<()>> {
    thread::Builder::new().name(name.to_string()).spawn(body)
}

struct NativeLauncher;

impl Launcher for NativeLauncher {
    fn command(&self, mode: &Mode) -> io::Result<Command> {
        let executable = std::env::current_exe()?;
        let mut command = Command::new(executable);
        command.args(mode.arguments());
        Ok(command)
    }
}

struct NativeInstaller;

impl Installer for NativeInstaller {
    fn locate(&self, base: &Path) -> Result<Option<Installed>, InstallError> {
        languagetool_install::locate(base)
    }

    fn install(&self, base: &Path, cancel: &AtomicBool, progress: &dyn Fn(Progress)) -> Result<Installed, InstallError> {
        languagetool_install::install(base, cancel, progress)
    }
}

/// The real health/warm-up exchange, measured against one literal loopback endpoint.
///
/// Named outside this module only so the tests of the private path can ask the exchange the
/// program itself runs; nothing of the exchange changes for it.
pub(crate) struct NativeReadiness;

impl Readiness for NativeReadiness {
    fn warm(&self, endpoint: &str, deadline: Instant, cancelled: &dyn Fn() -> bool) -> Result<(), Issue> {
        if !warm_endpoint_allowed(endpoint) {
            return Err(Issue::StartFailed);
        }
        if cancelled() {
            return Err(Issue::Cancelled);
        }
        // A bodyless `/v2/languages` first: the server answers, and the answer is the shape the
        // adapter defines. It proves readiness, not either grammar model.
        let patience = warm_patience(deadline)?;
        languagetool::ready(endpoint, patience.min(WARM_PATIENCE)).map_err(readiness_issue)?;
        let english = warm_check(endpoint, ENGLISH_WARMUP, Language::EnCa, deadline, cancelled)?;
        let french = warm_check(endpoint, FRENCH_WARMUP, Language::FrCa, deadline, cancelled)?;
        if warm_outcomes_ok(&english, &french) { Ok(()) } else { Err(Issue::StartFailed) }
    }
}

fn warm_check(endpoint: &str, text: &str, language: Language, deadline: Instant, cancelled: &dyn Fn() -> bool) -> Result<CheckOutcome, Issue> {
    for attempt in 1..=WARM_ATTEMPTS {
        if cancelled() {
            return Err(Issue::Cancelled);
        }
        // What the start's own budget has left, worked out again for this attempt: a retry never
        // borrows patience the start does not have.
        let patience = warm_patience(deadline)?;
        match languagetool::check_multilingual(endpoint, text, &[language], &[], patience.min(WARM_PATIENCE)) {
            Ok(outcome) => return Ok(outcome),
            // The server answered and said it could not check the text. On a cold start that is a
            // model still loading, which answers the very same check a second time; any other
            // answer (garbled, refused, too long, too slow) is not mended by asking again.
            Err(LanguageToolError::Failed) if attempt < WARM_ATTEMPTS => {}
            Err(error) => return Err(readiness_issue(error)),
        }
    }
    Err(Issue::StartFailed)
}

/// Both synthetic sentences must actually have been checked. An `Ok` with every segment skipped
/// (for example by the adapter's confidence gate) is not proof that English and French grammar
/// models started, and must not publish an endpoint.
pub(crate) fn warm_outcomes_ok(english: &CheckOutcome, french: &CheckOutcome) -> bool {
    english.checked_segments > 0 && french.checked_segments > 0
}

fn warm_patience(deadline: Instant) -> Result<Duration, Issue> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() { Err(Issue::StartTimedOut) } else { Ok(left) }
}

fn warm_endpoint_allowed(endpoint: &str) -> bool {
    let Some(port) = endpoint.strip_prefix("http://127.0.0.1:") else { return false };
    !port.is_empty() && port.len() <= 5 && port.bytes().all(|byte| byte.is_ascii_digit()) && port.parse::<u16>().is_ok_and(|port| port != 0)
}

fn readiness_issue(error: LanguageToolError) -> Issue {
    match error {
        LanguageToolError::TooSlow => Issue::StartTimedOut,
        _ => Issue::StartFailed,
    }
}

fn issue_for_install(error: InstallError) -> Issue {
    match error {
        InstallError::Download => Issue::DownloadFailed,
        InstallError::Integrity => Issue::IntegrityFailed,
        InstallError::Archive => Issue::InvalidArchive,
        InstallError::Io => Issue::InstallFailed,
        InstallError::Busy => Issue::InstallFailed,
        InstallError::Cancelled => Issue::Cancelled,
    }
}

// ---------------------------------------------------------------------------------------------
// Configuration and constructors.
// ---------------------------------------------------------------------------------------------

/// Test-only launch gate: parks `run_start` at its entry until the test has queued a cancellation,
/// so the pre-launch checks are exercised deterministically instead of racing the worker.
/// Production builds have no gate and pay nothing.
#[cfg(test)]
#[derive(Clone)]
pub(crate) struct LaunchGate {
    state: Arc<(Mutex<GateState>, Condvar)>,
}

#[cfg(test)]
#[derive(Clone, Copy, Default)]
struct GateState {
    armed: bool,
    parked: bool,
}

#[cfg(test)]
impl LaunchGate {
    pub(crate) fn new() -> Self {
        Self { state: Arc::new((Mutex::new(GateState::default()), Condvar::new())) }
    }

    /// Arm the gate for the next `run_start` entry.
    pub(crate) fn arm(&self) {
        let (lock, ready) = &*self.state;
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.armed = true;
        state.parked = false;
        ready.notify_all();
    }

    /// Park the worker until the gate is opened. Called at `run_start`'s entry.
    fn park(&self) {
        let (lock, ready) = &*self.state;
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if !state.armed {
            return;
        }
        state.parked = true;
        ready.notify_all();
        while state.armed {
            state = ready.wait(state).unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        state.parked = false;
    }

    /// Wait, bounded, until `run_start` is actually parked at the gate.
    pub(crate) fn wait_parked(&self, limit: Duration) -> bool {
        let (lock, ready) = &*self.state;
        let started = Instant::now();
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        while !state.parked {
            let left = limit.saturating_sub(started.elapsed());
            if left.is_zero() {
                return false;
            }
            let (guard, _) = ready.wait_timeout(state, left).unwrap_or_else(|poisoned| poisoned.into_inner());
            state = guard;
        }
        true
    }

    /// Open the gate for good.
    pub(crate) fn open(&self) {
        let (lock, ready) = &*self.state;
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.armed = false;
        ready.notify_all();
    }
}

pub(crate) struct Config {
    base: PathBuf,
    wake: Wake,
    launcher: Arc<dyn Launcher>,
    installer: Arc<dyn Installer>,
    readiness: Arc<dyn Readiness>,
    spawn: Spawn,
    #[cfg(test)]
    launch_gate: Option<LaunchGate>,
}

impl Config {
    fn native(base: PathBuf, wake: Wake) -> Config {
        Config {
            base,
            wake,
            launcher: Arc::new(NativeLauncher),
            installer: Arc::new(NativeInstaller),
            readiness: Arc::new(NativeReadiness),
            spawn: Arc::new(spawn_thread),
            #[cfg(test)]
            launch_gate: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn testing(
        base: PathBuf,
        wake: Wake,
        launcher: Arc<dyn Launcher>,
        installer: Arc<dyn Installer>,
        readiness: Arc<dyn Readiness>,
        spawn: Spawn,
    ) -> Config {
        Config { base, wake, launcher, installer, readiness, spawn, launch_gate: None }
    }

    #[cfg(test)]
    pub(crate) fn with_launch_gate(mut self, gate: LaunchGate) -> Config {
        self.launch_gate = Some(gate);
        self
    }
}

/// The window's service factory. `None`/a relative base is a passive `Unavailable` host.
pub fn service(base: Option<PathBuf>, wake: Arc<dyn Fn() + Send + Sync>) -> Arc<dyn ManagedServer> {
    match base {
        Some(base) if base.is_absolute() => start(Config::native(base, wake)),
        _ => Arc::new(Unavailable),
    }
}

/// Start a host from a prepared configuration. Production reaches it through [`service`].
pub(crate) fn start(config: Config) -> Arc<dyn ManagedServer> {
    let shared = Arc::new(Shared {
        base: config.base,
        wake: config.wake,
        cancel: Arc::new(AtomicBool::new(false)),
        cancel_ticket: AtomicU64::new(0),
        inner: Mutex::new(Inner { state: State::new(true), pending: None, closed: false, finished: false, worker: None }),
        ready: Condvar::new(),
        #[cfg(test)]
        launch_gate: config.launch_gate,
    });
    Arc::new(Host { shared, launcher: config.launcher, installer: config.installer, readiness: config.readiness, spawn: config.spawn })
}

struct Unavailable;

impl ManagedServer for Unavailable {
    fn status(&self) -> Status {
        Status { phase: Phase::Unavailable, issue: Some(Issue::Unavailable), ..Status::default() }
    }

    fn request(&self, _action: Action) -> Result<(), Issue> {
        Err(Issue::Unavailable)
    }
}

impl fmt::Debug for Unavailable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ProofHost { phase: Unavailable }")
    }
}

// ---------------------------------------------------------------------------------------------
// The host and its one worker.
// ---------------------------------------------------------------------------------------------

struct Host {
    shared: Arc<Shared>,
    launcher: Arc<dyn Launcher>,
    installer: Arc<dyn Installer>,
    readiness: Arc<dyn Readiness>,
    spawn: Spawn,
}

pub(crate) struct Shared {
    base: PathBuf,
    wake: Wake,
    /// What the real installer was last allowed to see. Only a cancellation or a new operation moves it.
    cancel: Arc<AtomicBool>,
    /// The one generation whose cancellation is currently requested; zero when none is.
    cancel_ticket: AtomicU64,
    inner: Mutex<Inner>,
    ready: Condvar,
    /// Test-only: the deterministic launch gate described above.
    #[cfg(test)]
    launch_gate: Option<LaunchGate>,
}

struct Inner {
    state: State,
    /// At most the one operation the shared state admitted and the worker has not taken yet.
    pending: Option<Work>,
    closed: bool,
    finished: bool,
    worker: Option<JoinHandle<()>>,
}

#[derive(Clone, Copy)]
struct Work {
    ticket: u64,
    action: Action,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn snapshot(&self) -> Status {
        self.lock().state.snapshot()
    }

    fn phase(&self) -> Phase {
        self.lock().state.phase()
    }

    fn cancelled_for(&self, ticket: u64) -> bool {
        self.cancel_ticket.load(Ordering::SeqCst) == ticket
    }

    fn closed(&self) -> bool {
        self.lock().closed
    }

    fn wake(&self) {
        wake_now(&self.wake);
    }

    /// A helper's event reader saw a protocol violation. Revoke a published endpoint at once,
    /// under the same state lock the owner's own `Stop` uses, so a failed instance can never be
    /// reached while its cleanup is still pending. Only the generation that admitted that reader
    /// and only `Starting`/`Running` are moved: a detached reader of an old, already-reaped helper
    /// must never stop a newer instance, and a terminal or foreign state is left untouched.
    fn revoke_running(&self, ticket: u64) {
        let mut transitioned = false;
        {
            let mut inner = match self.inner.try_lock() {
                Ok(inner) => inner,
                Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
                // A revocation is never dropped because the short lock was momentarily held.
                Err(TryLockError::WouldBlock) => self.lock(),
            };
            if inner.state.ticket() == ticket
                && matches!(inner.state.phase(), Phase::Starting | Phase::Running)
                && inner.state.begin(Action::Stop).unwrap_or(false)
            {
                transitioned = true;
                self.ready.notify_all();
            }
        }
        // Woken only for an actual transition, and outside the lock: a window callback may re-enter.
        if transitioned {
            self.wake();
        }
    }
}

impl Host {
    fn worker_body(&self) -> Box<dyn FnOnce() + Send + 'static> {
        let shared = Arc::clone(&self.shared);
        let launcher = Arc::clone(&self.launcher);
        let installer = Arc::clone(&self.installer);
        let readiness = Arc::clone(&self.readiness);
        Box::new(move || worker_entry(shared, launcher, installer, readiness))
    }
}

impl fmt::Debug for Host {
    /// Counts and phases only: no base path, endpoint, version, address or helper output.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = self.shared.lock();
        formatter
            .debug_struct("ProofHost")
            .field("phase", &inner.state.phase())
            .field("ticket", &inner.state.ticket())
            .field("worker", &inner.worker.is_some())
            .field("pending", &inner.pending.is_some())
            .finish()
    }
}

impl ManagedServer for Host {
    fn status(&self) -> Status {
        self.shared.snapshot()
    }

    fn request(&self, action: Action) -> Result<(), Issue> {
        let mut failure = None;
        {
            let mut inner = self.shared.lock();
            match inner.state.begin(action) {
                // A refusal changes nothing and admits nothing.
                Err(issue) => return Err(issue),
                // An idempotent flag/no-op also queues nothing.
                Ok(false) => return Ok(()),
                Ok(true) => {
                    let ticket = inner.state.ticket();
                    match action {
                        Action::Check | Action::Download | Action::Start => {
                            // A new generation starts uncancelled.
                            self.shared.cancel.store(false, Ordering::SeqCst);
                            self.shared.cancel_ticket.store(0, Ordering::SeqCst);
                            if inner.worker.is_none() {
                                match (self.spawn)(WORKER_NAME, self.worker_body()) {
                                    Ok(handle) => inner.worker = Some(handle),
                                    Err(_) => {
                                        // Truthful rollback: no active phase and nothing queued.
                                        let _ = inner.state.finished(ticket, Phase::Failed, Some(Issue::StartFailed));
                                        inner.pending = None;
                                        failure = Some(Issue::StartFailed);
                                    }
                                }
                            }
                            if failure.is_none() {
                                inner.pending = Some(Work { ticket, action });
                            }
                        }
                        Action::Cancel | Action::Stop => {
                            // The endpoint was already revoked by `begin` under this same lock.
                            self.shared.cancel.store(true, Ordering::SeqCst);
                            self.shared.cancel_ticket.store(ticket, Ordering::SeqCst);
                        }
                    }
                }
            }
        }
        self.shared.ready.notify_all();
        self.shared.wake();
        match failure {
            Some(issue) => Err(issue),
            None => Ok(()),
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let mut inner = self.shared.lock();
        inner.closed = true;
        // No worker was ever made: there is nothing to wake or wait for.
        if inner.worker.is_none() {
            return;
        }
        let ticket = inner.state.ticket();
        if matches!(
            inner.state.phase(),
            Phase::Checking | Phase::Downloading | Phase::Extracting | Phase::Cancelling | Phase::Starting | Phase::Running | Phase::Stopping
        ) {
            self.shared.cancel.store(true, Ordering::SeqCst);
            self.shared.cancel_ticket.store(ticket, Ordering::SeqCst);
        }
        self.shared.ready.notify_all();
        let started = Instant::now();
        while !inner.finished {
            let left = DROP_WAIT.saturating_sub(started.elapsed());
            if left.is_zero() {
                break;
            }
            let (guard, _) = self.shared.ready.wait_timeout(inner, left).unwrap_or_else(|poisoned| poisoned.into_inner());
            inner = guard;
        }
        if inner.finished
            && let Some(handle) = inner.worker.take()
        {
            let _ = handle.join();
        }
    }
}

const WORKER_NAME: &str = "mailcraft-proof-host";

fn worker_entry(shared: Arc<Shared>, launcher: Arc<dyn Launcher>, installer: Arc<dyn Installer>, readiness: Arc<dyn Readiness>) {
    let _ = catch_unwind(AssertUnwindSafe(|| worker_loop(&shared, launcher.as_ref(), installer.as_ref(), readiness.as_ref())));
    let mut inner = shared.lock();
    inner.finished = true;
    shared.ready.notify_all();
}

fn worker_loop(shared: &Arc<Shared>, launcher: &dyn Launcher, installer: &dyn Installer, readiness: &dyn Readiness) {
    loop {
        let work = {
            let mut inner = shared.lock();
            loop {
                if inner.closed {
                    return;
                }
                if let Some(work) = inner.pending.take() {
                    break work;
                }
                inner = shared.ready.wait(inner).unwrap_or_else(|poisoned| poisoned.into_inner());
            }
        };
        let ran = catch_unwind(AssertUnwindSafe(|| run_job(shared, launcher, installer, readiness, work)));
        if ran.is_err() {
            // An injected backend panicked. Any helper owned by that job was reaped by its `Drop`
            // before this line; the operation fails rather than the worker.
            finish(shared, work.ticket, Phase::Failed, Some(Issue::StartFailed));
        }
    }
}

fn run_job(shared: &Arc<Shared>, launcher: &dyn Launcher, installer: &dyn Installer, readiness: &dyn Readiness, work: Work) {
    match work.action {
        Action::Check => run_check(shared, launcher, installer, work.ticket),
        Action::Download => run_download(shared, launcher, installer, work.ticket),
        Action::Start => run_start(shared, launcher, readiness, work.ticket),
        Action::Cancel | Action::Stop => {}
    }
}

fn finish(shared: &Shared, ticket: u64, phase: Phase, issue: Option<Issue>) -> bool {
    let changed = {
        let mut inner = shared.lock();
        inner.state.finished(ticket, phase, issue)
    };
    if changed {
        shared.wake();
    }
    changed
}

fn record_checked(shared: &Shared, ticket: u64, installed: bool, java: Java, version: &str) {
    let changed = {
        let mut inner = shared.lock();
        inner.state.checked(ticket, installed, java, version)
    };
    if changed {
        shared.wake();
    }
}

// ---------------------------------------------------------------------------------------------
// Check: read-only install verification and short Java probe.
// ---------------------------------------------------------------------------------------------

fn run_check(shared: &Arc<Shared>, launcher: &dyn Launcher, installer: &dyn Installer, ticket: u64) {
    let base = shared.base.clone();
    let java_before = shared.snapshot().java;
    let located = catch_unwind(AssertUnwindSafe(|| installer.locate(&base)));
    let found = match located {
        Ok(Ok(found)) => found,
        Ok(Err(error)) => {
            if !matches!(error, InstallError::Integrity) && shared.cancelled_for(ticket) {
                // An unreadable installation is not an absence: end the operation through the
                // ordinary cancellation path, keeping every previously verified fact. Confirmed
                // corruption is different: cancellation cannot preserve an invalid installation.
                finish(shared, ticket, Phase::Cancelled, Some(Issue::Cancelled));
            } else {
                finish(shared, ticket, Phase::Failed, Some(issue_for_install(error)));
            }
            return;
        }
        Err(_) => {
            // A backend panic is a fixed failure; no helper was started and nothing leaked.
            finish(shared, ticket, Phase::Failed, Some(Issue::StartFailed));
            return;
        }
    };
    let installed = found.is_some();
    let version = found.as_ref().map(|found| found.version.as_str()).unwrap_or_default();
    // Locate is read-only and cannot be interrupted; a cancellation that arrived while it
    // ran still records a verified distribution truthfully, never a false absence.
    if shared.cancelled_for(ticket) {
        record_checked(shared, ticket, installed, java_before, version);
        return;
    }
    match probe(shared, launcher, ticket) {
        Ok(java) => record_checked(shared, ticket, installed, java, version),
        Err(ProbeFailure::Cancelled) => record_checked(shared, ticket, installed, java_before, version),
        Err(ProbeFailure::Issue(issue)) => {
            finish(shared, ticket, Phase::Failed, Some(issue));
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Download: pinned installer, progress, cancellation, no server.
// ---------------------------------------------------------------------------------------------

fn run_download(shared: &Arc<Shared>, launcher: &dyn Launcher, installer: &dyn Installer, ticket: u64) {
    let base = shared.base.clone();
    let result = catch_unwind(AssertUnwindSafe(|| installer.install(&base, &shared.cancel, &|progress| report_progress(shared, ticket, progress))));
    match result {
        Ok(Ok(found)) => {
            // A promotion that completed while cancellation was pending is described truthfully.
            // Its own probe never ran, so the Download saw no Java fact and must not reuse an
            // older one: `Unknown` until a fresh Check confirms it.
            if shared.cancelled_for(ticket) {
                record_checked(shared, ticket, true, Java::Unknown, &found.version);
                return;
            }
            // Probe Java only as part of this explicit Download. Any unconfirmed probe (a
            // cancellation or a structurally failed helper) leaves the new installation with no
            // Java fact of its own, so a final `Installed` cannot start until a fresh Check.
            let java = match probe(shared, launcher, ticket) {
                Ok(java) => java,
                Err(ProbeFailure::Cancelled) | Err(ProbeFailure::Issue(_)) => Java::Unknown,
            };
            record_checked(shared, ticket, true, java, &found.version);
        }
        Ok(Err(InstallError::Cancelled)) => {
            // The installer stopped before promoting anything. That is not an absence fact: keep
            // whatever was verified before and end through the ordinary cancellation path.
            finish(shared, ticket, Phase::Cancelled, Some(Issue::Cancelled));
        }
        Ok(Err(error)) => {
            // A confirmed integrity failure is a fact about the installation and must not be
            // relabelled as a cancellation that kept a stale verified install. Any other error
            // while cancelling says nothing new about what is on disk, so the cached facts stay.
            if matches!(error, InstallError::Integrity) || !shared.cancelled_for(ticket) {
                finish(shared, ticket, Phase::Failed, Some(issue_for_install(error)));
            } else {
                finish(shared, ticket, Phase::Cancelled, Some(Issue::Cancelled));
            }
        }
        Err(_) => {
            finish(shared, ticket, Phase::Failed, Some(Issue::StartFailed));
        }
    }
}

fn report_progress(shared: &Shared, ticket: u64, progress: Progress) {
    let (written, total, extracting) = match progress {
        Progress::Download { written, total } => (written, total, false),
        Progress::Extract { files, total } => (files, total, true),
    };
    let changed = {
        let mut inner = shared.lock();
        inner.state.progress(ticket, written, total, extracting)
    };
    if changed {
        shared.wake();
    }
}

// ---------------------------------------------------------------------------------------------
// Probe helper: exactly one fixed event, a clean natural EOF and a successful exit, then the
// process ends by itself. It is never killed: its own Java discovery must be allowed to finish.
// ---------------------------------------------------------------------------------------------

enum ProbeFailure {
    Cancelled,
    Issue(Issue),
}

/// What the reader saw after the single Probe event it may deliver.
enum StreamEnd {
    /// Natural end of file with no further frame.
    Clean,
    /// A second frame, a malformed/partial/oversized frame, a read error or a dropped reader.
    Violation,
}

fn probe(shared: &Shared, launcher: &dyn Launcher, ticket: u64) -> Result<Java, ProbeFailure> {
    if shared.cancelled_for(ticket) || shared.closed() {
        return Err(ProbeFailure::Cancelled);
    }
    let command = launcher.command(&Mode::Probe).map_err(|_| ProbeFailure::Issue(Issue::StartFailed))?;
    // The launcher may have taken time; a cancellation or shutdown known now must not spawn a
    // helper whose only job is to discover Java for an operation the owner has left.
    if shared.cancelled_for(ticket) || shared.closed() {
        return Err(ProbeFailure::Cancelled);
    }
    let mut helper = Helper::spawn(command, None).map_err(|_| ProbeFailure::Issue(Issue::StartFailed))?;
    let deadline = Instant::now() + PROBE_BUDGET;
    let mut seen: Option<Java> = None;
    let mut end = None;
    while end.is_none() {
        if shared.cancelled_for(ticket) {
            return Err(ProbeFailure::Cancelled);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(ProbeFailure::Issue(Issue::StartTimedOut));
        }
        match helper.events.recv_timeout(left.min(STARTUP_POLL)) {
            Ok(Message::Event(Event::Probe(java))) => {
                if seen.replace(java).is_some() {
                    return Err(ProbeFailure::Issue(Issue::StartFailed));
                }
            }
            // The stream ended. Whatever the child exited with decides afterwards; a `Closed`
            // frame here is a natural EOF, anything else is a violation of the fixed protocol.
            Ok(Message::Closed) => end = Some(StreamEnd::Clean),
            Ok(Message::Event(_)) | Ok(Message::Error) | Err(RecvTimeoutError::Disconnected) => end = Some(StreamEnd::Violation),
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
    // Closing the private lifetime pipe is the only shutdown a helper is ever given. The child is
    // then reaped for real, and only its own exit status may complete the Check.
    helper.close_stdin();
    helper.reap();
    let code = helper.exit_code;
    match (end, seen, code) {
        (Some(StreamEnd::Clean), Some(java), Some(Some(0))) => Ok(java),
        (_, _, _) => Err(ProbeFailure::Issue(Issue::StartFailed)),
    }
}

// ---------------------------------------------------------------------------------------------
// Start: one Serve helper, BOUND + liveness + readiness, then the running supervisor.
// ---------------------------------------------------------------------------------------------

enum BoundFailure {
    Cancelled,
    /// The helper said no (`FAILED`) or ended cleanly without binding. The owned exit status is
    /// the only truthful reason, so cleanup reads it before the terminal issue is chosen.
    Refused,
    Issue(Issue),
}

fn run_start(shared: &Arc<Shared>, launcher: &dyn Launcher, readiness: &dyn Readiness, ticket: u64) {
    #[cfg(test)]
    if let Some(gate) = &shared.launch_gate {
        gate.park();
    }
    // A start the owner has already cancelled must never reach a launcher or a helper: there is
    // nothing to own, so it finishes through the ordinary terminal path and keeps its facts.
    if shared.cancelled_for(ticket) || shared.closed() {
        finish_cancel_without_helper(shared, ticket);
        return;
    }
    let deadline = Instant::now() + START_BUDGET;
    let mode = Mode::Serve { base: shared.base.clone() };
    let command = match launcher.command(&mode) {
        Ok(command) => command,
        Err(_) => {
            finish(shared, ticket, Phase::Failed, Some(Issue::StartFailed));
            return;
        }
    };
    // The launcher may have taken time; a cancellation that arrived meanwhile is still honoured
    // before any process exists.
    if shared.cancelled_for(ticket) || shared.closed() {
        finish_cancel_without_helper(shared, ticket);
        return;
    }
    let revoke = Some(Revoke { shared: Arc::downgrade(shared), ticket });
    let mut helper = match Helper::spawn(command, revoke) {
        Ok(helper) => helper,
        Err(_) => {
            finish(shared, ticket, Phase::Failed, Some(Issue::StartFailed));
            return;
        }
    };
    let port = match wait_bound(shared, &mut helper, ticket, deadline) {
        Ok(port) => port,
        Err(BoundFailure::Cancelled) => {
            finalize_cancel(shared, &mut helper, ticket);
            return;
        }
        Err(BoundFailure::Refused) => {
            finalize_failure(shared, &mut helper, ticket, Issue::StartFailed);
            return;
        }
        Err(BoundFailure::Issue(issue)) => {
            finalize_failure(shared, &mut helper, ticket, issue);
            return;
        }
    };
    // A BOUND from a child that is already gone proves nothing about ownership.
    if !helper.alive() {
        finalize_failure(shared, &mut helper, ticket, Issue::StartFailed);
        return;
    }
    let endpoint = format!("http://127.0.0.1:{port}");
    let outcome = catch_unwind(AssertUnwindSafe(|| readiness.warm(&endpoint, deadline, &|| shared.cancelled_for(ticket))));
    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(Issue::Cancelled)) => {
            finalize_cancel(shared, &mut helper, ticket);
            return;
        }
        Ok(Err(issue)) => {
            finalize_failure(shared, &mut helper, ticket, issue);
            return;
        }
        Err(_) => {
            finalize_failure(shared, &mut helper, ticket, Issue::StartFailed);
            return;
        }
    }
    // Liveness is re-established after the exchange: a readiness answer alone is not owned-process
    // evidence, and a cancellation/stop during warm-up may not publish late.
    if shared.cancelled_for(ticket) {
        finalize_cancel(shared, &mut helper, ticket);
        return;
    }
    if !helper.alive() {
        finalize_failure(shared, &mut helper, ticket, Issue::StartFailed);
        return;
    }
    let published = {
        let mut inner = shared.lock();
        inner.state.publish(ticket, port)
    };
    if !published {
        // Owner intent decides the description. A `Stopping` phase can also have been produced by
        // a helper's own protocol violation, which is a failure, not the owner's stop.
        if shared.cancelled_for(ticket) || shared.closed() {
            finalize_cancel(shared, &mut helper, ticket);
        } else {
            finalize_failure(shared, &mut helper, ticket, Issue::StartFailed);
        }
        return;
    }
    shared.wake();
    supervise_running(shared, &mut helper, ticket);
}

fn wait_bound(shared: &Shared, helper: &mut Helper, ticket: u64, deadline: Instant) -> Result<u16, BoundFailure> {
    loop {
        if shared.cancelled_for(ticket) {
            return Err(BoundFailure::Cancelled);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(BoundFailure::Issue(Issue::StartTimedOut));
        }
        match helper.events.recv_timeout(left.min(STARTUP_POLL)) {
            Ok(Message::Event(Event::Bound(port))) => return Ok(port),
            // A refusal is not the reason. The caller closes the lifetime pipe and reaps first;
            // the owned exit status then names the reason truthfully.
            Ok(Message::Event(Event::Failed)) => return Err(BoundFailure::Refused),
            Ok(Message::Event(_)) | Ok(Message::Error) | Err(RecvTimeoutError::Disconnected) => {
                return Err(BoundFailure::Issue(Issue::StartFailed));
            }
            Ok(Message::Closed) => return Err(BoundFailure::Refused),
            Err(RecvTimeoutError::Timeout) => {}
        }
        if let Some(code) = helper.exited() {
            return Err(BoundFailure::Issue(proof_protocol::issue_for_exit(code)));
        }
    }
}

fn supervise_running(shared: &Shared, helper: &mut Helper, ticket: u64) {
    loop {
        if shared.cancelled_for(ticket) || shared.closed() {
            finalize_cancel(shared, helper, ticket);
            return;
        }
        // One unexpected frame ends the running instance; a closed stdout is checked against the
        // child itself below. Only the bounded first frame is inspected here.
        match helper.events.try_recv() {
            Ok(Message::Event(Event::Bound(_)))
            | Ok(Message::Event(Event::Probe(_)))
            | Ok(Message::Event(Event::Failed))
            | Ok(Message::Event(Event::Stopped))
            | Ok(Message::Error) => {
                finalize_failure(shared, helper, ticket, Issue::StartFailed);
                return;
            }
            Ok(Message::Closed) | Err(TryRecvError::Disconnected) | Err(TryRecvError::Empty) => {}
        }
        if let Some(code) = helper.exited() {
            helper.close_stdin();
            helper.reap();
            finish(shared, ticket, Phase::Failed, Some(proof_protocol::issue_for_exit(code)));
            return;
        }
        // Liveness that could not be verified is not ownership: revoke the endpoint at once and stay
        // pending until the owned child's own exit status is observed. Nothing is ever killed here.
        if helper.liveness_unverified() {
            shared.revoke_running(ticket);
        }
        let inner = shared.lock();
        let _ = shared.ready.wait_timeout(inner, RUN_POLL);
    }
}

fn finalize_cancel(shared: &Shared, helper: &mut Helper, ticket: u64) {
    let phase = shared.phase();
    helper.close_stdin();
    helper.reap();
    let (terminal, issue) = if phase == Phase::Stopping { (Phase::Stopped, None) } else { (Phase::Cancelled, Some(Issue::Cancelled)) };
    finish(shared, ticket, terminal, issue);
}

/// Finish a cancellation that was known before any helper existed. There is no pipe to close and no
/// child to reap, so the ordinary terminal phases apply directly; `State::finished` keeps a
/// verified installation (and its version) when the cancelled operation was a `Start`.
fn finish_cancel_without_helper(shared: &Shared, ticket: u64) {
    let (terminal, issue) = if shared.phase() == Phase::Stopping { (Phase::Stopped, None) } else { (Phase::Cancelled, Some(Issue::Cancelled)) };
    finish(shared, ticket, terminal, issue);
}

/// Revoke first, under the state lock, so no endpoint outlives the failure; then close the
/// lifetime pipe and wait for the owned helper to be reaped; only then claim the terminal phase.
/// A `FAILED`/clean-exit start failure takes its reason from the owned exit status.
fn finalize_failure(shared: &Shared, helper: &mut Helper, ticket: u64, issue: Issue) {
    // Revoke (or move an owned Starting/Running instance to Stopping) before any wait. `begin`
    // is the one transition that takes the endpoint away under the same short lock an owner's
    // Stop uses; a terminal or foreign generation is left untouched.
    let pending = {
        let mut inner = shared.lock();
        let before = inner.state.phase();
        let moved = inner.state.begin(Action::Stop);
        (before, moved, inner.state.phase())
    };
    helper.close_stdin();
    let code = helper.reap();
    let (issue, terminal) = match pending.2 {
        Phase::Cancelling => (Issue::Cancelled, Phase::Cancelled),
        // A refresh that arrived after the failure is cancelled truthfully.
        _ if shared.cancelled_for(ticket) => (Issue::Cancelled, Phase::Cancelled),
        _ => {
            let issue = match (issue, code) {
                (Issue::StartFailed, Some(Some(exit))) if exit != 0 => proof_protocol::issue_for_exit(Some(exit)),
                (issue, _) => issue,
            };
            (issue, Phase::Failed)
        }
    };
    finish(shared, ticket, terminal, Some(issue));
}

// ---------------------------------------------------------------------------------------------
// The owned helper process and its one bounded reader.
// ---------------------------------------------------------------------------------------------

/// What one `try_wait` observation proved about the owned child. An I/O error proves nothing: the
/// child is still ours, its exit is unknown, and cleanup must wait for a positive observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Observation {
    Alive,
    Exited(Option<i32>),
    Unverified,
}

fn observed_wait_result(result: io::Result<Option<std::process::ExitStatus>>) -> Observation {
    match result {
        Ok(None) => Observation::Alive,
        Ok(Some(status)) => Observation::Exited(status.code()),
        // Not an exit: an I/O error never becomes evidence that the helper is gone.
        Err(_) => Observation::Unverified,
    }
}

/// The only message an event reader may deliver. A read error carries no error body: every
/// failure the host reports is a fixed [`Issue`].
enum Message {
    Event(Event),
    Error,
    Closed,
}

/// One helper's event reader and the one operation that admitted it. A protocol violation in this
/// stream may revoke only that generation: a reader detached from an old helper outlives its own
/// process and must never move a newer owned instance.
pub(crate) struct Revoke {
    shared: Weak<Shared>,
    ticket: u64,
}

impl Revoke {
    fn revoke(&self) {
        if let Some(shared) = self.shared.upgrade() {
            shared.revoke_running(self.ticket);
        }
    }
}

/// The owned helper process and its admitted event reader. Every path that ends an operation has
/// reaped the child first; `Drop` is the backstop for a worker panic and is not a licence to
/// report cleanup that has not happened.
pub struct Helper {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    events: Receiver<Message>,
    reader: Option<JoinHandle<()>>,
    /// The reaped exit status, cached because `try_wait` only reports it on its first success.
    exit_code: Option<Option<i32>>,
    /// The last poll saw an I/O error: liveness is unknown, so no endpoint may stay published and
    /// no exit may be claimed until a positive observation replaces it.
    unverified: bool,
    /// One-shot, test-only: the next poll reports an I/O error instead of asking the OS.
    #[cfg(test)]
    injected_wait_error: bool,
}

impl Helper {
    /// Spawn the helper only after admitting one event reader from the process-wide bounded pool.
    /// `revoke` lets a `Serve` reader revoke its own generation's published endpoint the moment it
    /// sees a protocol violation, before the worker's own poll notices.
    pub(crate) fn spawn(mut command: Command, revoke: Option<Revoke>) -> io::Result<Helper> {
        let slot = acquire_event_slot().ok_or_else(|| io::Error::other("no event-reader capacity"))?;
        command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // No console window for a helper that is never seen. Compile-time guarded; unexercised here.
            let _ = command.creation_flags(0x0800_0000);
        }
        let mut child = command.spawn()?;
        let stdin = child.stdin.take();
        let Some(stdout) = child.stdout.take() else {
            // We cannot supervise a helper whose stdout was not a pipe. Close its lifetime pipe and
            // reap it here: an operation is never left holding an unreaped child.
            drop(stdin);
            reap_child_now(child);
            drop(slot);
            return Err(io::Error::other("helper stdout was not a pipe"));
        };
        let (sender, events) = mpsc::sync_channel(EVENT_CHANNEL);
        let name = "mailcraft-proof-events".to_string();
        let reader = match thread::Builder::new().name(name).spawn(move || {
            let _slot = slot;
            read_events(stdout, sender, revoke.as_ref());
            // The permit leaves with this reader: a detached reader keeps its own slot, never more.
        }) {
            Ok(handle) => Some(handle),
            Err(error) => {
                drop(stdin);
                reap_child_now(child);
                return Err(error);
            }
        };
        Ok(Helper {
            child: Some(child),
            stdin,
            events,
            reader,
            exit_code: None,
            unverified: false,
            #[cfg(test)]
            injected_wait_error: false,
        })
    }

    fn close_stdin(&mut self) {
        let _ = self.stdin.take();
    }

    /// Whether the helper may be treated as alive: only a positive `Alive` observation proves it.
    /// An unverified poll is not proof of life, and no more proof that it is gone.
    pub(crate) fn alive(&mut self) -> bool {
        matches!(self.observe(), Observation::Alive)
    }

    /// Whether the last poll could not tell life from death. The caller revokes the endpoint and
    /// keeps the child owned rather than inventing an exit.
    fn liveness_unverified(&self) -> bool {
        self.unverified
    }

    /// One `try_wait` observation of the owned child, caching only an exit the OS actually reported.
    fn observe(&mut self) -> Observation {
        if self.child.is_none() {
            return Observation::Exited(self.exit_code.unwrap_or(None));
        }
        let observation = match self.child.as_mut() {
            Some(child) => {
                let result = child.try_wait();
                #[cfg(test)]
                let result = if std::mem::take(&mut self.injected_wait_error) { Err(io::Error::other("injected wait error")) } else { result };
                observed_wait_result(result)
            }
            None => Observation::Exited(self.exit_code.unwrap_or(None)),
        };
        self.unverified = observation == Observation::Unverified;
        if let Observation::Exited(code) = observation {
            self.exit_code = Some(code);
            self.child = None;
        }
        observation
    }

    /// Poll for the owned child's end and cache its exit status. `None` means it is still alive or
    /// its liveness could not be verified; the child remains owned in both cases.
    fn reap_once(&mut self) -> Option<Option<i32>> {
        match self.observe() {
            Observation::Exited(code) => Some(code),
            Observation::Alive | Observation::Unverified => None,
        }
    }

    /// The exit status once the owned child has ended. `try_wait` reaps it on the first success.
    pub(crate) fn exited(&mut self) -> Option<Option<i32>> {
        self.reap_once()
    }

    /// Whether this helper still owns its child process. Test-only: the production paths never need
    /// to ask, they simply keep waiting.
    #[cfg(test)]
    pub(crate) fn owns_child(&self) -> bool {
        self.child.is_some()
    }

    /// Make exactly the next poll report an I/O error, as a real `try_wait` failure would. Test-only
    /// and one-shot: no process is signalled or otherwise disturbed to provoke it.
    #[cfg(test)]
    pub(crate) fn inject_wait_error_once(&mut self) {
        self.injected_wait_error = true;
    }

    /// Close the private lifetime pipe and wait, without ever killing the helper, for the owned
    /// process to be reaped. Returns its cached exit status. This is the only cleanup a helper is
    /// given, and it is idempotent.
    pub(crate) fn reap(&mut self) -> Option<Option<i32>> {
        self.close_stdin();
        while self.reap_once().is_none() {
            // Keep the bounded channel drained so the reader never blocks on a full queue.
            while self.events.try_recv().is_ok() {}
            thread::sleep(REAP_POLL);
        }
        // A reader may still be blocked on a full channel or on a descendant's copy of the write
        // end; drain, then join only the reader that has already finished and detach the rest.
        while self.events.try_recv().is_ok() {}
        self.join_finished_reader();
        self.exit_code
    }

    fn join_finished_reader(&mut self) {
        if self.reader.as_ref().is_some_and(JoinHandle::is_finished)
            && let Some(handle) = self.reader.take()
        {
            let _ = handle.join();
        }
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        // Closing the private lifetime pipe is the only shutdown a helper is ever given here. A
        // helper still owned during a worker panic must still be reaped before the operation is
        // reported failed: every normal end path reaps explicitly first, so this backstop waits
        // for the pipe to do its work rather than detaching an orphan or pretending it is gone.
        self.reap();
        // A reader blocked on a descendant's pipe is detached, never joined.
        let _ = self.reader.take();
    }
}

/// Close the child's lifetime pipe and wait for it to be reaped, on the caller's thread. Used
/// only on the rare error paths where a helper exists but no supervisor will ever own it.
fn reap_child_now(mut child: Child) {
    let _ = child.stdin.take();
    // An I/O error is not an exit: wait for the positive observation instead of detaching a child
    // whose status was never read.
    while !matches!(child.try_wait(), Ok(Some(_))) {
        thread::sleep(REAP_POLL);
    }
}

/// Read at most `MAX_HELPER_EVENTS` whole frames. The channel is bounded; a worker that stops
/// reading makes `send` fail and this thread ends. No second reader is ever made for one helper.
fn read_events(mut stdout: std::process::ChildStdout, sender: SyncSender<Message>, revoke: Option<&Revoke>) {
    for _ in 0..MAX_HELPER_EVENTS {
        match proof_protocol::read_event(&mut stdout) {
            Ok(Some(event)) => {
                if sender.send(Message::Event(event)).is_err() {
                    return;
                }
            }
            Ok(None) => {
                let _ = sender.send(Message::Closed);
                return;
            }
            Err(_) => {
                if let Some(revoke) = revoke {
                    revoke.revoke();
                }
                let _ = sender.send(Message::Error);
                return;
            }
        }
    }
    // More frames than the fixed protocol allows: refuse the whole helper, never a prefix.
    if let Some(revoke) = revoke {
        revoke.revoke();
    }
    let _ = sender.send(Message::Error);
}

fn wake_now(wake: &Wake) {
    // The window's callback is not this module's to trust; a panic in it must not kill a worker.
    let _ = catch_unwind(AssertUnwindSafe(|| wake()));
}
