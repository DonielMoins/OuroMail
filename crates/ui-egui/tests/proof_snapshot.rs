// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! The passive proofing snapshot of `ui.inspect`, and the language metadata of a place as the
//! review dialog converts it. Written before the snapshot field existed: every `proofing`
//! assertion below is a behavioural RED on the baseline (the field is absent, never a compile
//! error), and the language assertions read the walk's already-existing `Diagnostic::language`
//! rather than a `Place` field that does not exist yet.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use egui_kittest::Harness;
use mailcraft_engine::Session;
use mailcraft_model::proof::{Diagnostic, Kind, Language, Source, Span};
use mailcraft_ui_egui::proof_server::{Action, Issue, ManagedServer, Phase, Status};
use mailcraft_ui_egui::proof_service::{
    Failure, GrammarReply, GrammarRequest, ProofService, SpellingReply, SpellingRequest, SuggestionReply, SuggestionRequest,
};
use mailcraft_ui_egui::{MailApp, Services, control};
use serde_json::{Value, json};

/// Wednesday 2026-10-14 16:00 UTC, the sample mailbox's clock.
const NOW: i64 = 1_791_993_600;

const URL: &str = "http://127.0.0.1:8081";

type Window = Harness<'static, MailApp>;

/// Every request the window made, with the sender that answers it.
#[derive(Default)]
struct Answers {
    spelling: Mutex<Vec<(SpellingRequest, Sender<SpellingReply>)>>,
    grammar: Mutex<Vec<(GrammarRequest, Sender<GrammarReply>)>>,
    suggestions: Mutex<Vec<(SuggestionRequest, Sender<SuggestionReply>)>>,
}

struct Fake {
    answers: Arc<Answers>,
}

impl ProofService for Fake {
    fn spelling(&self, request: SpellingRequest) -> Receiver<SpellingReply> {
        let (tx, rx) = channel();
        self.answers.spelling.lock().expect("answers lock").push((request, tx));
        rx
    }
    fn grammar(&self, request: GrammarRequest) -> Receiver<GrammarReply> {
        let (tx, rx) = channel();
        self.answers.grammar.lock().expect("answers lock").push((request, tx));
        rx
    }
    fn suggest(&self, request: SuggestionRequest) -> Receiver<SuggestionReply> {
        let (tx, rx) = channel();
        self.answers.suggestions.lock().expect("answers lock").push((request, tx));
        rx
    }
}

fn service() -> (Arc<Answers>, Services) {
    let answers = Arc::new(Answers::default());
    let services = Services { proof_native: Some(Arc::new(Fake { answers: Arc::clone(&answers) })), ..Services::default() };
    (answers, services)
}

fn new_app(services: Services) -> MailApp {
    let mut app = MailApp::new(Session::at(mailcraft_engine::sample::mailbox(NOW, 0), NOW, 0), services);
    app.autosave = false;
    app.session.driver.set_auto_sync(false);
    app
}

/// Grammar on, at a loopback address; spelling as the test says. No frame is drawn yet.
fn grammar_prefs(app: &mut MailApp, spelling: bool) {
    let proof = &mut app.session.prefs.proof;
    proof.check_spelling = spelling;
    proof.check_grammar = true;
    proof.as_you_type = true;
    proof.languages = vec![Language::EnCa];
    proof.grammar_url = URL.into();
    proof.managed_server = false;
}

fn open_window(app: MailApp) -> Window {
    let mut window = Harness::builder()
        .with_accessibility_check(false)
        .allow_missing_glyphs()
        .with_size(egui::vec2(1280.0, 900.0))
        .with_step_dt(1.0 / 60.0)
        .build_ui_state(
            |ui, app: &mut MailApp| {
                let ctx = ui.ctx().clone();
                app.logic(&ctx);
                app.ui(ui);
            },
            app,
        );
    window.run_steps(2);
    window
}

fn inspected(window: &mut Window) -> Value {
    let ctx = window.ctx.clone();
    control::inspect(window.state_mut(), &ctx)
}

fn proofing(window: &mut Window) -> Value {
    let value = inspected(window);
    let field = value["proofing"].clone();
    assert!(field.is_object(), "ui.inspect has a proofing object while an editor is open: {field}");
    field
}

fn grammar_count(answers: &Answers) -> usize {
    answers.grammar.lock().expect("answers lock").len()
}

