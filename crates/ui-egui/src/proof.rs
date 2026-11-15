// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Spelling and grammar under the text of a message being written (#954): what is looked at, when,
//! and what comes back.
//!
//! - **The host does the work**: this module never reads a dictionary, generates a suggestion, or
//!   opens a connection on the window's thread. It asks [`ProofService`] (spelling, grammar, and
//!   on-demand suggestions through [`Services::proof_native`]) and takes the answers back on a
//!   channel. Every call from a frame is a bounded queued request; no frame ever waits for one.
//! - **A snapshot is the only thing a check may be of**: which document (a message being written,
//!   by its id, or a signature editor instance, by its id), the text, and the settings the text is
//!   proofed under. Any difference is a new revision: what was found for the old one goes at once,
//!   a request still on its way for it is cancelled, and an answer for it is dropped whole. Two
//!   drafts with the same words, or a signature opened a second time, are never another one's.
//! - **Nothing is asked for while the owner types**: the text rests for [`REST`] seconds first, one
//!   request of each kind is on its way at a time for the whole window, a failed or answered
//!   revision is never asked for again by another frame, and the frame that can ask next is
//!   scheduled rather than waited for from an event that may never come.
//! - **Spelling and grammar are kept apart**, each of their own revision: a spelling answer never
//!   erases what the server found (or the other way round), and a change to one provider's
//!   settings re-asks only that provider. This computer's spelling is the only spelling shown: a
//!   spelling diagnostic a grammar server sends is filtered out, and so is a grammar rule the
//!   owner told to be quiet.
//! - **What nothing could be checked against is said**, not shown as a clean text: no dictionary,
//!   no spelling service, no grammar service, a failed or partial answer, and a text longer than
//!   can be checked all leave a note ([`Proof::note`], [`Proof::spelling_note`]); more places than
//!   can be drawn is said too ([`Proof::capped`]).
//!
//! A place is a stretch of **characters** of the body's text (the positions the editor and the
//! commands count in), worked out once from the byte positions a check answers with. A place is
//! drawn only while the text under it is still the word it was found in, and the menu that offers
//! a correction keeps the text it opened on: a correction can never land in other words because
//! the message was edited meanwhile.
//!
//! [`Services::proof_native`]: crate::Services::proof_native

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;

use egui::{Popup, Response, Ui};
use mailcraft_model::proof::{Diagnostic, Kind, Language, ProofSettings, Source, Span};
use serde_json::{Value, json};

use crate::MailApp;
use crate::proof_server::Phase;
use crate::proof_service::{Cancellation, Failure, GrammarReply, GrammarRequest, SpellingReply, SpellingRequest, SuggestionReply, SuggestionRequest};

/// The interface's names for the proofing languages; the model keeps its English metadata
/// (`Language::name`), which the engine reports and the log reads. The literals here are also what
/// makes each name visible to the catalogue extractor, which does not look inside the model's
/// `name` — a window that translated the model's text in place would show English in French.
pub(crate) fn language_label(language: Language) -> &'static str {
    match language {
        Language::EnCa => crate::tl!("English (Canada)"),
        Language::FrCa => crate::tl!("French (Canada)"),
        Language::EnUs => crate::tl!("English (United States)"),
        Language::FrFr => crate::tl!("French (France)"),
    }
}

/// The interface's words for a note the window wrote itself.
///
/// The fixed sentences below are this module's own English literals, and they are what a cache
/// keeps (`Doc::note`, `Doc::spelling_note`, `GrammarKey::why`): compared as they are, so a change
/// of interface language can never look like a change of settings. They are named here, at read
/// time, in the language in force at that moment, borrowing a catalogue string rather than
/// building one.
///
/// Only those sentences are the window's own words. A server's message, a dictionary's note or
/// any other text that arrives from elsewhere is shown exactly as it came, which is what the last
/// arm is for.
fn note_text(note: &str) -> &str {
    match note {
        "This build cannot run the built-in grammar server." => crate::tl!("This build cannot run the built-in grammar server."),
        "The built-in grammar server is not running." => crate::tl!("The built-in grammar server is not running."),
        "The built-in grammar server's address is not one this computer may use." => {
            crate::tl!("The built-in grammar server's address is not one this computer may use.")
        }
        "That server is on another computer, and File › Options does not allow one." => {
            crate::tl!("That server is on another computer, and File › Options does not allow one.")
        }
        "Name the grammar server's address first." => crate::tl!("Name the grammar server's address first."),
        "Spelling is checked by the host, and this build has none to ask." => {
            crate::tl!("Spelling is checked by the host, and this build has none to ask.")
        }
        "Grammar is checked by a LanguageTool server; this build has none to ask." => {
            crate::tl!("Grammar is checked by a LanguageTool server; this build has none to ask.")
        }
        other => other,
    }
}

/// How long the text may rest before it is looked at again, in seconds.
const REST: f64 = 0.25;

/// The least time between two grammar requests for one document, in seconds.
const ASK_EVERY: f64 = 0.8;

/// How often a frame is asked for while an answer is on its way.
const POLL: Duration = Duration::from_millis(120);

/// How many passes a place's menu stays while the body is not drawn in them (the window closed).
const MENU_PASSES: u64 = 30;

/// Most places drawn at once: a message with more mistakes than this shows the first ones.
pub const MOST_PLACES: usize = 2_000;

/// Most documents whose proofing state is kept at once. A window draws only the messages it has
/// open and the signature it is editing; this is a hard bound on top of that, so a session that
/// opened many drafts cannot grow this state without end.
const MOST_DOCUMENTS: usize = 32;

/// Most suggestions asked for a menu or the dialog.
const SUGGESTIONS: usize = 6;

/// Which text is being proofed: the message being written, by its id, or a signature editor, by
/// the id of that editor instance.
///
/// The id is part of it because two messages may hold the same words, and a signature opened a
/// second time is another editor: an answer found for one is never another one's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Body {
    /// The message being written, by its id ([`Session::compose_id`](mailcraft_engine::Session::compose_id)).
    Message(u64),
    /// The signature open in File › Signatures, by the id of this editing instance
    /// ([`Session::signature_id`](mailcraft_engine::Session::signature_id)).
    Signature(u64),
}

/// One place under the text, in characters of the body's text.
#[derive(Clone, PartialEq, Eq)]
pub struct Place {
    pub from: usize,
    pub to: usize,
    pub kind: Kind,
    /// Who found it: this computer's dictionaries, or the grammar server.
    pub source: Source,
    /// The language the provider said the place is in, when it said one. Kept so that a review
    /// dialog's serialized place does not lose the metadata the answer carried.
    pub language: Option<Language>,
    /// The word as it was when it was found: a place is drawn only while the text still says it.
    pub word: String,
    /// The grammar rule it came of, for leaving that rule alone from now on.
    pub rule: Option<String>,
    /// What the server said of it, in its own words.
    pub message: String,
    /// What could stand there instead: the server's own, or what the host's dictionaries offered
    /// when the menu was opened (never worked out eagerly for the whole document).
    pub suggestions: Vec<String>,
}

impl std::fmt::Debug for Place {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // A provider can repeat the draft in any string field, even the rule identifier.
        f.debug_struct("Place")
            .field("from", &self.from)
            .field("to", &self.to)
            .field("kind", &self.kind)
            .field("source", &self.source)
            .field("suggestions", &self.suggestions.len())
            .finish_non_exhaustive()
    }
}

/// What the host's grammar adapter is asked for, when the window asks it directly (File ›
/// Options' Test the connection): the grammar server's address, the text, every language the owner
/// checks it as, and the rules they told to be quiet.
///
/// This is the older host interface, kept for compatibility. Draft proofing uses
/// [`ProofService`](crate::proof_service::ProofService) instead.
#[derive(Clone)]
pub struct Request {
    pub url: String,
    pub text: String,
    /// The language asked for when a server can be asked for one only (the first of
    /// [`Request::languages`]).
    pub language: Language,
    /// Every language the owner turned on, so that a server that can check them together is
    /// asked about all of them at once.
    pub languages: Vec<Language>,
    /// The grammar rules the owner told to be quiet (LanguageTool's rule ids).
    pub ignored: Vec<String>,
}

/// A grammar request is a message's text, and a message is nobody's but its owner's: what `Debug`
/// says of one never includes the draft or the address it was to go to.
impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Request")
            .field("languages", &self.languages)
            .field("ignored", &self.ignored.len())
            .field("chars", &self.text.chars().count())
            .finish_non_exhaustive()
    }
}

