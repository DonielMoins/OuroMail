// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! What a message being written does in a reading pane too narrow for its whole row of actions,
//! and at the footer's proofing languages: every control the owner needs — Send, the way out, and
//! the other actions — is painted where it can be pressed, inside the composer's own pane, in the
//! narrowest window the program opens (760 points) as in a wide one, in either appearance and in
//! the interface's longer words. Nothing overlaps, nothing runs past the pane's edge, and what is
//! moved behind `…` is still one press away and does what it did in the row.
//!
//! The footer's `▾` is a dropdown affordance drawn apart from the languages' names: those may be
//! elided to fit, and the arrow must be painted all the same. The tests read the rectangles the
//! owner presses (the hit map agents use) and the shapes the frame really drew, so a control whose
//! hit rectangle is inside the pane while its arrow is cut off still fails.

use egui::{Event, Modifiers, PointerButton};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use mailcraft_engine::Session;
use mailcraft_model::proof::Language;
use mailcraft_ui_egui::{MailApp, Services, control};
use serde_json::{Value, json};
use std::sync::{Mutex, MutexGuard};

/// Wednesday 2026-10-14 16:00 UTC, the sample mailbox's clock.
const NOW: i64 = 1_791_993_600;
const HEIGHT: f32 = 900.0;
/// The narrowest window the program opens (`window_geometry::MIN_SIZE`).
const NARROW: f32 = 760.0;
/// A pane wide enough for the whole row.
const WIDE: f32 = 1440.0;

type Window = Harness<'static, MailApp>;

/// Every frame puts its window's language in force for the whole test process. Hold this across
/// each test, including English-only tests, so another window cannot change it during painting.
static LANGUAGE: Mutex<()> = Mutex::new(());

struct HoldLanguage {
    _guard: MutexGuard<'static, ()>,
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

/// The window these tests draw in: on the sample mailbox, its proofing providers off, as the
/// given theme and interface language, with `defaults` as the languages proofing prefers.
fn window(width: f32, theme: &str, language: &str, defaults: &[Language]) -> Window {
    window_split(width, theme, language, defaults, 230.0, 380.0)
}

/// As [`window`], with the folder pane `folder` wide and the message list `list` wide. They are
/// set before the first frame: a panel takes its width when it is first laid out, so a width set
/// after the harness has drawn would never reach it.
fn window_split(width: f32, theme: &str, language: &str, defaults: &[Language], folder: f32, list: f32) -> Window {
    let mut app = MailApp::new(Session::at(mailcraft_engine::sample::mailbox(NOW, 0), NOW, 0), Services::default());
    app.autosave = false;
    app.session.driver.set_auto_sync(false);
    app.ui.folder_pane_width = folder;
    app.ui.list_width = list;
    app.session.prefs.proof.languages = defaults.to_vec();
    app.session.prefs.proof.check_spelling = false;
    app.session.prefs.proof.check_grammar = false;
    app.execute("ui.theme", json!({ "value": theme })).expect("the appearance");
    app.execute("ui.language", json!({ "language": language })).expect("the interface language");
    let mut window = Harness::builder()
        .with_accessibility_check(false)
        .allow_missing_glyphs()
        .with_size(egui::vec2(width, HEIGHT))
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

/// A new message being written in the reading pane, with all four proofing languages and `text`
/// in it: the longest caption the footer can have. Its id.
fn open(window: &mut Window, text: &str) -> u64 {
    let answer = window.state_mut().execute("mail.new", json!({})).expect("a new message");
    let id = answer["compose"].as_u64().expect("the new message's id");
    let codes: Vec<&str> = Language::ALL.iter().map(|language| language.code()).collect();
    window.state_mut().execute("compose.set", json!({ "compose": id, "body": text, "proofLanguages": codes })).expect("its text and languages");
    window.run_steps(6);
    id
}

fn key(compose: u64, what: &str) -> String {
    format!("composeAction:{compose}:{what}")
}

fn menu_key(compose: u64, what: &str) -> String {
    format!("composeActionMenu:{compose}:{what}")
}

fn arrow_key(compose: u64) -> String {
    format!("proofComposeArrow:{compose}")
}

fn selector_key(compose: u64) -> String {
    format!("proofComposeLanguages:{compose}")
}

fn screen(width: f32) -> egui::Rect {
    egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, HEIGHT))
}

