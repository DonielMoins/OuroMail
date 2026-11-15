// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! The session's proofing: the dictionaries it read from this computer, the words the owner added,
//! and what Review › Spelling & Grammar is walking through (#954).
//!
//! The words are `mailcraft-model`'s ([`mailcraft_model::proof`]); what is here is where they come
//! from — files this computer already has, read once and kept for the session — and the walk
//! through the message being written, which is a list of places and an index into it.
//!
//! Nothing here opens a connection and nothing here reads a mailbox: a dictionary is a file, and
//! grammar is the host's business (`mailcraft_net::languagetool`, asked by the window or the CLI,
//! never by the engine).
//!
//! A message's text is data. It is checked, corrected and forgotten; it is never logged, never put
//! in an error's words. The grammar transport accepts only a server on this computer
//! ([`mailcraft_model::proof::ProofSettings::grammar_allowed`]).

use std::io::Read;
use std::path::{Path, PathBuf};

use mailcraft_model::proof::{Diagnostic, Kind, Language, ProofSettings, Source, Span, Speller};

/// Most bytes one dictionary file may be: a `.dic` of the largest languages is a few megabytes.
const MAX_DICT_BYTES: u64 = 16 * 1024 * 1024;

/// Which editor a review belongs to: a message being written or a signature being edited, each by
/// the identity it was given. Never by its words — an editor of the same text is still another
/// editor, and a review of the one closed is not a review of its successor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Editor {
    /// The message being written, by its id ([`crate::Session::compose_id`]).
    Compose(u64),
    /// The signature being edited, by its id ([`crate::Session::signature_id`]).
    Signature(u64),
}

/// The owner's proofing state for the session: the dictionaries, and the walk.
#[derive(Clone, Default)]
pub struct Proofing {
    speller: Speller,
    /// Updating personal words must not reread and expand every installed dictionary.
    personal: Vec<String>,
    /// A review belongs to this editor instance, even when another editor has identical words.
    document: Option<Editor>,
    /// A review's places also belong to the exact effective configuration that found them.
    /// Changing languages without changing the text must not leave an old correction usable.
    review_settings: Option<ProofSettings>,
    /// What the speller was read for (the languages and the folder): read again when that changes.
    key: Option<Key>,
    /// Why there is no dictionary, in words for File › Options, when there is none.
    pub note: Option<String>,
    /// The dictionaries that were read, as File › Options names them.
    pub read: Vec<String>,
    /// Review › Spelling & Grammar, while it is open.
    pub walk: Option<Walk>,
}

/// What the dictionaries were read from and for.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Key {
    languages: Vec<Language>,
    dir: String,
}

/// Review › Spelling & Grammar: the places to look at, and which one is on screen.
#[derive(Clone, PartialEq, Eq)]
pub struct Walk {
    /// The text the places are in, as it was when they were found: a place is applied only while
    /// the text is still this one.
    pub text: String,
    /// The places, in order.
    pub items: Vec<Diagnostic>,
    /// The one on screen.
    pub at: usize,
    /// The places the owner said to leave alone this time, by their place in the text.
    pub ignored: Vec<Span>,
}

impl std::fmt::Debug for Proofing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Proofing").field("loaded", &self.key.is_some()).field("walk", &self.walk).finish()
    }
}

impl std::fmt::Debug for Walk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Walk").field("places", &self.items.len()).field("at", &self.at).finish_non_exhaustive()
    }
}

impl Walk {
    /// The place on screen, if any.
    pub fn current(&self) -> Option<&Diagnostic> {
        self.items.get(self.at)
    }

    /// How many places are left after this one.
    pub fn left(&self) -> usize {
        self.items.len().saturating_sub(self.at + 1)
    }
}

impl Proofing {
    /// Forget a walk when its effective languages, words or other proofing settings change.
    /// This compares cached state only; it never reads dictionaries or asks a provider.
    pub fn bind_settings(&mut self, settings: &ProofSettings) {
        if self.review_settings.as_ref() != Some(settings) {
            self.walk = None;
            self.review_settings = Some(settings.clone());
        }
    }

    /// Another editor instance invalidates a review; dictionary caches are independent of drafts.
    pub fn bind_document(&mut self, editor: Option<Editor>) {
        if self.document != editor {
            self.walk = None;
            self.document = editor;
        }
    }