/// What the older host interface answers: the text that was checked, the places in it (bytes), or
/// why there are none.
#[derive(Clone)]
pub struct Reply {
    pub text: String,
    pub diagnostics: Vec<Diagnostic>,
    pub error: Option<String>,
}

impl std::fmt::Debug for Reply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reply")
            .field("chars", &self.text.chars().count())
            .field("places", &self.diagnostics.len())
            .field("error", &self.error.as_ref().map(|_| "…"))
            .finish()
    }
}

/// Runs proofing for a frame: the body's text looked at when it has rested, both providers asked
/// through the host, and any answer taken. Called by the composer, once a frame for each body it
/// draws.
pub(crate) fn run(app: &mut MailApp, ctx: &egui::Context, body: Body, text: &str) {
    // Taken out and put back so that the window is not borrowed twice: a check needs the session
    // and the places side by side, and both live in it.
    let mut proof = std::mem::take(&mut app.proof);
    proof.frame(app, ctx, body, text);
    app.proof = proof;
}

/// The document drawn now, when there is one: the signature editor while one is open, else the
/// message being written.
pub(crate) fn body_of(app: &MailApp) -> Option<Body> {
    if let Some(id) = app.session.signature_id() {
        return Some(Body::Signature(id));
    }
    app.session.compose_id().map(Body::Message)
}

/// The plain text of `body`, when it is still open: the snapshot a correction is to land in.
pub(crate) fn text_of(app: &MailApp, body: Body) -> Option<String> {
    match body {
        Body::Message(id) => app.session.composing_by_id(id).map(|c| c.body.plain()),
        Body::Signature(id) => app.session.signature_edit.as_ref().filter(|e| e.id == id).map(|e| e.body.plain()),
    }
}

/// The settings proofing uses for `body`: the message being written has its own language choice
/// over the owner's defaults ([`Session::proof_settings_for`]), and a signature editor has no choice
/// of its own and is checked in the defaults. Pure: nothing is read, nothing changes.
///
/// [`Session::proof_settings_for`]: mailcraft_engine::Session::proof_settings_for
pub(crate) fn settings_for(app: &MailApp, body: Body) -> ProofSettings {
    match body {
        Body::Message(id) => app.session.proof_settings_for(id),
        Body::Signature(_) => app.session.prefs.proof.clone(),
    }
}

/// The settings a spelling answer is of: what changes which words are refused.
#[derive(Clone, PartialEq, Eq)]
struct SpellKey {
    enabled: bool,
    languages: Vec<Language>,
    personal: Vec<String>,
    dictionary_dir: String,
}

/// The settings and server a grammar answer is of. The epoch is the running instance's: progress
/// of a download or a start never changes it, so it never throws work away, but a different
/// instance does.
#[derive(Clone, PartialEq, Eq)]
struct GrammarKey {
    enabled: bool,
    languages: Vec<Language>,
    ignored_rules: Vec<String>,
    url: String,
    epoch: u64,
    /// Why there is no server to ask, when there is none (and none is running).
    why: Option<String>,
}

struct Keys {
    spell: SpellKey,
    grammar: GrammarKey,
}

impl Keys {
    fn of(app: &MailApp, settings: &ProofSettings) -> Keys {
        let (url, epoch, why) = grammar_endpoint(app, settings);
        Keys {
            spell: SpellKey {
                enabled: settings.check_spelling,
                languages: settings.active(),
                personal: settings.personal.clone(),
                dictionary_dir: settings.dictionary_dir.clone(),
            },
            grammar: GrammarKey {
                enabled: settings.check_grammar,
                languages: settings.active(),
                ignored_rules: settings.ignored_rules.clone(),
                url,
                epoch,
                why,
            },
        }
    }
}

/// The address grammar is asked at, the epoch of the running instance there, and why there is no
/// server when there is none.
///
/// The owner's manual address is never rewritten: choosing the built-in server adds a way to run
/// one, and turning that off again finds the same manual address. Status is a cached snapshot the
/// host promises is cheap; nothing here downloads, starts, or waits.
fn grammar_endpoint(app: &MailApp, settings: &ProofSettings) -> (String, u64, Option<String>) {
    if !settings.check_grammar {
        return (String::new(), 0, None);
    }
    if settings.managed_server {
        let Some(server) = app.services.proof_server.as_ref() else {
            return (String::new(), 0, Some("This build cannot run the built-in grammar server.".into()));
        };
        let status = server.status();
        if status.phase != Phase::Running {
            return (String::new(), status.epoch, Some("The built-in grammar server is not running.".into()));
        }
        match status.endpoint.as_deref() {
            Some(url) if mailcraft_model::proof::address_allowed(url, false) => (url.to_string(), status.epoch, None),
            _ => (String::new(), status.epoch, Some("The built-in grammar server's address is not one this computer may use.".into())),
        }
    } else if settings.grammar_url.trim().is_empty() {
        // An empty field is the owner's own step, not a server elsewhere: `address_allowed` reads
        // an empty address as one this computer may not use, so this reason must come first or the
        // owner is told about a server on another computer that was never named.
        (String::new(), 0, Some("Name the grammar server's address first.".into()))
    } else if !settings.grammar_allowed() {
        (String::new(), 0, Some("That server is on another computer, and File › Options does not allow one.".into()))
    } else {
        (settings.grammar_url.trim().to_string(), 0, None)
    }
}

/// A request on its way to a provider: the document and that provider's revision it was asked
/// for, and the channel its answer will come on.
struct Waiting<A> {
    body: Body,
    revision: u64,
    channel: Receiver<A>,
    cancelled: Cancellation,
    /// When only part of a too-long text was sent: what the owner is told of it.
    cap: Option<String>,
}

/// What a successful grammar answer said of how much of the text it checked, kept as scalars so
/// that the passive inspection can report it without holding the reply or asking again.
#[derive(Clone, Copy)]
struct GrammarOutcome {
    checked_segments: usize,
    skipped_segments: usize,
    partial: bool,
}

/// What proofing knows of one document: the text, a revision for each provider, the two answers
/// kept apart, and what has been answered for it.
#[derive(Default)]
struct Doc {
    /// The text the places below are in: a place is of this text alone.
    text: String,
    spell: Option<SpellKey>,
    grammar: Option<GrammarKey>,
    /// This computer's revision: goes up when the text or the spelling settings differ.
    spell_rev: u64,
    /// The grammar server's revision: goes up when the text or the grammar settings differ.
    grammar_rev: u64,
    /// Goes up on any change, for the menu: a menu of another revision is no menu.
    stamp: u64,
    /// When the revisions last changed (the text may rest from then).
    changed_at: Option<f64>,
    /// This computer's spelling places of the revision, sorted.
    spelling: Vec<Place>,
    /// The grammar server's places of the revision, sorted. Never mixed with the spelling's.
    grammar_places: Vec<Place>,
    /// Occurrences the owner ignored this time, at this text.
    ignored: Vec<Ignored>,
    /// The revision spelling was last answered for (a failure too).
    spelling_done: Option<u64>,
    /// Why spelling could not check (no dictionary, no service), in words for the owner.
    spelling_note: Option<String>,
    /// More spelling places were found than are kept.
    spelling_more: bool,
    /// The revision grammar was last answered for (a failure too).
    grammar_done: Option<u64>,
    /// How much the last successful grammar answer checked, for the passive inspection. `None`
    /// unless a successful reply for the document's current revision was taken.
    grammar_outcome: Option<GrammarOutcome>,
    /// When the grammar server was last asked.
    asked_at: Option<f64>,
    /// The last thing the grammar server said of this document, or why it was not asked, in
    /// words for the owner.
    note: Option<String>,
    /// Whether the grammar server answered this document's last request.
    online: Option<bool>,
    /// More grammar places were found than are kept.
    grammar_more: bool,
    /// Look again now, whatever the rest period says (a manual recheck).
    again: bool,
    /// The menu of a right click on a place, while it is open.
    menu: Option<Menu>,
}

/// An occurrence the owner said to leave alone this time: the stretch of text, and the word it
/// held, so that it is that occurrence that goes and not another one of the same word.
#[derive(Clone, PartialEq, Eq)]
struct Ignored {
    from: usize,
    to: usize,
    word: String,
}

/// The suggestions of the place a menu was opened on: the server's own, kept from the place, or
/// the host's dictionaries' asked for once when the menu opened.
enum MenuSuggestions {
    None,
    Asking { channel: Receiver<SuggestionReply>, cancelled: Cancellation },
    Ready(Vec<String>),
}

