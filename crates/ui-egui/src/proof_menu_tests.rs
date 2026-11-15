// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! The place menu under the text: opened once for the revision it was clicked on, kept across
//! passes while its dictionary answers, and gone the moment the words under it change.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, Sender, channel};

use egui::accesskit::Role;
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT as _, Queryable as _};
use mailcraft_engine::Session;
use mailcraft_model::FolderKind;
use mailcraft_model::proof::{Diagnostic, Kind, Language, Source, Span};
use serde_json::json;

use crate::proof::Body;
use crate::proof_service::{GrammarReply, GrammarRequest, ProofService, SpellingReply, SpellingRequest, SuggestionReply, SuggestionRequest};
use crate::{MailApp, Services};

const NOW: i64 = 1_791_993_600;

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

fn app(spelling: bool, grammar: bool) -> (MailApp, Arc<Answers>) {
    let answers = Arc::new(Answers::default());
    let services = Services { proof_native: Some(Arc::new(Fake { answers: Arc::clone(&answers) })), ..Services::default() };
    let mut app = MailApp::new(Session::at(mailcraft_engine::sample::mailbox(NOW, 0), NOW, 0), services);
    app.autosave = false;
    app.execute("mail.new", json!({})).expect("a new draft");
    let len = app.session.doc().expect("draft").0.text().len();
    app.execute("compose.select", json!({"from": 0, "to": len})).expect("select all");
    app.execute("compose.type", json!({"text": "She recieve the documents."})).expect("type");
    app.session.prefs.proof.check_spelling = spelling;
    app.session.prefs.proof.check_grammar = grammar;
    app.session.prefs.proof.grammar_url = "http://127.0.0.1:8081".into();
    (app, answers)
}

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

fn draw_menu(ctx: &egui::Context, app: &mut MailApp, seconds: f64) {
    let input = egui::RawInput { time: Some(seconds), ..Default::default() };
    let ctx = ctx.clone();
    let draw = ctx.clone();
    let mut output = ctx.run_ui(input, |ui| {
        let resp = ui.button("a place under the text");
        let body = body(app);
        crate::proof::menu(app, &draw, &resp, body);
    });
    output.textures_delta.clear();
}

fn spelling_place(answers: &Answers) {
    let (text, tx) = {
        let requests = answers.spelling.lock().expect("answers lock");
        let (request, tx) = requests.first().expect("the spelling request is on record");
        (request.text.clone(), tx.clone())
    };
    let diagnostic = Diagnostic::new(Span::of(&text, 4, 11).expect("the word"), Kind::Spelling, Source::Local);
    tx.send(SpellingReply { diagnostics: vec![diagnostic], ..Default::default() }).expect("waiting for the spelling answer");
}

fn grammar_place(answers: &Answers) {
    let (text, tx) = {
        let requests = answers.grammar.lock().expect("answers lock");
        let (request, tx) = requests.first().expect("the grammar request is on record");
        (request.text.clone(), tx.clone())
    };
    let diagnostic = Diagnostic::new(Span::of(&text, 4, 8).expect("the word"), Kind::Grammar, Source::Server);
    tx.send(GrammarReply { diagnostics: vec![diagnostic], ..Default::default() }).expect("waiting for the grammar answer");
}

fn open_spelling_menu(app: &mut MailApp, answers: &Answers, ctx: &egui::Context) {
    frame(ctx, app, 1.0);
    frame(ctx, app, 1.4);
    spelling_place(answers);
    frame(ctx, app, 1.5);
    assert_eq!(app.proof.places(body(app), &text(app)).len(), 1);
    let body = body(app);
    let text = text(app);
    crate::proof::right_click(app, body, &text, Some(6), egui::pos2(40.0, 40.0));
    assert!(crate::proof::menu_open(app, body), "the right click opens the place's menu");
}