fn spelling_count(answers: &Answers) -> usize {
    answers.spelling.lock().expect("answers lock").len()
}

/// A new message being written, with `text` in it; its id.
fn new_draft(window: &mut Window, text: &str) -> u64 {
    let answer = window.state_mut().execute("mail.new", json!({})).expect("a new message");
    let id = answer["compose"].as_u64().expect("the new message's id");
    window.state_mut().execute("compose.set", json!({ "compose": id, "body": text })).expect("its text");
    window.run_steps(1);
    id
}

/// The text the window last asked the grammar server to check.
fn asked_text(answers: &Answers, index: usize) -> String {
    answers.grammar.lock().expect("answers lock").get(index).expect("the request is on record").0.text.clone()
}

fn place(text: &str, word: &str, kind: Kind, source: Source, language: Option<Language>) -> Diagnostic {
    let start = text.find(word).expect("the word is in the text");
    let mut diagnostic = Diagnostic::new(Span::of(text, start, start + word.len()).expect("a span"), kind, source)
        .saying("A synthetic message.")
        .suggesting([word.to_string()]);
    diagnostic.language = language;
    diagnostic
}

fn respond_grammar(answers: &Answers, index: usize, reply: GrammarReply) {
    let sent = {
        let requests = answers.grammar.lock().expect("answers lock");
        let (_, tx) = requests.get(index).expect("the request is on record");
        tx.send(reply)
    };
    sent.expect("the window is still waiting for its grammar answer");
}

fn respond_spelling(answers: &Answers, index: usize, reply: SpellingReply) {
    let sent = {
        let requests = answers.spelling.lock().expect("answers lock");
        let (_, tx) = requests.get(index).expect("the request is on record");
        tx.send(reply)
    };
    sent.expect("the window is still waiting for its spelling answer");
}

fn drop_grammar_answer(answers: &Answers, index: usize) {
    answers.grammar.lock().expect("answers lock").remove(index);
}

#[test]
fn no_editor_means_a_null_proofing_field() {
    let mut app = new_app(Services::default());
    let ctx = egui::Context::default();
    let value = control::inspect(&mut app, &ctx);
    assert!(value["proofing"].is_null(), "no editable body is open: {}", value["proofing"]);
}

#[test]
fn an_unobserved_editor_is_not_current_and_inspection_stays_passive() {
    let (answers, services) = service();
    let mut app = new_app(services);
    grammar_prefs(&mut app, false);
    app.execute("mail.new", json!({})).expect("a new message");
    let ctx = egui::Context::default();
    let first = control::inspect(&mut app, &ctx)["proofing"].clone();
    assert!(first.is_object(), "a body is open but not drawn yet: {first}");
    assert_eq!(first["body"]["kind"], "compose");
    assert_eq!(first["current"], false, "an editor no frame has observed is not current");
    assert!(first["revision"].is_null(), "no cached document has a revision");
    assert_eq!(first["spelling"]["done"], false);
    assert!(first["spelling"]["places"].is_null());
    assert!(first["spelling"]["hasNote"].is_null());
    assert_eq!(first["grammar"]["done"], false);
    assert!(first["grammar"]["places"].is_null());
    assert!(first["grammar"]["online"].is_null());
    assert!(first["grammar"]["checkedSegments"].is_null());
    assert_eq!(first["requests"]["spelling"], 0);
    assert_eq!(first["requests"]["grammar"], 0);

    for _ in 0..4 {
        let again = control::inspect(&mut app, &ctx)["proofing"].clone();
        assert_eq!(again, first, "repeated inspection reports the same cached state");
    }
    assert_eq!(grammar_count(&answers), 0, "inspection never asks the provider");
    assert_eq!(spelling_count(&answers), 0);
}

