// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Tests of the native bounded proofreading service (#954), kept out of the source file as the
//! repository requires.
//!
//! Two kinds of host stand in the service's place. The synthetic ones make coalescing,
//! cancellation, spawn failure and shutdown deterministic: a gate holds a call inside a worker,
//! so the test knows exactly which request is active and which is waiting. The real ones
//! ([`LocalSpelling`], [`LocalGrammar`]) are exercised too, against the Hunspell fixtures under
//! `/home/donielmoins/.hermes/cache/scratch/proofing-evidence/real-dictionaries` (copied into a
//! scratch folder first, so a test may remove them) and against addresses with no server: those
//! tests print a labelled skip when the fixtures are absent, and never touch the owner's
//! LanguageTool server on 127.0.0.1:18081.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use mailcraft_model::proof::{Diagnostic, Kind, Language, ProofSettings, Source, Span};
use mailcraft_net::languagetool::LanguageToolError;
use mailcraft_ui_egui::proof_service::{
    Cancellation, Failure, GrammarReply, GrammarRequest, ProofService, SpellingReply, SpellingRequest, SuggestionReply, SuggestionRequest,
};

use crate::proof_native::{Config, GrammarHost, LocalGrammar, LocalSpelling, Service, SpellingHost, failure_of};

/// How long a reply may take when nothing is meant to hold it up.
const SOON: Duration = Duration::from_secs(5);

/// Long enough for a real dictionary read on this machine, short enough to fail a stuck worker.
const REAL_WORK: Duration = Duration::from_secs(60);

/// Where the real Hunspell fixtures live. Provenance, printed by the tests that use them:
/// `en_CA.dic`/`en_CA.aff` are a real Hunspell English (Canada) pair; `fr_CA.dic`/`fr_CA.aff` are
/// a French (France) dictionary installed under the Canadian names (an alias), which is why the
/// French test says so rather than claiming a Canadian French dictionary was read.
const FIXTURES: &str = "/home/donielmoins/.hermes/cache/scratch/proofing-evidence/real-dictionaries";

// ---------------------------------------------------------------------------------------------
// Deterministic helpers.
// ---------------------------------------------------------------------------------------------

/// A one-shot gate: a host call waits on it until the test opens it.
#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    ready: Condvar,
}

impl Gate {
    fn wait(&self) {
        let mut open = self.open.lock().expect("gate");
        while !*open {
            open = self.ready.wait(open).expect("gate wait");
        }
    }

    fn open(&self) {
        *self.open.lock().expect("gate") = true;
        self.ready.notify_all();
    }
}

/// Calls entered, with a wait that gives up rather than hanging a test run.
#[derive(Default)]
struct Entered {
    count: Mutex<usize>,
    ready: Condvar,
}

impl Entered {
    fn bump(&self) {
        *self.count.lock().expect("entered") += 1;
        self.ready.notify_all();
    }

    fn wait_for(&self, want: usize) {
        let mut count = self.count.lock().expect("entered");
        let deadline = Instant::now() + Duration::from_secs(10);
        while *count < want {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                panic!("a worker call did not start in ten seconds");
            }
            let (guard, _) = self.ready.wait_timeout(count, left).expect("entered wait");
            count = guard;
        }
    }
}

fn never_cancelled() -> Cancellation {
    Arc::new(AtomicBool::new(false))
}

fn cancelled() -> Cancellation {
    Arc::new(AtomicBool::new(true))
}

fn wake() -> Arc<dyn Fn() + Send + Sync> {
    Arc::new(|| {})
}

fn counted_wake() -> (Arc<AtomicUsize>, Arc<dyn Fn() + Send + Sync>) {
    let count = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&count);
    (
        count,
        Arc::new(move || {
            seen.fetch_add(1, Ordering::Relaxed);
        }),
    )
}

fn settings(languages: &[Language]) -> ProofSettings {
    ProofSettings { languages: languages.to_vec(), ..ProofSettings::default() }
}

fn with_dir(languages: &[Language], dir: &Path) -> ProofSettings {
    ProofSettings { languages: languages.to_vec(), dictionary_dir: dir.to_string_lossy().into_owned(), ..ProofSettings::default() }
}

fn spell_request(text: &str, settings: ProofSettings) -> SpellingRequest {
    SpellingRequest { text: text.to_string(), settings, cancelled: never_cancelled() }
}

fn suggest_request(word: &str, settings: ProofSettings) -> SuggestionRequest {
    SuggestionRequest { word: word.to_string(), settings, limit: 8, cancelled: never_cancelled() }
}

fn grammar_request(text: &str) -> GrammarRequest {
    GrammarRequest {
        url: "http://127.0.0.1:18081".to_string(),
        text: text.to_string(),
        languages: vec![Language::EnCa],
        ignored_rules: Vec::new(),
        cancelled: never_cancelled(),
    }
}

fn reply<T>(rx: &Receiver<T>, within: Duration) -> T {
    rx.recv_timeout(within).expect("a reply before the deadline")
}

fn a_place(text: &str) -> Diagnostic {
    let span = Span::of(text, 0, 1).expect("a span of the first character");
    Diagnostic::new(span, Kind::Spelling, Source::Local)
}

// ---------------------------------------------------------------------------------------------
// Synthetic hosts.
// ---------------------------------------------------------------------------------------------

/// Holds every scan until the gate opens, and records what it was asked, in order.
struct BlockingSpelling {
    gate: Arc<Gate>,
    entered: Arc<Entered>,
    log: Mutex<Vec<String>>,
}