/// The menu of a right click on a place: what it offered, where, and for which revision.
///
/// It is opened once, when the click lands on a place, and drawn every frame after until it is
/// closed: a menu drawn only in the pass of the click disappears at the next one.
struct Menu {
    /// The revision it was opened for; another one's menu is no menu.
    stamp: u64,
    /// The place it was opened on, and what could stand there (read once, when it opened).
    place: Place,
    suggestions: MenuSuggestions,
    /// Whether egui still has it open (`false` once something was chosen, or it was clicked away).
    open: bool,
    /// Where the click was.
    at: egui::Pos2,
    /// The last pass the body was drawn in with the menu open (`0`: not drawn yet).
    pass: u64,
}

/// What a colour of the menu was chosen to do.
enum Choice {
    Replace(String),
    IgnoreOnce,
    AddWord,
    IgnoreRule(String),
}

/// What File › Options' connection test belongs to: one exact configuration — the mode in use, the
/// address the owner typed, the grammar switch, the languages, and the rules told to be quiet.
///
/// A key is deliberately not `Debug`: the manual address is the owner's own and a key is never
/// printed, logged, or carried anywhere.
#[derive(PartialEq, Eq)]
struct ConnectionKey {
    managed: bool,
    url: String,
    check_grammar: bool,
    languages: Vec<Language>,
    ignored_rules: Vec<String>,
}

impl ConnectionKey {
    fn of(settings: &ProofSettings) -> ConnectionKey {
        ConnectionKey {
            managed: settings.managed_server,
            url: settings.grammar_url.clone(),
            check_grammar: settings.check_grammar,
            languages: settings.active(),
            ignored_rules: settings.ignored_rules.clone(),
        }
    }
}

/// What proofing has found, per document, and what it is waiting for.
///
/// The window is one [`MailApp`] with several messages being written open in it at once (#286),
/// and each pass draws each of their bodies: the state of a check is the document's, never the
/// window's, so that a frame of one message can neither wait on nor throw away the places of
/// another. A provider's request slot, though, is the window's: there is never more than one
/// spelling request and one grammar request on their way at a time, and a revision that had to
/// wait its turn (another document was being checked) is asked for on the pass that frees it.
#[derive(Default)]
pub struct Proof {
    docs: HashMap<Body, Doc>,
    spelling: Option<Waiting<SpellingReply>>,
    grammar: Option<Waiting<GrammarReply>>,
    /// How many grammar requests were made this session, for the tests and the benchmarks.
    pub(crate) asked: usize,
    /// How many spelling requests were made this session.
    pub(crate) spelled: usize,
    /// The configuration the connection test's state below belongs to
    /// ([`sync_connection_test`]); `None` until the first frame that draws it.
    connection: Option<ConnectionKey>,
    /// A connection test on its way (File › Options), and what the last one came to.
    pub(crate) testing: Option<Receiver<Reply>>,
    pub(crate) testing_native: Option<Receiver<GrammarReply>>,
    /// The cancellation flag of the native test on its way, kept so that leaving the
    /// configuration it was asked for calls it off instead of waiting for an answer no one
    /// should see.
    testing_native_cancel: Option<Cancellation>,
    pub(crate) tested: Option<String>,
}

impl std::fmt::Debug for Proof {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Proof")
            .field("documents", &self.docs.len())
            .field("places", &self.docs.values().map(|d| d.spelling.len() + d.grammar_places.len()).sum::<usize>())
            .field("asking", &(self.spelling.is_some() || self.grammar.is_some()))
            .finish()
    }
}

impl Proof {
    /// The places of `body`'s text as they are now: those of the revision `text` belongs to,
    /// without the occurrences the owner left alone this time, in one sorted list at most
    /// [`MOST_PLACES`] long.
    ///
    /// A text that is not the one this document was checked as has no places: an edit clears them
    /// before anything is asked again.
    pub fn places(&self, body: Body, text: &str) -> Vec<Place> {
        self.docs.get(&body).map(|d| d.places(text)).unwrap_or_default()
    }

    /// Why the document's grammar is not checked, or what the server last said, in words for the
    /// owner. The note is cached as the English the window wrote (`note_text`) and named in the
    /// language in force now, so a change of interface language repaints it without asking
    /// anything again.
    pub fn note(&self, body: Body) -> Option<&str> {
        self.docs.get(&body)?.note.as_deref().map(note_text)
    }

    /// Why spelling could not check `body`, or what its dictionaries came to, in words for the
    /// owner: a text nothing could be checked against is never shown as a clean one. Cached and
    /// named like [`Proof::note`].
    pub fn spelling_note(&self, body: Body) -> Option<&str> {
        self.docs.get(&body)?.spelling_note.as_deref().map(note_text)
    }

    /// Whether more places were found for `body` than are kept and drawn.
    pub fn capped(&self, body: Body) -> bool {
        self.docs.get(&body).is_some_and(|d| d.spelling_more || d.grammar_more)
    }

    /// Forget the connection test whole: what it last said, both channels of a test on its way,
    /// and the native request's cancellation flag, which is set so a late answer is never sent
    /// for a configuration that is gone. An older-interface receiver is simply let go: its sender
    /// finds no one to answer, and nothing of it is read.
    fn forget_test(&mut self) {
        if let Some(cancelled) = self.testing_native_cancel.take() {
            cancelled.store(true, Ordering::Relaxed);
        }
        self.testing_native = None;
        self.testing = None;
        self.tested = None;
    }

    /// Check `body` again, even though it was checked already (the owner asked for it): one
    /// request of each kind is all it makes. An edit or a change of settings asks by itself.
    pub(crate) fn recheck(&mut self, body: Body) {
        let doc = self.docs.entry(body).or_default();
        doc.spelling_done = None;
        doc.grammar_done = None;
        doc.spelling_note = None;
        doc.note = None;
        doc.again = true;
    }

    /// The owner left `place` alone this time: that occurrence, and no other one of the same word
    /// and no other place of the document.
    pub(crate) fn ignore_once(&mut self, body: Body, place: &Place) {
        if let Some(doc) = self.docs.get_mut(&body) {
            doc.ignore_once(place);
        }
    }

    /// The owner put a word in their dictionary: what was found of it goes now, and the next pass
    /// checks the text again (the settings changed, which is a new revision).
    pub(crate) fn forget_word(&mut self, body: Body, word: &str) {
        if let Some(doc) = self.docs.get_mut(&body) {
            doc.spelling.retain(|p| !mailcraft_model::proof::same_word(&p.word, word));
        }
    }

    /// The owner told a grammar rule to be quiet: what it found of it goes now, and the outstanding
    /// answer of the old settings is invalidated (the next pass re-asks under the new ones).
    pub(crate) fn forget_rule(&mut self, body: Body, rule: &str) {
        if let Some(doc) = self.docs.get_mut(&body) {
            doc.grammar_places.retain(|p| p.rule.as_deref() != Some(rule));
            doc.close_menu();
        }
    }

    /// Whether `body` is still open in the window.
    fn open(app: &MailApp, body: Body) -> bool {
        match body {
            Body::Message(id) => app.session.composing_by_id(id).is_some(),
            Body::Signature(id) => app.session.signature_id() == Some(id),
        }
    }

    /// Looks at `text` again if it has rested, asks the host when it is on, and takes any answer
    /// that has come. Called once a frame for each body drawn.
    pub(crate) fn frame(&mut self, app: &mut MailApp, ctx: &egui::Context, body: Body, text: &str) {
        let now = ctx.input(|i| i.time);
        // The message being written is checked in its own choice of languages; a signature editor,
        // which has no choice of its own, in the owner's defaults. Choosing by the body is what
        // keeps one draft's footer from re-checking another draft, and a French draft from being
        // checked — or reviewed, or suggested for — in File › Options' English.
        let settings = settings_for(app, body);
        // An answer is taken whenever it comes, whichever window is drawing: a request belongs to
        // the window, and is never left waiting for that document to be drawn again.
        self.take_answers(ctx);
        let keys = Keys::of(app, &settings);
        if !settings.any() {
            // Checking is off: what was found goes, and nothing is asked. A request already on its
            // way is cancelled; its answer is dropped.
            if let Some(doc) = self.docs.get_mut(&body) {
                doc.clear(text);
            }
            self.cancel_for(body);
            self.retain(app);
            return;
        }
        let (ask_now, rest) = {
            let doc = self.docs.entry(body).or_default();
            doc.observe(text, keys.spell.clone(), keys.grammar.clone(), now);
            let rested = doc.changed_at.is_none_or(|changed| now - changed >= REST);
            (doc.again || (settings.as_you_type && rested), (!rested).then(|| doc.changed_at.unwrap_or(now)))
        };
        self.cancel_stale(body);
        if let Some(changed) = rest
            && settings.as_you_type
        {
            // The frame that looks at it again is asked for, rather than waited for from another
            // event that may never come: typing nothing more is a rest.
            ctx.request_repaint_after(Duration::from_secs_f64((REST - (now - changed)).clamp(0.001, REST)));
        }
        if ask_now {
            let busy = self.ask(app, ctx, body, now, &settings, &keys);
            if !busy && let Some(doc) = self.docs.get_mut(&body) {
                // No provider this revision needs is working for another document, so this
                // revision has had all the asking it is owed; one that had to wait its turn keeps
                // `again` and asks on the pass that takes the other answer.
                doc.again = false;
            }
        }
        self.retain(app);
    }