/// Everything drawn inside `outer`'s top level, as shapes were painted, with the clip each was
/// given: the frame's own shapes, not the hit map.
fn painted(window: &Window) -> Vec<(egui::Shape, egui::Rect)> {
    window.output().shapes.iter().map(|clipped| (clipped.shape.clone(), clipped.clip_rect)).collect()
}

/// The shapes of a frame drawn inside `at` (a path's own points, not its bounding box alone).
fn shapes_in(shapes: &[(egui::Shape, egui::Rect)], at: egui::Rect) -> Vec<egui::Shape> {
    fn points(shape: &egui::Shape, out: &mut Vec<egui::Pos2>) {
        match shape {
            egui::Shape::Path(path) => out.extend(path.points.iter().copied()),
            egui::Shape::Vec(shapes) => shapes.iter().for_each(|shape| points(shape, out)),
            egui::Shape::LineSegment { points: [a, b], .. } => out.extend([*a, *b]),
            egui::Shape::Rect(rect) => out.extend([rect.rect.min, rect.rect.max]),
            _ => {}
        }
    }
    shapes
        .iter()
        .filter(|(shape, clip)| {
            let mut at_points = Vec::new();
            points(shape, &mut at_points);
            !at_points.is_empty() && at_points.iter().all(|p| at.contains(*p)) && clip.intersects(at)
        })
        .map(|(shape, _)| shape.clone())
        .collect()
}