#[test]
fn a_menu_stays_open_while_its_suggestions_are_asked_and_takes_them_once() {
    let (mut app, answers) = app(true, false);
    let ctx = egui::Context::default();
    open_spelling_menu(&mut app, &answers, &ctx);
    {
        let requests = answers.suggestions.lock().expect("answers lock");
        assert_eq!(requests.len(), 1, "the dictionary is asked once when the menu opens");
        assert!(!requests[0].0.cancelled.load(Ordering::Relaxed));
    }
    // No answer yet: the popup is drawn again, pass after pass, and is still the same menu.
    draw_menu(&ctx, &mut app, 1.6);
    draw_menu(&ctx, &mut app, 1.7);
    assert!(crate::proof::menu_open(&app, body(&app)), "a menu waiting for its dictionary stays open");
    assert_eq!(answers.suggestions.lock().expect("answers lock").len(), 1, "not asked again with every frame");
    {
        let mut requests = answers.suggestions.lock().expect("answers lock");
        let (_, tx) = requests.remove(0);
        tx.send(SuggestionReply { suggestions: vec!["receive".into(), "relieve".into()], error: None }).expect("the menu is still open");
    }
    // The answer comes after the passes above: a delayed suggestion still reaches the same menu.
    draw_menu(&ctx, &mut app, 1.8);
    assert!(crate::proof::menu_open(&app, body(&app)));
    assert_eq!(crate::proof::menu_suggestions(&app, body(&app)), Some(vec!["receive".to_string(), "relieve".to_string()]));
    let body = body(&app);
    crate::proof::close_menu(&mut app, body);
    assert!(!crate::proof::menu_open(&app, body));
}

#[test]
fn closing_a_menu_cancels_the_question_it_asked() {
    let (mut app, answers) = app(true, false);
    let ctx = egui::Context::default();
    open_spelling_menu(&mut app, &answers, &ctx);
    let cancelled = Arc::clone(&answers.suggestions.lock().expect("answers lock")[0].0.cancelled);
    draw_menu(&ctx, &mut app, 1.6);
    assert!(!cancelled.load(Ordering::Relaxed), "the question is still wanted while the menu is open");
    let body = body(&app);
    crate::proof::close_menu(&mut app, body);
    assert!(!crate::proof::menu_open(&app, body));
    assert!(cancelled.load(Ordering::Relaxed), "closing the menu cancels what it asked for");
}

#[test]
fn a_menu_is_gone_when_the_words_under_it_change() {
    let (mut app, answers) = app(true, false);
    let ctx = egui::Context::default();
    open_spelling_menu(&mut app, &answers, &ctx);
    let cancelled = Arc::clone(&answers.suggestions.lock().expect("answers lock")[0].0.cancelled);
    // Edit the draft: the menu kept the text it opened on, and a place that no longer fits is no
    // place at all.
    let len = app.session.doc().expect("draft").0.text().len();
    app.execute("compose.select", json!({"from": 0, "to": len})).expect("select all");
    app.execute("compose.type", json!({"text": "Something else entirely."})).expect("type");
    frame(&ctx, &mut app, 1.6);
    draw_menu(&ctx, &mut app, 1.7);
    assert!(!crate::proof::menu_open(&app, body(&app)), "the menu of another revision is no menu");
    assert!(cancelled.load(Ordering::Relaxed), "the question asked for the old place is cancelled");
    // A late answer cannot reopen it.
    {
        let mut requests = answers.suggestions.lock().expect("answers lock");
        let (_, tx) = requests.remove(0);
        // The menu's receiver is gone, so this old question has nowhere to land: that is the
        // point. A channel that still accepted it could not reopen the menu either.
        let _ = tx.send(SuggestionReply { suggestions: vec!["receive".into()], error: None });
    }
    draw_menu(&ctx, &mut app, 1.8);
    assert!(!crate::proof::menu_open(&app, body(&app)));
    assert_eq!(crate::proof::menu_suggestions(&app, body(&app)), None);
}