    /// Cancels the request of `body` (it is closed, or checking was turned off). Its answer is
    /// dropped when it comes.
    fn cancel_for(&mut self, body: Body) {
        for cancelled in [
            self.spelling.as_ref().filter(|w| w.body == body).map(|w| w.cancelled.clone()),
            self.grammar.as_ref().filter(|w| w.body == body).map(|w| w.cancelled.clone()),
        ]
        .into_iter()
        .flatten()
        {
            cancelled.store(true, Ordering::Relaxed);
        }
        if self.spelling.as_ref().is_some_and(|w| w.body == body) {
            self.spelling = None;
        }
        if self.grammar.as_ref().is_some_and(|w| w.body == body) {
            self.grammar = None;
        }
    }

    /// Asks a request already on its way to stop when the revision it was for is gone.
    fn cancel_stale(&mut self, body: Body) {
        let Some(doc) = self.docs.get(&body) else { return };
        if let Some(waiting) = &self.spelling
            && waiting.body == body
            && waiting.revision != doc.spell_rev
        {
            waiting.cancelled.store(true, Ordering::Relaxed);
        }
        if let Some(waiting) = &self.grammar
            && waiting.body == body
            && waiting.revision != doc.grammar_rev
        {
            waiting.cancelled.store(true, Ordering::Relaxed);
        }
    }

    /// Takes whatever answer has come, of either kind, and hands it to the document that asked.
    /// One that is for another revision, or for a document that was closed meanwhile, is dropped
    /// whole, and the request it answers is over all the same.
    fn take_answers(&mut self, ctx: &egui::Context) {
        let Proof { docs, spelling, grammar, .. } = self;
        if let Some(waiting) = spelling.take() {
            match waiting.channel.try_recv() {
                Ok(reply) => {
                    if let Some(doc) = docs.get_mut(&waiting.body) {
                        doc.take_spelling(waiting.revision, waiting.cap, reply);
                    }
                }
                Err(TryRecvError::Empty) => *spelling = Some(waiting),
                Err(TryRecvError::Disconnected) => {
                    if let Some(doc) = docs.get_mut(&waiting.body) {
                        doc.take_spelling_lost(waiting.revision, waiting.cap);
                    }
                }
            }
        }
        if let Some(waiting) = grammar.take() {
            match waiting.channel.try_recv() {
                Ok(reply) => {
                    if let Some(doc) = docs.get_mut(&waiting.body) {
                        doc.take_grammar(waiting.revision, waiting.cap, reply);
                    }
                }
                Err(TryRecvError::Empty) => *grammar = Some(waiting),
                Err(TryRecvError::Disconnected) => {
                    if let Some(doc) = docs.get_mut(&waiting.body) {
                        doc.take_grammar_lost(waiting.revision, waiting.cap);
                    }
                }
            }
        }
        if spelling.is_some() || grammar.is_some() {
            ctx.request_repaint_after(POLL);
        }
    }

    /// Asks the host for `body`'s newest revision, each provider once, one request of each kind at
    /// a time across the whole window. Answers whether a provider this revision needs is still
    /// working for another document (so that the caller knows this document's turn is still to
    /// come).
    fn ask(&mut self, app: &mut MailApp, ctx: &egui::Context, body: Body, now: f64, settings: &ProofSettings, keys: &Keys) -> bool {
        let Proof { docs, spelling, grammar, spelled, asked, .. } = self;
        let Some(doc) = docs.get_mut(&body) else {
            return false;
        };
        if doc.text.trim().is_empty() {
            doc.spelling.clear();
            doc.grammar_places.clear();
            doc.spelling_done = Some(doc.spell_rev);
            doc.grammar_done = Some(doc.grammar_rev);
            doc.spelling_note = None;
            doc.note = None;
            doc.online = None;
            doc.spelling_more = false;
            doc.grammar_more = false;
            doc.grammar_outcome = None;
            return false;
        }
        let mut busy = false;
        if keys.spell.enabled && doc.spelling_done != Some(doc.spell_rev) {
            match spelling.as_ref() {
                Some(waiting) if waiting.body == body => {
                    if waiting.revision != doc.spell_rev {
                        // The revision this request was for is gone: ask it to stop, and ask the
                        // newest one in its place (the host keeps one active and one newest).
                        if let Some(old) = spelling.take() {
                            old.cancelled.store(true, Ordering::Relaxed);
                        }
                    } else {
                        busy = true;
                    }
                }
                Some(_) => busy = true,
                None => {}
            }
            if !busy && spelling.is_none() {
                if let Some(service) = app.services.proof_native.as_ref() {
                    let cancelled = Arc::new(AtomicBool::new(false));
                    let channel =
                        service.spelling(SpellingRequest { text: doc.text.clone(), settings: settings.clone(), cancelled: cancelled.clone() });
                    *spelling = Some(Waiting { body, revision: doc.spell_rev, channel, cancelled, cap: None });
                    *spelled = spelled.saturating_add(1);
                    ctx.request_repaint_after(POLL);
                } else {
                    doc.spelling_done = Some(doc.spell_rev);
                    doc.spelling_note = Some("Spelling is checked by the host, and this build has none to ask.".to_string());
                }
            }
        }
        if !keys.grammar.enabled || doc.grammar_done == Some(doc.grammar_rev) {
            return busy;
        }
        match grammar.as_ref() {
            Some(waiting) if waiting.body == body => {
                if waiting.revision != doc.grammar_rev {
                    if let Some(old) = grammar.take() {
                        old.cancelled.store(true, Ordering::Relaxed);
                    }
                } else {
                    return true;
                }
            }
            Some(_) => return true,
            None => {}
        }
        if grammar.is_some() {
            return true;
        }
        if keys.grammar.url.is_empty() {
            doc.grammar_done = Some(doc.grammar_rev);
            doc.online = Some(false);
            doc.note = keys.grammar.why.clone();
            doc.grammar_outcome = None;
            return busy;
        }
        // Two requests for one document are never asked for closer together than `ASK_EVERY`, and
        // the frame that may ask for the next one is scheduled now: nothing else need happen for
        // it to come.
        if !doc.again
            && let Some(at) = doc.asked_at
            && now - at < ASK_EVERY
        {
            ctx.request_repaint_after(Duration::from_secs_f64((ASK_EVERY - (now - at)).clamp(0.001, ASK_EVERY)));
            return true;
        }
        let Some(service) = app.services.proof_native.as_ref() else {
            doc.grammar_done = Some(doc.grammar_rev);
            doc.online = Some(false);
            doc.note = Some("Grammar is checked by a LanguageTool server; this build has none to ask.".to_string());
            doc.grammar_outcome = None;
            return busy;
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        let channel = service.grammar(GrammarRequest {
            url: keys.grammar.url.clone(),
            text: doc.text.clone(),
            languages: keys.grammar.languages.clone(),
            ignored_rules: keys.grammar.ignored_rules.clone(),
            cancelled: cancelled.clone(),
        });
        *grammar = Some(Waiting { body, revision: doc.grammar_rev, channel, cancelled, cap: None });
        doc.asked_at = Some(now);
        *asked = asked.saturating_add(1);
        ctx.request_repaint_after(POLL);
        busy
    }

    /// Lets go of the documents that are no longer open, and of a request they still had on its
    /// way: closing a draft neither keeps its places nor keeps a provider busy on its behalf.
    fn retain(&mut self, app: &MailApp) {
        for cancelled in [
            self.spelling.as_ref().filter(|w| !Self::open(app, w.body)).map(|w| w.cancelled.clone()),
            self.grammar.as_ref().filter(|w| !Self::open(app, w.body)).map(|w| w.cancelled.clone()),
        ]
        .into_iter()
        .flatten()
        {
            cancelled.store(true, Ordering::Relaxed);
        }
        if self.spelling.as_ref().is_some_and(|w| !Self::open(app, w.body)) {
            self.spelling = None;
        }
        if self.grammar.as_ref().is_some_and(|w| !Self::open(app, w.body)) {
            self.grammar = None;
        }
        let (spelling_body, grammar_body) = (self.spelling.as_ref().map(|w| w.body), self.grammar.as_ref().map(|w| w.body));
        // A document that is no longer open has no menu: closing it asks whatever the menu had
        // asked of the host to stop, as closing the menu itself does.
        for (body, doc) in self.docs.iter_mut() {
            if !Self::open(app, *body) {
                doc.close_menu();
            }
        }
        self.docs.retain(|b, _| Some(*b) == spelling_body || Some(*b) == grammar_body || Self::open(app, *b));
        if self.docs.len() > MOST_DOCUMENTS {
            let mut idle: Vec<Body> = self.docs.keys().filter(|b| Some(**b) != spelling_body && Some(**b) != grammar_body).copied().collect();
            idle.sort_unstable();
            for body in idle.into_iter().take(self.docs.len() - MOST_DOCUMENTS) {
                self.docs.remove(&body);
            }
        }
    }
}

impl Doc {
    /// Closes the place menu, stopping whatever it had asked the host for.
    fn close_menu(&mut self) {
        if let Some(menu) = self.menu.take() {
            abandon(&menu);
        }
    }