/// The text of every shape painted, with where it was painted.
fn painted_texts(shapes: &[(egui::Shape, egui::Rect)]) -> Vec<(String, egui::Rect)> {
    fn walk(shape: &egui::Shape, out: &mut Vec<(String, egui::Rect)>) {
        match shape {
            egui::Shape::Text(text) => out.push((text.galley.text().to_string(), text.galley.rect.translate(text.pos.to_vec2()))),
            egui::Shape::Vec(shapes) => shapes.iter().for_each(|shape| walk(shape, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    for (shape, _) in shapes {
        walk(shape, &mut out);
    }
    out
}

/// The rendered action bar and its controls all live inside the composer's own pane, and none of
/// them is drawn over another, at 900 points and at the narrowest window, in either appearance,
/// with the interface in English and in French (whose words are longer).
#[test]
fn a_narrow_composer_keeps_every_action_inside_its_pane_without_overlap() {
    let _language = HoldLanguage::hold();
    for (width, theme, language) in [(900.0, "dark", "en"), (NARROW, "dark", "en"), (900.0, "light", "fr"), (NARROW, "light", "fr")] {
        let mut window = window(width, theme, language, &Language::ALL);
        let id = open(&mut window, "Bonjour. Hello.");
        let whole = screen(width);
        let bar = rect(&mut window, &format!("composeActionBar:{id}"));
        assert!(whole.contains_rect(bar), "at {width} {theme} {language}: the bar is on screen: {bar:?} in {whole:?}");
        assert!(bar.width() > 100.0 && bar.max.x <= whole.max.x, "at {width}: the bar is the pane, no wider: {bar:?}");

        // Send is a button with its word on it, and the way out is at the bar's end.
        let send = rect(&mut window, &key(id, "send"));
        let close = rect(&mut window, &key(id, "close"));
        let pop = rect(&mut window, &key(id, "popOut"));
        let more = rect(&mut window, &key(id, "more"));
        assert!(send.width() >= 44.0 && send.height() >= 24.0, "at {width} {language}: Send is a button: {send:?}");
        let mut shown = vec![("send", send), ("more", more), ("close", close), ("popOut", pop)];
        if let Some(title) = optional_rect(&mut window, &key(id, "title")) {
            shown.push(("title", title));
        }
        for (name, at) in &shown {
            assert!(bar.contains_rect(*at), "at {width} {theme} {language}: {name} is inside the bar: {at:?} in {bar:?}");
            assert!(whole.contains_rect(*at), "at {width} {theme} {language}: {name} is on screen: {at:?}");
        }
        // Nothing is drawn over anything else, and nothing runs past the right edge.
        for (i, (one, a)) in shown.iter().enumerate() {
            for (other, b) in shown.iter().skip(i + 1) {
                assert!(!a.intersects(*b), "at {width} {theme} {language}: {one} {a:?} overlaps {other} {b:?}");
            }
            assert!(a.max.x <= bar.max.x - 0.5, "at {width} {theme} {language}: {one} runs past the pane's edge: {a:?} in {bar:?}");
        }
        // The three that give way are not drawn separately at this width: they are behind `…`.
        for what in ["attach", "saveDraft", "discard"] {
            assert!(optional_rect(&mut window, &key(id, what)).is_none(), "at {width} {theme} {language}: {what} is in the row it does not fit in");
        }
        // And the row really is one row: nothing pokes above or below it.
        assert!(send.min.y >= bar.min.y && send.max.y <= bar.max.y, "at {width}: Send is inside the bar's height: {send:?} in {bar:?}");
    }
}

/// Wide, the bar keeps every action in the row with its word, and no `…`: the narrow arrangement
/// is a way of fitting, not the new shape of the bar.
#[test]
fn a_wide_composer_keeps_the_whole_row_of_actions_with_their_words() {
    let _language = HoldLanguage::hold();
    let mut window = window(WIDE, "dark", "en", &[Language::EnCa]);
    let id = open(&mut window, "Hello the document.");
    let whole = screen(WIDE);
    let bar = rect(&mut window, &format!("composeActionBar:{id}"));
    let mut shown = Vec::new();
    for what in ["send", "attach", "saveDraft", "discard", "close", "popOut"] {
        let at = rect(&mut window, &key(id, what));
        assert!(bar.contains_rect(at) && whole.contains_rect(at), "{what} is in the bar: {at:?}");
        shown.push((what, at));
    }
    assert!(optional_rect(&mut window, &key(id, "more")).is_none(), "a wide bar needs no `…`");
    // Send, Attach, Save draft and Discard, in that order, each with room for its word.
    let words = ["send", "attach", "saveDraft", "discard"];
    for pair in words.windows(2) {
        let (a, b) = (rect(&mut window, &key(id, pair[0])), rect(&mut window, &key(id, pair[1])));
        assert!(a.max.x <= b.min.x + 0.5, "{} comes before {}: {a:?} {b:?}", pair[0], pair[1]);
    }
    let send = rect(&mut window, &key(id, "send"));
    assert!(send.width() >= 44.0, "Send's word is drawn: {send:?}");
    for (one, a) in &shown {
        for (other, b) in shown.iter().filter(|(other, _)| other != one) {
            assert!(!a.intersects(*b), "{one} {a:?} overlaps {other} {b:?}");
        }
    }
}

/// What is behind `…` is still one press away, and does what the row's button did: it saves the
/// draft the owner is writing, and it sends nothing.
#[test]
fn the_narrow_overflow_menu_keeps_the_other_actions_working() {
    let _language = HoldLanguage::hold();
    let mut window = window(NARROW, "dark", "en", &[Language::EnCa]);
    let id = open(&mut window, "Unfinished business");
    window.state_mut().execute("compose.set", json!({ "subject": "Unfinished business" })).expect("its subject");
    let before = window.state().session.mailbox.messages.values().filter(|m| !m.draft).count();
    assert!(window.state().session.composing_by_id(id).is_some_and(|c| c.draft.is_none()), "nothing is saved yet");

    click_key(&mut window, &key(id, "more"));
    window.run_steps(4);
    for what in ["attach", "saveDraft", "discard"] {
        rect(&mut window, &menu_key(id, what));
    }
    click_key(&mut window, &menu_key(id, "saveDraft"));
    window.run_steps(6);

    let draft = window.state().session.mailbox.messages.values().find(|m| m.draft && m.subject == "Unfinished business").map(|m| m.id);
    assert!(draft.is_some(), "Save draft, behind `…`, writes the draft");
    assert_eq!(window.state().session.composing_by_id(id).and_then(|c| c.draft), draft, "the message being written knows its draft");
    assert_eq!(window.state().session.compose_id(), Some(id), "the message is still being written");
    let after = window.state().session.mailbox.messages.values().filter(|m| !m.draft).count();
    assert_eq!(after, before, "nothing was sent");
    assert_eq!(
        window.state().session.composing_by_id(id).map(|c| c.body.plain()),
        Some("Unfinished business".to_string()),
        "the words are as they were"
    );
}

/// Every control a frame records for `id`'s bar, by the name the bar gives it: whichever of them
/// the arrangement drew (a control in `…` is not in the bar and has no rectangle here).
fn bar_controls(window: &mut Window, id: u64) -> Vec<(&'static str, egui::Rect)> {
    let mut out = Vec::new();
    for what in ["send", "more", "attach", "saveDraft", "discard", "popOut", "close", "title"] {
        if let Some(at) = optional_rect(window, &key(id, what)) {
            out.push((what, at));
        }
    }
    out
}

/// The reading pane at its narrowest the splitters can reach: the window at its least, the folder
/// pane at its widest cap (420) and the message list at its widest, and at its floor. The modules'
/// rail and the window's own frame take their room too, so that pane is under 20 points at the
/// list's widest and about 36 at its floor — less than `…` and Close themselves need. Whether it
/// draws one menu or `…` and Close, every rectangle the bar records is inside it, on screen, of a
/// size that is not negative, and clear of the others. The last case is a window below the program's
/// least, so that the pane has no width at all: there is nothing to press in it, and still no
/// control is drawn at a negative size or outside the bar.
#[test]
fn a_composer_at_the_narrowest_reading_pane_keeps_every_control_inside_the_bar() {
    let _language = HoldLanguage::hold();
    // (window width, folder pane, message list, the widest the bar may be for this to be the
    // compressed pane, theme, interface language).
    for (width, folder, list, most, theme, language) in [
        (NARROW, 420.0, 380.0, 20.0, "dark", "en"),
        (NARROW, 420.0, 240.0, 40.0, "light", "fr"),
        (830.0, 420.0, 380.0, 100.0, "dark", "en"),
        (900.0, 420.0, 240.0, 180.0, "light", "fr"),
        (720.0, 420.0, 380.0, 2.0, "dark", "en"),
    ] {
        let mut window = window_split(width, theme, language, &Language::ALL, folder, list);
        let id = open(&mut window, "Bonjour. Hello.");
        let whole = screen(width);
        let bar = rect(&mut window, &format!("composeActionBar:{id}"));
        assert!(whole.contains_rect(bar), "at {width} folder {folder} list {list}: the bar is on screen: {bar:?}");
        let (folder_shown, list_shown) = (window.state().ui.folder_pane_width, window.state().ui.list_width);
        // The fixture is the compressed pane this test is about — not the default one.
        assert!(bar.width() <= most, "at {width} folder {folder} list {list} (drawn {folder_shown} {list_shown}): the pane is compressed: {bar:?}");

        let shown = bar_controls(&mut window, id);
        assert!(!shown.is_empty(), "at {width} folder {folder} list {list}: the bar draws something");
        for (name, at) in &shown {
            assert!(at.width() >= 0.0 && at.height() >= 0.0, "at {width} folder {folder} list {list}: {name} has a size: {at:?}");
            assert!(bar.contains_rect(*at), "at {width} folder {folder} list {list}: {name} is inside the bar: {at:?} in {bar:?}");
            assert!(whole.contains_rect(*at), "at {width} folder {folder} list {list}: {name} is on screen: {at:?}");
        }
        for (i, (one, a)) in shown.iter().enumerate() {
            for (other, b) in shown.iter().skip(i + 1) {
                assert!(!a.intersects(*b), "at {width} folder {folder} list {list}: {one} {a:?} overlaps {other} {b:?}");
            }
        }
        if bar.width() >= 50.0 {
            // Room for `…` and Close: the way out is drawn in the bar.
            assert!(shown.iter().any(|(what, _)| *what == "close"), "at {width} folder {folder} list {list}: Close is in the bar");
        } else {
            // Less room than `…` and Close need: the bar draws the one menu that holds them all.
            assert!(shown.iter().any(|(what, _)| *what == "more"), "at {width} folder {folder} list {list}: the bar draws its one menu");
        }
    }
}

/// When the row cannot hold Send beside `…` and Close, Send is not drawn crossing them: it is in
/// `…`, with the other actions and the way out, and everything behind `…` is still one press away
/// — Save draft included, which writes the draft, leaves the words and the caret as they were,
/// and sends nothing.
#[test]
fn the_overflow_menu_at_the_narrowest_pane_holds_every_action_the_bar_cannot() {
    let _language = HoldLanguage::hold();
    for list in [380.0, 240.0] {
        let mut window = window_split(NARROW, "dark", "en", &Language::ALL, 420.0, list);
        let id = open(&mut window, "Unfinished business");
        window.state_mut().execute("compose.set", json!({ "subject": "Unfinished business" })).expect("its subject");
        window.run_steps(2);
        let bar = rect(&mut window, &format!("composeActionBar:{id}"));
        assert!(bar.width() < 50.0, "list {list}: the pane has less room than `…` and Close need: {bar:?}");
        let body = |window: &Window| window.state().session.composing_by_id(id).map(|c| serde_json::to_value(&c.body).expect("the body"));
        let caret = |window: &Window| window.state().session.composing_by_id(id).map(|c| c.body.selection());
        let (words, at) = (body(&window), caret(&window));
        let before = window.state().session.mailbox.messages.values().filter(|m| !m.draft).count();

        // What the bar cannot hold is not in it...
        for what in ["send", "popOut", "close"] {
            assert!(optional_rect(&mut window, &key(id, what)).is_none(), "list {list}: {what} is not drawn in a bar with no room for it");
        }
        // ...and the one menu the bar drew holds all of it, on screen, Close included.
        click_key(&mut window, &key(id, "more"));
        window.run_steps(4);
        for what in ["send", "attach", "saveDraft", "discard", "popOut", "close"] {
            let at = rect(&mut window, &menu_key(id, what));
            assert!(screen(NARROW).contains_rect(at), "list {list}: {what} in `…` is on screen: {at:?}");
        }

        click_key(&mut window, &menu_key(id, "saveDraft"));
        window.run_steps(6);
        let draft = window.state().session.mailbox.messages.values().find(|m| m.draft && m.subject == "Unfinished business").map(|m| m.id);
        assert!(draft.is_some(), "list {list}: Save draft, behind `…`, writes the draft");
        assert_eq!(window.state().session.composing_by_id(id).and_then(|c| c.draft), draft, "list {list}: the message being written knows its draft");
        assert_eq!(window.state().session.compose_id(), Some(id), "list {list}: the message is still being written");
        assert_eq!(window.state().session.mailbox.messages.values().filter(|m| !m.draft).count(), before, "list {list}: nothing was sent");
        assert_eq!(body(&window), words, "list {list}: the overflow changed no word of the draft");
        assert_eq!(caret(&window), at, "list {list}: and left the caret where it was");
    }
}

/// The title over the lines was a label before the bar painted it, and it still is one: what the
/// bar paints for "New message  Not saved" is reachable by that name, over the rectangle it senses
/// the pointer in — inside the bar, never wider than the words it drew.
#[test]
fn the_painted_title_is_still_a_label_to_a_screen_reader() {
    let _language = HoldLanguage::hold();
    let mut window = window(WIDE, "dark", "en", &[Language::EnCa]);
    let id = open(&mut window, "Hello the document.");
    let bar = rect(&mut window, &format!("composeActionBar:{id}"));
    let at = rect(&mut window, &key(id, "title"));
    assert!(bar.contains_rect(at) && at.width() > 0.0, "the title has a rectangle inside the bar: {at:?} in {bar:?}");
    assert!(window.query_by_label("New message  Not saved").is_some(), "a screen reader reaches the bar's title by its words");
}

/// The footer's arrow is drawn on its own, right of the languages' names: the names may be elided
/// to fit (all four of them, in the narrowest window), and the `▾` is painted all the same, inside
/// the selector, with no name drawn over it.
#[test]
fn the_proofing_arrow_is_painted_apart_from_the_names_it_follows() {
    let _language = HoldLanguage::hold();
    for width in [900.0, NARROW] {
        for theme in ["light", "dark"] {
            let mut window = window(width, theme, "en", &Language::ALL);
            let id = open(&mut window, "Bonjour. Hello.");
            let whole = screen(width);
            let selector = rect(&mut window, &selector_key(id));
            let arrow = rect(&mut window, &arrow_key(id));
            assert!(whole.contains_rect(selector), "at {width} {theme}: the selector is on screen: {selector:?}");
            assert!(selector.contains_rect(arrow), "at {width} {theme}: the arrow is inside the selector: {arrow:?} in {selector:?}");
            assert!(arrow.width() >= 6.0 && arrow.height() >= 6.0, "at {width} {theme}: the arrow has a size: {arrow:?}");

            // The frame really paints the arrow there: a path of its own inside the arrow's place.
            let shapes = painted(&window);
            assert!(!shapes_in(&shapes, arrow).is_empty(), "at {width} {theme}: nothing is painted where the arrow is: {arrow:?}");

            // And the names end before it: what was elided is the caption, never the arrow.
            let names: Vec<(String, egui::Rect)> = painted_texts(&shapes).into_iter().filter(|(text, _)| text.contains("English (Canada)")).collect();
            assert!(!names.is_empty(), "at {width} {theme}: the footer names the languages chosen");
            for (text, at) in names {
                assert!(at.max.x <= arrow.min.x + 0.5, "at {width} {theme}: the names {text:?} are not drawn over the arrow: {at:?} vs {arrow:?}");
                assert!(at.max.x <= selector.max.x, "at {width} {theme}: {text:?} is inside the selector: {at:?}");
            }
        }
    }
}

/// At the narrowest reading pane the splitters reach, the languages' names have no room left at
/// all, but the footer's `▾` is still painted — inside the selector, in the room that is left —
/// so the one control that opens the list is never the part elided away.
#[test]
fn the_proofing_arrow_survives_the_narrowest_reading_pane() {
    let _language = HoldLanguage::hold();
    for list in [380.0, 240.0] {
        for theme in ["light", "dark"] {
            let mut window = window_split(NARROW, theme, "en", &Language::ALL, 420.0, list);
            let id = open(&mut window, "Bonjour. Hello.");
            let whole = screen(NARROW);
            let selector = rect(&mut window, &selector_key(id));
            let arrow = rect(&mut window, &arrow_key(id));
            assert!(whole.contains_rect(selector), "list {list} {theme}: the selector is on screen: {selector:?}");
            assert!(
                selector.contains_rect(arrow) && arrow.width() >= 6.0 && arrow.height() >= 6.0,
                "list {list} {theme}: the arrow has its room inside the selector: {arrow:?} in {selector:?}"
            );
            let shapes = painted(&window);
            assert!(!shapes_in(&shapes, arrow).is_empty(), "list {list} {theme}: the arrow is painted: {arrow:?}");
            // Whatever of the names is still drawn ends before it.
            for (text, at) in painted_texts(&shapes) {
                if text.contains("English (Canada)") {
                    assert!(at.max.x <= arrow.min.x + 0.5, "list {list} {theme}: {text:?} is not drawn over the arrow: {at:?} vs {arrow:?}");
                }
            }
        }
    }
}

/// Pressing the footer at a narrow width still chooses a language for this draft alone, and no
/// word of the body (nor the caret in it) moves.
#[test]
fn the_narrow_footer_still_chooses_a_language_and_leaves_the_body_alone() {
    let _language = HoldLanguage::hold();
    let mut window = window(NARROW, "dark", "en", &[Language::EnCa]);
    let id = open(&mut window, "Bonjour. Hello.");
    let body = rect(&mut window, "composeBody");
    click(&mut window, egui::pos2(body.min.x + 90.0, body.min.y + 12.0));
    window.run_steps(2);
    let text = |window: &Window| window.state().session.composing_by_id(id).map(|c| serde_json::to_value(&c.body).expect("the body"));
    let caret = |window: &Window| window.state().session.composing_by_id(id).map(|c| c.body.selection());
    let (words, at) = (text(&window), caret(&window));

    click_key(&mut window, &selector_key(id));
    window.run_steps(4);
    click_key(&mut window, &format!("proofComposeLanguage:{id}:fr-CA"));
    window.run_steps(4);

    assert_eq!(text(&window), words, "choosing a language changes no word of the draft");
    assert_eq!(caret(&window), at, "and leaves the caret where it was");
    // This draft opened with all four: French (Canada) taken off leaves the other three.
    let chosen: Vec<Language> = window.state().session.proof_settings_for(id).languages;
    assert_eq!(chosen, vec![Language::EnCa, Language::EnUs, Language::FrFr], "the one pressed was taken away, beside the others which stay");
    // Pressing it again brings it back, and the last language still cannot be taken away.
    click_key(&mut window, &format!("proofComposeLanguage:{id}:fr-CA"));
    window.run_steps(4);
    assert_eq!(window.state().session.proof_settings_for(id).languages.len(), Language::ALL.len(), "all four are chosen again");
    assert_eq!(text(&window), words, "still no word of the draft changed");
}

/// At the narrowest window, with the panes where they open, the bar has no room for the title:
/// the overflow control's menu must still carry the whole of it, `New message  Not saved`, as a
/// label a screen reader reaches — not only as the actions' buttons. The bar stays the narrow
/// arrangement it was, and opening the menu changes no word in the message.
#[test]
fn the_narrow_overflow_menu_gives_the_unsaved_state_a_label() {
    let _language = HoldLanguage::hold();
    let mut window = window(NARROW, "dark", "en", &[Language::EnCa]);
    let id = open(&mut window, "Unfinished business");
    window.state_mut().execute("compose.set", json!({ "subject": "Unfinished business" })).expect("its subject");
    window.run_steps(2);
    let whole = screen(NARROW);
    let bar = rect(&mut window, &format!("composeActionBar:{id}"));
    assert!(whole.contains_rect(bar) && bar.width() > 100.0, "the narrow bar is on screen: {bar:?} in {whole:?}");
    // The arrangement the proposal draws at 760 with the default split: Send, `…`, Close and
    // Pop Out in the bar, the three other actions behind `…`, and no title of its own.
    let mut shown = Vec::new();
    for what in ["send", "more", "close", "popOut"] {
        let at = rect(&mut window, &key(id, what));
        assert!(bar.contains_rect(at) && whole.contains_rect(at), "{what} is in the narrow bar: {at:?} in {bar:?}");
        shown.push((what, at));
    }
    for what in ["attach", "saveDraft", "discard", "title"] {
        assert!(optional_rect(&mut window, &key(id, what)).is_none(), "{what} is not drawn in the narrow bar");
    }
    for (i, (one, a)) in shown.iter().enumerate() {
        for (other, b) in shown.iter().skip(i + 1) {
            assert!(!a.intersects(*b), "{one} {a:?} overlaps {other} {b:?}");
        }
    }
    // `…` keeps the name it has; the state is to be added beside it, not in place of it.
    assert!(
        window.query_by_role_and_label(egui::accesskit::Role::Button, "More commands").is_some(),
        "the overflow control is still named More commands"
    );
    let words = window.state().session.composing_by_id(id).map(|c| c.body.plain()).expect("the draft's words");
    click_key(&mut window, &key(id, "more"));
    window.run_steps(4);
    let state = window
        .query_by_role_and_label(egui::accesskit::Role::Label, "New message  Not saved")
        .expect("the `…` menu exposes the whole title and save state as a label");
    let at = state.rect();
    assert!(at.width() > 0.0 && at.height() > 0.0 && whole.contains_rect(at), "the label is drawn on screen: {at:?}");
    assert_eq!(window.state().session.composing_by_id(id).map(|c| c.body.plain()), Some(words), "opening `…` changed no word of the draft");
}

/// Saving the narrow draft through the action behind `…`, closing it and opening it from Drafts
/// leaves the bar the same and gives the whole title and its saved state, `Draft  Saved to
/// Drafts`, as a label in `…`: a screen reader reaches the save state where the bar has no room
/// to draw it. The words are exactly as written and nothing is sent.
#[test]
fn a_reopened_narrow_draft_gives_the_saved_state_a_label() {
    let _language = HoldLanguage::hold();
    let mut window = window(NARROW, "dark", "en", &[Language::EnCa]);
    let id = open(&mut window, "Unfinished business");
    window.state_mut().execute("compose.set", json!({ "subject": "Unfinished business" })).expect("its subject");
    window.run_steps(2);
    let before = window.state().session.mailbox.messages.values().filter(|m| !m.draft).count();
    let words = window.state().session.composing_by_id(id).map(|c| c.body.plain()).expect("the draft's words");

    // Save it through the action the overflow menu already holds.
    click_key(&mut window, &key(id, "more"));
    window.run_steps(4);
    click_key(&mut window, &menu_key(id, "saveDraft"));
    window.run_steps(6);
    let draft = window.state().session.mailbox.messages.values().find(|m| m.draft && m.subject == "Unfinished business").map(|m| m.id);
    assert!(draft.is_some(), "Save draft, behind `…`, writes the draft");
    assert_eq!(window.state().session.composing_by_id(id).and_then(|c| c.draft), draft, "the message being written knows its draft");
    assert_eq!(window.state().session.composing_by_id(id).map(|c| c.dirty), Some(false), "saving makes it clean");
    assert_eq!(window.state().session.composing_by_id(id).map(|c| c.body.plain()), Some(words.clone()), "saving changes no word");
    assert_eq!(window.state().session.mailbox.messages.values().filter(|m| !m.draft).count(), before, "nothing was sent");

    // Close it and open it again from Drafts: `Draft  Saved to Drafts`.
    window.state_mut().execute("compose.close", json!({})).expect("closing the saved draft");
    window.run_steps(2);
    let answer = window.state_mut().execute("mail.editDraft", json!({ "id": draft })).expect("opening the draft from Drafts");
    let reopened = answer["compose"].as_u64().expect("the reopened message's id");
    assert_eq!(window.state().session.composing_by_id(reopened).and_then(|c| c.draft), draft, "the reopened message is that draft");
    assert_eq!(window.state().session.composing_by_id(reopened).map(|c| c.dirty), Some(false), "a saved draft opens saved");
    window.run_steps(6);

    let whole = screen(NARROW);
    let bar = rect(&mut window, &format!("composeActionBar:{reopened}"));
    assert!(whole.contains_rect(bar) && bar.width() > 100.0, "the narrow bar is on screen: {bar:?} in {whole:?}");
    let mut shown = Vec::new();
    for what in ["send", "more", "close", "popOut"] {
        let at = rect(&mut window, &key(reopened, what));
        assert!(bar.contains_rect(at) && whole.contains_rect(at), "{what} is in the narrow bar: {at:?} in {bar:?}");
        shown.push((what, at));
    }
    for what in ["attach", "saveDraft", "discard", "title"] {
        assert!(optional_rect(&mut window, &key(reopened, what)).is_none(), "{what} is not drawn in the narrow bar");
    }
    for (i, (one, a)) in shown.iter().enumerate() {
        for (other, b) in shown.iter().skip(i + 1) {
            assert!(!a.intersects(*b), "{one} {a:?} overlaps {other} {b:?}");
        }
    }
    assert!(
        window.query_by_role_and_label(egui::accesskit::Role::Button, "More commands").is_some(),
        "the overflow control is still named More commands"
    );
    click_key(&mut window, &key(reopened, "more"));
    window.run_steps(4);
    let state = window
        .query_by_role_and_label(egui::accesskit::Role::Label, "Draft  Saved to Drafts")
        .expect("the `…` menu exposes the reopened draft's whole title and saved state as a label");
    let at = state.rect();
    assert!(at.width() > 0.0 && at.height() > 0.0 && whole.contains_rect(at), "the label is drawn on screen: {at:?}");
    assert_eq!(window.state().session.composing_by_id(reopened).map(|c| c.body.plain()), Some(words), "reopening the draft changed no word of it");
    assert_eq!(
        window.state().session.mailbox.message(draft.expect("the draft")).map(|m| m.body.text.clone()),
        Some("Unfinished business".to_string()),
        "the draft in Drafts holds exactly what was written"
    );
    assert_eq!(window.state().session.mailbox.messages.values().filter(|m| !m.draft).count(), before, "still nothing was sent");
}