    /// The words of every dictionary the settings turn on, read from this computer if they are not
    /// read yet (or were read for another set of languages).
    pub fn words(&mut self, settings: &ProofSettings) -> &Speller {
        let key = Key { languages: settings.active(), dir: settings.dictionary_dir.clone() };
        if self.key.as_ref() != Some(&key) {
            let (speller, note) = read_dictionaries(settings);
            self.read = speller.languages().iter().map(|l| l.name().to_string()).collect();
            self.speller = speller;
            self.personal.clear();
            self.note = note;
            self.key = Some(key);
        }
        if !self.personal.iter().eq(settings.personal.iter().take(mailcraft_model::proof::MAX_PERSONAL)) {
            self.speller.set_personal(&settings.personal_words());
            self.personal = settings.personal.iter().take(mailcraft_model::proof::MAX_PERSONAL).cloned().collect();
        }
        &self.speller
    }

    /// Already prepared words only: mutation responses and UI frames must never start file I/O.
    pub fn cached_words(&self, settings: &ProofSettings) -> Option<&Speller> {
        let key = Key { languages: settings.active(), dir: settings.dictionary_dir.clone() };
        (self.key.as_ref() == Some(&key)).then_some(&self.speller)
    }

    /// Forgets what was read, and what was said about it, when the settings now name another set
    /// of languages or another folder. Nothing is read here: a settings save never waits on files,
    /// and the next check reads the dictionaries the new settings turn on.
    pub fn forget_stale_cache(&mut self, settings: &ProofSettings) {
        let key = Key { languages: settings.active(), dir: settings.dictionary_dir.clone() };
        if self.key.is_some() && self.key.as_ref() != Some(&key) {
            self.key = None;
            self.speller = Speller::new();
            self.personal.clear();
            self.read.clear();
            self.note = None;
        }
    }

    /// Whether a dictionary was read.
    pub fn is_loaded(&mut self, settings: &ProofSettings) -> bool {
        !self.words(settings).is_empty()
    }

    /// The spelling places of `text`, this computer's dictionaries alone.
    ///
    /// Nothing at all when there is no dictionary: without words to check against, every word
    /// would be wrong, and a window that underlined a whole message would be lying about it.
    pub fn check(&mut self, settings: &ProofSettings, text: &str) -> Vec<Diagnostic> {
        let words = self.words(settings);
        if words.is_empty() { Vec::new() } else { words.check(text) }
    }

    /// What could stand in for `word`.
    pub fn suggest(&mut self, settings: &ProofSettings, word: &str, limit: usize) -> Vec<String> {
        self.words(settings).suggest(word, limit)
    }

    /// Whether there are words to check against at all.
    pub fn ready(&mut self, settings: &ProofSettings) -> bool {
        !self.words(settings).is_empty()
    }

    /// Starts the walk over `text`, with the places `found` (spelling found here, and grammar the
    /// host brought back — each taken only while it still fits `text`).
    pub fn start(&mut self, text: &str, mut found: Vec<Diagnostic>) -> Walk {
        found.retain(|d| d.span.fits(text));
        found.sort_by_key(|d| (d.span.start, d.span.end));
        let walk = Walk { text: text.to_string(), items: found, at: 0, ignored: Vec::new() };
        self.walk = Some(walk.clone());
        walk
    }

    /// The walk, if the text it was started on is still the text there is.
    pub fn current_walk(&self, text: &str) -> Option<&Walk> {
        self.walk.as_ref().filter(|w| w.text == text)
    }

    /// Goes to the next place, past the one on screen (the walk is over when there is none).
    pub fn next(&mut self, text: &str) -> Option<&Walk> {
        if self.walk.as_ref().is_some_and(|w| w.text != text) {
            self.walk = None;
            return None;
        }
        let walk = self.walk.as_mut()?;
        walk.at = walk.at.saturating_add(1);
        Some(walk)
    }

    /// Drops the place on screen (the owner ignored it), keeping the walk where it is.
    pub fn ignore(&mut self, text: &str) -> Option<&Walk> {
        if self.walk.as_ref().is_some_and(|w| w.text != text) {
            self.walk = None;
            return None;
        }
        let walk = self.walk.as_mut()?;
        if let Some(d) = walk.items.get(walk.at) {
            walk.ignored.push(d.span);
        }
        walk.at = walk.at.saturating_add(1);
        Some(walk)
    }

    /// Closes the walk.
    pub fn close(&mut self) {
        self.walk = None;
    }
}

