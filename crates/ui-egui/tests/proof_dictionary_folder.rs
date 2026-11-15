// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! File › Options › Spelling & Grammar › Dictionary folder, through the window itself: typing
//! survives frames, leaving the field commits nothing, Browse only stages a folder, and `Save`
//! is the one thing that writes the setting. A cancelled dialog leaves both the typed text and
//! the saved folder alone.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use egui::{Event, Modifiers, PointerButton};
use egui_kittest::Harness;
use mailcraft_engine::Session;
use mailcraft_engine::agent::{Access, Class};
use mailcraft_ui_egui::{FileDialogRequest, MailApp, Services, control};
use serde_json::{Value, json};

/// Wednesday 2026-10-14 16:00 UTC, the sample mailbox's clock.
const NOW: i64 = 1_791_993_600;

/// Synthetic folders: nothing here is read or written.
const SAVED: &str = "/synthetic/dictionaries/installed";
const PICKED: &str = "/synthetic/dictionaries/picked";
const TYPED: &str = "/synthetic/dictionaries/typed";

type Window = Harness<'static, MailApp>;

/// A host that answers the folder dialog from a queue, one answer per request, and records what
/// it was asked. No queue left means cancelled.
#[derive(Clone, Default)]
struct Host {
    requests: Arc<Mutex<Vec<FileDialogRequest>>>,
    answers: Arc<Mutex<VecDeque<Option<String>>>>,
}

impl Host {
    fn cancelling() -> Host {
        Host::default()
    }

    fn picking(path: &str) -> Host {
        let host = Host::default();
        host.answers.lock().unwrap().push_back(Some(path.to_string()));
        host
    }

    fn services(&self) -> Services {
        let dialogs = self.clone();
        Services {
            file_dialog: Some(Box::new(move |request| {
                dialogs.requests.lock().unwrap().push(request);
                let answer = dialogs.answers.lock().unwrap().pop_front().flatten().map(|path| vec![path]);
                let (sender, receiver) = std::sync::mpsc::channel();
                let _ = sender.send(answer);
                receiver
            })),
            ..Services::default()
        }
    }

    fn requests(&self) -> Vec<FileDialogRequest> {
        self.requests.lock().unwrap().clone()
    }
}

