// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! The proofing notes and place captions a window says (#954), in English and in French.
//!
//! These are semantic checks, not screenshots: the window is put in French through its own
//! `ui.language`, the note is read from the text the frame actually painted, and the catalogue is
//! checked directly. No dictionary is read, no server is contacted and no mailbox is written: the
//! host's two services are fakes whose cached answers are fixed, and drawing asks them for
//! nothing.
//!
//! Two things are separate here and must read separately:
//! - a **catalogue** entry missing is a missing translation (the official extractor and the
//!   reviewed French fills), never a widget-routing bug;
//! - a **painted** note in the wrong language is a routing bug in the window, whatever the
//!   catalogue holds.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use egui_kittest::Harness;
use mailcraft_engine::Session;
use mailcraft_model::proof::{Diagnostic, Kind, Language, Source, Span};
use mailcraft_ui_egui::i18n::{Lang, set_current};
use mailcraft_ui_egui::proof_server::{Action, Issue, ManagedServer, Phase, Status};
use mailcraft_ui_egui::proof_service::{
    Failure, GrammarReply, GrammarRequest, ProofService, SpellingReply, SpellingRequest, SuggestionReply, SuggestionRequest,
};
use mailcraft_ui_egui::{MailApp, Services, control};
use serde_json::{Value, json};

/// Wednesday 2026-10-14 16:00 UTC, the sample mailbox's clock.
const NOW: i64 = 1_791_993_600;

/// A loopback grammar address: the only kind File › Options may speak to.
const URL: &str = "http://127.0.0.1:8081";

/// A synthetic message, long enough that a frame checks it, in words this window invented.
const BODY: &str = "The team have sent the documents.";

/// A word of [`BODY`], for the place a provider answers with.
const WORD: &str = "have";

/// `This build cannot run the built-in grammar server.`, and its reviewed French.
const CANNOT_RUN_SERVER: (&str, &str) =
    ("This build cannot run the built-in grammar server.", "Cette version ne peut pas exécuter le serveur de grammaire intégré.");

/// `The built-in grammar server is not running.`, and its reviewed French.
const SERVER_NOT_RUNNING: (&str, &str) =
    ("The built-in grammar server is not running.", "Le serveur de grammaire intégré n’est pas en cours d’exécution.");

/// `The built-in grammar server's address is not one this computer may use.`, and its French.
const SERVER_ADDRESS_UNUSABLE: (&str, &str) = (
    "The built-in grammar server's address is not one this computer may use.",
    "L’adresse du serveur de grammaire intégré n’est pas une adresse que cet ordinateur peut utiliser.",
);

/// `That server is on another computer, and File › Options does not allow one.`, and its French.
const SERVER_ELSEWHERE: (&str, &str) = (
    "That server is on another computer, and File › Options does not allow one.",
    "Ce serveur se trouve sur un autre ordinateur, et Fichier › Options n’en autorise aucun.",
);

/// `Name the grammar server's address first.`, and its French.
const NAME_ADDRESS_FIRST: (&str, &str) = ("Name the grammar server's address first.", "Indiquez d’abord l’adresse du serveur de grammaire.");

/// `Spelling is checked by the host, and this build has none to ask.`, and its French.
const NO_SPELLING_SERVICE: (&str, &str) = (
    "Spelling is checked by the host, and this build has none to ask.",
    "L’orthographe est vérifiée par l’hôte, et cette version n’a aucun service à interroger.",
);

/// `Grammar is checked by a LanguageTool server; this build has none to ask.`, and its French.
const NO_GRAMMAR_SERVICE: (&str, &str) = (
    "Grammar is checked by a LanguageTool server; this build has none to ask.",
    "La grammaire est vérifiée par un serveur LanguageTool ; cette version n’en a aucun à interroger.",
);

/// The seven notes the window writes itself: every one has a French catalogue entry.
const NOTES: [(&str, &str); 7] =
    [CANNOT_RUN_SERVER, SERVER_NOT_RUNNING, SERVER_ADDRESS_UNUSABLE, SERVER_ELSEWHERE, NAME_ADDRESS_FIRST, NO_SPELLING_SERVICE, NO_GRAMMAR_SERVICE];

/// `Not in dictionary:`, already translated for spelling places, and its French. U+202F before
/// the colon, as elsewhere in `fr.po`.
const NOT_IN_DICTIONARY: (&str, &str) = ("Not in dictionary:", "Absent du dictionnaire\u{202f}:");

/// `Grammar:`, the heading a grammar place gets, and its reviewed French.
const GRAMMAR_CAPTION: (&str, &str) = ("Grammar:", "Grammaire\u{202f}:");

