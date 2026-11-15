// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! A message being written is proofread in the languages chosen at the bottom of its own composer:
//! the choice belongs to that draft, two languages are chosen together, and the window asks its
//! host for exactly the languages of the draft it is checking — never another open draft's, never
//! File › Options' defaults once the draft has a choice of its own, and never the answer to "save
//! the dictionaries" or "start a server" behind the owner's back.
//!
//! The control is reached the way the owner reaches it: a press on the footer's selector opens the
//! list, a press on a language's box chooses it, and the effective settings are read back through
//! the engine's own contract (`Session::proof_settings_for`). No dictionary, mailbox or server is
//! touched: the host below answers from memory, and the clock is the harness's fixed one.

use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};

use egui::{Event, Modifiers, PointerButton};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use mailcraft_engine::Session;
use mailcraft_model::proof::Language;
use mailcraft_ui_egui::proof_service::{
    GrammarReply, GrammarRequest, ProofService, SpellingReply, SpellingRequest, SuggestionReply, SuggestionRequest,
};
use mailcraft_ui_egui::{MailApp, Services, control};
use serde_json::{Value, json};

/// Wednesday 2026-10-14 16:00 UTC, the sample mailbox's clock.
const NOW: i64 = 1_791_993_600;

/// The window these tests draw in.
const WIDTH: f32 = 1280.0;
const HEIGHT: f32 = 900.0;

type Window = Harness<'static, MailApp>;

/// The window's language is one value for the whole test binary, and every window puts its own
/// back in force at every frame. Tests draw side by side, so one test's frame could be painted in
/// the other's language; each test holds this for its whole body, and puts English back after, so
/// the language it chose stays chosen for as long as it reads the frame.
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
        mailcraft_ui_egui::i18n::set_current(mailcraft_ui_egui::i18n::Lang::English);
    }
}

/// The selector of one draft's languages and one of its four choices, as agents and the hit map
/// name them.
fn selector(compose: u64) -> String {
    format!("proofComposeLanguages:{compose}")
}

fn language_key(compose: u64, code: &str) -> String {
    format!("proofComposeLanguage:{compose}:{code}")
}

/// The window on the sample mailbox, its proofing preferences as the test says, and nothing
/// written to disk. The provider, when one is given, is the whole of the host: no dictionary
/// folder is read and no server is started.
fn window(services: Services, defaults: &[Language]) -> Window {
    window_sized(services, defaults, WIDTH, HEIGHT)
}

fn window_sized(services: Services, defaults: &[Language], width: f32, height: f32) -> Window {
    window_in(services, defaults, width, height, "en")
}

/// As [`window_sized`], with the interface in `language`: the owner's choice runs through the
/// window's own `ui.language` command (the same one File › Options runs), not through a global set
/// before the window exists — the window puts its own language in force every frame.
fn window_in(services: Services, defaults: &[Language], width: f32, height: f32, language: &str) -> Window {
    let mut app = MailApp::new(Session::at(mailcraft_engine::sample::mailbox(NOW, 0), NOW, 0), services);
    app.autosave = false;
    app.auto_sync = false;
    app.session.prefs.proof.languages = defaults.to_vec();
    // Neither provider is on: what these tests press must never be the thing that starts one.
    app.session.prefs.proof.check_spelling = false;
    app.session.prefs.proof.check_grammar = false;
    app.execute("ui.language", json!({ "language": language })).expect("the interface language");
    let mut window = Harness::builder()
        .with_accessibility_check(false)
        .allow_missing_glyphs()
        .with_size(egui::vec2(width, height))
        .with_step_dt(1.0 / 60.0)
        .build_ui_state(
            |ui, app: &mut MailApp| {
                let ctx = ui.ctx().clone();
                app.logic(&ctx);
                app.ui(ui);
            },
            app,
        );
    window.run_steps(4);
    window
}