/// File › Options on the sample mailbox, with no spelling or grammar provider and nothing written
/// to disk.
fn window(services: Services, saved: &str) -> Window {
    let mut app = MailApp::new(Session::at(mailcraft_engine::sample::mailbox(NOW, 0), NOW, 0), services);
    app.autosave = false;
    app.ui.backstage = true;
    app.ui.backstage_page = "options".into();
    app.session.prefs.proof.check_spelling = false;
    app.session.prefs.proof.check_grammar = false;
    app.session.prefs.proof.dictionary_dir = saved.to_string();
    let mut window = Harness::builder()
        .with_accessibility_check(false)
        .allow_missing_glyphs()
        .with_size(egui::vec2(1280.0, 4000.0))
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

/// Focus the field with a real press, select the saved folder, then type over it.
fn type_into_field(window: &mut Window, text: &str) {
    let at = rect(window, "proofDictionaryDir").center();
    click(window, at);
    for pressed in [true, false] {
        window.input_mut().events.push(Event::Key { key: egui::Key::A, physical_key: None, pressed, repeat: false, modifiers: Modifiers::COMMAND });
        window.step();
    }
    type_more(window, text);
}

/// Type into the field that already has the focus, as a later frame's keystrokes arrive.
fn type_more(window: &mut Window, text: &str) {
    window.input_mut().events.push(Event::Text(text.to_string()));
    window.step();
}

/// Leave the field by pressing somewhere else on the page: a blur, not a click on Save.
fn leave_the_field(window: &mut Window) {
    let field = rect(window, "proofDictionaryDir");
    click(window, egui::pos2((field.left() - 24.0).max(1.0), field.center().y));
    window.run_steps(2);
}

fn saved(window: &Window) -> String {
    window.state().session.prefs.proof.dictionary_dir.clone()
}

#[test]
fn typing_survives_intervening_frames_and_only_save_commits_it() {
    let mut window = window(Services::default(), SAVED);
    type_into_field(&mut window, "/synthetic/dictionaries/");
    window.run_steps(4);
    type_more(&mut window, "typed");
    window.run_steps(2);
    assert_eq!(saved(&window), SAVED, "keystrokes alone commit nothing");
    click_key(&mut window, "proofDictionarySave");
    window.run_steps(2);
    assert_eq!(saved(&window), TYPED, "Save writes the text the field has kept across frames");
}

#[test]
fn leaving_the_field_does_not_commit_until_the_explicit_save() {
    let mut window = window(Services::default(), SAVED);
    type_into_field(&mut window, TYPED);
    leave_the_field(&mut window);
    assert_eq!(saved(&window), SAVED, "a blur is not a save");
    window.run_steps(6);
    assert_eq!(saved(&window), SAVED, "and no later frame saves it by itself");
    click_key(&mut window, "proofDictionarySave");
    window.run_steps(2);
    assert_eq!(saved(&window), TYPED, "the explicit Save commits the pending text");
}

#[test]
fn the_owner_command_browses_and_stages_a_folder_without_persisting_it() {
    let host = Host::picking(PICKED);
    let mut window = window(host.services(), SAVED);
    window.state_mut().execute("ui.proofDictionaryFolder", json!({})).expect("ui.proofDictionaryFolder is a UI command with no parameters");
    window.run_steps(4);
    assert_eq!(host.requests(), vec![FileDialogRequest::Folder], "the command asks for a folder, once");
    assert_eq!(saved(&window), SAVED, "browsing stages the choice and writes nothing");
    click_key(&mut window, "proofDictionarySave");
    window.run_steps(2);
    assert_eq!(saved(&window), PICKED, "Save commits the folder the dialog delivered");
}

#[test]
fn browse_cancel_keeps_the_pending_typing_and_the_saved_folder() {
    let host = Host::cancelling();
    let mut window = window(host.services(), SAVED);
    type_into_field(&mut window, TYPED);
    assert_eq!(saved(&window), SAVED);
    click_key(&mut window, "proofDictionaryBrowse");
    window.run_steps(4);
    assert_eq!(host.requests(), vec![FileDialogRequest::Folder], "Browse asks for a folder");
    assert_eq!(saved(&window), SAVED, "a cancelled dialog saves nothing");
    click_key(&mut window, "proofDictionarySave");
    window.run_steps(2);
    assert_eq!(saved(&window), TYPED, "the text typed before the cancelled dialog is still pending");
}

#[test]
fn browse_replaces_the_pending_typing_with_the_picked_folder_but_saves_nothing() {
    let host = Host::picking(PICKED);
    let mut window = window(host.services(), SAVED);
    type_into_field(&mut window, "/synthetic/dictionaries/old");
    click_key(&mut window, "proofDictionaryBrowse");
    window.run_steps(4);
    assert_eq!(host.requests(), vec![FileDialogRequest::Folder]);
    assert_eq!(saved(&window), SAVED, "a picked folder is staged, not saved");
    click_key(&mut window, "proofDictionarySave");
    window.run_steps(2);
    assert_eq!(saved(&window), PICKED, "the picker's folder, not the stale typing, is what Save commits");
}

#[test]
fn without_a_dialog_host_a_typed_folder_is_still_saved_by_hand() {
    let mut window = window(Services::default(), SAVED);
    if let Some(browse) = optional_rect(&mut window, "proofDictionaryBrowse") {
        click(&mut window, browse.center());
        window.run_steps(3);
    }
    type_into_field(&mut window, TYPED);
    assert_eq!(saved(&window), SAVED, "no picker is offered where there is no host");
    click_key(&mut window, "proofDictionarySave");
    window.run_steps(2);
    assert_eq!(saved(&window), TYPED, "manual entry and Save need no dialog host");
}

/// A host that answers when the test says so, or goes away without an answer: what a native
/// dialog does while it is open, and what a host that is closed leaves behind.
#[derive(Clone, Default)]
struct Slow {
    requests: Arc<Mutex<Vec<FileDialogRequest>>>,
    answers: Arc<Mutex<Vec<std::sync::mpsc::Sender<Option<Vec<String>>>>>>,
}

impl Slow {
    fn services(&self) -> Services {
        let host = self.clone();
        Services {
            file_dialog: Some(Box::new(move |request| {
                host.requests.lock().unwrap().push(request);
                let (sender, receiver) = std::sync::mpsc::channel();
                host.answers.lock().unwrap().push(sender);
                receiver
            })),
            ..Services::default()
        }
    }

    fn requests(&self) -> Vec<FileDialogRequest> {
        self.requests.lock().unwrap().clone()
    }

    /// Answer the dialog that is open: a folder chosen, or `None` for a cancel.
    fn answer(&self, path: Option<&str>) {
        let sender = self.answers.lock().unwrap().pop().expect("a dialog is open");
        let _ = sender.send(path.map(|path| vec![path.to_string()]));
    }

    /// Close the dialog without any answer at all: the host went away.
    fn close(&self) {
        self.answers.lock().unwrap().pop().expect("a dialog is open");
    }
}

/// A folder path of exactly `bytes` bytes, made of a prefix and copies of `fill`.
fn folder_of(bytes: usize, fill: &str) -> String {
    let mut path = "/synthetic/".to_string();
    while path.len() + fill.len() <= bytes {
        path.push_str(fill);
    }
    while path.len() < bytes {
        path.push('x');
    }
    assert_eq!(path.len(), bytes);
    path
}

#[test]
fn the_command_takes_no_parameters_and_is_never_an_agents() {
    let host = Host::picking(PICKED);
    let mut window = window(host.services(), SAVED);
    // Anything but nothing is refused before the host is asked for anything.
    for params in [json!({"path": PICKED}), json!({"junk": true})] {
        assert!(window.state_mut().execute("ui.proofDictionaryFolder", params.clone()).is_err(), "{params}");
    }
    assert!(host.requests().is_empty(), "a refused call opens no dialog");
    // The engine's own registry says the same: a dialog on the owner's screen is Never an agent's.
    assert!(matches!(mailcraft_engine::agent::access("ui.proofDictionaryFolder"), Some(Access::Never)));
    // Nor is it the agent's even when it was granted every class of permission.
    window.state_mut().session.agent = Some(Class::ALL.to_vec());
    assert!(window.state_mut().execute("ui.proofDictionaryFolder", json!({})).is_err());
    assert!(host.requests().is_empty(), "never an agent's dialog");
    // The owner's own press, or a script's call: no parameters at all is a call too.
    window.state_mut().session.agent = None;
    window.state_mut().execute("ui.proofDictionaryFolder", Value::Null).expect("null is no parameters");
    assert_eq!(host.requests(), vec![FileDialogRequest::Folder], "the same one folder dialog");
}

#[test]
fn a_folder_that_arrives_late_is_staged_then_saved_and_only_one_dialog_opens() {
    let host = Slow::default();
    let mut window = window(host.services(), SAVED);
    click_key(&mut window, "proofDictionaryBrowse");
    window.run_steps(3);
    assert_eq!(host.requests(), vec![FileDialogRequest::Folder], "Browse asks for a folder, once");
    // A second press while the dialog is open asks nothing again.
    click_key(&mut window, "proofDictionaryBrowse");
    window.run_steps(3);
    assert_eq!(host.requests(), vec![FileDialogRequest::Folder], "one dialog at a time");
    assert_eq!(saved(&window), SAVED, "an open dialog writes nothing");
    host.answer(Some(PICKED));
    window.run_steps(3);
    assert_eq!(saved(&window), SAVED, "the answer is staged, never saved by itself");
    click_key(&mut window, "proofDictionarySave");
    window.run_steps(2);
    assert_eq!(saved(&window), PICKED, "Save commits the folder that arrived");
}

#[test]
fn a_dialog_that_goes_away_without_an_answer_keeps_the_typing() {
    let host = Slow::default();
    let mut window = window(host.services(), SAVED);
    type_into_field(&mut window, TYPED);
    click_key(&mut window, "proofDictionaryBrowse");
    window.run_steps(2);
    assert_eq!(host.requests(), vec![FileDialogRequest::Folder]);
    host.close();
    window.run_steps(3);
    assert_eq!(saved(&window), SAVED, "a host that went away saves nothing");
    click_key(&mut window, "proofDictionarySave");
    window.run_steps(2);
    assert_eq!(saved(&window), TYPED, "the typing from before the dialog is still pending");
}

#[test]
fn a_folder_past_the_limit_is_refused_whole_and_never_shortened() {
    // 1,025 bytes: one past what a setting keeps. Truncating it would have saved a folder the
    // owner never named, so nothing is saved at all.
    let long = folder_of(1_025, "x");
    let host = Host::picking(&long);
    let mut window = window(host.services(), SAVED);
    click_key(&mut window, "proofDictionaryBrowse");
    window.run_steps(4);
    assert_eq!(saved(&window), SAVED, "a picked folder is staged, not saved");
    click_key(&mut window, "proofDictionarySave");
    window.run_steps(2);
    assert_eq!(saved(&window), SAVED, "a path past the limit is refused, never shortened and saved");
    assert!(
        window.state().status_msg.as_ref().is_some_and(|(said, _)| said.contains("1024")),
        "the refusal says why: {:?}",
        window.state().status_msg
    );
    // The field still holds the refused text, so a second Save refuses the same way.
    click_key(&mut window, "proofDictionarySave");
    window.run_steps(2);
    assert_eq!(saved(&window), SAVED);
}

#[test]
fn a_picked_folder_holding_a_nul_is_refused_and_the_field_still_works() {
    // A picked path is kept exactly as the host gave it; one with a NUL character cannot be.
    let nul = format!("{PICKED}\0more");
    let host = Host::picking(&nul);
    let mut window = window(host.services(), SAVED);
    type_into_field(&mut window, TYPED);
    click_key(&mut window, "proofDictionaryBrowse");
    window.run_steps(4);
    click_key(&mut window, "proofDictionarySave");
    window.run_steps(2);
    assert_eq!(saved(&window), SAVED, "a folder holding a NUL character is refused");
    // And the field is still usable afterwards, with no half path left in it.
    type_into_field(&mut window, TYPED);
    click_key(&mut window, "proofDictionarySave");
    window.run_steps(2);
    assert_eq!(saved(&window), TYPED, "the typing after a refusal is saved as it is");
}

#[test]
fn a_utf8_folder_at_the_limit_is_kept_whole() {
    let folder = folder_of(1_024, "é");
    let host = Host::picking(&folder);
    let mut window = window(host.services(), SAVED);
    click_key(&mut window, "proofDictionaryBrowse");
    window.run_steps(4);
    click_key(&mut window, "proofDictionarySave");
    window.run_steps(2);
    assert_eq!(saved(&window), folder, "1,024 bytes of a path are kept, character boundaries and all");
}

#[test]
fn a_folder_already_kept_is_never_written_again() {
    // A value only a script leaves with spaces round it: Save runs no command, so the engine's
    // trim cannot quietly rewrite what is already kept.
    let kept = format!("  {SAVED}  ");
    let mut window = window(Services::default(), &kept);
    let said = window.state().status_msg.clone();
    click_key(&mut window, "proofDictionarySave");
    window.run_steps(2);
    assert_eq!(saved(&window), kept, "saving a folder that is already kept runs nothing");
    assert_eq!(window.state().status_msg, said, "and says nothing");
}

#[test]
fn a_typed_folder_ending_in_spaces_is_kept_exactly() {
    // A folder's name is a filesystem path, not prose: spaces at its end are part of the name the
    // owner typed, and `proof.set` keeps the string it was given, byte for byte. Nothing shortens
    // it, and the field shows what was kept.
    let typed = format!("{TYPED}  ");
    let mut window = window(Services::default(), SAVED);
    type_into_field(&mut window, &typed);
    click_key(&mut window, "proofDictionarySave");
    window.run_steps(2);
    assert_eq!(saved(&window), typed, "the trailing spaces are part of the folder's name");
}
