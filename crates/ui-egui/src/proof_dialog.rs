// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Review › Spelling & Grammar (#954): one place at a time, what could stand there instead, and
//! Change, Ignore Once, Ignore This Rule, Add to Dictionary or Close.
//!
//! The places are the window's, brought back by the host's spelling and grammar services and kept
//! for exactly this text ([`crate::proof::review_places`]): opening the dialog reads no dictionary
//! and works no suggestion out on this thread. What could stand in for a word this computer's
//! dictionaries do not know is asked of the host, once, while the dialog is open. The dialog
//! changes nothing itself — every button runs the command a script or an agent runs, so a message
//! is corrected the same way from the window, the CLI or MCP, and a correction is one step of the
//! message's own undo.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;

use egui::{Align, Layout, RichText, Ui, vec2};
use mailcraft_model::proof::{Diagnostic, Kind};
use serde_json::{Value, json};

use crate::MailApp;
use crate::dialogs::button_row;
use crate::proof::{self, Body};
use crate::proof_service::SuggestionReply;
use crate::theme::Tokens;
use crate::widgets::primary_button;

pub const WIDTH: f32 = 460.0;

/// How often a frame is asked for while a suggestion is on its way.
const POLL: Duration = Duration::from_millis(120);

/// The document and text the dialog last asked about: a different one is a different walk.
#[derive(Clone, PartialEq, Eq)]
struct Asked {
    body: Body,
    text: String,
}

/// Which place's suggestions these are.
#[derive(Clone, PartialEq, Eq)]
struct SuggestKey {
    body: Body,
    from: usize,
    to: usize,
    word: String,
}

/// The suggestions of the place on screen, asked of the host when the place changes.
#[derive(Default)]
enum Suggestions {
    #[default]
    None,
    Asking {
        key: SuggestKey,
        channel: Receiver<SuggestionReply>,
        cancelled: Arc<AtomicBool>,
    },
    Ready {
        key: SuggestKey,
        list: Vec<String>,
    },
}

impl Suggestions {
    fn key(&self) -> Option<&SuggestKey> {
        match self {
            Suggestions::None => None,
            Suggestions::Asking { key, .. } | Suggestions::Ready { key, .. } => Some(key),
        }
    }

    fn list(&self) -> &[String] {
        match self {
            Suggestions::Ready { list, .. } => list,
            _ => &[],
        }
    }

    /// Stops whatever was asked for: the place it was for is not on screen any more.
    fn cancel(&self) {
        if let Suggestions::Asking { cancelled, .. } = self {
            cancelled.store(true, Ordering::Relaxed);
        }
    }
}

/// The dialog: what is to be put in place of the place on screen, and the last thing that was done.
#[derive(Default)]
pub struct Spelling {
    /// What Change puts in: the first suggestion to begin with, and whatever the owner types.
    pub replacement: String,
    /// What the last button came to, in a line (a word was added, a rule is quiet…).
    pub note: String,
    /// The snapshot the walk is of.
    asked: Option<Asked>,
    /// Whether the walk was started for `asked` (a walk of this text is on screen).
    started: bool,
    /// The suggestions of the place on screen.
    suggestions: Suggestions,
}

impl std::fmt::Debug for Spelling {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Spelling").field("started", &self.started).finish_non_exhaustive()
    }
}

impl Spelling {
    /// Opens over the message being written (the check itself starts at the dialog's first frame).
    pub fn open(app: &MailApp) -> Option<Spelling> {
        app.session.doc()?;
        Some(Spelling::default())
    }