    /// The places of `text` as they are now: this computer's and the server's merged in order, the
    /// occurrences the owner left alone this time left out, at most [`MOST_PLACES`] of them.
    fn places(&self, text: &str) -> Vec<Place> {
        if text != self.text {
            return Vec::new();
        }
        let mut out: Vec<Place> = self.spelling.iter().chain(&self.grammar_places).cloned().collect();
        out.retain(|p| !self.ignored.iter().any(|i| same_occurrence(i, p)));
        out.sort_by_key(|p| (p.from, p.to));
        out.truncate(MOST_PLACES);
        out
    }

    /// The place under the character `at` of `text`, if there is one drawn there.
    fn place_at(&self, text: &str, at: usize) -> Option<Place> {
        let mut at_text = Cursor::new(text);
        self.places(text).into_iter().find(|p| p.from <= at && at <= p.to && at_text.slice(p.from, p.to).is_some_and(|w| w == p.word))
    }

    /// How many of this computer's places would be drawn now: the occurrences the owner left
    /// alone are not ones the window offers, so they are not counted either.
    fn spelling_shown(&self) -> usize {
        self.spelling.iter().filter(|place| !self.ignored.iter().any(|i| same_occurrence(i, place))).count()
    }

    /// How many of the grammar server's places would be drawn now.
    fn grammar_shown(&self) -> usize {
        self.grammar_places.iter().filter(|place| !self.ignored.iter().any(|i| same_occurrence(i, place))).count()
    }

    /// The owner left `place` alone this time: that occurrence, and no other one of the same
    /// word and no other place of the document.
    fn ignore_once(&mut self, place: &Place) {
        let ignored = Ignored { from: place.from, to: place.to, word: place.word.clone() };
        if !self.ignored.contains(&ignored) {
            self.ignored.push(ignored);
        }
    }

    /// Forgets what was found, when checking is turned off. A request already on its way is the
    /// window's and is left to answer; its answer is dropped when it comes.
    fn clear(&mut self, text: &str) {
        self.text = text.to_string();
        self.spell = None;
        self.grammar = None;
        self.changed_at = None;
        self.spelling.clear();
        self.grammar_places.clear();
        self.ignored.clear();
        self.spelling_done = None;
        self.spelling_note = None;
        self.spelling_more = false;
        self.grammar_done = None;
        self.note = None;
        self.grammar_more = false;
        self.grammar_outcome = None;
        self.close_menu();
        self.again = false;
        self.stamp = self.stamp.saturating_add(1);
    }

    /// Takes the revisions in: a new text, or new settings for one provider, is a new revision
    /// for it, and what was found for the old one goes at once.
    fn observe(&mut self, text: &str, spell: SpellKey, grammar: GrammarKey, now: f64) {
        let text_changed = self.text != text;
        let spell_changed = self.spell.as_ref() != Some(&spell);
        let grammar_changed = self.grammar.as_ref() != Some(&grammar);
        if !(text_changed || spell_changed || grammar_changed) {
            if self.changed_at.is_none() {
                self.changed_at = Some(now);
            }
            return;
        }
        self.stamp = self.stamp.saturating_add(1);
        if text_changed {
            self.text = text.to_string();
            self.spelling.clear();
            self.grammar_places.clear();
            self.ignored.clear();
            self.spelling_done = None;
            self.spelling_note = None;
            self.spelling_more = false;
            self.grammar_done = None;
            self.note = None;
            self.online = None;
            self.grammar_more = false;
            self.grammar_outcome = None;
            self.close_menu();
            self.spell_rev = self.spell_rev.saturating_add(1);
            self.grammar_rev = self.grammar_rev.saturating_add(1);
        } else {
            if spell_changed {
                self.spell_rev = self.spell_rev.saturating_add(1);
                self.spelling.clear();
                self.spelling_done = None;
                self.spelling_note = None;
                self.spelling_more = false;
                self.close_menu();
            }
            if grammar_changed {
                self.grammar_rev = self.grammar_rev.saturating_add(1);
                self.grammar_done = None;
                self.note = None;
                self.online = None;
                self.grammar_more = false;
                self.grammar_outcome = None;
                self.close_menu();
                // A rule the owner just told to be quiet leaves the list now; any other change
                // (another server, other languages) makes the list of another check altogether.
                let kept_ignored = self
                    .grammar
                    .as_ref()
                    .is_some_and(|old| old.enabled == grammar.enabled && old.url == grammar.url && old.languages == grammar.languages);
                if kept_ignored {
                    self.grammar_places.retain(|p| p.rule.as_ref().is_none_or(|r| !grammar.ignored_rules.contains(r)));
                } else {
                    self.grammar_places.clear();
                }
            }
        }
        self.spell = Some(spell);
        self.grammar = Some(grammar);
        self.changed_at = Some(now);
    }

    /// A spelling answer: the places of the revision it was asked for, or none at all.
    fn take_spelling(&mut self, revision: u64, cap: Option<String>, reply: SpellingReply) {
        // Checking was turned off, or another revision took its place: the answer is of a
        // snapshot that is no longer the document's, and is dropped whole.
        if revision != self.spell_rev {
            return;
        }
        // Asked for and answered: this revision is not asked for again, however it came out.
        self.spelling_done = Some(revision);
        let mut notes: Vec<String> = cap.into_iter().collect();
        if let Some(failure) = reply.error {
            self.spelling.clear();
            self.spelling_more = false;
            notes.insert(0, spelling_failure(failure).to_string());
            self.spelling_note = Some(notes.join(" "));
            return;
        }
        if let Some(note) = reply.note {
            notes.push(note);
        }
        let mut places: Vec<Place> = reply
            .diagnostics
            .iter()
            // This computer's spelling is the only spelling: the server's own is not shown.
            .filter(|d| d.kind == Kind::Spelling && d.source == Source::Local)
            .filter_map(|d| as_place(&self.text, d))
            .collect();
        places.sort_by_key(|p| (p.from, p.to));
        self.spelling_more = places.len() > MOST_PLACES;
        places.truncate(MOST_PLACES);
        self.spelling = places;
        self.spelling_note = (!notes.is_empty()).then(|| notes.join(" "));
    }

    /// A spelling check whose channel closed without an answer: said, never retried per frame.
    fn take_spelling_lost(&mut self, revision: u64, cap: Option<String>) {
        if revision != self.spell_rev {
            return;
        }
        self.spelling_done = Some(revision);
        self.spelling.clear();
        self.spelling_more = false;
        let mut notes: Vec<String> = cap.into_iter().collect();
        notes.insert(0, "The spelling check was lost before it answered.".to_string());
        self.spelling_note = Some(notes.join(" "));
    }