#[test]
fn a_frame_taken_reply_makes_the_editor_current_with_exact_counters() {
    let (answers, services) = service();
    let mut app = new_app(services);
    grammar_prefs(&mut app, true);
    let mut window = open_window(app);
    let id = new_draft(&mut window, "She have sent the documents.");

    window.run_steps(30);
    assert_eq!(grammar_count(&answers), 1, "the grammar request is on record");
    assert_eq!(spelling_count(&answers), 1, "the spelling request is on record");
    let text = asked_text(&answers, 0);
    let mut grammar = place(&text, "have", Kind::Grammar, Source::Server, Some(Language::EnUs));
    grammar.suggestions = vec!["has".to_string()];
    respond_grammar(&answers, 0, GrammarReply { diagnostics: vec![grammar], checked_segments: 3, skipped_segments: 0, partial: false, error: None });
    let spelling = place(&text, "have", Kind::Spelling, Source::Local, None);
    respond_spelling(&answers, 0, SpellingReply { diagnostics: vec![spelling], note: None, error: None });
    window.run_steps(2);

    let field = proofing(&mut window);
    assert_eq!(field["body"]["kind"], "compose");
    assert_eq!(field["body"]["id"], id);
    assert_eq!(field["current"], true, "the frame took the answers for exactly this text");
    assert!(field["revision"].as_u64().is_some(), "a cached document has a revision");
    assert_eq!(field["languages"], json!(["en-CA"]));
    assert_eq!(field["asYouType"], true);
    assert_eq!(field["available"], true);
    assert_eq!(field["spelling"]["enabled"], true);
    assert_eq!(field["spelling"]["done"], true);
    assert_eq!(field["spelling"]["pending"], false);
    assert_eq!(field["spelling"]["places"], 1);
    assert_eq!(field["spelling"]["hasNote"], false);
    assert_eq!(field["spelling"]["capped"], false);
    assert_eq!(field["grammar"]["enabled"], true);
    assert_eq!(field["grammar"]["done"], true);
    assert_eq!(field["grammar"]["pending"], false);
    assert_eq!(field["grammar"]["places"], 1);
    assert_eq!(field["grammar"]["hasNote"], false);
    assert_eq!(field["grammar"]["capped"], false);
    assert_eq!(field["grammar"]["online"], true);
    assert_eq!(field["grammar"]["checkedSegments"], 3);
    assert_eq!(field["grammar"]["skippedSegments"], 0);
    assert_eq!(field["grammar"]["partial"], false);
    assert!(field["requests"]["spelling"].as_u64().is_some_and(|n| n >= 1));
    assert!(field["requests"]["grammar"].as_u64().is_some_and(|n| n >= 1));

    let before = grammar_count(&answers);
    let second = proofing(&mut window);
    assert!(second["requests"]["grammar"].as_u64().unwrap() >= field["requests"]["grammar"].as_u64().unwrap(), "operations are monotonic");
    assert_eq!(grammar_count(&answers), before, "inspection starts no operation");
    assert_eq!(second["current"], true);
}

#[test]
fn a_text_change_before_a_frame_is_not_current() {
    let (answers, services) = service();
    let mut app = new_app(services);
    grammar_prefs(&mut app, false);
    let mut window = open_window(app);
    let id = new_draft(&mut window, "She have sent the documents.");
    window.run_steps(30);
    let text = asked_text(&answers, 0);
    respond_grammar(&answers, 0, GrammarReply { diagnostics: vec![place(&text, "have", Kind::Grammar, Source::Server, None)], ..Default::default() });
    window.run_steps(2);
    assert_eq!(proofing(&mut window)["current"], true);
    let operations = grammar_count(&answers);

    window.state_mut().execute("compose.set", json!({ "compose": id, "body": "She has sent the documents." })).expect("new text");
    let field = proofing(&mut window);
    assert_eq!(field["current"], false, "the cached document is another text");
    assert_eq!(field["grammar"]["done"], false);
    assert!(field["grammar"]["places"].is_null());
    assert!(field["grammar"]["checkedSegments"].is_null());
    assert!(field["revision"].as_u64().is_some(), "the cached revision is still reported");
    assert_eq!(grammar_count(&answers), operations, "inspection changes nothing");
}

#[test]
fn a_same_text_effective_settings_change_is_not_current() {
    let (answers, services) = service();
    let mut app = new_app(services);
    grammar_prefs(&mut app, false);
    let mut window = open_window(app);
    let id = new_draft(&mut window, "She have sent the documents.");
    window.run_steps(30);
    let text = asked_text(&answers, 0);
    respond_grammar(&answers, 0, GrammarReply { diagnostics: vec![place(&text, "have", Kind::Grammar, Source::Server, None)], ..Default::default() });
    window.run_steps(2);
    assert_eq!(proofing(&mut window)["current"], true);

    // The same words, now proofed under another language: the cached check is of the old keys.
    window
        .state_mut()
        .execute("compose.set", json!({ "compose": id, "body": "She have sent the documents.", "proofLanguages": ["fr-FR"] }))
        .expect("another language");
    let field = proofing(&mut window);
    assert_eq!(field["current"], false);
    assert_eq!(field["languages"], json!(["fr-FR"]), "the live effective language is still reported");
    assert_eq!(field["grammar"]["done"], false);
    assert!(field["grammar"]["checkedSegments"].is_null());
}