/// Where the last frame drew the control recorded under `key`, read through the public inspection
/// of the hit map (the same keys and rectangles agents use).
fn optional_rect(window: &mut Window, key: &str) -> Option<egui::Rect> {
    let ctx = window.ctx.clone();
    let items = control::inspect(window.state_mut(), &ctx);
    let items = items.get("items")?.as_array()?;
    let item = items.iter().find(|item| item.get("kind").and_then(Value::as_str) == Some(key))?;
    let rect = item.get("rect")?.as_array()?;
    let at = |index: usize| rect.get(index).and_then(Value::as_f64).map(|value| value as f32);
    Some(egui::Rect::from_min_size(egui::pos2(at(0)?, at(1)?), egui::vec2(at(2)?, at(3)?)))
}

fn rect(window: &mut Window, key: &str) -> egui::Rect {
    optional_rect(window, key).unwrap_or_else(|| panic!("{key} is not on screen"))
}

/// A press and release on `pos`, one pointer event per frame.
fn click(window: &mut Window, pos: egui::Pos2) {
    window.input_mut().events.push(Event::PointerMoved(pos));
    window.step();
    for pressed in [true, false] {
        window.input_mut().events.push(Event::PointerButton { pos, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE });
        window.step();
    }
    window.step();
}

fn click_key(window: &mut Window, key: &str) {
    let at = rect(window, key).center();
    click(window, at);
}

/// What `ui.inspect` says of the keyboard: whether the message's body has it.
fn body_focused(window: &mut Window) -> bool {
    let ctx = window.ctx.clone();
    control::inspect(window.state_mut(), &ctx)["input"]["bodyFocused"].as_bool().unwrap_or(false)
}

/// Where the last frame drew the control recorded under `key`, and which window of its own it is
/// in (0: the main window).
fn rect_and_window(window: &mut Window, key: &str) -> Option<(egui::Rect, u64)> {
    let ctx = window.ctx.clone();
    let items = control::inspect(window.state_mut(), &ctx);
    let items = items.get("items")?.as_array()?;
    let item = items.iter().find(|item| item.get("kind").and_then(Value::as_str) == Some(key))?;
    let rect = item.get("rect")?.as_array()?;
    let at = |index: usize| rect.get(index).and_then(Value::as_f64).map(|value| value as f32);
    let window = item.get("window").and_then(Value::as_u64).unwrap_or(0);
    Some((egui::Rect::from_min_size(egui::pos2(at(0)?, at(1)?), egui::vec2(at(2)?, at(3)?)), window))
}

/// A new message being written, with `text` in it; its id.
fn open(window: &mut Window, text: &str) -> u64 {
    let answer = window.state_mut().execute("mail.new", json!({})).expect("a new message");
    let id = answer["compose"].as_u64().expect("the new message's id");
    window.state_mut().execute("compose.set", json!({ "compose": id, "body": text })).expect("its text");
    window.run_steps(2);
    id
}

/// The languages kept on the draft itself: defaults are snapshotted at open, and a later
/// footer choice replaces only that draft's snapshot.
fn chosen(window: &Window, compose: u64) -> Option<Vec<Language>> {
    window.state().session.composing_by_id(compose).and_then(|c| c.proof_languages.clone())
}

/// What proofing actually checks the draft against.
fn effective(window: &Window, compose: u64) -> Vec<Language> {
    window.state().session.proof_settings_for(compose).languages
}

/// The list in the order File › Options names languages, so that a choice is compared as a set.
fn ordered(languages: &[Language]) -> Vec<Language> {
    Language::ALL.iter().copied().filter(|language| languages.contains(language)).collect()
}

/// Choose, or un-choose, one language at the current draft's footer: open the list if it is not
/// already open, then press the language's box.
fn toggle(window: &mut Window, compose: u64, code: &str) {
    let key = language_key(compose, code);
    if optional_rect(window, &key).is_none() {
        click_key(window, &selector(compose));
        window.run_steps(4);
    }
    click_key(window, &key);
    window.run_steps(4);
}

/// A host of this test's own: every request is kept, and answered at once from memory, so that one
/// request of a kind at a time can still move from draft to draft as frames come.
#[derive(Default)]
struct Seen {
    grammar: Mutex<Vec<GrammarRequest>>,
    spelling: Mutex<Vec<SpellingRequest>>,
}

impl Seen {
    fn grammar(&self) -> Vec<GrammarRequest> {
        self.grammar.lock().unwrap().clone()
    }
}

struct Fake {
    seen: Arc<Seen>,
}