#[test]
fn a_grammar_place_never_asks_a_dictionary() {
    let (mut app, answers) = app(false, true);
    let ctx = egui::Context::default();
    frame(&ctx, &mut app, 1.0);
    frame(&ctx, &mut app, 1.4);
    grammar_place(&answers);
    frame(&ctx, &mut app, 1.5);
    assert_eq!(app.proof.places(body(&app), &text(&app)).len(), 1);
    let body = body(&app);
    let text = text(&app);
    crate::proof::right_click(&mut app, body, &text, Some(6), egui::pos2(40.0, 40.0));
    assert!(crate::proof::menu_open(&app, body));
    assert!(answers.suggestions.lock().expect("answers lock").is_empty(), "a grammar place asks no dictionary");
}

/// A draft that chose its own language at the footer: beside the owner's defaults, never instead
/// of them.
fn in_french(app: &mut MailApp) -> u64 {
    let compose = app.session.compose_id().expect("a draft is open");
    app.execute("compose.set", json!({ "compose": compose, "proofLanguages": ["fr-CA"] })).expect("the draft's own languages");
    compose
}

#[test]
fn changing_a_drafts_languages_calls_off_and_drops_the_check_that_was_on_its_way() {
    let (mut app, answers) = app(true, false);
    let defaults = app.session.prefs.proof.active();
    let compose = in_french(&mut app);
    let ctx = egui::Context::default();
    frame(&ctx, &mut app, 1.0);
    frame(&ctx, &mut app, 1.4);
    assert_eq!(answers.spelling.lock().expect("answers lock").len(), 1, "the draft was asked for in its French");
    let (old_text, cancelled, old_languages, answer) = {
        let requests = answers.spelling.lock().expect("answers lock");
        let (request, sender) = &requests[0];
        (request.text.clone(), Arc::clone(&request.cancelled), request.settings.languages.clone(), sender.clone())
    };
    assert_eq!(old_languages, vec![Language::FrCa], "the request on its way is of the draft's own choice");
    assert_ne!(old_languages, defaults, "and not of File › Options");

    // The footer chooses French alone and then English beside it: another revision, under new
    // languages. What was on its way for the old one is called off.
    app.execute("compose.set", json!({ "compose": compose, "proofLanguages": ["en-CA", "fr-CA"] })).expect("the draft's change");
    frame(&ctx, &mut app, 1.5);
    assert!(cancelled.load(Ordering::Relaxed), "the request for the old languages is cancelled");

    // An answer that arrives for the old revision is nobody's: the words did not change, but the
    // languages the text was checked under did.
    let diagnostic = Diagnostic::new(Span::of(&old_text, 4, 11).expect("the word"), Kind::Spelling, Source::Local);
    answer.send(SpellingReply { diagnostics: vec![diagnostic], ..Default::default() }).expect("the old check still waits");
    frame(&ctx, &mut app, 1.6);
    assert!(app.proof.places(body(&app), &text(&app)).is_empty(), "an answer for other languages is dropped whole");

    // The next check is asked for again, and carries the new list.
    frame(&ctx, &mut app, 2.0);
    let requests = answers.spelling.lock().expect("answers lock");
    assert_eq!(requests.len(), 2, "the new revision is checked once");
    assert_eq!(requests[1].0.settings.languages, vec![Language::EnCa, Language::FrCa], "the next check is in both languages");
    drop(requests);
    assert_eq!(app.session.prefs.proof.languages, defaults, "no draft's choice moved File › Options");
}