/// Reads every dictionary the settings turn on, in the language asked for or one of the same
/// tongue when that one is not installed.
///
/// What was read and what was not is said in words (`note`), so that File › Options can tell the
/// owner that French is checked and Canadian French is not installed, rather than silently doing
/// nothing.
pub fn read_dictionaries(settings: &ProofSettings) -> (Speller, Option<String>) {
    let mut speller = Speller::new();
    let dirs = dictionary_dirs(settings);
    let mut missing: Vec<&str> = Vec::new();
    let mut used: Vec<String> = Vec::new();
    for lang in settings.active() {
        let mut got = false;
        for try_lang in [lang, lang.kin()] {
            if let Some(dict) = find(try_lang, &dirs) {
                used.push(try_lang.name().to_string());
                if try_lang != lang {
                    missing.push(lang.name());
                }
                speller.put(dict);
                got = true;
                break;
            }
        }
        if !got {
            missing.push(lang.name());
        }
    }
    let note = if used.is_empty() {
        Some("No dictionary was found: install one (hunspell-en-ca and hunspell-fr on Linux, for instance), or name its folder in File › Options › Spelling & Grammar.".to_string())
    } else if !missing.is_empty() {
        Some(format!("No dictionary for {}.", missing.join(", ")))
    } else {
        None
    };
    (speller, note)
}

/// The folders dictionaries are looked in, in order: the owner's own if they named one.
pub fn dictionary_dirs(settings: &ProofSettings) -> Vec<PathBuf> {
    // A named folder is a filesystem path and is used exactly as it was kept: its spaces, leading,
    // trailing or all of them, are part of its name. Only an exact empty string means no folder
    // was named, and then the usual places are used.
    let named = settings.dictionary_dir.as_str();
    if !named.is_empty() {
        return vec![PathBuf::from(named)];
    }
    let mut out: Vec<PathBuf> = Vec::new();
    if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
        let home = PathBuf::from(home);
        out.push(home.join(".local/share/hunspell"));
        out.push(home.join(".local/share/mailcraft/dictionaries"));
        out.push(home.join("Library/Spelling"));
    }
    if let Ok(data) = std::env::var("XDG_DATA_HOME") {
        out.push(PathBuf::from(data).join("hunspell"));
    }
    if let Ok(appdata) = std::env::var("APPDATA") {
        out.push(PathBuf::from(appdata).join("MailCraft/dictionaries"));
    }
    for fixed in ["/usr/share/hunspell", "/usr/local/share/hunspell", "/usr/share/myspell/dicts", "/usr/share/myspell"] {
        out.push(PathBuf::from(fixed));
    }
    out
}

/// The first readable dictionary, not merely the first file with a dictionary's name.
fn find(lang: Language, dirs: &[PathBuf]) -> Option<mailcraft_model::proof::Dictionary> {
    let codes = names_for(lang);
    for dir in dirs {
        for code in &codes {
            let dic = dir.join(format!("{code}.dic"));
            let Some(text) = read_capped(&dic) else { continue };
            let aff = read_capped(&dir.join(format!("{code}.aff"))).unwrap_or_default();
            if let Ok(dictionary) = mailcraft_model::proof::Dictionary::from_hunspell(lang, &aff, &text)
                && !dictionary.is_empty()
            {
                return Some(dictionary);
            }
        }
    }
    None
}

/// The names a dictionary of `lang` is installed under, most particular first.
fn names_for(lang: Language) -> Vec<String> {
    let (base, region) = match lang {
        Language::EnCa => ("en", "CA"),
        Language::EnUs => ("en", "US"),
        Language::FrCa => ("fr", "CA"),
        Language::FrFr => ("fr", "FR"),
    };
    vec![
        format!("{base}_{region}"),
        format!("{base}-{region}"),
        format!("{}_{}", base, region.to_ascii_lowercase()),
        base.to_string(),
        format!("{base}/{base}_{region}"),
    ]
}

/// A file's text, when it is a file of no more than [`MAX_DICT_BYTES`]: anything larger, or
/// anything that is not UTF-8, is no dictionary.
fn read_capped(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_DICT_BYTES {
        return None;
    }
    // The file may grow after metadata was read. Bound the read itself, not only that hint.
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_DICT_BYTES.saturating_add(1)).read_to_end(&mut bytes).ok()?;
    if u64::try_from(bytes.len()).ok()? > MAX_DICT_BYTES {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// Whether `d` is a place worth showing in the walk's own list: nothing here decides.
pub fn is_sound(d: &Diagnostic, text: &str) -> bool {
    d.is_sound()
        && d.span.fits(text)
        && matches!(d.kind, Kind::Spelling | Kind::Grammar | Kind::Style)
        && matches!(d.source, Source::Local | Source::Server)
}