struct EpochServer {
    epoch: AtomicU64,
}

impl ManagedServer for EpochServer {
    fn status(&self) -> Status {
        Status {
            phase: Phase::Running,
            endpoint: Some("http://127.0.0.1:18081".into()),
            epoch: self.epoch.load(Ordering::Relaxed),
            ..Status::default()
        }
    }
    fn request(&self, _: Action) -> Result<(), Issue> {
        Ok(())
    }
}

#[test]
fn a_managed_epoch_change_is_not_current_even_with_the_same_text() {
    let (answers, services) = service();
    let server = Arc::new(EpochServer { epoch: AtomicU64::new(1) });
    let services = Services { proof_server: Some(server.clone()), ..services };
    let mut app = new_app(services);
    grammar_prefs(&mut app, false);
    app.session.prefs.proof.managed_server = true;
    let mut window = open_window(app);
    new_draft(&mut window, "She have sent the documents.");
    window.run_steps(30);
    assert_eq!(grammar_count(&answers), 1);
    let text = asked_text(&answers, 0);
    respond_grammar(&answers, 0, GrammarReply { diagnostics: vec![place(&text, "have", Kind::Grammar, Source::Server, None)], ..Default::default() });
    window.run_steps(2);
    assert_eq!(proofing(&mut window)["current"], true);

    server.epoch.store(2, Ordering::Relaxed);
    let field = proofing(&mut window);
    assert_eq!(field["current"], false, "another running instance is another grammar key");
    assert_eq!(field["grammar"]["done"], false);
    assert!(field["grammar"]["checkedSegments"].is_null());
}

#[test]
fn inspecting_never_consumes_a_queued_reply() {
    let (answers, services) = service();
    let mut app = new_app(services);
    grammar_prefs(&mut app, false);
    let mut window = open_window(app);
    new_draft(&mut window, "She have sent the documents.");
    window.run_steps(30);
    assert_eq!(grammar_count(&answers), 1);
    let text = asked_text(&answers, 0);
    respond_grammar(
        &answers,
        0,
        GrammarReply { diagnostics: vec![place(&text, "have", Kind::Grammar, Source::Server, None)], checked_segments: 2, ..Default::default() },
    );

    // The answer is queued; no frame has taken it yet.
    for _ in 0..3 {
        let field = proofing(&mut window);
        assert_eq!(field["current"], true, "the request is for the live revision");
        assert_eq!(field["grammar"]["pending"], true);
        assert_eq!(field["grammar"]["done"], false, "inspection did not take the reply");
        assert!(field["grammar"]["checkedSegments"].is_null());
    }
    assert_eq!(grammar_count(&answers), 1, "inspection starts no new operation");
    window.run_steps(1);
    let field = proofing(&mut window);
    assert_eq!(field["grammar"]["done"], true, "the frame takes the queued reply");
    assert_eq!(field["grammar"]["pending"], false);
    assert_eq!(field["grammar"]["checkedSegments"], 2);
}