#[test]
fn a_signature_is_checked_in_file_options_defaults_while_a_french_draft_is_open() {
    let (mut app, answers) = app(false, true);
    let defaults = app.session.prefs.proof.active();
    let draft = in_french(&mut app);
    let ctx = egui::Context::default();
    frame(&ctx, &mut app, 1.0);
    frame(&ctx, &mut app, 1.4);
    {
        let requests = answers.grammar.lock().expect("answers lock");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].0.languages, vec![Language::FrCa], "the draft is checked in French");
    }
    grammar_place(&answers);
    frame(&ctx, &mut app, 1.5);

    // A signature has no language choice of its own: it is checked in the owner's defaults, even
    // while the French draft is still open behind it.
    app.execute("signature.open", json!({ "name": "Standard" })).expect("open the signature");
    frame(&ctx, &mut app, 1.6);
    frame(&ctx, &mut app, 1.9);
    let requests = answers.grammar.lock().expect("answers lock");
    assert_eq!(requests.len(), 2, "the signature is checked");
    assert_eq!(requests[1].0.languages, defaults, "the signature is checked in File › Options' defaults");
    assert!(requests[1].0.text.contains("Avery Quinn"), "the signature's own words were checked");
    drop(requests);
    assert_eq!(app.session.proof_settings_for(draft).languages, vec![Language::FrCa], "the draft behind keeps its French");
}

#[test]
fn a_french_draft_is_reviewed_under_its_own_languages_not_the_defaults() {
    let (mut app, answers) = app(false, true);
    let draft = in_french(&mut app);
    let ctx = egui::Context::default();
    frame(&ctx, &mut app, 1.0);
    frame(&ctx, &mut app, 1.4);
    grammar_place(&answers);
    frame(&ctx, &mut app, 1.5);
    assert_eq!(app.session.proof_settings_for(draft).languages, vec![Language::FrCa]);
    // A change of File › Options while the draft is open does not take the draft's review away:
    // the walk that is on screen belongs to the draft's own languages.
    app.execute("proof.set", json!({ "languages": ["en-US"] })).expect("new defaults");
    let found = crate::proof::review_places(&app, &text(&app)).expect("the French draft is reviewed under its own choice");
    assert_eq!(found.len(), 1, "the walk has the place that was found in French");
    assert_eq!(app.session.proof_settings_for(draft).languages, vec![Language::FrCa], "the draft kept its French");
}

#[test]
fn a_place_menu_and_a_review_ask_the_dictionary_in_the_languages_of_their_own_draft() {
    let (mut app, answers) = app(true, false);
    let defaults = app.session.prefs.proof.active();
    in_french(&mut app);
    let ctx = egui::Context::default();
    frame(&ctx, &mut app, 1.0);
    frame(&ctx, &mut app, 1.4);
    spelling_place(&answers);
    frame(&ctx, &mut app, 1.5);
    {
        let requests = answers.spelling.lock().expect("answers lock");
        assert_eq!(requests[0].0.settings.languages, vec![Language::FrCa], "the check is French");
    }

    // The right click carries its own body: the menu asks for that draft's languages, even after
    // File › Options' defaults were changed while the draft stayed open.
    app.execute("proof.set", json!({ "languages": ["en-US"] })).expect("new defaults");
    let body = body(&app);
    let text = text(&app);
    crate::proof::right_click(&mut app, body, &text, Some(6), egui::pos2(40.0, 40.0));
    {
        let requests = answers.suggestions.lock().expect("answers lock");
        assert_eq!(requests.len(), 1, "the menu asked the dictionary once");
        assert_eq!(requests[0].0.settings.languages, vec![Language::FrCa], "the menu asks in the draft's own French");
        assert_ne!(requests[0].0.settings.languages, defaults);
    }

    // The review dialog's question resolves the document it walks, which is the same draft.
    let _ = crate::proof::suggest(&app, "recieve", 6).expect("a host to ask");
    let requests = answers.suggestions.lock().expect("answers lock");
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].0.settings.languages, vec![Language::FrCa], "the review's dictionary question follows the same choice");
}

// --- The dialog's own action row (#954): five actions at the width it actually opens at. ---

/// The five actions of a grammar place that names a rule, left to right as the dialog shows them,
/// in the languages whose catalogues translate every label.
const ACTIONS_EN: [&str; 5] = ["Change", "Add to Dictionary", "Ignore This Rule", "Ignore Once", "Close"];
const ACTIONS_FR: [&str; 5] = ["Remplacer", "Ajouter au dictionnaire", "Ignorer cette règle", "Ignorer une fois", "Fermer"];