    /// The dialog's contents; whether it is done.
    pub fn show(&mut self, app: &mut MailApp, ui: &mut Ui, enter: bool) -> bool {
        let t = Tokens::get(ui.ctx());
        let ctx = ui.ctx().clone();
        let mut done = false;
        let Some(body) = proof::body_of(app) else {
            ui.label(crate::tl!("Spelling check is complete."));
            ui.add_space(6.0);
            button_row(ui, |ui| {
                if ui.add(egui::Button::new(crate::tl!("Close")).min_size(vec2(80.0, 26.0))).clicked() {
                    done = true;
                }
            });
            return done;
        };
        let Some(text) = proof::text_of(app, body) else {
            ui.label(crate::tl!("Spelling check is complete."));
            ui.add_space(6.0);
            button_row(ui, |ui| {
                if ui.add(egui::Button::new(crate::tl!("Close")).min_size(vec2(80.0, 26.0))).clicked() {
                    done = true;
                }
            });
            return done;
        };
        // A new document, or an edit: the walk is of another snapshot now, and the check is asked
        // for afresh (the dialog works even with checking as you type turned off).
        if self.asked.as_ref() != Some(&Asked { body, text: text.clone() }) {
            self.asked = Some(Asked { body, text: text.clone() });
            self.started = false;
            self.replacement.clear();
            self.note.clear();
            self.suggestions.cancel();
            self.suggestions = Suggestions::None;
            app.session.proof.close();
            app.proof.recheck(body);
        }
        self.poll_suggestions(&ctx);
        let settings = app.session.prefs.proof.clone();
        if !settings.any() {
            ui.label(RichText::new("Spelling and grammar are both turned off in File › Options.").color(t.text_dim));
            ui.add_space(6.0);
            button_row(ui, |ui| {
                if ui.add(egui::Button::new(crate::tl!("Close")).min_size(vec2(80.0, 26.0))).clicked() {
                    done = true;
                }
            });
            return done;
        }
        // The walk starts only once every provider that is on has answered for exactly this text
        // and these settings: a long check is waited for with a frame scheduled, never with a
        // dictionary read on this thread.
        if !self.started {
            match proof::review_places(app, &text) {
                Some(found) => {
                    let (spelling, grammar): (Vec<Value>, Vec<Value>) = found.as_slice().partition_map();
                    let mut params = json!({
                        "again": true,
                        "expectedText": text,
                        "spelling": spelling,
                        "grammar": grammar,
                    });
                    match body {
                        Body::Message(id) => params["composeId"] = json!(id),
                        Body::Signature(id) => {
                            params["signature"] = json!(true);
                            params["signatureId"] = json!(id);
                        }
                    }
                    match app.execute("review.spelling", params) {
                        Ok(answer) => {
                            self.replacement = first_suggestion(&answer);
                            self.started = true;
                        }
                        Err(e) => {
                            self.note = crate::tl!(e.as_str()).to_string();
                            self.started = true;
                        }
                    }
                }
                None => {
                    ctx.request_repaint_after(POLL);
                    ui.label(RichText::new(crate::tl!("Checking…")).color(t.text_dim));
                    self.notes(app, body, ui, &t);
                    ui.add_space(6.0);
                    button_row(ui, |ui| {
                        if ui.add(egui::Button::new(crate::tl!("Close")).min_size(vec2(80.0, 26.0))).clicked() {
                            done = true;
                        }
                    });
                    return done;
                }
            }
        }
        let walk = app.session.proof.walk.clone();
        let Some(walk) = walk else {
            ui.label(crate::tl!("Spelling check is complete."));
            self.notes(app, body, ui, &t);
            ui.add_space(6.0);
            button_row(ui, |ui| {
                if ui.add(egui::Button::new(crate::tl!("Close")).min_size(vec2(80.0, 26.0))).clicked() {
                    done = true;
                }
            });
            return done;
        };
        let total = walk.items.len();
        let Some(place) = walk.current() else {
            ui.label(crate::tl!("Spelling check is complete."));
            self.notes(app, body, ui, &t);
            ui.add_space(6.0);
            button_row(ui, |ui| {
                if ui.add(egui::Button::new(crate::tl!("Close")).min_size(vec2(80.0, 26.0))).clicked() {
                    done = true;
                }
            });
            return done;
        };
        let word = walk.text.get(place.span.start..place.span.end).unwrap_or_default().to_string();
        let (from, to) = chars_of(&walk.text, place.span);
        // The server's own suggestions, or this computer's asked for once for this place.
        if place.suggestions.is_empty() && place.kind == Kind::Spelling {
            let key = SuggestKey { body, from, to, word: word.clone() };
            if self.suggestions.key() != Some(&key) {
                self.suggestions.cancel();
                self.suggestions = match proof::suggest(app, &word, 6) {
                    Some((channel, cancelled)) => Suggestions::Asking { key, channel, cancelled },
                    None => Suggestions::None,
                };
            }
        } else {
            self.suggestions.cancel();
            self.suggestions = Suggestions::Ready { key: SuggestKey { body, from, to, word: word.clone() }, list: place.suggestions.clone() };
        }
        ui.label(RichText::new(place_caption(place.kind)).small().color(t.text_dim));
        ui.label(RichText::new(&word).strong().size(15.0));
        if !place.message.is_empty() {
            ui.label(RichText::new(place.message.as_str()).color(t.text_dim));
        }
        ui.add_space(4.0);
        ui.label(crate::tl!("Suggestions"));
        // The suggestions, each a button: the one clicked goes in the field (Change then puts it
        // in), and the field can always be typed in instead.
        let list = self.suggestions.list().to_vec();
        if !list.is_empty() {
            if self.replacement.trim().is_empty()
                && let Some(first) = list.first()
            {
                self.replacement = first.clone();
            }
            ui.horizontal_wrapped(|ui| {
                for s in &list {
                    if ui.add(egui::Button::new(s).frame(false)).clicked() {
                        self.replacement = s.clone();
                    }
                }
            });
        } else if matches!(self.suggestions, Suggestions::Asking { .. }) {
            ui.label(RichText::new(crate::tl!("Asking…")).color(t.text_dim));
        } else {
            ui.label(RichText::new(crate::tl!("No suggestions")).color(t.text_dim));
        }
        ui.add_space(4.0);
        ui.label(crate::tl!("Change to"));
        let resp = ui.add(egui::TextEdit::singleline(&mut self.replacement).desired_width(f32::INFINITY));
        let resp = crate::text_menu::field(resp);
        app.hit.add("proofChangeTo", 0, resp.rect);
        self.notes(app, body, ui, &t);
        if !self.note.is_empty() {
            ui.label(RichText::new(self.note.as_str()).color(t.text_dim));
        }
        ui.add_space(6.0);
        let mut restart = false;
        let can = !self.replacement.trim().is_empty() && self.replacement != word;
        let mut actions = vec![Action::Change, Action::AddToDictionary];
        if let Some(rule) = place.rule.clone() {
            actions.push(Action::IgnoreRule(rule));
        }
        actions.push(Action::IgnoreOnce);
        actions.push(Action::Close);
        // Measured here and wrapped: the shared one-row layout puts the leftmost action (Change)
        // past this dialog's left edge at the width it opens at, where it is neither painted nor
        // clickable. Every action keeps its label, its key and its place in the order.
        for row in action_rows(ui, &actions) {
            ui.allocate_ui_with_layout(vec2(ui.available_width(), 26.0), Layout::right_to_left(Align::Center), |ui| {
                for action in row.iter().rev() {
                    match action {
                        Action::Close => {
                            let b = ui.add(egui::Button::new(crate::tl!("Close")).min_size(vec2(72.0, 26.0)));
                            app.hit.add("proofClose", 0, b.rect);
                            if b.clicked() {
                                done = true;
                            }
                        }
                        Action::IgnoreOnce => {
                            let b = ui.add(egui::Button::new(crate::tl!("Ignore Once")).min_size(vec2(72.0, 26.0)));
                            app.hit.add("proofIgnore", 0, b.rect);
                            if b.clicked() {
                                self.note = said(app.execute("proof.ignore", json!({})));
                                self.replacement = next_suggestion(app);
                                self.suggestions.cancel();
                                self.suggestions = Suggestions::None;
                            }
                        }
                        Action::IgnoreRule(rule) => {
                            let b = ui.add(egui::Button::new(crate::tl!("Ignore This Rule")).min_size(vec2(72.0, 26.0)));
                            app.hit.add("proofIgnoreRule", 0, b.rect);
                            if b.clicked() {
                                let _ = app.execute("proof.ignoreRule", json!({ "rule": rule.clone() }));
                                // What that rule found leaves the list now; the walk is started
                                // again over what is left, without asking the server a second time.
                                app.proof.forget_rule(body, rule);
                                app.session.proof.close();
                                restart = true;
                            }
                        }
                        Action::AddToDictionary => {
                            let b = ui.add(egui::Button::new(crate::tl!("Add to Dictionary")).min_size(vec2(72.0, 26.0)));
                            app.hit.add("proofAdd", 0, b.rect);
                            if b.clicked() && app.execute("proof.addWord", json!({ "word": word.clone() })).is_ok() {
                                app.proof.forget_word(body, &word);
                                // The dictionaries changed: the host is asked for the text again,
                                // and the walk starts over when it has answered.
                                app.session.proof.close();
                                restart = true;
                            }
                        }
                        Action::Change => {
                            let b = primary_button(ui, crate::tl!("Change"), can);
                            app.hit.add("proofChange", 0, b.rect);
                            if (b.clicked() || (enter && can)) && can {
                                let mut params = json!({
                                    "text": self.replacement.clone(),
                                    "from": from,
                                    "to": to,
                                    "expectedText": walk.text,
                                });
                                match body {
                                    Body::Message(id) => params["composeId"] = json!(id),
                                    Body::Signature(id) => {
                                        params["signature"] = json!(true);
                                        params["signatureId"] = json!(id);
                                    }
                                }
                                let answer = app.execute("proof.change", params);
                                let ok = answer.is_ok();
                                self.note = said(answer);
                                if ok {
                                    self.suggestions.cancel();
                                    self.suggestions = Suggestions::None;
                                    self.started = false;
                                    app.session.proof.close();
                                }
                            }
                        }
                    }
                }
            });
        }
        if restart {
            self.started = false;
        }
        ui.add_space(2.0);
        ui.label(RichText::new(format!("{} / {}", walk.at.min(total).saturating_add(1), total)).small().color(t.text_dim));
        done
    }