#[test]
fn grammar_error_lost_absent_and_empty_never_claim_a_checked_batch() {
    // A failed answer.
    let (answers, services) = service();
    let mut app = new_app(services);
    grammar_prefs(&mut app, false);
    let mut window = open_window(app);
    new_draft(&mut window, "She have sent the documents.");
    window.run_steps(30);
    respond_grammar(&answers, 0, GrammarReply { error: Some(Failure::Failed), ..Default::default() });
    window.run_steps(2);
    let failed = proofing(&mut window);
    assert_eq!(failed["grammar"]["done"], true);
    assert_eq!(failed["grammar"]["online"], false);
    assert_eq!(failed["grammar"]["hasNote"], true, "a failure is said, not shown as clean");
    assert!(failed["grammar"]["checkedSegments"].is_null());
    assert!(failed["grammar"]["skippedSegments"].is_null());
    assert!(failed["grammar"]["partial"].is_null());

    // A lost answer.
    let (answers, services) = service();
    let mut app = new_app(services);
    grammar_prefs(&mut app, false);
    let mut window = open_window(app);
    new_draft(&mut window, "She have sent the documents.");
    window.run_steps(30);
    drop_grammar_answer(&answers, 0);
    window.run_steps(2);
    let lost = proofing(&mut window);
    assert_eq!(lost["grammar"]["done"], true);
    assert_eq!(lost["grammar"]["online"], false);
    assert!(lost["grammar"]["checkedSegments"].is_null());

    // No host at all.
    let mut app = new_app(Services::default());
    grammar_prefs(&mut app, false);
    let mut window = open_window(app);
    new_draft(&mut window, "She have sent the documents.");
    window.run_steps(30);
    let absent = proofing(&mut window);
    assert_eq!(absent["available"], false);
    assert_eq!(absent["grammar"]["done"], true);
    assert_eq!(absent["grammar"]["online"], false);
    assert_eq!(absent["grammar"]["hasNote"], true);
    assert!(absent["grammar"]["checkedSegments"].is_null());

    // An empty body is not a checked grammar batch.
    let (answers, services) = service();
    let mut app = new_app(services);
    grammar_prefs(&mut app, false);
    let mut window = open_window(app);
    new_draft(&mut window, "");
    assert!(
        window.state().session.composing_by_id(window.state().session.compose_id().expect("a draft")).expect("the draft").body.plain().is_empty()
    );
    window.run_steps(30);
    let empty = proofing(&mut window);
    assert_eq!(empty["current"], true);
    assert_eq!(grammar_count(&answers), 0, "an empty text asks no server");
    assert_eq!(empty["grammar"]["done"], true);
    assert!(empty["grammar"]["online"].is_null());
    assert!(empty["grammar"]["checkedSegments"].is_null());
}

#[test]
fn a_reopened_signature_does_not_inherit_a_late_reply() {
    let (answers, services) = service();
    let mut app = new_app(services);
    grammar_prefs(&mut app, false);
    let mut window = open_window(app);
    window.state_mut().execute("ui.backstage", json!({ "value": true, "page": "signatures" })).expect("File › Signatures");
    window.run_steps(30);
    assert_eq!(grammar_count(&answers), 1, "the signature editor is proofed");
    let first = proofing(&mut window);
    assert_eq!(first["body"]["kind"], "signature");
    let old_id = first["body"]["id"].as_u64().expect("a signature editor id");
    let text = asked_text(&answers, 0);
    respond_grammar(
        &answers,
        0,
        GrammarReply { diagnostics: vec![place(&text, "Avery", Kind::Grammar, Source::Server, None)], ..Default::default() },
    );
    window.run_steps(2);
    assert_eq!(proofing(&mut window)["current"], true);

    // Another grammar key asks again for the same editor; the answer is left on its way.
    window.state_mut().session.prefs.proof.languages = vec![Language::FrFr];
    window.run_steps(70);
    assert_eq!(grammar_count(&answers), 2, "the signature asked again under the new key");

    let reopened = {
        window.state_mut().execute("signature.close", json!({ "discard": true })).expect("close");
        window.state_mut().execute("signature.open", json!({ "name": "Standard" })).expect("open again");
        window.state().session.signature_id().expect("a new editor")
    };
    assert_ne!(reopened, old_id, "reopening is a new editor instance");
    respond_grammar(
        &answers,
        1,
        GrammarReply { diagnostics: vec![place(&text, "Avery", Kind::Grammar, Source::Server, None)], checked_segments: 5, ..Default::default() },
    );
    window.run_steps(2);

    let field = proofing(&mut window);
    assert_eq!(field["body"]["kind"], "signature");
    assert_eq!(field["body"]["id"], reopened);
    assert_eq!(field["current"], true, "the reopened editor has its own text");
    assert_eq!(field["grammar"]["done"], false, "the old editor's late reply is not this one's");
    assert!(field["grammar"]["checkedSegments"].is_null());
    assert_eq!(field["grammar"]["places"], 0);
}