/// `Style:`, the heading a style place gets, and its reviewed French.
const STYLE_CAPTION: (&str, &str) = ("Style:", "Style\u{202f}:");

/// Every place heading, and its French: the caption is chosen by kind, so all three are named.
const CAPTIONS: [(&str, &str); 3] = [NOT_IN_DICTIONARY, GRAMMAR_CAPTION, STYLE_CAPTION];

/// The window's language is one value for the whole test binary, and every window puts its own
/// language in force at every frame. Tests in one binary draw side by side, so each test holds
/// this for its whole body and puts English back after it, however it ends.
static LANGUAGE: Mutex<()> = Mutex::new(());

struct HoldLanguage {
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl HoldLanguage {
    fn hold() -> Self {
        Self { _guard: LANGUAGE.lock().unwrap_or_else(std::sync::PoisonError::into_inner) }
    }
}

impl Drop for HoldLanguage {
    fn drop(&mut self) {
        set_current(Lang::English);
    }
}

type Window = Harness<'static, MailApp>;

/// A window on the sample mailbox, in French through the command File › Options runs.
fn window(services: Services, french: bool) -> Window {
    let mut app = MailApp::new(Session::at(mailcraft_engine::sample::mailbox(NOW, 0), NOW, 0), services);
    app.autosave = false;
    app.session.driver.set_auto_sync(false);
    if french {
        app.execute("ui.language", json!({ "language": "fr" })).expect("the interface in French");
    }
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
    window.run_steps(3);
    assert_eq!(window.state().language(), if french { Lang::French } else { Lang::English }, "the window's language");
    window
}

/// A message being written, holding `text`, so a frame has something to check; its id.
fn new_draft(window: &mut Window, text: &str) -> u64 {
    let answer = window.state_mut().execute("mail.new", json!({})).expect("a new message");
    let id = answer["compose"].as_u64().expect("the new message's id");
    window.state_mut().execute("compose.set", json!({ "compose": id, "body": text })).expect("its text");
    window.run_steps(1);
    id
}

/// The proofing settings File › Options would have written.
fn prefs(window: &mut Window, spelling: bool, grammar: bool, managed: bool, url: &str) {
    let proof = &mut window.state_mut().session.prefs.proof;
    proof.check_spelling = spelling;
    proof.check_grammar = grammar;
    proof.as_you_type = true;
    proof.managed_server = managed;
    proof.grammar_url = url.to_string();
}

/// Review › Spelling & Grammar, opened by the command the ribbon runs: the user's own route, which
/// is the one that puts the dialog on screen.
fn open_review(window: &mut Window) {
    let opened = window.state_mut().run("review.spelling", json!({})).expect("Review › Spelling & Grammar");
    assert_eq!(opened["pending"], "spelling", "the user-action path opens the real dialog, not an engine-only walk");
    window.run_steps(3);
}

/// Every piece of text the last frame painted, as it was drawn.
fn painted_texts(window: &Window) -> Vec<String> {
    fn walk(shape: &egui::Shape, out: &mut Vec<String>) {
        match shape {
            egui::Shape::Text(text) => out.push(text.galley.text().to_string()),
            egui::Shape::Vec(shapes) => shapes.iter().for_each(|shape| walk(shape, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    for clipped in &window.output().shapes {
        walk(&clipped.shape, &mut out);
    }
    out
}

fn painted(window: &Window, text: &str) -> bool {
    painted_texts(window).iter().any(|drawn| drawn.contains(text))
}

/// The French note is painted, and the English one is not: the window says the note it cached in
/// the language in force, and only that.
fn assert_note(window: &Window, note: (&str, &str)) {
    let (english, french) = note;
    assert!(painted(window, french), "the French review does not paint {french:?}; it painted {:?}", painted_texts(window));
    assert!(!painted(window, english), "the French review still paints the English {english:?}");
}

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

/// A managed server whose cached status is fixed and whose work queue is counted: drawing a page
/// or a dialog never starts, downloads or probes anything.
struct FixedServer {
    status: Status,
    requests: AtomicUsize,
}

impl FixedServer {
    fn new(status: Status) -> Arc<FixedServer> {
        Arc::new(FixedServer { status, requests: AtomicUsize::new(0) })
    }

    fn requests(&self) -> usize {
        self.requests.load(Ordering::Relaxed)
    }
}

impl ManagedServer for FixedServer {
    fn status(&self) -> Status {
        self.status.clone()
    }

    fn request(&self, _action: Action) -> Result<(), Issue> {
        self.requests.fetch_add(1, Ordering::Relaxed);
        Err(Issue::Busy)
    }
}

fn grammar_count(answers: &Answers) -> usize {
    answers.grammar.lock().expect("answers lock").len()
}

fn spelling_count(answers: &Answers) -> usize {
    answers.spelling.lock().expect("answers lock").len()
}

/// The text the window last asked the grammar server to check.
fn asked_text(answers: &Answers, index: usize) -> String {
    answers.grammar.lock().expect("answers lock").get(index).expect("the request is on record").0.text.clone()
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

/// A place of `kind` under `word` of `text`, as a grammar server would send it.
fn place(text: &str, word: &str, kind: Kind) -> Diagnostic {
    let start = text.find(word).expect("the word is in the text");
    let mut diagnostic = Diagnostic::new(Span::of(text, start, start + word.len()).expect("a span"), kind, Source::Server)
        .saying("A synthetic message.")
        .suggesting([word.to_string()]);
    diagnostic.language = Some(Language::EnCa);
    diagnostic
}

/// The proofing snapshot `ui.inspect` reports: cached state only, no request queued.
fn proofing(window: &mut Window) -> Value {
    let ctx = window.ctx.clone();
    let value = control::inspect(window.state_mut(), &ctx);
    let field = value["proofing"].clone();
    assert!(field.is_object(), "ui.inspect has a proofing object while a draft is open: {field}");
    field
}

/// The catalogue itself carries every note: the window can only paint French if the extractor saw
/// each literal and the French catalogue has an entry for it.
#[test]
fn the_french_catalogue_carries_the_staged_proofing_notes() {
    for (english, french) in NOTES {
        assert_eq!(Lang::French.t(english), french, "the French catalogue has no entry for {english:?}");
    }
}

/// The four File › Options labels proofing already used, whose French entries were empty.
#[test]
fn the_french_catalogue_carries_the_proofing_options_labels() {
    let labels = [
        ("running", "en cours d’exécution"),
        ("Download", "Télécharger"),
        ("Built-in Proofing Server", "Serveur de vérification intégré"),
        ("Dictionary Folder", "Dossier des dictionnaires"),
    ];
    for (english, french) in labels {
        assert_eq!(Lang::French.t(english), french, "the French catalogue has no entry for {english:?}");
    }
}

/// The three place headings, by kind: the two new ones go through the official extractor too.
#[test]
fn the_french_catalogue_carries_the_place_captions() {
    for (english, french) in CAPTIONS {
        assert_eq!(Lang::French.t(english), french, "the French catalogue has no entry for {english:?}");
    }
    assert_eq!(NOT_IN_DICTIONARY.1, "Absent du dictionnaire\u{202f}:", "the existing heading keeps its narrow no-break space");
}

#[test]
fn a_french_review_says_that_this_build_has_no_spelling_service() {
    let _language = HoldLanguage::hold();
    let mut window = window(Services::default(), true);
    new_draft(&mut window, BODY);
    prefs(&mut window, true, false, false, URL);
    window.run_steps(30);
    open_review(&mut window);
    window.run_steps(6);
    assert_note(&window, NO_SPELLING_SERVICE);
}

#[test]
fn a_french_review_says_that_this_build_has_no_grammar_server() {
    let _language = HoldLanguage::hold();
    let mut window = window(Services::default(), true);
    new_draft(&mut window, BODY);
    prefs(&mut window, false, true, false, URL);
    window.run_steps(30);
    open_review(&mut window);
    window.run_steps(6);
    assert_note(&window, NO_GRAMMAR_SERVICE);
}

/// An empty address is the owner's field to fill in, not a server on another computer: the
/// settings refuse an empty address, so it must be named before the refusal is said.
#[test]
fn a_french_review_names_the_missing_grammar_address() {
    let _language = HoldLanguage::hold();
    let mut window = window(Services::default(), true);
    new_draft(&mut window, BODY);
    prefs(&mut window, false, true, false, "");
    window.run_steps(30);
    open_review(&mut window);
    window.run_steps(6);
    assert_note(&window, NAME_ADDRESS_FIRST);
    assert!(!painted(&window, SERVER_ELSEWHERE.1), "an empty address is not a server on another computer: {:?}", painted_texts(&window));
}

#[test]
fn a_french_review_refuses_a_server_on_another_computer() {
    let _language = HoldLanguage::hold();
    let mut window = window(Services::default(), true);
    new_draft(&mut window, BODY);
    prefs(&mut window, false, true, false, "http://example.com:8081");
    window.run_steps(30);
    open_review(&mut window);
    window.run_steps(6);
    assert_note(&window, SERVER_ELSEWHERE);
}

#[test]
fn a_french_review_says_that_this_build_cannot_run_the_builtin_server() {
    let _language = HoldLanguage::hold();
    let mut window = window(Services::default(), true);
    new_draft(&mut window, BODY);
    prefs(&mut window, false, true, true, URL);
    window.run_steps(30);
    open_review(&mut window);
    window.run_steps(6);
    assert_note(&window, CANNOT_RUN_SERVER);
}

#[test]
fn a_french_review_says_that_the_builtin_server_is_not_running() {
    let _language = HoldLanguage::hold();
    let server = FixedServer::new(Status { phase: Phase::NotInstalled, ..Default::default() });
    let mut window = window(Services { proof_server: Some(server.clone()), ..Services::default() }, true);
    new_draft(&mut window, BODY);
    prefs(&mut window, false, true, true, URL);
    window.run_steps(30);
    open_review(&mut window);
    window.run_steps(6);
    assert_note(&window, SERVER_NOT_RUNNING);
    assert_eq!(server.requests(), 0, "drawing the review asks the host for nothing");
}

#[test]
fn a_french_review_refuses_the_builtin_servers_foreign_address() {
    let _language = HoldLanguage::hold();
    let status = Status { phase: Phase::Running, endpoint: Some("http://example.com:8081".into()), ..Default::default() };
    let server = FixedServer::new(status);
    let mut window = window(Services { proof_server: Some(server.clone()), ..Services::default() }, true);
    new_draft(&mut window, BODY);
    prefs(&mut window, false, true, true, URL);
    window.run_steps(30);
    open_review(&mut window);
    window.run_steps(6);
    assert_note(&window, SERVER_ADDRESS_UNUSABLE);
    assert_eq!(server.requests(), 0, "drawing the review asks the host for nothing");
}

/// A change of interface language repaints the note the document already has: the cached note is
/// English whatever the window says, so nothing is asked again and no revision is made.
#[test]
fn the_note_follows_the_interface_language_without_asking_anything_again() {
    let _language = HoldLanguage::hold();
    let (answers, services) = service();
    let mut window = window(services, false);
    new_draft(&mut window, BODY);
    prefs(&mut window, true, true, true, URL);
    window.run_steps(30);
    assert_eq!(spelling_count(&answers), 1, "the spelling check is asked for once");
    respond_spelling(&answers, 0, SpellingReply { diagnostics: Vec::new(), note: None, error: None });
    window.run_steps(2);
    open_review(&mut window);
    window.run_steps(6);
    assert!(painted(&window, CANNOT_RUN_SERVER.0), "the English note is painted first: {:?}", painted_texts(&window));
    let before = proofing(&mut window);

    window.state_mut().execute("ui.language", json!({ "language": "fr" })).expect("the interface in French");
    window.run_steps(6);
    assert_eq!(window.state().language(), Lang::French, "the window is in French now");
    let after = proofing(&mut window);
    assert_eq!(after["revision"], before["revision"], "the cached document is the same one, at the same revision");
    assert_eq!(after["requests"], before["requests"], "a change of language asks no provider anything");
    assert_note(&window, CANNOT_RUN_SERVER);
}

/// The host's own words are the host's: a dictionary's note is painted exactly as it came, even in
/// a French window, and is never looked up in the catalogue.
#[test]
fn a_dictionary_note_from_the_host_is_shown_as_it_came() {
    let _language = HoldLanguage::hold();
    let (answers, services) = service();
    let mut window = window(services, true);
    new_draft(&mut window, BODY);
    prefs(&mut window, true, false, false, URL);
    window.run_steps(30);
    assert_eq!(spelling_count(&answers), 1, "the spelling check is asked for once");
    let provider = "No dictionary for English (Canada).";
    respond_spelling(&answers, 0, SpellingReply { diagnostics: Vec::new(), note: Some(provider.into()), error: None });
    window.run_steps(2);
    open_review(&mut window);
    window.run_steps(4);
    assert_eq!(spelling_count(&answers), 2, "the dialog asks for its own fresh check");
    respond_spelling(&answers, 1, SpellingReply { diagnostics: Vec::new(), note: Some(provider.into()), error: None });
    window.run_steps(4);
    assert!(painted(&window, provider), "the host's own words are painted as they came: {:?}", painted_texts(&window));
}

/// A sentence this slice does not name — the fixed failure sentences are the next slice — is
/// shown exactly as it was cached. A later slice that localizes them updates this test.
#[test]
fn a_note_this_slice_does_not_name_is_shown_as_it_came() {
    let _language = HoldLanguage::hold();
    let (answers, services) = service();
    let mut window = window(services, true);
    new_draft(&mut window, BODY);
    prefs(&mut window, false, true, false, URL);
    window.run_steps(30);
    assert_eq!(grammar_count(&answers), 1, "the grammar check is asked for once");
    respond_grammar(&answers, 0, GrammarReply { error: Some(Failure::Failed), ..Default::default() });
    window.run_steps(2);
    let failure = "The grammar check failed.";
    open_review(&mut window);
    window.run_steps(4);
    assert_eq!(grammar_count(&answers), 2, "the dialog asks for its own fresh check");
    respond_grammar(&answers, 1, GrammarReply { error: Some(Failure::Failed), ..Default::default() });
    window.run_steps(4);
    assert!(painted(&window, failure), "a sentence outside this slice is painted as it came: {:?}", painted_texts(&window));
}

/// The dialog up to the place on screen: the window's own check answered once (so the walk can
/// start), then the fresh check the dialog opens with, answered with a place of `kind`.
fn review_one_place(kind: Kind, french: bool) -> Vec<String> {
    let (answers, services) = service();
    let mut window = window(services, french);
    new_draft(&mut window, BODY);
    prefs(&mut window, false, true, false, URL);
    window.run_steps(30);
    assert_eq!(grammar_count(&answers), 1, "the window checks the text once");
    let checked = asked_text(&answers, 0);
    respond_grammar(&answers, 0, GrammarReply { diagnostics: vec![place(&checked, WORD, kind)], ..Default::default() });
    window.run_steps(2);

    open_review(&mut window);
    window.run_steps(4);
    assert_eq!(grammar_count(&answers), 2, "the dialog asks for its own fresh check");
    let fresh = asked_text(&answers, 1);
    assert_eq!(fresh, BODY, "the fresh check is of the same synthetic document");
    respond_grammar(&answers, 1, GrammarReply { diagnostics: vec![place(&fresh, WORD, kind)], ..Default::default() });
    window.run_steps(4);
    painted_texts(&window)
}

/// The heading and the two it must not be, in one language: the painted text is checked in both
/// languages, so a heading that only follows the kind in English is caught here.
fn assert_caption(texts: &[String], caption: (&str, &str), not: [(&str, &str); 2], language: &str) {
    let (_, french) = caption;
    assert!(texts.iter().any(|text| text.contains(french)), "a {language} place is not headed {french:?}: {texts:?}");
    for (english, other_french) in not {
        assert!(!texts.iter().any(|text| text.contains(other_french)), "a {language} place is headed {other_french:?}: {texts:?}");
        assert!(!texts.iter().any(|text| text.contains(english)), "a {language} place is headed {english:?}: {texts:?}");
    }
}

#[test]
fn a_grammar_place_is_not_called_a_missing_dictionary_word() {
    let _language = HoldLanguage::hold();
    let english = review_one_place(Kind::Grammar, false);
    assert!(english.iter().any(|text| text.contains(GRAMMAR_CAPTION.0)), "an English grammar place is headed {:?}: {english:?}", GRAMMAR_CAPTION.0);
    assert!(!english.iter().any(|text| text.contains(NOT_IN_DICTIONARY.0)), "a grammar place is not a missing dictionary word: {english:?}");
    assert!(!english.iter().any(|text| text.contains(STYLE_CAPTION.0)), "a grammar place is not a style place: {english:?}");

    let french = review_one_place(Kind::Grammar, true);
    assert_caption(&french, GRAMMAR_CAPTION, [NOT_IN_DICTIONARY, STYLE_CAPTION], "French grammar");
}

#[test]
fn a_style_place_is_not_called_a_missing_dictionary_word() {
    let _language = HoldLanguage::hold();
    let english = review_one_place(Kind::Style, false);
    assert!(english.iter().any(|text| text.contains(STYLE_CAPTION.0)), "an English style place is headed {:?}: {english:?}", STYLE_CAPTION.0);
    assert!(!english.iter().any(|text| text.contains(NOT_IN_DICTIONARY.0)), "a style place is not a missing dictionary word: {english:?}");
    assert!(!english.iter().any(|text| text.contains(GRAMMAR_CAPTION.0)), "a style place is not a grammar place: {english:?}");

    let french = review_one_place(Kind::Style, true);
    assert_caption(&french, STYLE_CAPTION, [NOT_IN_DICTIONARY, GRAMMAR_CAPTION], "French style");
}