    /// Why something could not be checked, and whether there is more than can be shown: said in
    /// the dialog rather than left to look like a clean message.
    fn notes(&self, app: &MailApp, body: Body, ui: &mut Ui, t: &Tokens) {
        let mut said: Vec<String> = Vec::new();
        if let Some(note) = app.proof.spelling_note(body) {
            said.push(note.to_string());
        }
        if let Some(note) = app.proof.note(body) {
            said.push(note.to_string());
        }
        if app.proof.capped(body) {
            said.push(format!("There are more than {} places; the first ones are shown.", proof::MOST_PLACES));
        }
        for note in said {
            ui.label(RichText::new(note).small().color(t.text_dim));
        }
    }

    /// Takes a suggestion answer that has come, and keeps asking for a frame while one is on its
    /// way: a delayed suggestion still reaches a dialog that has not been drawn in.
    fn poll_suggestions(&mut self, ctx: &egui::Context) {
        if let Suggestions::Asking { key, channel, .. } = &self.suggestions {
            let key = key.clone();
            match channel.try_recv() {
                Ok(reply) => {
                    let list = if reply.error.is_none() { reply.suggestions } else { Vec::new() };
                    self.suggestions = if list.is_empty() { Suggestions::None } else { Suggestions::Ready { key, list } };
                }
                Err(TryRecvError::Empty) => ctx.request_repaint_after(POLL),
                Err(TryRecvError::Disconnected) => self.suggestions = Suggestions::None,
            }
        }
    }
}

