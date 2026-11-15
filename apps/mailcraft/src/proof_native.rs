// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! The native host's bounded proofreading service (#954): the window's
//! [`ProofService`](mailcraft_ui_egui::proof_service::ProofService) contract, backed by one
//! long-lived spelling worker and one long-lived grammar worker.
//!
//! Nothing here is called on the window's thread for longer than it takes to put a request in a
//! one-deep slot and wake a worker. Dictionaries are read on the spelling worker alone, which is
//! also where a word's suggestions come from: a suggestion for the settings a scan just used
//! reuses the dictionary that scan read, a changed language set or folder is read once more on
//! that worker, and a frame never opens a file.
//!
//! Contract points implemented here:
//! - **One worker each.** Two threads are made when the service is made, and every request is run
//!   on one of them: no thread per request, and no unbounded queue of requests between the window
//!   and a worker (each reply travels on its own one-message channel). A request owns its
//!   snapshot; the caller's text is moved in, never copied for a queue.
//! - **One active and one newest pending per kind.** A spelling scan, a suggestion and a grammar
//!   check each have a slot that holds at most one waiting request; a newer request replaces the
//!   one waiting, which is answered [`Failure::Cancelled`] — never a clean empty success. The
//!   spelling scans and the suggestions share one worker (and one dictionary cache) but keep
//!   separate slots, and a waiting suggestion is run before a waiting scan, so a burst of scans
//!   can neither drop the suggestion the owner just asked for nor push it back behind every
//!   keystroke.
//! - **Cancellation is not a clean answer.** A request's [`Cancellation`] flag is read before any
//!   work begins, between the tokens of a fast scan (`Speller::scan_cancellable`), and once more
//!   before the reply is sent, whatever that reply would have been; a called-off request is
//!   answered [`Failure::Cancelled`], never a failure the host happened to return after the caller
//!   moved on. Since a grammar request is one bounded HTTP exchange that cannot be interrupted,
//!   cancellation of an in-flight check is noticed after it returns, and the answer is then
//!   dropped rather than delivered as a complete one. The wait for a grammar *reply* is therefore
//!   at most two full checks — the one on the wire and the caller's own — each bounded by the
//!   eight-second patience, and it is spent on the grammar worker, never on the window's thread.
//!   The adapter itself permits a batch of up to 32 sentence requests and 300 seconds
//!   (`languagetool::MAX_BATCH`); this service asks it for [`languagetool::PATIENCE`], which is
//!   the deadline for the whole batch, so the batch stops asking at eight seconds however many
//!   sentences it holds; a sentence already on the wire may take the HTTP layer's upload
//!   allowance (at most a second for a 16 KB sentence) to finish. A *pending* grammar request
//!   that is cancelled is answered before any request is made.
//! - **A missing dictionary is said, not hidden.** The spelling reply's `note` carries the
//!   engine's own words about a dictionary that is absent or stood in for by another language,
//!   and the service adds the caps it applied: only the first [`MAX_CHECKED`] characters are read
//!   and only the first [`MAX_DIAGNOSTICS`] places are kept. A reply with no dictionary has no
//!   diagnostics (nothing to check against) and says so.
//! - **Static failures only.** Every error is a [`Failure`]; no server body, draft, word, address,
//!   path or operating-system error is carried or logged. The service logs nothing at all —
//!   there is no logger in this file and a worker's error text never leaves it.
//! - **Grammar is the real multilingual adapter.** [`languagetool::check_multilingual`] is called
//!   with every enabled language and the owner's ignored rules, and its typed diagnostics and
//!   partial/checked/skipped counters are passed through unchanged. There is no single-language
//!   check and no `parse_answer` here.
//!
//! Lifetime: dropping the service closes both queues, wakes both workers and gives them
//! [`STOP_WAIT`] to answer every request still waiting [`Failure::Unavailable`] and end. A worker
//! still inside a dictionary pass or an exchange that long is not waited for: its handle is
//! dropped, so it finishes the call it is in, answers what is waiting and ends by itself. A
//! window's drop therefore never blocks on a scan or on the wire. A caller that drops its receiver
//! is not an error: the reply is sent and forgotten.
//!
//! The window's side needs two lines in the crate root, as every other host service in this
//! program has:
//! ```text
//! mod proof_native;
//! #[cfg(test)]
//! mod proof_native_tests;
//! ```
//! (the test module may be added only when the tests are run; see `proof_native_tests.rs`).