impl ProofService for Fake {
    fn spelling(&self, request: SpellingRequest) -> Receiver<SpellingReply> {
        self.seen.spelling.lock().unwrap().push(request);
        answered(SpellingReply::default())
    }

    fn grammar(&self, request: GrammarRequest) -> Receiver<GrammarReply> {
        self.seen.grammar.lock().unwrap().push(request);
        answered(GrammarReply::default())
    }

    fn suggest(&self, _request: SuggestionRequest) -> Receiver<SuggestionReply> {
        answered(SuggestionReply::default())
    }
}

fn answered<T: Send + 'static>(reply: T) -> Receiver<T> {
    let (sender, receiver) = channel();
    let _ = sender.send(reply);
    receiver
}

#[test]
fn the_footer_toggles_the_drafts_languages_and_leaves_every_other_draft_alone() {
    let _language = HoldLanguage::hold();
    let mut window = window(Services::default(), &[Language::EnCa]);
    let first = open(&mut window, "Bonjour le courriel");
    let second = open(&mut window, "Hello the document");
    assert!(window.state_mut().session.show_compose(first), "the first message is open");
    window.run_steps(6);
    let before = serde_json::to_value(&window.state().session.composing_by_id(first).expect("the draft").body).expect("the draft's text");

    // The footer is on screen for the message on screen, and for no other: a draft's languages are
    // not shown against a window that is writing another one.
    let at = rect(&mut window, &selector(first));
    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(WIDTH, HEIGHT));
    assert!(screen.contains_rect(at), "the footer is on screen, under the body: {at:?} in {screen:?}");
    assert!(optional_rect(&mut window, &selector(second)).is_none(), "a draft that is not on screen has no footer on it");

    // English is what this draft opened with; French is chosen beside it, not instead of it.
    click_key(&mut window, &selector(first));
    window.run_steps(4);
    for code in ["en-CA", "fr-CA", "en-US", "fr-FR"] {
        rect(&mut window, &language_key(first, code));
    }
    click_key(&mut window, &language_key(first, "fr-CA"));
    window.run_steps(4);
    assert_eq!(
        chosen(&window, first).map(|list| ordered(&list)),
        Some(vec![Language::EnCa, Language::FrCa]),
        "English and French are chosen together"
    );
    assert_eq!(effective(&window, first).len(), 2);

    // Nothing else moved: not the other draft, not the defaults, not the words, not the window.
    assert_eq!(chosen(&window, second), Some(vec![Language::EnCa]), "the other draft keeps its original defaults snapshot");
    assert_eq!(effective(&window, second), vec![Language::EnCa], "the other draft is read as it was");
    assert_eq!(window.state().session.prefs.proof.languages, vec![Language::EnCa], "File › Options did not move");
    assert!(
        !window.state().session.prefs.proof.check_spelling && !window.state().session.prefs.proof.check_grammar,
        "choosing a language starts no dictionary and no server"
    );
    assert_eq!(window.state().session.compose_id(), Some(first), "the window is writing the same message");
    assert_eq!(window.state().session.composing_count(), 2, "no message was opened or closed");
    let after = serde_json::to_value(&window.state().session.composing_by_id(first).expect("the draft").body).expect("the draft's text");
    assert_eq!(after, before, "choosing a language changes no word of the draft");

    // One language may be taken away, and the last one may not: proofing always has one.
    toggle(&mut window, first, "en-CA");
    assert_eq!(chosen(&window, first).map(|list| ordered(&list)), Some(vec![Language::FrCa]), "English alone was taken away");
    toggle(&mut window, first, "fr-CA");
    assert_eq!(
        chosen(&window, first).map(|list| ordered(&list)),
        Some(vec![Language::FrCa]),
        "the last language is kept, refused rather than left with none"
    );
    assert_eq!(effective(&window, first), vec![Language::FrCa], "and it is what proofing still uses");
}