    /// A grammar answer: the places of the revision it was asked for, or none at all (a failed
    /// request is not a text with no mistakes in it, it is a text nothing is known about).
    fn take_grammar(&mut self, revision: u64, cap: Option<String>, reply: GrammarReply) {
        if revision != self.grammar_rev {
            return;
        }
        self.grammar_done = Some(revision);
        let mut notes: Vec<String> = cap.into_iter().collect();
        self.online = Some(reply.error.is_none());
        if let Some(failure) = reply.error {
            self.grammar_places.clear();
            self.grammar_more = false;
            self.grammar_outcome = None;
            notes.insert(0, grammar_failure(failure).to_string());
            self.note = Some(notes.join(" "));
            return;
        }
        // A successful answer is the only thing that may populate the checked counters: a
        // failure, a lost channel and an unasked empty text keep them null.
        self.grammar_outcome =
            Some(GrammarOutcome { checked_segments: reply.checked_segments, skipped_segments: reply.skipped_segments, partial: reply.partial });
        if reply.partial || reply.skipped_segments > 0 {
            notes.push(format!(
                "Only part of the message's grammar was checked: {} parts checked, {} skipped.",
                reply.checked_segments, reply.skipped_segments
            ));
        }
        let quiet = self.grammar.as_ref().map(|k| k.ignored_rules.clone()).unwrap_or_default();
        let mut places: Vec<Place> = reply
            .diagnostics
            .iter()
            .filter(|d| d.kind != Kind::Spelling)
            .filter(|d| d.rule.as_ref().is_none_or(|r| !quiet.contains(r)))
            .filter_map(|d| as_place(&self.text, d))
            .collect();
        places.sort_by_key(|p| (p.from, p.to));
        self.grammar_more = places.len() > MOST_PLACES;
        places.truncate(MOST_PLACES);
        self.grammar_places = places;
        self.note = (!notes.is_empty()).then(|| notes.join(" "));
    }

    /// A grammar check whose channel closed without an answer.
    fn take_grammar_lost(&mut self, revision: u64, cap: Option<String>) {
        if revision != self.grammar_rev {
            return;
        }
        self.grammar_done = Some(revision);
        self.grammar_places.clear();
        self.grammar_more = false;
        self.online = Some(false);
        self.grammar_outcome = None;
        let mut notes: Vec<String> = cap.into_iter().collect();
        notes.insert(0, "The grammar check was lost before it answered.".to_string());
        self.note = Some(notes.join(" "));
    }
}

/// What a fixed spelling failure says to the owner: a kind, never an error body or a draft.
fn spelling_failure(failure: Failure) -> &'static str {
    match failure {
        Failure::Unavailable => "Spelling is not checked: this build has no spelling service to ask.",
        Failure::Busy => "Spelling is busy; the message is checked again when it changes.",
        Failure::Cancelled => "The spelling check was set aside.",
        Failure::Refused => "The spelling check was refused.",
        Failure::TooSlow => "The spelling check took too long.",
        Failure::TooLong => "The spelling check was too long.",
        Failure::Failed => "The spelling check failed.",
        Failure::Garbled => "The spelling check came back unreadable.",
    }
}

/// What a fixed grammar failure says to the owner.
fn grammar_failure(failure: Failure) -> &'static str {
    match failure {
        Failure::Unavailable => "The grammar server is not available.",
        Failure::Busy => "The grammar server is busy; the message is checked again when it changes.",
        Failure::Cancelled => "The grammar check was set aside.",
        Failure::Refused => "The grammar server refused the message.",
        Failure::TooSlow => "The grammar server took too long to answer.",
        Failure::TooLong => "The message is too long for the grammar server.",
        Failure::Failed => "The grammar check failed.",
        Failure::Garbled => "The grammar server's answer was unreadable.",
    }
}

/// Whether `place` is the occurrence `ignored` names: the same stretch of the same text, holding
/// the same word, so that a second `recieve` is not left alone by the first one's Ignore Once.
fn same_occurrence(ignored: &Ignored, place: &Place) -> bool {
    ignored.from == place.from && ignored.to == place.to && ignored.word == place.word
}

/// The characters `from..to` of `text`.
pub fn chars_slice(text: &str, from: usize, to: usize) -> Option<&str> {
    Cursor::new(text).slice(from, to)
}

/// Where characters are in a text, walked once from the start: the places of a check are in order,
/// so asking for their text in order costs one pass over the message rather than one per place.
struct Cursor<'t> {
    text: &'t str,
    /// The position of the walk: how many characters have been counted, and where they end.
    chars: usize,
    byte: usize,
}

impl<'t> Cursor<'t> {
    fn new(text: &'t str) -> Cursor<'t> {
        Cursor { text, chars: 0, byte: 0 }
    }

    /// The byte of the character at `at`, walked to from where the last question left off (or from
    /// the start, for a question about an earlier character).
    fn byte_of(&mut self, at: usize) -> Option<usize> {
        if at < self.chars {
            *self = Cursor::new(self.text);
        }
        while self.chars < at {
            let c = self.text.get(self.byte..)?.chars().next()?;
            self.byte += c.len_utf8();
            self.chars += 1;
        }
        Some(self.byte)
    }

    /// The characters `from..to`, when that is a stretch of the text at all.
    fn slice(&mut self, from: usize, to: usize) -> Option<&'t str> {
        if from >= to {
            return None;
        }
        let start = self.byte_of(from)?;
        let end = self.byte_of(to)?;
        self.text.get(start..end)
    }
}

/// A diagnostic's place, in characters, with the word it is on.
fn as_place(text: &str, d: &Diagnostic) -> Option<Place> {
    if !d.span.fits(text) {
        return None;
    }
    let from = text.get(..d.span.start)?.chars().count();
    let to = text.get(..d.span.end)?.chars().count();
    let word = text.get(d.span.start..d.span.end)?.to_string();
    (from < to).then_some(Place {
        from,
        to,
        kind: d.kind,
        source: d.source,
        language: d.language,
        word,
        rule: d.rule.clone(),
        message: d.message.clone(),
        suggestions: d.suggestions.clone(),
    })
}

/// What the whole body was checked as the owner asked (Review › Spelling & Grammar): every place
/// the window has for exactly this text, in bytes, with what could stand there instead.
///
/// `None` while a check of this text under these settings is still on its way (nothing is read
/// from a dictionary here: the host's answer is the list). An error is an answer: the places of
/// that revision are none, and why is said in words by [`Proof::note`] and
/// [`Proof::spelling_note`], never by an empty list pretending the text is clean.
pub(crate) fn review_places(app: &MailApp, text: &str) -> Option<Vec<Diagnostic>> {
    let body = body_of(app)?;
    let doc = app.proof.docs.get(&body)?;
    // The walk is of this body's own settings, as the frame checked it: a French draft must be
    // reviewable under its French choice, not only after File › Options was made French too.
    let settings = settings_for(app, body);
    let keys = Keys::of(app, &settings);
    if doc.text != text || doc.grammar.as_ref() != Some(&keys.grammar) || doc.spell.as_ref() != Some(&keys.spell) {
        return None;
    }
    if settings.check_spelling && doc.spelling_done != Some(doc.spell_rev) {
        return None;
    }
    if settings.check_grammar && doc.grammar_done != Some(doc.grammar_rev) {
        return None;
    }
    let mut at = Cursor::new(text);
    Some(
        doc.places(text)
            .into_iter()
            .filter_map(|p| {
                let start = at.byte_of(p.from)?;
                let end = at.byte_of(p.to)?;
                (text.get(start..end)? == p.word.as_str()).then_some(Diagnostic {
                    span: Span { start, end },
                    kind: p.kind,
                    language: p.language,
                    message: p.message,
                    suggestions: p.suggestions,
                    rule: p.rule,
                    source: p.source,
                })
            })
            .collect(),
    )
}