/// The heading over the place on screen: a word no dictionary knows is a spelling place, and a
/// grammar or a style place is the server's — calling either of those a missing dictionary word
/// would say something false about it. The server's own words are painted under the heading as
/// they came.
fn place_caption(kind: Kind) -> &'static str {
    match kind {
        Kind::Spelling => crate::tl!("Not in dictionary:"),
        Kind::Grammar => crate::tl!("Grammar:"),
        Kind::Style => crate::tl!("Style:"),
    }
}

/// The characters of `span` (bytes of `text`), when they are a stretch of it.
fn chars_of(text: &str, span: mailcraft_model::proof::Span) -> (usize, usize) {
    let from = text.get(..span.start).map_or(0, |s| s.chars().count());
    let to = text.get(..span.end).map_or(from, |s| s.chars().count());
    (from, to)
}

/// One action of the review dialog, in the order the owner reads them (left to right).
#[derive(Clone)]
enum Action {
    Change,
    AddToDictionary,
    IgnoreRule(String),
    IgnoreOnce,
    Close,
}

impl Action {
    fn label(&self) -> &'static str {
        match self {
            Action::Change => crate::tl!("Change"),
            Action::AddToDictionary => crate::tl!("Add to Dictionary"),
            Action::IgnoreRule(_) => crate::tl!("Ignore This Rule"),
            Action::IgnoreOnce => crate::tl!("Ignore Once"),
            Action::Close => crate::tl!("Close"),
        }
    }

    /// The width its button is drawn at: what its label takes, or the least the button accepts,
    /// whichever is larger. Measured with the same font and padding the button is drawn with.
    fn width(&self, ui: &Ui) -> f32 {
        let text = crate::widgets::label_width(ui, self.label(), 12.5);
        match self {
            Action::Change => text.max(44.0) + 26.0,
            _ => (text + 16.0).max(72.0),
        }
    }
}