use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use mailcraft_engine::proof::Proofing;
use mailcraft_model::proof::{Diagnostic, Language, MAX_CHECKED, MAX_DIAGNOSTICS, MAX_SUGGESTIONS, MAX_WORD, ProofSettings};
use mailcraft_net::languagetool::{self, LanguageToolError};
use mailcraft_ui_egui::proof_service::{
    Cancellation, Failure, GrammarReply, GrammarRequest, ProofService, SpellingReply, SpellingRequest, SuggestionReply, SuggestionRequest,
};

/// Most grammar rule ids taken from one request. The adapter caps its own list again; this bound
/// is here so a hostile settings file cannot make the snapshot itself large.
const MAX_RULES: usize = 256;

/// Most bytes of one rule id taken from a request; longer entries are not a rule's id.
const MAX_RULE_BYTES: usize = 128;

/// Most bytes of a grammar address taken from a request; the adapter refuses a longer one too.
const MAX_URL_BYTES: usize = 2_048;

/// What the note says when only part of a long text was looked at.
const NOTE_TRUNCATED: &str = "Only the first 100,000 characters were checked.";

/// What the note says when the diagnostics cap cut the list short.
const NOTE_DIAGNOSTIC_CAP: &str = "Only the first 4,000 places are shown.";

/// What the note says when there was no dictionary and the engine gave no reason of its own.
const NOTE_NO_DICTIONARY: &str = "No dictionary was found: spelling is not checked.";

/// How long a dropped worker is waited for. A window closes or replaces the service on its own
/// thread, and must never wait for a dictionary pass or an exchange there: the worker is told to
/// stop and given this long to answer what is waiting and end, and is then left to finish the call
/// it is in by itself.
const STOP_WAIT: Duration = Duration::from_millis(100);

/// Called, on a worker thread, whenever a reply is ready for the window.
type Wake = Arc<dyn Fn() + Send + Sync>;

/// Whether a worker's thread has run its loop out, so that a drop can reap it when it is done and
/// leave it running when it is not.
#[derive(Default)]
struct Finished {
    done: Mutex<bool>,
    ready: Condvar,
}

impl Finished {
    fn mark(&self) {
        *lock_guard(&self.done) = true;
        self.ready.notify_all();
    }

    /// `true` when the thread ended within `limit`; a spurious wake-up is waited out again.
    fn wait(&self, limit: Duration) -> bool {
        let started = Instant::now();
        let mut done = lock_guard(&self.done);
        while !*done {
            let left = limit.saturating_sub(started.elapsed());
            if left.is_zero() {
                return false;
            }
            let (guard, _) = self.ready.wait_timeout(done, left).unwrap_or_else(|poisoned| poisoned.into_inner());
            done = guard;
        }
        true
    }
}

/// How a worker thread is made. A function pointer so that the tests can make it fail.
type Spawn = fn(&str, Box<dyn FnOnce() + Send + 'static>) -> std::io::Result<JoinHandle<()>>;

/// Starts the native service the window installs.
///
/// `wake` is called from a worker thread each time a reply is ready — it is the window's own
/// request to be looked at again (`egui::Context::request_repaint`), so this module needs no
/// window type. Every method of [`ProofService`] returns at once with the channel the reply will
/// arrive on; no file, socket or dictionary is touched on the caller's thread.
pub fn service(wake: Wake) -> Arc<dyn ProofService> {
    Service::start(Config::native(wake))
}

/// What the spelling worker runs. Two methods so that scans and suggestions share one worker (and
/// one dictionary cache) while keeping their own one-deep pending slots.
pub(crate) trait SpellingHost: Send + Sync + 'static {
    /// The spelling places of `text`, with no suggestion kept in any of them. `cancelled` is asked
    /// between tokens; a host that cannot honour it must still answer only after the service's own
    /// pre- and post-checks.
    fn scan(&self, settings: &ProofSettings, text: &str, cancelled: &dyn Fn() -> bool) -> SpellingReply;

    /// What could stand in for `word`, at most `limit` of them.
    fn suggest(&self, settings: &ProofSettings, word: &str, limit: usize) -> SuggestionReply;
}

