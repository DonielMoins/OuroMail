// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! The asynchronous proofing lifecycle, driven by a fixed egui clock and a host of this test's
//! own. A finished request is not permission to check the same draft again, and an old answer is
//! never permission to mark another draft.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

use mailcraft_engine::Session;
use mailcraft_model::proof::{Diagnostic, Kind, Source, Span};
use serde_json::json;

use crate::proof::Body;
use crate::proof_service::{Failure, GrammarReply, GrammarRequest, ProofService, SpellingReply, SpellingRequest, SuggestionReply, SuggestionRequest};
use crate::{MailApp, Services};

const NOW: i64 = 1_791_993_600;

/// Every request the window made, with the sender that answers it. Kept apart by provider, so a
/// test can delay one without holding up the other.
#[derive(Default)]
struct Answers {
    spelling: std::sync::Mutex<Vec<(SpellingRequest, Sender<SpellingReply>)>>,
    grammar: std::sync::Mutex<Vec<(GrammarRequest, Sender<GrammarReply>)>>,
    suggestions: std::sync::Mutex<Vec<(SuggestionRequest, Sender<SuggestionReply>)>>,
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

fn count<T>(items: &std::sync::Mutex<Vec<T>>) -> usize {
    items.lock().expect("answers lock").len()
}

fn app() -> (MailApp, Arc<Answers>) {
    let answers = Arc::new(Answers::default());
    let services = Services { proof_native: Some(Arc::new(Fake { answers: Arc::clone(&answers) })), ..Services::default() };
    let mut app = MailApp::new(Session::at(mailcraft_engine::sample::mailbox(NOW, 0), NOW, 0), services);
    app.autosave = false;
    app.execute("mail.new", json!({})).expect("a new draft");
    let len = app.session.doc().expect("draft").0.text().len();
    app.execute("compose.select", json!({"from": 0, "to": len})).expect("select all");
    app.execute("compose.type", json!({"text": "She have sent the documents."})).expect("type");
    app.session.prefs.proof.check_spelling = false;
    app.session.prefs.proof.check_grammar = true;
    app.session.prefs.proof.grammar_url = "http://127.0.0.1:8081".into();
    (app, answers)
}

/// The body the window is proofing now (a signature editor takes precedence over the draft).
fn body(app: &MailApp) -> Body {
    crate::proof::body_of(app).expect("a document is open")
}

fn text(app: &MailApp) -> String {
    app.session.doc().expect("a document is open").0.plain()
}

fn frame(ctx: &egui::Context, app: &mut MailApp, seconds: f64) {
    let input = egui::RawInput { time: Some(seconds), ..Default::default() };
    let mut output = ctx.run_ui(input, |_| {
        let body = body(app);
        let text = text(app);
        crate::proof::run(app, ctx, body, &text);
    });
    output.textures_delta.clear();
}

fn respond_grammar(answers: &Answers, index: usize, error: Option<Failure>) {
    let (text, tx) = {
        let requests = answers.grammar.lock().expect("answers lock");
        let (request, tx) = requests.get(index).expect("the request is on record");
        (request.text.clone(), tx.clone())
    };
    let diagnostic = Diagnostic::new(Span::of(&text, 4, 8).expect("a span in the draft"), Kind::Grammar, Source::Server)
        .saying("The verb does not agree.")
        .suggesting(["has".to_string()]);
    tx.send(GrammarReply { diagnostics: vec![diagnostic], error, ..Default::default() }).expect("the draft is still waiting for the grammar answer");
}

fn respond_spelling(answers: &Answers, index: usize, error: Option<Failure>) {
    let (text, tx) = {
        let requests = answers.spelling.lock().expect("answers lock");
        let (request, tx) = requests.get(index).expect("the request is on record");
        (request.text.clone(), tx.clone())
    };
    let diagnostic = Diagnostic::new(Span::of(&text, 4, 8).expect("a span in the draft"), Kind::Spelling, Source::Local);
    tx.send(SpellingReply { diagnostics: vec![diagnostic], error, ..Default::default() })
        .expect("the draft is still waiting for the spelling answer");
}

fn places(app: &MailApp) -> Vec<crate::proof::Place> {
    app.proof.places(body(app), &text(app))
}

#[test]
fn an_unchanged_draft_is_checked_once_not_once_per_frame() {
    let (mut app, answers) = app();
    let ctx = egui::Context::default();
    frame(&ctx, &mut app, 1.0);
    frame(&ctx, &mut app, 1.4);
    assert_eq!(count(&answers.grammar), 1);
    assert_eq!(count(&answers.spelling), 0, "spelling is turned off");
    respond_grammar(&answers, 0, None);
    frame(&ctx, &mut app, 1.5);
    frame(&ctx, &mut app, 3.0);
    frame(&ctx, &mut app, 5.0);
    assert_eq!(count(&answers.grammar), 1, "a completed check is not another edit");
    assert_eq!(places(&app).len(), 1);
}

#[test]
fn an_unavailable_server_is_not_retried_in_a_loop() {
    let (mut app, answers) = app();
    let ctx = egui::Context::default();
    frame(&ctx, &mut app, 1.0);
    frame(&ctx, &mut app, 1.4);
    respond_grammar(&answers, 0, Some(Failure::Unavailable));
    frame(&ctx, &mut app, 1.5);
    frame(&ctx, &mut app, 3.0);
    frame(&ctx, &mut app, 9.0);
    assert_eq!(count(&answers.grammar), 1, "retry requires an edit or an explicit request");
    assert!(places(&app).is_empty(), "a failed answer supplies no diagnostics");
    assert!(app.proof.note(body(&app)).is_some(), "a failed answer is said, not shown as a clean text");
}

#[test]
fn turning_live_checks_off_sends_no_draft() {
    let (mut app, answers) = app();
    app.session.prefs.proof.as_you_type = false;
    let ctx = egui::Context::default();
    frame(&ctx, &mut app, 1.0);
    frame(&ctx, &mut app, 2.0);
    assert_eq!(count(&answers.grammar), 0);
    assert_eq!(count(&answers.spelling), 0);
}

#[test]
fn disabling_grammar_discards_the_answer_already_in_flight() {
    let (mut app, answers) = app();
    let ctx = egui::Context::default();
    frame(&ctx, &mut app, 1.0);
    frame(&ctx, &mut app, 1.4);
    let old_text = text(&app);
    // Turn grammar off, and spelling on, while the grammar answer is still on its way.
    app.session.prefs.proof.check_grammar = false;
    app.session.prefs.proof.check_spelling = true;
    app.session.prefs.proof.dictionary_dir = "/no/such/proofing/dictionaries".into();
    frame(&ctx, &mut app, 1.5);
    frame(&ctx, &mut app, 1.8);
    assert_eq!(count(&answers.spelling), 1, "the provider turned on is asked");
    respond_grammar(&answers, 0, None);
    respond_spelling(&answers, 0, None);
    frame(&ctx, &mut app, 1.9);
    let found = places(&app);
    assert_eq!(found.len(), 1, "the old grammar answer for {:?} is dropped whole", old_text);
    assert!(found.iter().all(|p| p.kind == Kind::Spelling), "grammar turned off keeps no grammar place");
}

#[test]
fn grammar_is_not_erased_by_the_next_spelling_pass() {
    let (mut app, answers) = app();
    app.session.prefs.proof.check_spelling = true;
    app.session.prefs.proof.dictionary_dir = "/no/such/proofing/dictionaries".into();
    let ctx = egui::Context::default();
    frame(&ctx, &mut app, 1.0);
    frame(&ctx, &mut app, 1.4);
    assert_eq!(count(&answers.spelling), 1);
    assert_eq!(count(&answers.grammar), 1);
    respond_grammar(&answers, 0, None);
    respond_spelling(&answers, 0, None);
    frame(&ctx, &mut app, 1.5);
    frame(&ctx, &mut app, 1.6);
    let found = places(&app);
    assert!(found.iter().any(|p| p.kind == Kind::Grammar), "providers keep independent results");
    assert!(found.iter().any(|p| p.kind == Kind::Spelling), "the spelling answer is kept too");
}

#[test]
fn a_reply_for_an_identical_text_in_a_different_draft_is_stale() {
    let (mut app, answers) = app();
    let ctx = egui::Context::default();
    frame(&ctx, &mut app, 1.0);
    frame(&ctx, &mut app, 1.4);
    let old_id = app.session.compose_id();
    let old_text = text(&app);
    app.execute("mail.new", json!({})).expect("another draft");
    let len = app.session.doc().expect("draft").0.text().len();
    app.execute("compose.select", json!({"from": 0, "to": len})).expect("select all");
    app.execute("compose.type", json!({"text": old_text})).expect("type the same words");
    assert_ne!(app.session.compose_id(), old_id);
    respond_grammar(&answers, 0, None);
    frame(&ctx, &mut app, 1.5);
    assert!(places(&app).is_empty(), "document identity is part of a check");
    assert_eq!(count(&answers.grammar), 1, "the reopened draft is not asked before it rests");
}

#[test]
fn a_reply_for_a_reopened_signature_is_stale() {
    let (mut app, answers) = app();
    let ctx = egui::Context::default();
    app.execute("signature.open", json!({"name": "Standard"})).expect("open the signature");
    let old_id = app.session.signature_id().expect("signature editor id");
    let old_text = text(&app);
    frame(&ctx, &mut app, 1.0);
    frame(&ctx, &mut app, 1.4);
    assert_eq!(count(&answers.grammar), 1);
    app.execute("signature.close", json!({})).expect("close the signature");
    app.execute("signature.open", json!({"name": "Standard"})).expect("open the same signature again");
    let new_id = app.session.signature_id().expect("a new signature editor id");
    assert_ne!(new_id, old_id, "a reopened signature is another editor");
    assert_eq!(text(&app), old_text, "the same words are still another document");
    respond_grammar(&answers, 0, None);
    frame(&ctx, &mut app, 1.5);
    assert!(places(&app).is_empty(), "an answer for the closed editor is nobody's");
    assert_eq!(count(&answers.grammar), 1, "the reopened editor is not asked before it rests");
}

#[test]
fn a_manual_review_checks_once_with_as_you_type_off() {
    let (mut app, answers) = app();
    app.session.prefs.proof.as_you_type = false;
    let ctx = egui::Context::default();
    frame(&ctx, &mut app, 1.0);
    frame(&ctx, &mut app, 1.2);
    assert_eq!(count(&answers.grammar), 0, "nothing is asked while the owner types");
    app.proof.recheck(body(&app));
    frame(&ctx, &mut app, 1.3);
    assert_eq!(count(&answers.grammar), 1, "a manual review asks the provider once");
    respond_grammar(&answers, 0, None);
    frame(&ctx, &mut app, 1.4);
    frame(&ctx, &mut app, 2.0);
    assert_eq!(count(&answers.grammar), 1, "the answered review is not repeated per frame");
    assert_eq!(places(&app).len(), 1);
}