#[test]
fn a_draft_shows_the_defaults_it_was_opened_with_and_a_change_at_its_footer_writes_only_its_own_choice() {
    let _language = HoldLanguage::hold();
    let mut window = window(Services::default(), &[Language::FrCa, Language::EnUs]);
    let older = open(&mut window, "Bonjour le courriel");
    window.run_steps(6);
    assert_eq!(effective(&window, older), vec![Language::FrCa, Language::EnUs], "opening a message takes the defaults of the moment");
    assert_eq!(
        chosen(&window, older).map(|list| ordered(&list)),
        Some(vec![Language::FrCa, Language::EnUs]),
        "and keeps them on the draft, not by following File › Options"
    );
    rect(&mut window, &selector(older));

    // The defaults change while this message is being written: its footer is the message's own.
    window.state_mut().execute("proof.set", json!({ "languages": ["en-CA"] })).expect("new defaults");
    window.run_steps(6);
    assert_eq!(effective(&window, older), vec![Language::FrCa, Language::EnUs], "the open message keeps what it opened with");
    rect(&mut window, &selector(older));

    // A message opened now takes the new defaults, and a choice made at its footer is its own.
    let newer = open(&mut window, "Hello the document");
    window.run_steps(6);
    assert_eq!(effective(&window, newer), vec![Language::EnCa], "a message opened after the change is English");
    toggle(&mut window, newer, "fr-FR");
    assert_eq!(
        chosen(&window, newer).map(|list| ordered(&list)),
        Some(vec![Language::EnCa, Language::FrFr]),
        "the newer message has French of its own"
    );
    assert_eq!(chosen(&window, older).map(|list| ordered(&list)), Some(vec![Language::FrCa, Language::EnUs]), "the older message is untouched");
    assert_eq!(effective(&window, older), vec![Language::FrCa, Language::EnUs]);
    assert_eq!(window.state().session.prefs.proof.languages, vec![Language::EnCa], "the footer wrote the draft, never the defaults");
}

#[test]
fn the_host_is_asked_for_the_languages_of_the_draft_it_checks() {
    let _language = HoldLanguage::hold();
    let seen = Arc::new(Seen::default());
    let services = Services { proof_native: Some(Arc::new(Fake { seen: Arc::clone(&seen) })), ..Services::default() };
    let mut window = window(services, &[Language::EnCa]);
    // Grammar is the provider that carries a language list; spelling is off, so one kind of
    // request is on record. The address is this computer's, as the settings allow, and the host
    // answers it from memory.
    window.state_mut().session.prefs.proof.check_spelling = false;
    window.state_mut().session.prefs.proof.check_grammar = true;
    window.state_mut().session.prefs.proof.managed_server = false;
    window.state_mut().session.prefs.proof.grammar_url = "http://127.0.0.1:8081".into();

    let french = open(&mut window, "Bonjour le courriel");
    window.state_mut().execute("compose.set", json!({ "compose": french, "proofLanguages": ["fr-CA"] })).expect("the draft's French");
    window.run_steps(60);
    let english = open(&mut window, "Hello the document");
    window.run_steps(60);

    let asked = seen.grammar();
    let texts: Vec<String> = asked.iter().map(|request| request.text.clone()).collect();
    assert!(
        !asked.is_empty(),
        "the window asked its host for nothing: no request reached the provider, so nothing about its languages can be judged"
    );
    assert!(texts.iter().any(|text| text.contains("Bonjour")), "the French draft was checked: {texts:?}");
    assert!(texts.iter().any(|text| text.contains("Hello")), "the English draft was checked: {texts:?}");

    // What the host was asked is what proofing uses for that draft, not the global defaults.
    for request in asked.iter().filter(|request| request.text.contains("Bonjour")) {
        assert_eq!(request.languages, vec![Language::FrCa], "the French draft is asked for in French alone, not in File › Options' English");
    }
    for request in asked.iter().filter(|request| request.text.contains("Hello")) {
        assert_eq!(request.languages, vec![Language::EnCa], "the other draft is still English");
    }
    assert_eq!(window.state().session.proof_settings_for(french).languages, vec![Language::FrCa], "the engine's own answer for the draft is French");
    assert_eq!(window.state().session.proof_settings_for(english).languages, vec![Language::EnCa]);
    assert_eq!(window.state().session.prefs.proof.languages, vec![Language::EnCa], "no request rewrote the defaults");
}