/// What the grammar worker runs: one bounded multilingual exchange.
pub(crate) trait GrammarHost: Send + Sync + 'static {
    fn check(&self, url: &str, text: &str, languages: &[Language], ignored: &[String], patience: Duration) -> GrammarReply;
}

/// How the service is built. Production uses [`Config::native`]; the tests replace either host and
/// the thread spawner to make coalescing, cancellation and failure deterministic.
pub(crate) struct Config {
    wake: Wake,
    spelling: Arc<dyn SpellingHost>,
    grammar: Arc<dyn GrammarHost>,
    grammar_patience: Duration,
    spawn: Spawn,
}

impl Config {
    fn native(wake: Wake) -> Config {
        Config {
            wake,
            spelling: Arc::new(LocalSpelling::new()),
            grammar: Arc::new(LocalGrammar),
            // The adapter's own patience: eight seconds for a whole batch, not for each sentence.
            grammar_patience: languagetool::PATIENCE,
            spawn: spawn_thread,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_spelling(mut self, host: Arc<dyn SpellingHost>) -> Config {
        self.spelling = host;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_grammar(mut self, host: Arc<dyn GrammarHost>) -> Config {
        self.grammar = host;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_patience(mut self, patience: Duration) -> Config {
        self.grammar_patience = patience;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_spawn(mut self, spawn: Spawn) -> Config {
        self.spawn = spawn;
        self
    }

    /// The tests' entry: the production hosts, to be replaced by the synthetic ones, and the
    /// test's own wake. Private to the module in a build of the program.
    #[cfg(test)]
    pub(crate) fn testing(wake: Arc<dyn Fn() + Send + Sync>) -> Config {
        Config::native(wake)
    }
}

/// The service itself: two workers, or the reason one is missing.
///
/// A worker that could not be started leaves its own methods answering [`Failure::Unavailable`];
/// the other worker keeps working, because a thread that could not be made is not a reason to stop
/// checking the other kind.
pub(crate) struct Service {
    spelling: Option<SpellingWorker>,
    grammar: Option<GrammarWorker>,
    wake: Wake,
}

impl Service {
    pub(crate) fn start(config: Config) -> Arc<Service> {
        let spelling = SpellingWorker::spawn(Arc::clone(&config.spelling), Arc::clone(&config.wake), config.spawn);
        let grammar = GrammarWorker::spawn(Arc::clone(&config.grammar), config.grammar_patience, Arc::clone(&config.wake), config.spawn);
        Arc::new(Service { spelling, grammar, wake: config.wake })
    }

    /// Calls the window's wake, and lets a wake that panics kill neither the worker nor a request.
    fn wake(&self) {
        wake_now(&self.wake);
    }
}

impl fmt::Debug for Service {
    /// Counts and liveness only: no draft, word, suggestion, address, path or dictionary name is in
    /// the service, and none can appear in a log line that formats it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProofNativeService")
            .field("spelling_worker", &self.spelling.is_some())
            .field("grammar_worker", &self.grammar.is_some())
            .finish()
    }
}

impl ProofService for Service {
    fn spelling(&self, request: SpellingRequest) -> Receiver<SpellingReply> {
        let (tx, rx) = mpsc::channel();
        let job = ScanJob { request, reply: tx };
        match self.spelling.as_ref() {
            Some(worker) => match worker.push_scan(job) {
                Ok(None) => {}
                Ok(Some(replaced)) => {
                    // The replaced request is not cleanly empty: it was called off.
                    let _ = replaced.reply.send(cancelled_spelling());
                    self.wake();
                }
                Err(job) => {
                    let _ = job.reply.send(unavailable_spelling());
                    self.wake();
                }
            },
            None => {
                let _ = job.reply.send(unavailable_spelling());
                self.wake();
            }
        }
        rx
    }

    fn suggest(&self, request: SuggestionRequest) -> Receiver<SuggestionReply> {
        let (tx, rx) = mpsc::channel();
        let job = SuggestJob { request, reply: tx };
        match self.spelling.as_ref() {
            Some(worker) => match worker.push_suggest(job) {
                Ok(None) => {}
                Ok(Some(replaced)) => {
                    let _ = replaced.reply.send(cancelled_suggestion());
                    self.wake();
                }
                Err(job) => {
                    let _ = job.reply.send(unavailable_suggestion());
                    self.wake();
                }
            },
            None => {
                let _ = job.reply.send(unavailable_suggestion());
                self.wake();
            }
        }
        rx
    }

    fn grammar(&self, request: GrammarRequest) -> Receiver<GrammarReply> {
        let (tx, rx) = mpsc::channel();
        let job = GrammarJob { request, reply: tx };
        match self.grammar.as_ref() {
            Some(worker) => match worker.push(job) {
                Ok(None) => {}
                Ok(Some(replaced)) => {
                    let _ = replaced.reply.send(cancelled_grammar());
                    self.wake();
                }
                Err(job) => {
                    let _ = job.reply.send(unavailable_grammar());
                    self.wake();
                }
            },
            None => {
                let _ = job.reply.send(unavailable_grammar());
                self.wake();
            }
        }
        rx
    }
}

// ---------------------------------------------------------------------------------------------
// Spelling worker: scans and suggestions, one dictionary cache, one active and one pending each.
// ---------------------------------------------------------------------------------------------

struct ScanJob {
    request: SpellingRequest,
    reply: Sender<SpellingReply>,
}

struct SuggestJob {
    request: SuggestionRequest,
    reply: Sender<SuggestionReply>,
}

enum SpellingJob {
    Scan(ScanJob),
    Suggest(SuggestJob),
}

#[derive(Default)]
struct SpellingQueue {
    /// The newest scan waiting. A scan is never in here while it is running.
    scan: Option<ScanJob>,
    /// The newest suggestion waiting, which a scan does not replace.
    suggest: Option<SuggestJob>,
    closed: bool,
}

type SpellingShared = (Mutex<SpellingQueue>, Condvar);

struct SpellingWorker {
    queue: Arc<SpellingShared>,
    finished: Arc<Finished>,
    handle: Option<JoinHandle<()>>,
}

impl SpellingWorker {
    fn spawn(host: Arc<dyn SpellingHost>, wake: Wake, spawn: Spawn) -> Option<SpellingWorker> {
        let queue = Arc::new((Mutex::new(SpellingQueue::default()), Condvar::new()));
        let finished = Arc::new(Finished::default());
        let body: Box<dyn FnOnce() + Send> = {
            let queue = Arc::clone(&queue);
            let finished = Arc::clone(&finished);
            Box::new(move || {
                spelling_loop(host, queue, wake);
                finished.mark();
            })
        };
        let handle = spawn("mailcraft-proof-spelling", body).ok()?;
        Some(SpellingWorker { queue, finished, handle: Some(handle) })
    }

    /// The newest waiting scan, when a request was already waiting. A request that arrives after
    /// the queue closed comes back boxed, so that a failure path does not make every caller's
    /// return value as large as a request.
    fn push_scan(&self, job: ScanJob) -> Result<Option<ScanJob>, Box<ScanJob>> {
        let (lock, ready) = &*self.queue;
        let mut queue = lock_guard(lock);
        if queue.closed {
            drop(queue);
            return Err(Box::new(job));
        }
        let replaced = queue.scan.replace(job);
        drop(queue);
        ready.notify_one();
        Ok(replaced)
    }

    fn push_suggest(&self, job: SuggestJob) -> Result<Option<SuggestJob>, Box<SuggestJob>> {
        let (lock, ready) = &*self.queue;
        let mut queue = lock_guard(lock);
        if queue.closed {
            drop(queue);
            return Err(Box::new(job));
        }
        let replaced = queue.suggest.replace(job);
        drop(queue);
        ready.notify_one();
        Ok(replaced)
    }
}

impl Drop for SpellingWorker {
    fn drop(&mut self) {
        {
            let (lock, ready) = &*self.queue;
            lock_guard(lock).closed = true;
            ready.notify_all();
        }
        reap(&self.finished, &mut self.handle);
    }
}

fn spelling_loop(host: Arc<dyn SpellingHost>, queue: Arc<SpellingShared>, wake: Wake) {
    while let Some(job) = next_spelling(&queue) {
        match job {
            SpellingJob::Scan(job) => {
                let reply = scan_on(host.as_ref(), &job.request);
                let _ = job.reply.send(reply);
            }
            SpellingJob::Suggest(job) => {
                let reply = suggest_on(host.as_ref(), &job.request);
                let _ = job.reply.send(reply);
            }
        }
        wake_now(&wake);
    }
    for job in drain_spelling(&queue) {
        match job {
            SpellingJob::Scan(job) => {
                let _ = job.reply.send(unavailable_spelling());
            }
            SpellingJob::Suggest(job) => {
                let _ = job.reply.send(unavailable_suggestion());
            }
        }
        wake_now(&wake);
    }
}

/// The newest request waiting, taking only the one slot it belongs to. `None` once closed.
fn next_spelling(shared: &Arc<SpellingShared>) -> Option<SpellingJob> {
    let (lock, ready) = &**shared;
    let mut queue = lock_guard(lock);
    loop {
        if queue.closed {
            return None;
        }
        match (queue.scan.take(), queue.suggest.take()) {
            (None, None) => queue = wait(ready, queue),
            (Some(scan), None) => return Some(SpellingJob::Scan(scan)),
            (None, Some(suggest)) => return Some(SpellingJob::Suggest(suggest)),
            (Some(scan), Some(suggest)) => {
                // Both slots have work: a suggestion is one word the owner is waiting for, while a
                // waiting scan is replaced by the next keystroke anyway, so the suggestion goes
                // first however new the scan is. Otherwise steady typing would push the owner's
                // suggestion back behind every keystroke.
                queue.scan = Some(scan);
                return Some(SpellingJob::Suggest(suggest));
            }
        }
    }
}

fn drain_spelling(shared: &Arc<SpellingShared>) -> Vec<SpellingJob> {
    let (lock, _) = &**shared;
    let mut queue = lock_guard(lock);
    let mut left = Vec::new();
    if let Some(job) = queue.scan.take() {
        left.push(SpellingJob::Scan(job));
    }
    if let Some(job) = queue.suggest.take() {
        left.push(SpellingJob::Suggest(job));
    }
    left
}

fn scan_on(host: &dyn SpellingHost, request: &SpellingRequest) -> SpellingReply {
    if is_cancelled(&request.cancelled) {
        return cancelled_spelling();
    }
    let answer = catch_unwind(AssertUnwindSafe(|| host.scan(&request.settings, &request.text, &|| is_cancelled(&request.cancelled))));
    let reply = match answer {
        Ok(reply) => reply,
        // A host that panicked is a host that failed this request, not a dead worker: the last
        // resort guard keeps the worker alive for every request after it.
        Err(_) => SpellingReply { error: Some(Failure::Failed), ..SpellingReply::default() },
    };
    if is_cancelled(&request.cancelled) {
        // Called off while the scan was running: not a clean answer, and not a failure either.
        return cancelled_spelling();
    }
    reply
}

fn suggest_on(host: &dyn SpellingHost, request: &SuggestionRequest) -> SuggestionReply {
    if is_cancelled(&request.cancelled) {
        return cancelled_suggestion();
    }
    let limit = request.limit.min(MAX_SUGGESTIONS);
    // A word no scan would look at is not one to search dictionaries for: the cap is the same one
    // the token reader uses, so a hostile request cannot make the worker search for a novel.
    if limit == 0 || request.word.char_indices().nth(MAX_WORD).is_some() {
        return SuggestionReply::default();
    }
    let answer = catch_unwind(AssertUnwindSafe(|| host.suggest(&request.settings, &request.word, limit)));
    let reply = match answer {
        Ok(reply) => reply,
        Err(_) => SuggestionReply { suggestions: Vec::new(), error: Some(Failure::Failed) },
    };
    if is_cancelled(&request.cancelled) {
        return cancelled_suggestion();
    }
    reply
}

// ---------------------------------------------------------------------------------------------
// Grammar worker: one bounded multilingual exchange at a time.
// ---------------------------------------------------------------------------------------------

struct GrammarJob {
    request: GrammarRequest,
    reply: Sender<GrammarReply>,
}

#[derive(Default)]
struct GrammarQueue {
    pending: Option<GrammarJob>,
    closed: bool,
}

type GrammarShared = (Mutex<GrammarQueue>, Condvar);

struct GrammarWorker {
    queue: Arc<GrammarShared>,
    finished: Arc<Finished>,
    handle: Option<JoinHandle<()>>,
}

impl GrammarWorker {
    fn spawn(host: Arc<dyn GrammarHost>, patience: Duration, wake: Wake, spawn: Spawn) -> Option<GrammarWorker> {
        let queue = Arc::new((Mutex::new(GrammarQueue::default()), Condvar::new()));
        let finished = Arc::new(Finished::default());
        let body: Box<dyn FnOnce() + Send> = {
            let queue = Arc::clone(&queue);
            let finished = Arc::clone(&finished);
            Box::new(move || {
                grammar_loop(host, queue, patience, wake);
                finished.mark();
            })
        };
        let handle = spawn("mailcraft-proof-grammar", body).ok()?;
        Some(GrammarWorker { queue, finished, handle: Some(handle) })
    }

    fn push(&self, job: GrammarJob) -> Result<Option<GrammarJob>, GrammarJob> {
        let (lock, ready) = &*self.queue;
        let mut queue = lock_guard(lock);
        if queue.closed {
            drop(queue);
            return Err(job);
        }
        let replaced = queue.pending.replace(job);
        drop(queue);
        ready.notify_one();
        Ok(replaced)
    }
}

impl Drop for GrammarWorker {
    fn drop(&mut self) {
        {
            let (lock, ready) = &*self.queue;
            lock_guard(lock).closed = true;
            ready.notify_all();
        }
        reap(&self.finished, &mut self.handle);
    }
}

/// Waits [`STOP_WAIT`] for a worker's loop to run out and reaps its thread when it has. A thread
/// still inside a dictionary pass or an exchange is left to finish by itself: its handle is
/// dropped here, so a window closing or replacing the service never waits for a file or the wire.
fn reap(finished: &Finished, handle: &mut Option<JoinHandle<()>>) {
    // A worker that could not be started has no thread to wait for.
    if handle.is_none() || !finished.wait(STOP_WAIT) {
        return;
    }
    if let Some(handle) = handle.take() {
        let _ = handle.join();
    }
}

fn grammar_loop(host: Arc<dyn GrammarHost>, queue: Arc<GrammarShared>, patience: Duration, wake: Wake) {
    while let Some(job) = next_grammar(&queue) {
        let reply = grammar_on(host.as_ref(), &job.request, patience);
        let _ = job.reply.send(reply);
        wake_now(&wake);
    }
    if let Some(job) = drain_grammar(&queue) {
        let _ = job.reply.send(unavailable_grammar());
        wake_now(&wake);
    }
}

fn next_grammar(shared: &Arc<GrammarShared>) -> Option<GrammarJob> {
    let (lock, ready) = &**shared;
    let mut queue = lock_guard(lock);
    loop {
        if queue.closed {
            return None;
        }
        if let Some(job) = queue.pending.take() {
            return Some(job);
        }
        queue = wait(ready, queue);
    }
}

fn drain_grammar(shared: &Arc<GrammarShared>) -> Option<GrammarJob> {
    let (lock, _) = &**shared;
    lock_guard(lock).pending.take()
}

fn grammar_on(host: &dyn GrammarHost, request: &GrammarRequest, patience: Duration) -> GrammarReply {
    if is_cancelled(&request.cancelled) {
        return cancelled_grammar();
    }
    if request.url.len() > MAX_URL_BYTES {
        // No request is made for an address no grammar server could be at.
        return GrammarReply { error: Some(Failure::Refused), ..GrammarReply::default() };
    }
    // Every enabled language, once each, bounded the way the adapter bounds its own list; and the
    // owner's quiet rules, bounded so a hostile settings file cannot enlarge the snapshot.
    let languages = bounded_languages(&request.languages);
    if languages.is_empty() {
        return GrammarReply { error: Some(Failure::Refused), ..GrammarReply::default() };
    }
    let ignored = bounded_rules(&request.ignored_rules);
    if !has_content(&request.text) {
        // Nothing to send: an empty check is a complete one, and saying so is honest.
        return GrammarReply::default();
    }
    let answer = catch_unwind(AssertUnwindSafe(|| host.check(&request.url, &request.text, &languages, &ignored, patience)));
    let reply = match answer {
        Ok(reply) => reply,
        Err(_) => GrammarReply { error: Some(Failure::Failed), ..GrammarReply::default() },
    };
    if is_cancelled(&request.cancelled) {
        // The check finished after the caller moved on; neither its answer nor its failure is
        // sent as the reply to a request nobody wants.
        return cancelled_grammar();
    }
    reply
}

// ---------------------------------------------------------------------------------------------
// The real hosts.
// ---------------------------------------------------------------------------------------------

/// The dictionaries of every enabled language, read once on the spelling worker and kept for the
/// session. Only that thread calls it, but the lock is what makes the host `Sync` as the contract
/// requires; it is never taken by the window.
pub(crate) struct LocalSpelling {
    proofing: Mutex<Proofing>,
}

impl LocalSpelling {
    pub(crate) fn new() -> LocalSpelling {
        LocalSpelling { proofing: Mutex::new(Proofing::default()) }
    }
}

/// What the fast scan answered, without the nested `Option` that would read badly.
enum Scan {
    Found(Vec<Diagnostic>),
    NoDictionary,
    Cancelled,
}

impl SpellingHost for LocalSpelling {
    fn scan(&self, settings: &ProofSettings, text: &str, cancelled: &dyn Fn() -> bool) -> SpellingReply {
        let mut proofing = lock_guard(&self.proofing);
        let cut = cut_after(text, MAX_CHECKED);
        let found = {
            // `words` reads the dictionaries if they were not read yet (or were read for another
            // set of languages); this is the only place in the service that touches a file.
            let speller = proofing.words(settings);
            if speller.is_empty() {
                Scan::NoDictionary
            } else {
                match speller.scan_cancellable(text, cancelled) {
                    Some(found) => Scan::Found(found),
                    None => Scan::Cancelled,
                }
            }
        };
        let dictionary_note = proofing.note.clone();
        match found {
            Scan::Cancelled => cancelled_spelling(),
            Scan::NoDictionary => SpellingReply {
                diagnostics: Vec::new(),
                note: join_notes([dictionary_note.or_else(|| Some(NOTE_NO_DICTIONARY.to_string())), cut_note(cut)]),
                error: None,
            },
            Scan::Found(found) => {
                let capped = found.len() >= MAX_DIAGNOSTICS;
                SpellingReply {
                    diagnostics: found,
                    note: join_notes([dictionary_note, cut_note(cut), capped.then(|| NOTE_DIAGNOSTIC_CAP.to_string())]),
                    error: None,
                }
            }
        }
    }

    fn suggest(&self, settings: &ProofSettings, word: &str, limit: usize) -> SuggestionReply {
        let mut proofing = lock_guard(&self.proofing);
        let limit = limit.min(MAX_SUGGESTIONS);
        let speller = proofing.words(settings);
        if limit == 0 || speller.is_empty() {
            // Without a dictionary there is nothing to offer, and claiming otherwise would be a
            // suggestion from nowhere. The scan reply's note is where the absence is said.
            return SuggestionReply::default();
        }
        SuggestionReply { suggestions: speller.suggest(word, limit), error: None }
    }
}

/// The real grammar adapter: one bounded multilingual check of every enabled language.
pub(crate) struct LocalGrammar;

impl GrammarHost for LocalGrammar {
    fn check(&self, url: &str, text: &str, languages: &[Language], ignored: &[String], patience: Duration) -> GrammarReply {
        match languagetool::check_multilingual(url, text, languages, ignored, patience) {
            Ok(outcome) => GrammarReply {
                diagnostics: outcome.diagnostics,
                checked_segments: outcome.checked_segments,
                skipped_segments: outcome.skipped_segments,
                partial: outcome.partial,
                error: None,
            },
            Err(error) => GrammarReply { error: Some(failure_of(error)), ..GrammarReply::default() },
        }
    }
}

/// The fixed failure an adapter error becomes. Nothing of the error's words is kept: the variant
/// is the whole message the window gets.
pub(crate) fn failure_of(error: LanguageToolError) -> Failure {
    match error {
        // An address or a language list no check may be asked with.
        LanguageToolError::Refused | LanguageToolError::Language => Failure::Refused,
        // Bounded text: both are "too much to look at", never a partial check.
        LanguageToolError::UnitTextTooLong | LanguageToolError::TooLong => Failure::TooLong,
        // Nothing answered, or the answer stopped: the server is not available.
        LanguageToolError::Unreachable | LanguageToolError::Closed => Failure::Unavailable,
        LanguageToolError::TooSlow => Failure::TooSlow,
        LanguageToolError::Failed => Failure::Failed,
        LanguageToolError::Garbled => Failure::Garbled,
    }
}

// ---------------------------------------------------------------------------------------------
// Fixed replies and small bounded helpers.
// ---------------------------------------------------------------------------------------------

fn cancelled_spelling() -> SpellingReply {
    SpellingReply { diagnostics: Vec::new(), note: None, error: Some(Failure::Cancelled) }
}

fn unavailable_spelling() -> SpellingReply {
    SpellingReply { diagnostics: Vec::new(), note: None, error: Some(Failure::Unavailable) }
}

fn cancelled_suggestion() -> SuggestionReply {
    SuggestionReply { suggestions: Vec::new(), error: Some(Failure::Cancelled) }
}

fn unavailable_suggestion() -> SuggestionReply {
    SuggestionReply { suggestions: Vec::new(), error: Some(Failure::Unavailable) }
}

fn cancelled_grammar() -> GrammarReply {
    GrammarReply { error: Some(Failure::Cancelled), ..GrammarReply::default() }
}

fn unavailable_grammar() -> GrammarReply {
    GrammarReply { error: Some(Failure::Unavailable), ..GrammarReply::default() }
}

fn is_cancelled(flag: &Cancellation) -> bool {
    flag.load(Ordering::Relaxed)
}

/// `true` when `text` has anything but white space in it, looked at only as far as the first such
/// character: an empty or blank draft is not sent anywhere.
fn has_content(text: &str) -> bool {
    text.chars().any(|c| !c.is_whitespace())
}

/// The byte offset after `most` characters, when the text has more than that many.
fn cut_after(text: &str, most: usize) -> Option<usize> {
    text.char_indices().nth(most).map(|(at, _)| at)
}

fn cut_note(cut: Option<usize>) -> Option<String> {
    cut.map(|_| NOTE_TRUNCATED.to_string())
}

/// The notes that apply, joined for File › Options and the review pane; `None` when there are none.
fn join_notes(parts: impl IntoIterator<Item = Option<String>>) -> Option<String> {
    let kept: Vec<String> = parts.into_iter().flatten().filter(|note| !note.is_empty()).collect();
    (!kept.is_empty()).then(|| kept.join(" "))
}

/// The enabled languages, without repeats and no more than the four proofing knows.
fn bounded_languages(languages: &[Language]) -> Vec<Language> {
    let mut out: Vec<Language> = Vec::new();
    for language in languages {
        if out.len() >= Language::ALL.len() {
            break;
        }
        if !out.contains(language) {
            out.push(*language);
        }
    }
    out
}

/// The owner's quiet rules, bounded by count and by the length of one id.
fn bounded_rules(rules: &[String]) -> Vec<String> {
    rules.iter().filter(|rule| rule.len() <= MAX_RULE_BYTES).take(MAX_RULES).cloned().collect()
}

fn wake_now(wake: &Wake) {
    // The window's callback is not this module's to trust: a panic in it must not kill a worker
    // and leave every later request unanswered.
    let _ = catch_unwind(AssertUnwindSafe(|| wake()));
}

fn spawn_thread(name: &str, body: Box<dyn FnOnce() + Send + 'static>) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new().name(name.to_string()).spawn(body)
}

fn lock_guard<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A poisoned lock still holds a usable value here: the guarded structures are plain slots and
    // flags, and a panic elsewhere must not make every later reply impossible.
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn wait<'a, T>(ready: &Condvar, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
    ready.wait(guard).unwrap_or_else(|poisoned| poisoned.into_inner())
}