/// The passive proofing snapshot `ui.inspect` reports: what the window already knows of the
/// document it is drawing, and nothing else. It reads cached state only — no frame is run, no
/// request is queued or polled, no dictionary or file is read, no server is asked, no revision is
/// advanced — so an agent may inspect as often as it likes without changing the window.
pub(crate) fn inspect(app: &MailApp) -> Value {
    let Some(body) = body_of(app) else {
        return Value::Null;
    };
    let settings = settings_for(app, body);
    let keys = Keys::of(app, &settings);
    let text = text_of(app, body);
    let doc = app.proof.docs.get(&body);
    // Current only when a frame has observed this body's live text under the live keys: a check
    // of other words, other settings or another running server instance is never reported as now.
    let current = doc.is_some_and(|doc| {
        text.as_deref().is_some_and(|text| doc.text == text) && doc.spell.as_ref() == Some(&keys.spell) && doc.grammar.as_ref() == Some(&keys.grammar)
    });
    let revision = doc.map_or(Value::Null, |doc| json!(doc.stamp));
    let languages: Vec<&str> = settings.active().iter().map(|l| l.code()).collect();

    let (spelling_done, spelling_pending, spelling_places, spelling_note, spelling_capped) = match doc.filter(|_| current) {
        Some(doc) => (
            doc.spelling_done == Some(doc.spell_rev),
            app.proof.spelling.as_ref().is_some_and(|waiting| waiting.body == body && waiting.revision == doc.spell_rev),
            Some(doc.spelling_shown()),
            Some(doc.spelling_note.is_some()),
            Some(doc.spelling_more),
        ),
        None => (false, false, None, None, None),
    };
    let grammar = doc.filter(|_| current);
    let grammar_done = grammar.is_some_and(|doc| doc.grammar_done == Some(doc.grammar_rev));
    let grammar_pending =
        grammar.is_some_and(|doc| app.proof.grammar.as_ref().is_some_and(|waiting| waiting.body == body && waiting.revision == doc.grammar_rev));
    let grammar_places = grammar.map(Doc::grammar_shown);
    let grammar_note = grammar.map(|doc| doc.note.is_some());
    let grammar_capped = grammar.map(|doc| doc.grammar_more);
    // The checked counters are of a successful reply actually taken for this revision: a pending,
    // failed, lost, empty, disabled or unobserved check reports null, never a clean zero.
    let outcome = grammar.filter(|_| grammar_done).and_then(|doc| doc.grammar_outcome);
    let online = grammar.filter(|_| grammar_done).and_then(|doc| doc.online);

    let body = match body {
        Body::Message(id) => json!({ "kind": "compose", "id": id }),
        Body::Signature(id) => json!({ "kind": "signature", "id": id }),
    };
    json!({
        "body": body,
        "current": current,
        "revision": revision,
        "languages": languages,
        "asYouType": settings.as_you_type,
        "available": app.services.proof_native.is_some(),
        "spelling": {
            "enabled": keys.spell.enabled,
            "done": spelling_done,
            "pending": spelling_pending,
            "places": spelling_places,
            "hasNote": spelling_note,
            "capped": spelling_capped,
        },
        "grammar": {
            "enabled": keys.grammar.enabled,
            "done": grammar_done,
            "pending": grammar_pending,
            "places": grammar_places,
            "hasNote": grammar_note,
            "capped": grammar_capped,
            "online": online,
            "checkedSegments": outcome.map(|o| o.checked_segments),
            "skippedSegments": outcome.map(|o| o.skipped_segments),
            "partial": outcome.map(|o| o.partial),
        },
        "requests": {
            "spelling": app.proof.spelled,
            "grammar": app.proof.asked,
        },
    })
}

/// Asks the host's dictionaries what could stand in for `word`, once, on demand. `None` when this
/// build has no host (nothing is worked out on this thread instead).
///
/// This is the current document's question (the review dialog's): it chooses the body the dialog
/// walks, and that body's own settings. A place menu knows its body already and asks
/// [`suggest_with`] instead, so a menu of one draft can never be answered for another's languages.
pub(crate) fn suggest(app: &MailApp, word: &str, limit: usize) -> Option<(Receiver<SuggestionReply>, Cancellation)> {
    let settings = body_of(app).map_or_else(|| app.session.prefs.proof.clone(), |body| settings_for(app, body));
    suggest_with(app, settings, word, limit)
}

/// Asks the host's dictionaries for `word` under exactly `settings`: the body the place was found
/// in, never whichever document happens to be drawn now.
fn suggest_with(app: &MailApp, settings: ProofSettings, word: &str, limit: usize) -> Option<(Receiver<SuggestionReply>, Cancellation)> {
    let service = app.services.proof_native.as_ref()?;
    let cancelled = Arc::new(AtomicBool::new(false));
    let request = SuggestionRequest { word: word.to_string(), settings, limit: limit.min(8), cancelled: cancelled.clone() };
    Some((service.suggest(request), cancelled))
}

/// A right click at the character `at` of `body`'s text: opens the menu of the place under it, or
/// closes the one that was open when the click landed anywhere else.
pub(crate) fn right_click(app: &mut MailApp, body: Body, text: &str, at: Option<usize>, pointer: egui::Pos2) {
    let Some(doc) = app.proof.docs.get_mut(&body) else {
        return;
    };
    if let Some(open) = doc.menu.take() {
        abandon(&open);
    }
    let stamp = doc.stamp;
    let place = at.and_then(|at| doc.place_at(text, at));
    // The document borrow ends here: the suggestions may be asked of the host, which needs the
    // window itself, before the menu is put back into the document.
    let menu = place.map(|place| {
        // The server's own suggestions are kept; this computer's are asked for once, now, and
        // never eagerly for a whole document. A grammar place asks no dictionary at all.
        let suggestions = if place.suggestions.is_empty() && place.kind == Kind::Spelling {
            match suggest_with(app, settings_for(app, body), &place.word, SUGGESTIONS) {
                Some((channel, cancelled)) => MenuSuggestions::Asking { channel, cancelled },
                None => MenuSuggestions::None,
            }
        } else {
            MenuSuggestions::Ready(place.suggestions.clone())
        };
        Menu { stamp, place, suggestions, open: true, at: pointer, pass: 0 }
    });
    if let Some(doc) = app.proof.docs.get_mut(&body) {
        doc.menu = menu;
    }
}

/// Whether `body`'s place menu is open.
pub(crate) fn menu_open(app: &MailApp, body: Body) -> bool {
    app.proof.docs.get(&body).is_some_and(|doc| doc.menu.is_some())
}

/// The suggestions a menu has taken, for the tests: the answer a delayed dictionary question
/// brought is visible without drawing the popup's buttons.
#[cfg(test)]
pub(crate) fn menu_suggestions(app: &MailApp, body: Body) -> Option<Vec<String>> {
    match &app.proof.docs.get(&body)?.menu.as_ref()?.suggestions {
        MenuSuggestions::Ready(list) => Some(list.clone()),
        _ => None,
    }
}

/// Closes `body`'s place menu, when the body beneath it changed or the owner chose to.
pub(crate) fn close_menu(app: &mut MailApp, body: Body) {
    if let Some(doc) = app.proof.docs.get_mut(&body)
        && let Some(open) = doc.menu.take()
    {
        abandon(&open);
    }
}

/// Stops whatever the menu had asked for: it is being closed, and its answer is nobody's.
fn abandon(menu: &Menu) {
    if let MenuSuggestions::Asking { cancelled, .. } = &menu.suggestions {
        cancelled.store(true, Ordering::Relaxed);
    }
}