/// Runs one review-dialog conversion and returns the language and byte span of the first place
/// the dialog's walk holds, or `None` when the walk is empty.
fn review_language(text: &str, word: &str, language: Option<Language>) -> (Option<Language>, Option<(usize, usize)>) {
    let (answers, services) = service();
    let mut app = new_app(services);
    grammar_prefs(&mut app, false);
    let mut window = open_window(app);
    new_draft(&mut window, text);
    window.run_steps(30);
    let checked = asked_text(&answers, 0);
    respond_grammar(
        &answers,
        0,
        GrammarReply { diagnostics: vec![place(&checked, word, Kind::Grammar, Source::Server, language)], ..Default::default() },
    );
    window.run_steps(2);
    assert_eq!(proofing(&mut window)["grammar"]["done"], true);

    // The real dialog: it reads the window's places, serializes them into `review.spelling`, and
    // the engine turns that answer back into the walk this test reads.
    let opened = window.state_mut().run("review.spelling", json!({})).expect("Review › Spelling & Grammar");
    assert_eq!(opened["pending"], "spelling", "the user-action path opens the real dialog, not an engine-only walk");
    window.run_steps(3);
    // Opening Review deliberately asks again. Its automatic-check answer is not a dialog
    // answer: while the new request is pending there is no walk to carry any language yet.
    assert_eq!(grammar_count(&answers), 2, "the dialog asks for its own fresh check");
    assert_eq!(proofing(&mut window)["grammar"]["pending"], true);
    assert!(window.state().session.proof.walk.is_none(), "the dialog waits for its fresh answer");
    let checked = asked_text(&answers, 1);
    assert_eq!(checked, text, "the fresh check is of the same synthetic document");
    respond_grammar(
        &answers,
        1,
        GrammarReply { diagnostics: vec![place(&checked, word, Kind::Grammar, Source::Server, language)], ..Default::default() },
    );
    window.run_steps(3);
    let walk = window.state().session.proof.walk.clone().expect("the answered dialog starts its walk");
    let diagnostic = walk.current().expect("the synthetic grammar place reached the walk");
    (diagnostic.language, Some((diagnostic.span.start, diagnostic.span.end)))
}

#[test]
fn french_and_english_language_metadata_survive_the_review_dialog() {
    // Accents before the place: a byte offset is not a character offset here.
    let french = "Déjà vu : Les document sont important.";
    let (language, span) = review_language(french, "document", Some(Language::FrFr));
    assert_eq!(language, Some(Language::FrFr), "French language metadata survives the dialog");
    let (start, end) = span.expect("the French place reached the walk");
    assert_eq!(french.get(start..end), Some("document"), "the accented bytes still map to the word");

    let english = "The colour of the centre is correct.";
    let (language, span) = review_language(english, "colour", Some(Language::EnUs));
    assert_eq!(language, Some(Language::EnUs), "English language metadata survives the dialog");
    let (start, end) = span.expect("the English place reached the walk");
    assert_eq!(english.get(start..end), Some("colour"));

    let (language, _) = review_language(english, "colour", None);
    assert_eq!(language, None, "a place that named no language keeps none");
}

#[test]
fn the_serialized_snapshot_and_debug_never_carry_private_canaries() {
    let body_canary = "synthetic-draft-canary-954";
    let endpoint_canary = "synthetic-endpoint-canary-954";
    let place_canary = "synthetic-place-canary-954";
    let (answers, services) = service();
    let mut app = new_app(services);
    grammar_prefs(&mut app, false);
    app.session.prefs.proof.grammar_url = format!("{URL}/{endpoint_canary}");
    let mut window = open_window(app);
    new_draft(&mut window, body_canary);
    window.run_steps(30);
    let text = asked_text(&answers, 0);
    let mut diagnostic = place(&text, body_canary, Kind::Grammar, Source::Server, Some(Language::FrCa));
    diagnostic.message = place_canary.into();
    diagnostic.suggestions = vec![place_canary.into()];
    diagnostic.rule = Some(place_canary.into());
    respond_grammar(&answers, 0, GrammarReply { diagnostics: vec![diagnostic], ..Default::default() });
    window.run_steps(2);

    let field = proofing(&mut window);
    let serialized = serde_json::to_string(&field).expect("the snapshot serializes");
    let debug = format!("{field:?}");
    for canary in [body_canary, endpoint_canary, place_canary] {
        assert!(!serialized.contains(canary), "the proofing snapshot leaked {canary}");
        assert!(!debug.contains(canary), "the proofing snapshot's Debug leaked {canary}");
    }
}