#[test]
fn a_long_message_scrolls_under_a_footer_that_stays_and_keeps_the_keyboard_in_the_body() {
    let _language = HoldLanguage::hold();
    let mut window = window(Services::default(), &[Language::EnCa]);
    // Long enough that the body is laid out far past the window: the footer must not scroll with it.
    let long: String = std::iter::repeat_n("Bonjour le courriel.\n", 400).collect();
    let id = open(&mut window, &long);
    window.run_steps(6);
    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(WIDTH, HEIGHT));

    // A press in the text gives the body the keyboard; the footer is then drawn every frame, and
    // never takes it away from the words being written.
    let body = rect(&mut window, "composeBody");
    click(&mut window, egui::pos2(body.min.x + 160.0, body.min.y + 20.0));
    window.run_steps(2);
    assert!(body_focused(&mut window), "a press in the text gives it the keyboard");
    let cursor = window.state().session.composing_by_id(id).expect("the draft").body.selection();
    let before = serde_json::to_value(&window.state().session.composing_by_id(id).expect("the draft").body).expect("the draft's text");
    let at = rect(&mut window, &selector(id));
    window.run_steps(10);
    assert!(body_focused(&mut window), "drawing the footer does not take the keyboard from the body");
    assert_eq!(window.state().session.composing_by_id(id).expect("the draft").body.selection(), cursor, "the caret stays where the owner left it");
    let after = serde_json::to_value(&window.state().session.composing_by_id(id).expect("the draft").body).expect("the draft's text");
    assert_eq!(after, before, "drawing the footer changes no word of the draft");

    // Still the last thing under the body, on screen, after the body has scrolled: not inside the
    // ScrollArea, where it would have gone out of view.
    assert!(screen.contains_rect(at), "the footer is on screen: {at:?} in {screen:?}");
    assert!(at.top() > HEIGHT - 80.0, "the footer sits at the bottom of the composer: {at:?}");
    assert!(at.bottom() <= HEIGHT);
    assert_eq!(rect(&mut window, &selector(id)), at, "the footer keeps its place while the body scrolls");
}

#[test]
fn a_message_in_a_window_of_its_own_has_the_same_footer_and_writes_only_its_own_languages() {
    let _language = HoldLanguage::hold();
    let mut window = window(Services::default(), &[Language::EnCa]);
    // File › Options › Mail › Open new messages, replies and forwards in a new window: the owner's
    // New Email opens one.
    window.state_mut().ui.replies_in_window = true;
    let answer = window.state_mut().run("mail.new", json!({})).expect("a new message in its own window");
    let id = answer["compose"].as_u64().expect("the new message's id");
    window.state_mut().execute("compose.set", json!({ "compose": id, "body": "Bonjour le courriel" })).expect("its text");
    window.state_mut().ui.replies_in_window = false;
    window.run_steps(8);
    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(WIDTH, HEIGHT));
    let Some((at, number)) = rect_and_window(&mut window, &selector(id)) else {
        panic!("a detached composer has no footer for message {id}");
    };
    assert_ne!(number, 0, "the footer is drawn in the message's own window");
    assert!(screen.contains_rect(at), "the footer is on screen in that window: {at:?}");

    click_key(&mut window, &selector(id));
    window.run_steps(4);
    for code in ["en-CA", "fr-CA", "en-US", "fr-FR"] {
        rect(&mut window, &language_key(id, code));
    }
    click_key(&mut window, &language_key(id, "fr-CA"));
    window.run_steps(4);
    assert_eq!(chosen(&window, id).map(|list| ordered(&list)), Some(vec![Language::EnCa, Language::FrCa]));
    assert_eq!(window.state().session.prefs.proof.languages, vec![Language::EnCa], "the window's footer did not write File › Options");
    assert_eq!(window.state().session.composing_count(), 1);
}

#[test]
fn all_languages_remain_reachable_in_a_narrow_composer() {
    let _language = HoldLanguage::hold();
    for width in [900.0, 760.0] {
        let mut window = window_sized(Services::default(), &Language::ALL, width, HEIGHT);
        let id = open(&mut window, "Bonjour. Hello.");
        window.run_steps(6);
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, HEIGHT));
        let at = rect(&mut window, &selector(id));
        assert!(screen.contains_rect(at), "all four languages must fit within the visible composer: {at:?} in {screen:?}");
        click_key(&mut window, &selector(id));
        window.run_steps(4);
        for language in Language::ALL {
            assert!(screen.contains_rect(rect(&mut window, &language_key(id, language.code()))));
        }
    }
}