/// Draws the menu of the place that was right-clicked in `body`, and does what its items say.
///
/// It is an egui menu popup of the body's own response, so egui keeps it open from pass to pass
/// and closes it on Escape or a click anywhere else; the window closes it too when the revision
/// it was opened on is gone. What could stand there was read once, when it opened: opening the
/// menu looks at no dictionary on this thread.
pub(crate) fn menu(app: &mut MailApp, ctx: &egui::Context, resp: &Response, body: Body) {
    let Some(text) = text_of(app, body) else {
        close_menu(app, body);
        return;
    };
    let pass = ctx.cumulative_pass_nr();
    let Some(mut menu) = app.proof.docs.get_mut(&body).and_then(|doc| doc.menu.take()) else {
        return;
    };
    let usable = app.proof.docs.get(&body).is_some_and(|doc| {
        menu.stamp == doc.stamp
            && text == doc.text
            && menu.open
            && (menu.pass == 0 || menu.pass.saturating_add(MENU_PASSES) >= pass)
            && chars_slice(&text, menu.place.from, menu.place.to) == Some(menu.place.word.as_str())
    });
    if !usable {
        abandon(&menu);
        return;
    }
    // The suggestions asked for when the menu opened: taken now, or waited for with a frame
    // scheduled, so a menu opened before its dictionary answered still shows them.
    if let MenuSuggestions::Asking { channel, .. } = &menu.suggestions {
        match channel.try_recv() {
            Ok(reply) => {
                menu.suggestions = if reply.error.is_none() && !reply.suggestions.is_empty() {
                    MenuSuggestions::Ready(reply.suggestions)
                } else {
                    MenuSuggestions::None
                };
            }
            Err(TryRecvError::Empty) => ctx.request_repaint_after(POLL),
            Err(TryRecvError::Disconnected) => menu.suggestions = MenuSuggestions::None,
        }
    }
    menu.pass = pass;
    let place = menu.place.clone();
    let at = menu.at;
    let mut open = menu.open;
    let mut choice: Option<Choice> = None;
    Popup::menu(resp).at_position(at).open_bool(&mut open).show(|ui: &mut Ui| {
        ui.set_max_width(320.0);
        if !place.word.is_empty() {
            ui.label(egui::RichText::new(&place.word).strong());
        }
        match &menu.suggestions {
            MenuSuggestions::Ready(list) if !list.is_empty() => {
                for s in list {
                    if ui.add(egui::Button::new(egui::RichText::new(s)).frame(false)).clicked() {
                        choice = Some(Choice::Replace(s.clone()));
                    }
                }
            }
            MenuSuggestions::Asking { .. } => {
                ui.label(egui::RichText::new(crate::tl!("Asking…")).small());
            }
            _ => {
                ui.label(crate::tl!("No suggestions"));
            }
        }
        if !place.message.is_empty() {
            ui.separator();
            ui.label(egui::RichText::new(&place.message).small());
        }
        ui.separator();
        if ui.button(crate::tl!("Ignore Once")).clicked() {
            choice = Some(Choice::IgnoreOnce);
        }
        // A dictionary has nothing to say about a grammar place, and a word no dictionary knows
        // is the only one there is to add.
        if place.kind == Kind::Spelling && ui.button(crate::tl!("Add to Dictionary")).clicked() {
            choice = Some(Choice::AddWord);
        }
        if let Some(rule) = place.rule.clone()
            && ui.button(crate::tl!("Ignore This Rule")).clicked()
        {
            choice = Some(Choice::IgnoreRule(rule));
        }
    });
    menu.open = open;
    if !open || choice.is_some() {
        abandon(&menu);
    } else if let Some(doc) = app.proof.docs.get_mut(&body) {
        doc.menu = Some(menu);
    }
    let Some(choice) = choice else { return };
    match choice {
        // The positions are the document's characters, and the text the menu was opened on goes
        // with them: a correction that would land in other words is refused, not guessed at. It
        // runs the command a script runs, so it is one step of the message's own undo and keeps
        // the command's permission boundary.
        Choice::Replace(with) => {
            let Some(full) = text_of(app, body) else { return };
            if chars_slice(&full, place.from, place.to) != Some(place.word.as_str()) {
                return;
            }
            let mut params = json!({
                "text": with,
                "from": place.from,
                "to": place.to,
                "expectedText": full,
            });
            match body {
                Body::Message(id) => params["composeId"] = json!(id),
                Body::Signature(id) => {
                    params["signature"] = json!(true);
                    params["signatureId"] = json!(id);
                }
            }
            let _ = app.execute("proof.change", params);
        }
        Choice::IgnoreOnce => app.proof.ignore_once(body, &place),
        Choice::AddWord => {
            if app.execute("proof.addWord", json!({ "word": place.word })).is_ok() {
                app.proof.forget_word(body, &place.word);
            }
        }
        Choice::IgnoreRule(rule) => {
            if app.execute("proof.ignoreRule", json!({ "rule": rule })).is_ok() {
                app.proof.forget_rule(body, &rule);
            }
        }
    }
}

/// Keep the connection test tied to the configuration it was made for.
///
/// File › Options calls this on every frame that draws the page, once the mode and address
/// controls are drawn and before either the test's answer or the built-in server's panel is read
/// for that frame. A command can change the settings between frames just as a click can, so what
/// is compared is the configuration itself — the mode, the address, the grammar switch, the
/// languages and the quiet rules — never a control's change flag. When it differs from the one
/// the test state was kept under, the last answer is dropped, both channels are let go, and a
/// native request still on its way has its cancellation flag set.
///
/// Nothing is asked of a host, read from disk, or waited for here. `true` when the configuration
/// changed, so the page can drop a refusal note that belonged to the old one.
pub(crate) fn sync_connection_test(app: &mut MailApp) -> bool {
    let key = ConnectionKey::of(&app.session.prefs.proof);
    if app.proof.connection.as_ref() == Some(&key) {
        return false;
    }
    // The first frame that draws the page only records the configuration: there is nothing yet
    // that could be stale.
    if app.proof.connection.replace(key).is_none() {
        return false;
    }
    app.proof.forget_test();
    true
}

/// Asks the grammar server, as File › Options' Test the connection does: one short text, and the
/// answer (or why there is none) said in words on the page, never in a dialog that would stop the
/// window.
pub(crate) fn test(app: &mut MailApp) {
    // The Server edit can commit immediately before this press. Record its new key now so
    // the later render synchronization does not cancel the test that was just requested.
    sync_connection_test(app);
    let settings = app.session.prefs.proof.clone();
    // A press is about the configuration as it is now: whatever an earlier press asked for is
    // called off rather than left to answer for a test that is no longer waited for.
    app.proof.forget_test();
    let (url, _, why) = grammar_endpoint(app, &settings);
    if !settings.check_grammar {
        app.proof.tested = Some(crate::tl!("Check grammar with a LanguageTool server").to_string());
        return;
    }
    if url.is_empty() {
        app.proof.tested = why;
        return;
    }
    let language = settings.active().first().copied().unwrap_or(Language::EnCa);
    // A text with something in it for every language's rules to look at, and nothing of the
    // owner's: the server is told only that this computer is checking a grammar server's answers.
    let text = match language {
        Language::FrFr | Language::FrCa => "Je vais envoyer le document demain.".to_string(),
        _ => "I will send the document tomorrow.".to_string(),
    };
    app.proof.tested = Some(crate::tl!("Asking…").to_string());
    if let Some(service) = app.services.proof_native.as_ref() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let channel = service.grammar(GrammarRequest {
            url,
            text,
            languages: settings.active(),
            ignored_rules: settings.ignored_rules.clone(),
            cancelled: Arc::clone(&cancelled),
        });
        app.proof.testing_native = Some(channel);
        // Kept, so that a configuration change can call the request off instead of waiting for
        // an answer that would belong to a mode or an address no longer chosen.
        app.proof.testing_native_cancel = Some(cancelled);
        return;
    }
    // The older host interface, kept for compatibility with a host that has only this one.
    if let Some(service) = app.services.proof_check.as_ref() {
        app.proof.testing = Some(service(Request { url, text, language, languages: settings.active(), ignored: settings.ignored_rules.clone() }));
    } else {
        app.proof.tested = Some(crate::tl!("This build has no grammar server to ask.").to_string());
    }
}

/// What the connection test said, once it has an answer; `None` when no test was made.
pub(crate) fn test_result(app: &mut MailApp) -> Option<String> {
    if let Some(channel) = app.proof.testing_native.as_ref() {
        let reply = match channel.try_recv() {
            Ok(reply) => reply,
            Err(TryRecvError::Empty) => return Some(crate::tl!("Asking…").to_string()),
            Err(TryRecvError::Disconnected) => GrammarReply { error: Some(Failure::Unavailable), ..Default::default() },
        };
        app.proof.testing_native = None;
        app.proof.testing_native_cancel = None;
        let said = match reply.error {
            Some(failure) => grammar_failure(failure).to_string(),
            None if reply.partial => "The server answered, but only part of the test text was checked.".to_string(),
            None => format!("{} {}", crate::tl!("The server answered:"), crate::tln!(reply.diagnostics.len(), "{n} note", "{n} notes")),
        };
        app.proof.tested = Some(said.clone());
        return Some(said);
    }
    let channel = app.proof.testing.as_ref()?;
    let reply = match channel.try_recv() {
        Ok(reply) => reply,
        Err(TryRecvError::Empty) => return Some(crate::tl!("Asking…").to_string()),
        Err(TryRecvError::Disconnected) => Reply { text: String::new(), diagnostics: Vec::new(), error: Some("the answer was lost".into()) },
    };
    app.proof.testing = None;
    let said = match reply.error {
        Some(error) => error,
        None => format!("{} {}", crate::tl!("The server answered:"), crate::tln!(reply.diagnostics.len(), "{n} note", "{n} notes")),
    };
    app.proof.tested = Some(said.clone());
    Some(said)
}

/// The value a walk's place is given as a supplied diagnostic, in bytes of the checked text.
pub(crate) fn supplied(d: &Diagnostic) -> Value {
    json!({
        "start": d.span.start,
        "end": d.span.end,
        "kind": match d.kind {
            Kind::Spelling => "spelling",
            Kind::Style => "style",
            Kind::Grammar => "grammar",
        },
        "message": d.message,
        "suggestions": d.suggestions,
        "rule": d.rule,
        "language": d.language.map(|l| l.code()),
    })
}