impl BlockingSpelling {
    fn new(gate: Arc<Gate>, entered: Arc<Entered>) -> BlockingSpelling {
        BlockingSpelling { gate, entered, log: Mutex::new(Vec::new()) }
    }

    fn log(&self) -> Vec<String> {
        self.log.lock().expect("log").clone()
    }
}

impl SpellingHost for BlockingSpelling {
    fn scan(&self, _settings: &ProofSettings, text: &str, _cancelled: &dyn Fn() -> bool) -> SpellingReply {
        self.log.lock().expect("log").push(format!("scan:{text}"));
        self.entered.bump();
        self.gate.wait();
        SpellingReply::default()
    }

    fn suggest(&self, _settings: &ProofSettings, word: &str, _limit: usize) -> SuggestionReply {
        self.log.lock().expect("log").push(format!("suggest:{word}"));
        self.entered.bump();
        SuggestionReply { suggestions: vec!["alpha".to_string()], error: None }
    }
}

/// Answers at once, and counts what it was asked.
#[derive(Default)]
struct StubSpelling {
    scans: AtomicUsize,
    suggests: AtomicUsize,
}

impl SpellingHost for StubSpelling {
    fn scan(&self, _settings: &ProofSettings, text: &str, _cancelled: &dyn Fn() -> bool) -> SpellingReply {
        self.scans.fetch_add(1, Ordering::Relaxed);
        if text.contains("spell-fail") {
            return SpellingReply { error: Some(Failure::Failed), ..SpellingReply::default() };
        }
        if text.contains("mistkae") {
            return SpellingReply { diagnostics: vec![a_place(text)], note: Some("stub".to_string()), error: None };
        }
        SpellingReply::default()
    }

    fn suggest(&self, _settings: &ProofSettings, word: &str, _limit: usize) -> SuggestionReply {
        self.suggests.fetch_add(1, Ordering::Relaxed);
        if word == "fail" {
            return SuggestionReply { error: Some(Failure::Failed), ..SuggestionReply::default() };
        }
        SuggestionReply { suggestions: vec!["suggestion".to_string()], error: None }
    }
}

/// Holds every check until the gate opens, counting how many run at once.
struct BlockingGrammar {
    gate: Arc<Gate>,
    entered: Arc<Entered>,
    running: AtomicUsize,
    most: AtomicUsize,
    checks: AtomicUsize,
}

impl BlockingGrammar {
    fn new(gate: Arc<Gate>, entered: Arc<Entered>) -> BlockingGrammar {
        BlockingGrammar { gate, entered, running: AtomicUsize::new(0), most: AtomicUsize::new(0), checks: AtomicUsize::new(0) }
    }

    fn most_at_once(&self) -> usize {
        self.most.load(Ordering::Relaxed)
    }

    fn checks(&self) -> usize {
        self.checks.load(Ordering::Relaxed)
    }
}

impl GrammarHost for BlockingGrammar {
    fn check(&self, _url: &str, _text: &str, _languages: &[Language], _ignored: &[String], _patience: Duration) -> GrammarReply {
        self.checks.fetch_add(1, Ordering::Relaxed);
        let now = self.running.fetch_add(1, Ordering::Relaxed) + 1;
        self.most.fetch_max(now, Ordering::Relaxed);
        self.entered.bump();
        self.gate.wait();
        self.running.fetch_sub(1, Ordering::Relaxed);
        GrammarReply { checked_segments: 1, ..GrammarReply::default() }
    }
}

/// Answers at once, with a chosen failure or a partial outcome, so pass-through can be seen.
struct StubGrammar;

impl GrammarHost for StubGrammar {
    fn check(&self, _url: &str, text: &str, _languages: &[Language], _ignored: &[String], _patience: Duration) -> GrammarReply {
        match text {
            "slow" => GrammarReply { error: Some(Failure::TooSlow), ..GrammarReply::default() },
            "garbled" => GrammarReply { error: Some(Failure::Garbled), ..GrammarReply::default() },
            "partial" => GrammarReply { checked_segments: 3, skipped_segments: 2, partial: true, ..GrammarReply::default() },
            _ => GrammarReply { diagnostics: vec![a_place(text)], checked_segments: 1, ..GrammarReply::default() },
        }
    }
}

/// Holds a call until the gate opens, then fails: the answer a called-off request must not get.
struct FailingGateGrammar {
    gate: Arc<Gate>,
    entered: Arc<Entered>,
}

impl GrammarHost for FailingGateGrammar {
    fn check(&self, _url: &str, _text: &str, _languages: &[Language], _ignored: &[String], _patience: Duration) -> GrammarReply {
        self.entered.bump();
        self.gate.wait();
        GrammarReply { error: Some(Failure::TooSlow), ..GrammarReply::default() }
    }
}

/// The spelling form of [`FailingGateGrammar`].
struct FailingGateSpelling {
    gate: Arc<Gate>,
    entered: Arc<Entered>,
}

impl SpellingHost for FailingGateSpelling {
    fn scan(&self, _settings: &ProofSettings, _text: &str, _cancelled: &dyn Fn() -> bool) -> SpellingReply {
        self.entered.bump();
        self.gate.wait();
        SpellingReply { error: Some(Failure::Failed), ..SpellingReply::default() }
    }

    fn suggest(&self, _settings: &ProofSettings, _word: &str, _limit: usize) -> SuggestionReply {
        SuggestionReply::default()
    }
}

// ---------------------------------------------------------------------------------------------
// Spawn fakes.
// ---------------------------------------------------------------------------------------------