/// The automation keys those buttons are recorded under, in the same order.
const ACTION_KEYS: [&str; 5] = ["proofChange", "proofAdd", "proofIgnoreRule", "proofIgnore", "proofClose"];

/// The dialog drawn on its own, at the width the window gives it, with the frame's clip kept so
/// that a button can be proven to be inside the part of the window that is actually painted.
struct Sheet {
    app: MailApp,
    dialog: crate::proof_dialog::Spelling,
    /// The part of the window the dialog's contents are laid out in (its frame's inner rect).
    bounds: egui::Rect,
    /// What egui will paint the contents into; kept for the evidence line only.
    clip: egui::Rect,
    dark: bool,
    enter: bool,
    styled: bool,
}

fn checked_window(app: MailApp, dark: bool, enter: bool, size: (f32, f32)) -> Harness<'static, Sheet> {
    let dialog = crate::proof_dialog::Spelling::open(&app).expect("a draft to check");
    let mut harness = Harness::builder()
        .with_accessibility_check(false)
        .allow_missing_glyphs()
        .with_size(egui::vec2(size.0, size.1))
        .with_step_dt(1.0 / 60.0)
        .build_ui_state(
            |ui, sheet: &mut Sheet| {
                let ctx = ui.ctx().clone();
                if !sheet.styled {
                    crate::theme::install_fonts(&ctx);
                    let (appearance, preference) = if sheet.dark {
                        (crate::theme::Appearance::Dark, egui::ThemePreference::Dark)
                    } else {
                        (crate::theme::Appearance::Light, egui::ThemePreference::Light)
                    };
                    crate::theme::apply(&ctx, preference, &crate::theme::ThemeChoice { appearance, ..Default::default() });
                    sheet.styled = true;
                }
                if let Some(body) = crate::proof::body_of(&sheet.app)
                    && let Some(text) = crate::proof::text_of(&sheet.app, body)
                {
                    crate::proof::run(&mut sheet.app, &ctx, body, &text);
                }
                let mut clip = None;
                let mut bounds = None;
                sheet.app.hit.clear();
                crate::dialogs::window(&ctx, crate::tl!("Spelling & Grammar"), crate::proof_dialog::WIDTH, |ui| {
                    bounds = Some(ui.max_rect());
                    clip = Some(ui.clip_rect());
                    let _ = sheet.dialog.show(&mut sheet.app, ui, sheet.enter);
                });
                if let Some(bounds) = bounds {
                    sheet.bounds = bounds;
                }
                if let Some(clip) = clip {
                    sheet.clip = clip;
                }
            },
            Sheet { app, dialog, bounds: egui::Rect::NOTHING, clip: egui::Rect::NOTHING, dark, enter, styled: false },
        );
    harness.run_steps(2);
    harness
}

/// A grammar place that names a rule and has a suggestion, so the dialog shows all five actions
/// (Change, Add to Dictionary, Ignore This Rule, Ignore Once, Close).
fn grammar_rule_place(answers: &Answers, rule: &str) {
    grammar_rule_place_with(answers, rule, "receive");
}

fn grammar_rule_place_with(answers: &Answers, rule: &str, suggestion: &str) {
    let (text, tx) = {
        let requests = answers.grammar.lock().expect("answers lock");
        let (request, tx) = requests.first().expect("the grammar request is on record");
        (request.text.clone(), tx.clone())
    };
    let mut diagnostic =
        Diagnostic::new(Span::of(&text, 4, 11).expect("the word"), Kind::Grammar, Source::Server).suggesting([suggestion.to_string()]);
    diagnostic.rule = Some(rule.to_string());
    tx.send(GrammarReply { diagnostics: vec![diagnostic], ..Default::default() }).expect("waiting for the grammar answer");
}