/// The actions in the fewest right-aligned rows their measured widths fit, in reading order. The
/// buttons of a row are added right to left, as before, so only how many rows there are changes.
fn action_rows(ui: &Ui, actions: &[Action]) -> Vec<Vec<Action>> {
    let gap = ui.spacing().item_spacing.x.max(0.0);
    let room = ui.available_width().max(0.0);
    let mut rows: Vec<Vec<Action>> = Vec::new();
    let mut row: Vec<Action> = Vec::new();
    let mut used = 0.0_f32;
    for action in actions {
        let width = action.width(ui);
        let needed = width + if row.is_empty() { 0.0 } else { gap };
        if !row.is_empty() && used + needed > room + 0.5 {
            rows.push(std::mem::take(&mut row));
            used = width;
        } else {
            used += needed;
        }
        row.push(action.clone());
    }
    if !row.is_empty() {
        rows.push(row);
    }
    rows
}

/// Splits byte diagnostics into the engine's supplied spelling and grammar lists.
trait Partition {
    fn partition_map(&self) -> (Vec<Value>, Vec<Value>);
}

impl Partition for [Diagnostic] {
    fn partition_map(&self) -> (Vec<Value>, Vec<Value>) {
        let mut spelling = Vec::new();
        let mut grammar = Vec::new();
        for d in self {
            if d.kind == Kind::Spelling {
                spelling.push(proof::supplied(d));
            } else {
                grammar.push(proof::supplied(d));
            }
        }
        (spelling, grammar)
    }
}

/// What a command's answer says, or why it said no.
fn said(answer: Result<Value, String>) -> String {
    match answer {
        Ok(_) => String::new(),
        Err(e) => crate::tl!(e.as_str()).to_string(),
    }
}

/// The first suggestion of the place on screen now, for the field after a change.
fn next_suggestion(app: &MailApp) -> String {
    app.session.proof.walk.as_ref().and_then(|w| w.current()).and_then(|d| d.suggestions.first().cloned()).unwrap_or_default()
}

/// The first suggestion of the place on screen, for the field when the walk starts.
fn first_suggestion(answer: &Value) -> String {
    answer
        .get("at")
        .and_then(|at| at.get("suggestions"))
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}