fn spawn_real(name: &str, body: Box<dyn FnOnce() + Send + 'static>) -> std::io::Result<thread::JoinHandle<()>> {
    thread::Builder::new().name(name.to_string()).spawn(body)
}

fn spawn_fail_spelling(name: &str, body: Box<dyn FnOnce() + Send + 'static>) -> std::io::Result<thread::JoinHandle<()>> {
    if name.contains("spelling") { Err(std::io::Error::other("no thread for the test")) } else { spawn_real(name, body) }
}

fn spawn_fail_grammar(name: &str, body: Box<dyn FnOnce() + Send + 'static>) -> std::io::Result<thread::JoinHandle<()>> {
    if name.contains("grammar") { Err(std::io::Error::other("no thread for the test")) } else { spawn_real(name, body) }
}

fn spawn_fail_everything(_name: &str, _body: Box<dyn FnOnce() + Send + 'static>) -> std::io::Result<thread::JoinHandle<()>> {
    Err(std::io::Error::other("no thread for the test"))
}

// ---------------------------------------------------------------------------------------------
// A scratch folder for dictionary fixtures, removed when the test ends.
// ---------------------------------------------------------------------------------------------

static SCRATCHES: AtomicUsize = AtomicUsize::new(0);

struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let unique = format!("mailcraft-proof-native-{tag}-{}-{}", std::process::id(), SCRATCHES.fetch_add(1, Ordering::Relaxed));
        let path = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&path).expect("a scratch folder");
        Scratch { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// Copies the real fixtures in, and says whether they were there at all.
    fn copy_fixtures(&self) -> bool {
        let from = Path::new(FIXTURES);
        let mut copied = 0;
        for name in ["en_CA.dic", "en_CA.aff"] {
            let source = from.join(name);
            if source.is_file() && std::fs::copy(&source, self.path.join(name)).is_ok() {
                copied += 1;
            }
        }
        copied == 2
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn fixtures_note(what: &str) -> bool {
    if Path::new(FIXTURES).join("en_CA.dic").is_file() && Path::new(FIXTURES).join("en_CA.aff").is_file() {
        return true;
    }
    println!("SKIP {what}: the real Hunspell fixtures are not at {FIXTURES}");
    false
}

// ---------------------------------------------------------------------------------------------
// Coalescing: one active and one newest pending, per kind.
// ---------------------------------------------------------------------------------------------

#[test]
fn newest_pending_scan_wins_and_the_replaced_one_is_cancelled() {
    let gate = Arc::new(Gate::default());
    let entered = Arc::new(Entered::default());
    let host = Arc::new(BlockingSpelling::new(Arc::clone(&gate), Arc::clone(&entered)));
    let service = Service::start(Config::testing(wake()).with_spelling(host.clone()));

    let first = service.spelling(spell_request("first", settings(&[Language::EnCa])));
    entered.wait_for(1);

    let replaced = service.spelling(spell_request("second", settings(&[Language::EnCa])));
    let newest = service.spelling(spell_request("third", settings(&[Language::EnCa])));

    // The replaced request is answered at once, and not with a clean empty success.
    assert_eq!(reply(&replaced, SOON).error, Some(Failure::Cancelled));

    gate.open();
    assert_eq!(reply(&first, SOON).error, None);
    assert_eq!(reply(&newest, SOON).error, None);

    // The worker ran the active scan and the newest waiting one; the middle one never started.
    assert_eq!(host.log(), vec!["scan:first".to_string(), "scan:third".to_string()]);
}

#[test]
fn a_burst_of_scans_does_not_delay_the_suggestion() {
    let gate = Arc::new(Gate::default());
    let entered = Arc::new(Entered::default());
    let host = Arc::new(BlockingSpelling::new(Arc::clone(&gate), Arc::clone(&entered)));
    let service = Service::start(Config::testing(wake()).with_spelling(host.clone()));

    let scans = service.spelling(spell_request("first", settings(&[Language::EnCa])));
    entered.wait_for(1);
    // The owner asks for a suggestion, then keeps typing: each new scan is newer than the
    // suggestion. If newer work always went first, steady typing would push the suggestion back
    // for ever, so the one-word answer the owner asked for must not lose to a scan that the next
    // keystroke would replace anyway.
    let suggestion = service.suggest(suggest_request("word", settings(&[Language::EnCa])));
    let waiting_scan = service.spelling(spell_request("second", settings(&[Language::EnCa])));

    gate.open();
    assert_eq!(reply(&scans, SOON).error, None);
    let got = reply(&suggestion, SOON);
    assert_eq!(got.error, None);
    assert_eq!(got.suggestions, vec!["alpha".to_string()]);
    assert_eq!(reply(&waiting_scan, SOON).error, None);

    assert_eq!(
        host.log(),
        vec!["scan:first".to_string(), "suggest:word".to_string(), "scan:second".to_string()],
        "a newer scan ran before the suggestion the owner asked for"
    );
}

#[test]
fn grammar_keeps_one_active_and_one_pending_and_runs_them_one_at_a_time() {
    let gate = Arc::new(Gate::default());
    let entered = Arc::new(Entered::default());
    let host = Arc::new(BlockingGrammar::new(Arc::clone(&gate), Arc::clone(&entered)));
    let service = Service::start(Config::testing(wake()).with_grammar(host.clone()));

    let first = service.grammar(grammar_request("one"));
    entered.wait_for(1);
    let mut replaced = Vec::new();
    for text in ["two", "three", "four"] {
        replaced.push(service.grammar(grammar_request(text)));
    }
    let newest = service.grammar(grammar_request("five"));

    for rx in &replaced {
        assert_eq!(reply(rx, SOON).error, Some(Failure::Cancelled));
    }

    gate.open();
    assert_eq!(reply(&first, SOON).error, None);
    assert_eq!(reply(&newest, SOON).error, None);

    // Every check that ran, ran alone: the worker is one thread and one active request.
    assert_eq!(host.most_at_once(), 1);
    assert_eq!(host.checks(), 2, "only the active and the newest waiting check should run");
}

#[test]
fn a_replaced_suggestion_is_cancelled_and_the_newest_one_runs() {
    let gate = Arc::new(Gate::default());
    let entered = Arc::new(Entered::default());
    let host = Arc::new(BlockingSpelling::new(Arc::clone(&gate), Arc::clone(&entered)));
    let service = Service::start(Config::testing(wake()).with_spelling(host.clone()));

    let scan = service.spelling(spell_request("first", settings(&[Language::EnCa])));
    entered.wait_for(1);
    let old = service.suggest(suggest_request("older", settings(&[Language::EnCa])));
    let newest = service.suggest(suggest_request("newer", settings(&[Language::EnCa])));

    assert_eq!(reply(&old, SOON).error, Some(Failure::Cancelled));
    gate.open();
    assert_eq!(reply(&scan, SOON).error, None);
    let got = reply(&newest, SOON);
    assert_eq!(got.error, None);
    assert_eq!(got.suggestions, vec!["alpha".to_string()]);

    assert_eq!(host.log(), vec!["scan:first".to_string(), "suggest:newer".to_string()]);
}

#[test]
fn a_grammar_request_with_no_language_is_refused_without_a_request() {
    let gate = Arc::new(Gate::default());
    let entered = Arc::new(Entered::default());
    let host = Arc::new(BlockingGrammar::new(Arc::clone(&gate), Arc::clone(&entered)));
    let service = Service::start(Config::testing(wake()).with_grammar(host.clone()));

    let mut request = grammar_request("one");
    request.languages = Vec::new();
    assert_eq!(reply(&service.grammar(request), SOON).error, Some(Failure::Refused));
    assert_eq!(host.checks(), 0, "no check can be asked for with no language to ask in");
}

// ---------------------------------------------------------------------------------------------
// Cancellation and replacement.
// ---------------------------------------------------------------------------------------------

#[test]
fn a_request_called_off_before_it_starts_never_reaches_the_host() {
    let host = Arc::new(StubSpelling::default());
    let grammar = Arc::new(StubGrammar);
    let service = Service::start(Config::testing(wake()).with_spelling(host.clone()).with_grammar(grammar));

    let scan = service.spelling(SpellingRequest { text: "anything".to_string(), settings: settings(&[Language::EnCa]), cancelled: cancelled() });
    assert_eq!(reply(&scan, SOON).error, Some(Failure::Cancelled));

    let suggest =
        service.suggest(SuggestionRequest { word: "anything".to_string(), settings: settings(&[Language::EnCa]), limit: 8, cancelled: cancelled() });
    assert_eq!(reply(&suggest, SOON).error, Some(Failure::Cancelled));

    let mut request = grammar_request("one");
    request.cancelled = cancelled();
    let check = service.grammar(request);
    assert_eq!(reply(&check, SOON).error, Some(Failure::Cancelled));

    assert_eq!(host.scans.load(Ordering::Relaxed), 0);
    assert_eq!(host.suggests.load(Ordering::Relaxed), 0);
}

#[test]
fn a_scan_called_off_while_it_runs_is_cancelled_not_answered_cleanly() {
    let gate = Arc::new(Gate::default());
    let entered = Arc::new(Entered::default());
    let host = Arc::new(BlockingSpelling::new(Arc::clone(&gate), Arc::clone(&entered)));
    let service = Service::start(Config::testing(wake()).with_spelling(host.clone()));

    let flag = never_cancelled();
    let request = SpellingRequest { text: "one".to_string(), settings: settings(&[Language::EnCa]), cancelled: Arc::clone(&flag) };
    let rx = service.spelling(request);
    entered.wait_for(1);

    // The owner moved on while the scan was inside the host.
    flag.store(true, Ordering::Relaxed);
    gate.open();

    let got = reply(&rx, SOON);
    assert_eq!(got.error, Some(Failure::Cancelled));
    assert!(got.diagnostics.is_empty());
}

#[test]
fn a_grammar_check_called_off_while_it_runs_is_cancelled_and_its_answer_dropped() {
    let gate = Arc::new(Gate::default());
    let entered = Arc::new(Entered::default());
    let host = Arc::new(BlockingGrammar::new(Arc::clone(&gate), Arc::clone(&entered)));
    let service = Service::start(Config::testing(wake()).with_grammar(host.clone()));

    let flag = never_cancelled();
    let mut request = grammar_request("one");
    request.cancelled = Arc::clone(&flag);
    let rx = service.grammar(request);
    entered.wait_for(1);

    flag.store(true, Ordering::Relaxed);
    gate.open();

    let got = reply(&rx, SOON);
    assert_eq!(got.error, Some(Failure::Cancelled));
    assert!(got.diagnostics.is_empty(), "a dropped check must not deliver its diagnostics");
}

#[test]
fn a_called_off_request_answers_cancelled_even_when_its_host_failed() {
    // The flag is read once more before the reply is sent, so a failure that arrived after the
    // caller moved on is not delivered as the answer to a request nobody wants.
    let gate = Arc::new(Gate::default());
    let entered = Arc::new(Entered::default());
    let grammar_host = Arc::new(FailingGateGrammar { gate: Arc::clone(&gate), entered: Arc::clone(&entered) });
    let service = Service::start(Config::testing(wake()).with_grammar(grammar_host));

    let flag = never_cancelled();
    let mut request = grammar_request("one");
    request.cancelled = Arc::clone(&flag);
    let rx = service.grammar(request);
    entered.wait_for(1);
    flag.store(true, Ordering::Relaxed);
    gate.open();
    assert_eq!(reply(&rx, SOON).error, Some(Failure::Cancelled));

    let gate = Arc::new(Gate::default());
    let entered = Arc::new(Entered::default());
    let spelling_host = Arc::new(FailingGateSpelling { gate: Arc::clone(&gate), entered: Arc::clone(&entered) });
    let service = Service::start(Config::testing(wake()).with_spelling(spelling_host));

    let flag = never_cancelled();
    let rx = service.spelling(SpellingRequest { text: "one".to_string(), settings: settings(&[Language::EnCa]), cancelled: Arc::clone(&flag) });
    entered.wait_for(1);
    flag.store(true, Ordering::Relaxed);
    gate.open();
    assert_eq!(reply(&rx, SOON).error, Some(Failure::Cancelled));
}

// ---------------------------------------------------------------------------------------------
// Real dictionaries: lazy suggestions, one cache, honest absence.
// ---------------------------------------------------------------------------------------------

#[test]
fn spelling_scan_keeps_no_suggestions_and_suggestions_reuse_the_cached_dictionary() {
    if !fixtures_note("lazy suggestions and the shared dictionary cache") {
        return;
    }
    let scratch = Scratch::new("cache");
    if !scratch.copy_fixtures() {
        println!("SKIP lazy suggestions: the en_CA fixtures could not be copied");
        return;
    }
    let proofing = with_dir(&[Language::EnCa], scratch.path());

    let service = Service::start(Config::testing(wake()).with_spelling(Arc::new(LocalSpelling::new())));

    let scan = service.spelling(spell_request("This has a mistkae in it.", proofing.clone()));
    let found = reply(&scan, REAL_WORK);
    assert_eq!(found.error, None);
    assert!(!found.diagnostics.is_empty(), "the real dictionary should flag a misspelling");
    assert!(found.diagnostics.iter().all(|d| d.suggestions.is_empty()), "a scan must keep places only; suggestions are asked for one word at a time");

    // The dictionary files are gone, and the service still answers: the cache is shared by the
    // scans and the suggestions, and neither rereads a file.
    for name in ["en_CA.dic", "en_CA.aff"] {
        std::fs::remove_file(scratch.path().join(name)).expect("remove the fixture copy");
    }

    let suggestion = service.suggest(suggest_request("mistkae", proofing.clone()));
    let got = reply(&suggestion, REAL_WORK);
    assert_eq!(got.error, None);
    assert!(got.suggestions.iter().any(|s| s == "mistake"), "expected 'mistake' among {:?}", got.suggestions);

    let again = service.spelling(spell_request("This has a mistkae in it.", proofing));
    assert!(!reply(&again, REAL_WORK).diagnostics.is_empty(), "the cached dictionary is still used");
}

#[test]
fn missing_dictionary_is_said_and_yields_no_diagnostics() {
    let scratch = Scratch::new("missing");
    let proofing = with_dir(&[Language::EnCa], scratch.path());
    let service = Service::start(Config::testing(wake()).with_spelling(Arc::new(LocalSpelling::new())));

    let scan = service.spelling(spell_request("This has a mistkae in it.", proofing.clone()));
    let got = reply(&scan, REAL_WORK);
    assert_eq!(got.error, None, "no dictionary is a stated condition, not an error");
    assert!(got.diagnostics.is_empty(), "nothing can be checked without a dictionary");
    let note = got.note.expect("a note about the missing dictionary");
    assert!(note.to_lowercase().contains("dictionary"), "the note must say what is missing: {note}");

    let suggestion = service.suggest(suggest_request("mistkae", proofing));
    let got = reply(&suggestion, REAL_WORK);
    assert_eq!(got.error, None);
    assert!(got.suggestions.is_empty(), "no dictionary, no suggestion from nowhere");
}

#[test]
fn a_corrupt_dictionary_is_treated_as_missing_and_never_panics() {
    let scratch = Scratch::new("corrupt");
    // Not UTF-8 at all: the dictionary reader refuses it, and the host must say so rather than
    // read a word list out of nothing.
    std::fs::write(scratch.path().join("en_CA.dic"), [0xff, 0xfe, 0x00, 0x01]).expect("write a corrupt dictionary");
    std::fs::write(scratch.path().join("en_CA.aff"), b"").expect("write an empty affix file");
    let proofing = with_dir(&[Language::EnCa], scratch.path());
    let service = Service::start(Config::testing(wake()).with_spelling(Arc::new(LocalSpelling::new())));

    let scan = service.spelling(spell_request("This has a mistkae in it.", proofing));
    let got = reply(&scan, REAL_WORK);
    assert_eq!(got.error, None);
    assert!(got.diagnostics.is_empty());
    assert!(got.note.is_some(), "the owner is told there is no usable dictionary");

    // A file that is a dictionary in name only, with no stem at all, is no dictionary either.
    std::fs::write(scratch.path().join("en_CA.dic"), b"0\n").expect("write an empty dictionary");
    let again = service.spelling(spell_request("Another mistkae.", with_dir(&[Language::EnCa], scratch.path())));
    let got = reply(&again, REAL_WORK);
    assert_eq!(got.error, None);
    assert!(got.diagnostics.is_empty());
    assert!(got.note.is_some());
}

// ---------------------------------------------------------------------------------------------
// Worker failures.
// ---------------------------------------------------------------------------------------------

#[test]
fn a_spelling_worker_that_cannot_start_leaves_grammar_working() {
    let spelling = Arc::new(StubSpelling::default());
    let service =
        Service::start(Config::testing(wake()).with_spelling(spelling.clone()).with_grammar(Arc::new(StubGrammar)).with_spawn(spawn_fail_spelling));

    assert_eq!(reply(&service.spelling(spell_request("one", settings(&[Language::EnCa]))), SOON).error, Some(Failure::Unavailable));
    assert_eq!(reply(&service.suggest(suggest_request("one", settings(&[Language::EnCa]))), SOON).error, Some(Failure::Unavailable));
    let grammar = reply(&service.grammar(grammar_request("one")), SOON);
    assert_eq!(grammar.error, None);
    assert_eq!(grammar.checked_segments, 1);
    assert_eq!(spelling.scans.load(Ordering::Relaxed), 0, "no worker, no host call");
}

#[test]
fn a_grammar_worker_that_cannot_start_leaves_spelling_working() {
    let host = Arc::new(StubSpelling::default());
    let service =
        Service::start(Config::testing(wake()).with_spelling(host.clone()).with_grammar(Arc::new(StubGrammar)).with_spawn(spawn_fail_grammar));

    assert_eq!(reply(&service.grammar(grammar_request("one")), SOON).error, Some(Failure::Unavailable));
    let scan = reply(&service.spelling(spell_request("text with mistkae", settings(&[Language::EnCa]))), SOON);
    assert_eq!(scan.error, None);
    assert_eq!(scan.diagnostics.len(), 1);
}

#[test]
fn no_worker_at_all_answers_unavailable_everywhere() {
    let service = Service::start(
        Config::testing(wake())
            .with_spelling(Arc::new(StubSpelling::default()))
            .with_grammar(Arc::new(StubGrammar))
            .with_spawn(spawn_fail_everything),
    );
    assert_eq!(reply(&service.spelling(spell_request("one", settings(&[Language::EnCa]))), SOON).error, Some(Failure::Unavailable));
    assert_eq!(reply(&service.grammar(grammar_request("one")), SOON).error, Some(Failure::Unavailable));
    assert_eq!(reply(&service.suggest(suggest_request("one", settings(&[Language::EnCa]))), SOON).error, Some(Failure::Unavailable));
}

// ---------------------------------------------------------------------------------------------
// Independent errors and pass-through.
// ---------------------------------------------------------------------------------------------

#[test]
fn spelling_and_grammar_failures_are_independent() {
    let service = Service::start(Config::testing(wake()).with_spelling(Arc::new(StubSpelling::default())).with_grammar(Arc::new(StubGrammar)));

    let bad_spelling = reply(&service.spelling(spell_request("spell-fail", settings(&[Language::EnCa]))), SOON);
    assert_eq!(bad_spelling.error, Some(Failure::Failed));
    let bad_grammar = reply(&service.grammar(grammar_request("slow")), SOON);
    assert_eq!(bad_grammar.error, Some(Failure::TooSlow));

    // Neither failure touches the other kind, and a later request is answered normally.
    assert_eq!(reply(&service.spelling(spell_request("fine", settings(&[Language::EnCa]))), SOON).error, None);
    assert_eq!(reply(&service.grammar(grammar_request("fine")), SOON).error, None);
    assert_eq!(reply(&service.grammar(grammar_request("garbled")), SOON).error, Some(Failure::Garbled));
}

#[test]
fn a_partial_grammar_answer_is_passed_through_as_partial() {
    let service = Service::start(Config::testing(wake()).with_grammar(Arc::new(StubGrammar)));
    let got = reply(&service.grammar(grammar_request("partial")), SOON);
    assert_eq!(got.error, None);
    assert!(got.partial, "a partial look must never be reported as complete");
    assert_eq!(got.checked_segments, 3);
    assert_eq!(got.skipped_segments, 2);
}

#[test]
fn adapter_errors_map_to_fixed_failures() {
    let cases = [
        (LanguageToolError::Refused, Failure::Refused),
        (LanguageToolError::Language, Failure::Refused),
        (LanguageToolError::UnitTextTooLong, Failure::TooLong),
        (LanguageToolError::TooLong, Failure::TooLong),
        (LanguageToolError::Unreachable, Failure::Unavailable),
        (LanguageToolError::Closed, Failure::Unavailable),
        (LanguageToolError::TooSlow, Failure::TooSlow),
        (LanguageToolError::Failed, Failure::Failed),
        (LanguageToolError::Garbled, Failure::Garbled),
    ];
    for (error, expected) in cases {
        assert_eq!(failure_of(error), expected, "{error:?}");
    }
}

#[test]
fn the_real_grammar_host_refuses_a_remote_address_and_survives_no_server() {
    let service = Service::start(Config::testing(wake()).with_patience(Duration::from_millis(300)));

    // Not this computer: refused before anything is sent, through the real adapter.
    let mut remote = grammar_request("one");
    remote.url = "https://example.com".to_string();
    assert_eq!(reply(&service.grammar(remote), SOON).error, Some(Failure::Refused));

    // Nothing listens on port 1 of this machine: unavailable, not a hang. The owner's server on
    // 18081 is never contacted by these tests.
    let mut none = grammar_request("one");
    none.url = "http://127.0.0.1:1".to_string();
    let got = reply(&service.grammar(none), SOON);
    assert_eq!(got.error, Some(Failure::Unavailable));
    assert!(!got.partial);
}

#[test]
fn the_real_grammar_host_says_nothing_for_blank_text() {
    let service = Service::start(Config::testing(wake()));
    let got = reply(&service.grammar(grammar_request("   \n\t ")), SOON);
    assert_eq!(got.error, None);
    assert_eq!(got.checked_segments, 0);
    assert!(!got.partial);
}

#[test]
fn an_address_too_long_to_be_a_server_is_refused_without_a_request() {
    let gate = Arc::new(Gate::default());
    let entered = Arc::new(Entered::default());
    let host = Arc::new(BlockingGrammar::new(Arc::clone(&gate), Arc::clone(&entered)));
    let service = Service::start(Config::testing(wake()).with_grammar(host.clone()));

    let mut request = grammar_request("one");
    request.url = format!("http://127.0.0.1/{}", "a".repeat(4_096));
    let got = reply(&service.grammar(request), SOON);
    assert_eq!(got.error, Some(Failure::Refused));
    assert_eq!(host.checks(), 0, "no host call for an address that cannot be one");
}

// ---------------------------------------------------------------------------------------------
// Channel failures: a caller that stopped listening is not a dead worker.
// ---------------------------------------------------------------------------------------------

#[test]
fn a_dropped_receiver_does_not_close_the_worker() {
    let service = Service::start(Config::testing(wake()).with_spelling(Arc::new(StubSpelling::default())).with_grammar(Arc::new(StubGrammar)));

    drop(service.spelling(spell_request("first", settings(&[Language::EnCa]))));
    drop(service.grammar(grammar_request("first")));
    drop(service.suggest(suggest_request("first", settings(&[Language::EnCa]))));

    // Give the workers a moment to answer into the channels that are gone.
    thread::sleep(Duration::from_millis(50));

    assert_eq!(reply(&service.spelling(spell_request("second", settings(&[Language::EnCa]))), SOON).error, None);
    assert_eq!(reply(&service.grammar(grammar_request("second")), SOON).error, None);
    assert_eq!(reply(&service.suggest(suggest_request("second", settings(&[Language::EnCa]))), SOON).error, None);
}

#[test]
fn a_replaced_request_whose_caller_left_does_not_stop_the_worker() {
    let gate = Arc::new(Gate::default());
    let entered = Arc::new(Entered::default());
    let host = Arc::new(BlockingSpelling::new(Arc::clone(&gate), Arc::clone(&entered)));
    let service = Service::start(Config::testing(wake()).with_spelling(host.clone()));

    let first = service.spelling(spell_request("first", settings(&[Language::EnCa])));
    entered.wait_for(1);
    let abandoned = service.spelling(spell_request("abandoned", settings(&[Language::EnCa])));
    drop(abandoned);
    // Replacing a request whose receiver is gone tries to answer into nothing: not an error, and
    // not a reason for the worker to stop.
    let newest = service.spelling(spell_request("newest", settings(&[Language::EnCa])));

    gate.open();
    assert_eq!(reply(&first, SOON).error, None);
    assert_eq!(reply(&newest, SOON).error, None);
    assert_eq!(host.log(), vec!["scan:first".to_string(), "scan:newest".to_string()]);
}

// ---------------------------------------------------------------------------------------------
// Bounded shutdown: the window's thread is never held for an in-flight check.
// ---------------------------------------------------------------------------------------------

#[test]
fn dropping_the_service_is_bounded_while_a_grammar_check_is_on_the_wire() {
    let gate = Arc::new(Gate::default());
    let entered = Arc::new(Entered::default());
    let host = Arc::new(BlockingGrammar::new(Arc::clone(&gate), Arc::clone(&entered)));
    let service = Service::start(Config::testing(wake()).with_grammar(host.clone()).with_patience(Duration::from_secs(30)));

    let in_flight = service.grammar(grammar_request("one"));
    entered.wait_for(1);
    let waiting = service.grammar(grammar_request("two"));

    // Drop on another thread so that a blocking join cannot hang the whole test run; a drop that
    // takes longer than half a second is the failure being looked for.
    let (done_tx, done_rx) = mpsc::channel();
    let dropper = thread::spawn(move || {
        let started = Instant::now();
        drop(service);
        let _ = done_tx.send(started.elapsed());
    });
    match done_rx.recv_timeout(Duration::from_millis(500)) {
        Ok(elapsed) => assert!(elapsed < Duration::from_millis(500), "dropping took {elapsed:?}"),
        Err(_) => {
            gate.open();
            let _ = dropper.join();
            panic!("dropping the service waited for the grammar check on the wire");
        }
    }

    // The check on the wire is allowed to finish; the waiting request is not left unanswered.
    gate.open();
    assert_eq!(reply(&in_flight, SOON).error, None);
    assert_eq!(reply(&waiting, SOON).error, Some(Failure::Unavailable));
    let _ = dropper.join();
}

#[test]
fn dropping_the_service_is_bounded_while_a_scan_runs_and_still_answers_the_waiting_one() {
    let gate = Arc::new(Gate::default());
    let entered = Arc::new(Entered::default());
    let host = Arc::new(BlockingSpelling::new(Arc::clone(&gate), Arc::clone(&entered)));
    let service = Service::start(Config::testing(wake()).with_spelling(host.clone()));

    let in_flight = service.spelling(spell_request("one", settings(&[Language::EnCa])));
    entered.wait_for(1);
    let waiting = service.spelling(spell_request("two", settings(&[Language::EnCa])));

    let (done_tx, done_rx) = mpsc::channel();
    let dropper = thread::spawn(move || {
        let started = Instant::now();
        drop(service);
        let _ = done_tx.send(started.elapsed());
    });
    match done_rx.recv_timeout(Duration::from_millis(500)) {
        Ok(elapsed) => assert!(elapsed < Duration::from_millis(500), "dropping took {elapsed:?}"),
        Err(_) => {
            gate.open();
            let _ = dropper.join();
            panic!("dropping the service waited for the scan on the wire");
        }
    }

    gate.open();
    assert_eq!(reply(&in_flight, SOON).error, None);
    assert_eq!(reply(&waiting, SOON).error, Some(Failure::Unavailable));
    let _ = dropper.join();
}

// ---------------------------------------------------------------------------------------------
// Nothing private in a log line.
// ---------------------------------------------------------------------------------------------

#[test]
fn the_service_and_its_replies_keep_text_out_of_debug() {
    let service = Service::start(Config::testing(wake()).with_spelling(Arc::new(StubSpelling::default())));
    let shown = format!("{service:?}");
    assert!(shown.contains("ProofNativeService"), "{shown}");
    assert!(!shown.contains("SECRET"), "{shown}");

    let spelling = SpellingRequest { text: "SECRET-BODY".to_string(), settings: settings(&[Language::EnCa]), cancelled: never_cancelled() };
    let suggestion =
        SuggestionRequest { word: "SECRETWORD".to_string(), settings: settings(&[Language::EnCa]), limit: 8, cancelled: never_cancelled() };
    let mut grammar = grammar_request("SECRET-GRAMMAR");
    grammar.url = "http://127.0.0.1/SECRET-URL".to_string();
    grammar.ignored_rules = vec!["SECRET-RULE".to_string()];

    let mut all = String::new();
    let _ = writeln!(all, "{spelling:?} {suggestion:?} {grammar:?}");
    assert!(!all.contains("SECRET"), "a request's Debug must hold counts only: {all}");

    let diagnostic = Diagnostic::new(Span::of("SECRET", 0, 6).expect("span"), Kind::Spelling, Source::Local)
        .saying("SECRET-MESSAGE")
        .suggesting(["SECRET-SUGGESTION".to_string()]);
    let spelling_reply = SpellingReply { diagnostics: vec![diagnostic.clone()], note: Some("SECRET-NOTE".to_string()), error: Some(Failure::Failed) };
    let grammar_reply = GrammarReply { diagnostics: vec![diagnostic], checked_segments: 1, skipped_segments: 2, partial: true, error: None };
    let suggestion_reply = SuggestionReply { suggestions: vec!["SECRET-SUGGESTION".to_string()], error: None };

    let mut replies = String::new();
    let _ = writeln!(replies, "{spelling_reply:?} {grammar_reply:?} {suggestion_reply:?}");
    assert!(!replies.contains("SECRET"), "a reply's Debug must hold counts only: {replies}");
}

// ---------------------------------------------------------------------------------------------
// The real host built the production way.
// ---------------------------------------------------------------------------------------------

#[test]
fn the_native_factory_answers_and_wakes_the_window() {
    let (wakes, wake) = counted_wake();
    let service = crate::proof_native::service(wake);
    let got = reply(&service.grammar(grammar_request("   ")), SOON);
    assert_eq!(got.error, None);
    assert!(wakes.load(Ordering::Relaxed) >= 1, "the window is asked to look again after a reply");
}

#[test]
fn the_native_factory_scans_with_a_real_dictionary() {
    if !fixtures_note("the production factory with a real dictionary") {
        return;
    }
    let scratch = Scratch::new("factory");
    if !scratch.copy_fixtures() {
        println!("SKIP production factory: the en_CA fixtures could not be copied");
        return;
    }
    // Exactly what the window gets: the module's own factory, its own worker, the engine's own
    // dictionary reader, no host or spawner replaced.
    let service = crate::proof_native::service(wake());
    let settings = with_dir(&[Language::EnCa], scratch.path());
    let got = reply(&service.spelling(spell_request("This has a mistkae in it.", settings.clone())), REAL_WORK);
    assert_eq!(got.error, None);
    assert_eq!(got.diagnostics.len(), 1, "one misspelling in the sentence: {got:?}");
    assert!(got.diagnostics[0].suggestions.is_empty(), "a scan keeps places only");

    let suggestion = reply(&service.suggest(suggest_request("mistkae", settings)), REAL_WORK);
    assert_eq!(suggestion.error, None);
    assert!(suggestion.suggestions.iter().any(|s| s == "mistake"), "{:?}", suggestion.suggestions);
}

#[test]
fn local_grammar_is_a_grammar_host_and_reads_the_adapter() {
    // A compile-time and behavioural check that the production host is the multilingual adapter:
    // a remote address is refused by the adapter itself, not by a local copy of its rules.
    let host = LocalGrammar;
    let got = host.check("https://example.com", "text", &[Language::EnCa], &[], Duration::from_millis(100));
    assert_eq!(got.error, Some(Failure::Refused));
    let _: &dyn GrammarHost = &host;
    let _: &dyn SpellingHost = &LocalSpelling::new();
}