/// The dialog with its one grammar place on screen.
fn shown_window(dark: bool, enter: bool, size: (f32, f32)) -> Harness<'static, Sheet> {
    let (app, answers) = app(false, true);
    let mut harness = checked_window(app, dark, enter, size);
    grammar_rule_place(&answers, "RECIEVE_RULE");
    harness.run_steps(3);
    harness
}

/// Where the accessible button with `label` is, and whether it is disabled.
fn button(h: &Harness<'_, Sheet>, label: &'static str) -> (egui::Rect, bool) {
    let nodes: Vec<_> = h.get_all_by_role_and_label(Role::Button, label).collect();
    assert_eq!(nodes.len(), 1, "{label:?} is one button of the dialog");
    let node = nodes[0];
    (node.rect(), node.accesskit_node().is_disabled())
}

/// A rect the dialog does not paint is not an action the owner can reach.
fn assert_inside(clip: egui::Rect, what: &str, rect: egui::Rect) {
    let slack = 0.5;
    assert!(
        rect.min.x >= clip.min.x - slack && rect.max.x <= clip.max.x + slack,
        "{what} is outside the dialog horizontally: rect {rect:?}, clip {clip:?}"
    );
    assert!(
        rect.min.y >= clip.min.y - slack && rect.max.y <= clip.max.y + slack,
        "{what} is outside the dialog vertically: rect {rect:?}, clip {clip:?}"
    );
}

#[test]
fn every_spelling_action_is_painted_inside_the_dialog_at_its_own_width() {
    let _language = crate::i18n::RestoreLanguage::hold();
    for (lang, labels) in [(crate::i18n::Lang::English, ACTIONS_EN), (crate::i18n::Lang::French, ACTIONS_FR)] {
        crate::i18n::set_current(lang);
        for dark in [false, true] {
            for size in [(1280.0, 900.0), (640.0, 520.0)] {
                let harness = shown_window(dark, false, size);
                let bounds = harness.state().bounds;
                let clip = harness.state().clip;
                assert!(bounds.width() > 0.0 && bounds.height() > 0.0, "{lang:?} dark={dark} is a laid-out dialog");
                let mut rects = Vec::new();
                for (label, key) in labels.iter().zip(ACTION_KEYS) {
                    let (rect, disabled) = button(&harness, label);
                    println!("{lang:?} dark={dark} {size:?}: {key} {label:?} rect={rect:?} bounds={bounds:?} clip={clip:?}");
                    let hit = harness.state().app.hit.rect(key, 0).unwrap_or_else(|| panic!("{key} is on record"));
                    println!("  hit={hit:?} disabled={disabled}");
                    rects.push((label, rect, disabled, key, hit));
                }
                for (label, rect, disabled, key, hit) in &rects {
                    assert!(!disabled, "{label:?} is an action the owner may take");
                    assert_inside(bounds, label, *rect);
                    // The automation key names the same painted button.
                    assert_inside(bounds, key, *hit);
                    assert!((hit.min - rect.min).length() <= 1.0 && (hit.max - rect.max).length() <= 1.0, "{key} records the button's own rect");
                }
                // Neighbours do not cover each other: every action is separately reachable.
                for (i, (one, a, ..)) in rects.iter().enumerate() {
                    for (two, b, ..) in rects.iter().skip(i + 1) {
                        assert!(!a.intersects(*b), "{one:?} and {two:?} overlap: {a:?} and {b:?}");
                    }
                }
            }
        }
    }
}

/// The dialog drawn through the window's own `dialogs::show`, as the application draws it, so the
/// click, the command and the dialog's own state are the real ones.
struct Running {
    app: MailApp,
    dark: bool,
    styled: bool,
}