/// The four proofing languages as the French window names them, beside the English the model
/// keeps for its own metadata (`Language::name`).
const FRENCH_NAMES: [(&str, &str); 4] = [
    ("English (Canada)", "Anglais (Canada)"),
    ("French (Canada)", "Français (Canada)"),
    ("English (United States)", "Anglais (États-Unis)"),
    ("French (France)", "Français (France)"),
];

/// Every piece of text the last frame painted, as it was drawn. The footer's caption is painted
/// with its own painter rather than laid out as a widget, so this is where its words are read.
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

/// A French window names the languages its footer shows in French — the model's English names are
/// never painted — and the caption is reachable by the same words a screen reader is given.
#[test]
fn a_french_footer_caption_names_the_chosen_languages_in_french() {
    let _language = HoldLanguage::hold();
    let mut window = window_in(Services::default(), &[Language::FrCa, Language::EnUs], WIDTH, HEIGHT, "fr");
    let id = open(&mut window, "Bonjour le courriel");
    window.run_steps(6);
    rect(&mut window, &selector(id));
    let caption = "Français (Canada), Anglais (États-Unis)";
    assert!(painted(&window, caption), "the French footer does not name its languages in French; it painted {:?}", painted_texts(&window));
    for (english, _) in FRENCH_NAMES {
        assert!(!painted(&window, english), "the French footer still paints the model's English {english:?}");
    }
    assert!(
        window.query_all_by_label_contains(caption).next().is_some(),
        "the selector's own words are not the French caption: {:?}",
        painted_texts(&window)
    );
}

/// The list the footer opens offers every one of the four languages under its French name, and
/// none of them under its English one.
#[test]
fn a_french_footer_lists_every_choice_under_its_french_name() {
    let _language = HoldLanguage::hold();
    let mut window = window_in(Services::default(), &[Language::EnCa], WIDTH, HEIGHT, "fr");
    let id = open(&mut window, "Bonjour le courriel");
    click_key(&mut window, &selector(id));
    window.run_steps(4);
    for (language, (_, french)) in Language::ALL.iter().zip(FRENCH_NAMES) {
        rect(&mut window, &language_key(id, language.code()));
        assert!(
            window.query_all_by_label_contains(french).next().is_some(),
            "the list does not offer {french:?} in French: {:?}",
            painted_texts(&window)
        );
    }
    for (english, _) in FRENCH_NAMES {
        assert!(window.query_all_by_label_contains(english).next().is_none(), "the list still offers {english:?} in English");
    }
}

/// Choosing a language at a French footer is answered in French: the caption names the pair in
/// French, the draft keeps both, and no English name is drawn.
#[test]
fn a_french_footer_names_a_language_chosen_from_the_list_in_french() {
    let _language = HoldLanguage::hold();
    let mut window = window_in(Services::default(), &[Language::EnCa], WIDTH, HEIGHT, "fr");
    let id = open(&mut window, "Bonjour le courriel");
    toggle(&mut window, id, "fr-CA");
    assert_eq!(chosen(&window, id).map(|list| ordered(&list)), Some(vec![Language::EnCa, Language::FrCa]));
    let caption = "Anglais (Canada), Français (Canada)";
    assert!(painted(&window, caption), "the caption after a choice is not French: {:?}", painted_texts(&window));
    for (english, _) in FRENCH_NAMES {
        assert!(!painted(&window, english), "the French footer still paints {english:?} after the choice");
    }
}

/// The catalogue itself carries the four names in French: the window can only show them if the
/// extractor sees the literals and the French catalogue has a translation for each.
#[test]
fn the_four_proofing_languages_have_french_catalogue_entries() {
    let _language = HoldLanguage::hold();
    let french = mailcraft_ui_egui::i18n::Lang::French;
    for (english, name) in FRENCH_NAMES {
        assert_eq!(french.t(english), name, "the French catalogue has no entry for {english:?}");
    }
}