fn running(app: MailApp, dark: bool) -> Harness<'static, Running> {
    let mut harness = Harness::builder()
        .with_accessibility_check(false)
        .allow_missing_glyphs()
        .with_size(egui::vec2(1280.0, 900.0))
        .with_step_dt(1.0 / 60.0)
        .build_ui_state(
            |ui, state: &mut Running| {
                let ctx = ui.ctx().clone();
                if !state.styled {
                    crate::theme::install_fonts(&ctx);
                    let (appearance, preference) = if state.dark {
                        (crate::theme::Appearance::Dark, egui::ThemePreference::Dark)
                    } else {
                        (crate::theme::Appearance::Light, egui::ThemePreference::Light)
                    };
                    crate::theme::apply(&ctx, preference, &crate::theme::ThemeChoice { appearance, ..Default::default() });
                    state.styled = true;
                }
                if let Some(body) = crate::proof::body_of(&state.app)
                    && let Some(text) = crate::proof::text_of(&state.app, body)
                {
                    crate::proof::run(&mut state.app, &ctx, body, &text);
                }
                crate::dialogs::show(&mut state.app, &ctx);
            },
            Running { app, dark, styled: false },
        );
    harness.run_steps(2);
    harness
}

/// The draft's text, as the editor holds it.
fn draft_text(app: &MailApp) -> String {
    app.session.doc().expect("the draft is open").0.plain()
}

/// How many messages the mailbox holds, draft included.
fn message_count(app: &MailApp) -> usize {
    app.session.mailbox.messages.values().count()
}

/// How many messages are waiting to go out: a correction may move none there.
fn outbox_count(app: &MailApp) -> usize {
    app.session
        .mailbox
        .accounts
        .iter()
        .filter_map(|account| app.session.mailbox.folder_of_kind(account.id, FolderKind::Outbox))
        .map(|folder| app.session.mailbox.count(folder))
        .sum()
}

fn shown_running() -> Harness<'static, Running> {
    let (mut app, answers) = app(false, true);
    app.execute("ui.dialog", json!({ "name": "spelling" })).expect("the Spelling & Grammar dialog");
    let mut harness = running(app, false);
    let before = draft_text(&harness.state().app);
    assert_eq!(before, "She recieve the documents.");
    grammar_rule_place(&answers, "RECIEVE_RULE");
    harness.run_steps(3);
    harness
}

#[test]
fn change_is_disabled_when_its_field_would_not_change_the_place() {
    let (app, answers) = app(false, true);
    let mut harness = checked_window(app, false, false, (1280.0, 900.0));
    grammar_rule_place_with(&answers, "RECIEVE_RULE", "recieve");
    harness.run_steps(3);
    assert!(button(&harness, "Change").1, "Change is disabled when it would change nothing");
    for label in ["Add to Dictionary", "Ignore This Rule", "Ignore Once", "Close"] {
        assert!(!button(&harness, label).1, "{label} stays an action the owner may take");
    }
}

#[test]
fn changing_a_place_from_the_dialog_keeps_the_rest_and_undoes_in_one_step() {
    let mut harness = shown_running();
    let before_count = message_count(&harness.state().app);
    harness.get_all_by_role_and_label(Role::Button, "Change").next().expect("the Change action").click();
    harness.run_steps(3);
    let app = &harness.state().app;
    assert_eq!(draft_text(app), "She receive the documents.", "Change puts the suggestion in and leaves the rest");
    assert_eq!(message_count(app), before_count, "a correction creates no message");
    assert_eq!(outbox_count(app), 0, "a correction sends nothing");
    assert!(app.session.compose.is_some(), "the corrected draft is still being written");
    harness.state_mut().app.execute("compose.undo", json!({})).expect("one undo step");
    assert_eq!(draft_text(&harness.state().app), "She recieve the documents.", "the correction is one undo step");
}

#[test]
fn enter_still_changes_the_place_the_dialog_shows() {
    let harness = shown_window(false, true, (1280.0, 900.0));
    let app = &harness.state().app;
    assert_eq!(draft_text(app), "She receive the documents.", "Enter takes the same Change the button does");
    assert_eq!(outbox_count(app), 0, "nothing was sent");
    assert!(app.session.compose.is_some(), "the correction stayed in the draft");
}
