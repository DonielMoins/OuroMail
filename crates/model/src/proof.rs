// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Proofing the message being written (#954): spelling checked against every dictionary the owner
//! turned on at once, and grammar read from a LanguageTool server on this computer.
//!
//! Nothing here does I/O and nothing here knows about a window. The text comes in as a string
//! whose provider positions are **byte** offsets (the editor converts them to character positions), the diagnostics go
//! out as [`Diagnostic`]s over the same positions, and what to check is [`ProofSettings`].
//!
//! - A word is accepted if **any** enabled language accepts it ([`Speller::accept`]); a sentence
//!   written half in English and half in French is never flagged for the mixing itself.
//! - Grammar comes from a server (LanguageTool's `/v2/check`); its answers are read by
//!   [`grammar`], which is as suspicious of them as of any other bytes from outside, and whose
//!   offsets are converted from UTF-16 code units to byte offsets here, once, with tests.
//! - A dictionary is a file on this computer ([`spell`]): no dictionary is fetched.
//!
//! What a diagnostic spans is a byte range that is always on character boundaries, always inside
//! the text it was found in, and never empty; a diagnostic that could not be placed is dropped
//! rather than clamped into something that would replace the wrong text.

use serde::{Deserialize, Serialize};
use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

use crate::ModelError;

mod grammar;
#[cfg(test)]
mod grammar_tests;
mod spell;
#[cfg(test)]
mod spell_tests;
#[cfg(test)]
mod tests;

pub use grammar::{CheckRequest, parse_answer};
pub use spell::{Dictionary, Speller};

/// Most characters of one text that are checked at once: a message body is capped well below
/// this ([`crate::rich::MAX_CHARS`]); a text longer than this is checked only up to here.
pub const MAX_CHECKED: usize = 100_000;

/// Longest word looked at, in characters; longer runs are a pasted address or a code, not a word.
pub const MAX_WORD: usize = 48;

/// Most diagnostics one check answers with.
pub const MAX_DIAGNOSTICS: usize = 4_000;

/// Most suggestions one diagnostic carries.
pub const MAX_SUGGESTIONS: usize = 8;

/// Most words a personal dictionary holds.
pub const MAX_PERSONAL: usize = 10_000;

/// Longest personal word, in characters.
pub const MAX_PERSONAL_WORD: usize = 64;

/// Longest grammar server answer read, in bytes.
pub const MAX_ANSWER: usize = 1024 * 1024;

/// Most characters of a suggestion, or of a message, that are kept: an answer from a grammar
/// server is from outside, and one of megabytes is not one to hold whole.
pub const MAX_TEXT: usize = 512;

/// The languages proofing knows. A dictionary says which one it is; the owner picks which are on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Language {
    /// Canadian English.
    #[serde(rename = "en-CA")]
    #[default]
    EnCa,
    /// Canadian French.
    #[serde(rename = "fr-CA")]
    FrCa,
    /// American English, for an owner who wants it beside the Canadian one.
    #[serde(rename = "en-US")]
    EnUs,
    /// French of France.
    #[serde(rename = "fr-FR")]
    FrFr,
}

impl Language {
    /// Every language proofing can be set to, in the order the settings list them.
    pub const ALL: [Language; 4] = [Language::EnCa, Language::FrCa, Language::EnUs, Language::FrFr];

    /// Its code, as a dictionary is named and as LanguageTool writes it: `en-CA`.
    pub fn code(self) -> &'static str {
        match self {
            Language::EnCa => "en-CA",
            Language::FrCa => "fr-CA",
            Language::EnUs => "en-US",
            Language::FrFr => "fr-FR",
        }
    }

    /// Its name in English, for the settings.
    pub fn name(self) -> &'static str {
        match self {
            Language::EnCa => "English (Canada)",
            Language::FrCa => "French (Canada)",
            Language::EnUs => "English (United States)",
            Language::FrFr => "French (France)",
        }
    }

    /// The language `code` names: `en-CA`, `en_CA`, `EN-ca`, or just `en` (Canadian English) or
    /// `fr` (Canadian French), which is what a system setting usually says.
    pub fn parse(code: &str) -> Option<Language> {
        let code = code.trim();
        if code.is_empty() || code.len() > 16 {
            return None;
        }
        let mut parts = code.split(['-', '_']);
        let tongue = parts.next()?.to_ascii_lowercase();
        let region = parts.next().unwrap_or_default().to_ascii_uppercase();
        if parts.next().is_some() {
            return None;
        }
        match (tongue.as_str(), region.as_str()) {
            ("en", "" | "CA" | "US") => Some(if region == "US" { Language::EnUs } else { Language::EnCa }),
            ("fr", "" | "CA" | "FR") => Some(if region == "FR" { Language::FrFr } else { Language::FrCa }),
            ("en", _) | ("fr", _) => None,
            _ => None,
        }
    }

    /// Whether it is English.
    pub fn is_english(self) -> bool {
        matches!(self, Language::EnCa | Language::EnUs)
    }

    /// The language on another side of the same tongue, or itself: an American dictionary stands
    /// in for a Canadian one that is missing, and the other way round.
    pub fn kin(self) -> Language {
        match self {
            Language::EnCa => Language::EnUs,
            Language::EnUs => Language::EnCa,
            Language::FrCa => Language::FrFr,
            Language::FrFr => Language::FrCa,
        }
    }
}

/// Why a piece of proofing could not be done.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProofError {
    /// A dictionary is not a dictionary: empty, or with a line that cannot be read.
    #[error("the dictionary {0} cannot be read")]
    Dictionary(String),
    /// The grammar server's answer is not one.
    #[error("the grammar server's answer cannot be read")]
    Answer,
    /// A text is too long to check.
    #[error("the text is too long to check")]
    TooLong,
    /// The settings name an address proofing does not speak to.
    #[error("{0}")]
    Refused(String),
}

impl From<ProofError> for ModelError {
    fn from(e: ProofError) -> ModelError {
        ModelError::Invalid(e.to_string())
    }
}

/// What a diagnostic is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// A word no enabled dictionary knows.
    Spelling,
    /// Grammar, from the server.
    Grammar,
    /// Style, from the server.
    Style,
}

/// Where a diagnostic came from, which is also where the text went for it: nowhere, or a server
/// named in the settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// This computer's dictionaries.
    Local,
    /// The grammar server.
    Server,
}

/// A range of the checked text, in bytes, on character boundaries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    /// The span `start..end` of `text`, or `None` when either end is past the text, is not on a
    /// character boundary, or the span is empty: what would put a correction in the wrong place.
    pub fn of(text: &str, start: usize, end: usize) -> Option<Span> {
        let len = text.len();
        (start < end && end <= len && text.is_char_boundary(start) && text.is_char_boundary(end)).then_some(Span { start, end })
    }

    /// Whether it is a span of `text`, on its boundaries and inside it.
    pub fn fits(&self, text: &str) -> bool {
        Span::of(text, self.start, self.end).is_some()
    }

    /// How many characters it spans.
    pub fn chars(&self, text: &str) -> usize {
        text.get(self.start..self.end).map_or(0, |s| s.chars().count())
    }
}

/// One thing to look at in the text, and what could be put in its place.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Diagnostic {
    pub span: Span,
    pub kind: Kind,
    /// The language it was found against, when one can be told: the dictionary that refused it, or
    /// the language the server was asked in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<Language>,
    /// What is wrong, in the server's words (English, from LanguageTool) or in none for spelling.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub message: String,
    /// What could stand there instead, the best first.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub suggestions: Vec<String>,
    /// The server's rule id, for a rule the owner may ignore.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    pub source: Source,
}

impl Diagnostic {
    /// A diagnostic, with its suggestions cut to [`MAX_SUGGESTIONS`] and each of them made safe to
    /// put in the text (one line, no control characters), and a message cut short.
    pub fn new(span: Span, kind: Kind, source: Source) -> Diagnostic {
        Diagnostic { span, kind, language: None, message: String::new(), suggestions: Vec::new(), rule: None, source }
    }

    /// The same with what could stand there instead, cleaned and cut.
    pub fn suggesting(mut self, suggestions: impl IntoIterator<Item = String>) -> Diagnostic {
        let mut kept: Vec<String> = Vec::new();
        for s in suggestions {
            if kept.len() >= MAX_SUGGESTIONS {
                break;
            }
            let clean = clean_text(&s);
            if !clean.is_empty() && !kept.contains(&clean) {
                kept.push(clean);
            }
        }
        self.suggestions = kept;
        self
    }

    /// The same, with the text the owner is told.
    pub fn saying(mut self, message: &str) -> Diagnostic {
        self.message = clean_text(message).chars().take(300).collect();
        self
    }

    /// Whether it is worth showing: a place in the text and something to say about it.
    pub fn is_sound(&self) -> bool {
        self.span.start < self.span.end
    }
}

/// One line of text, without control characters (except a tab, which becomes a space), and no
/// more than [`MAX_TEXT`] characters of it: an answer from a grammar server is from outside.
fn clean_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(MAX_TEXT * 4));
    for c in s.chars().take(MAX_TEXT) {
        match c {
            '\t' => out.push(' '),
            c if c.is_control() || c == '\u{2028}' || c == '\u{2029}' => {}
            c => out.push(c),
        }
    }
    out.trim().to_string()
}

/// What proofing is set to do, kept in the owner's settings between runs ([`crate::proof::Prefs`]
/// is where the front end stores it).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ProofSettings {
    /// Spelling is checked against the dictionaries below.
    pub check_spelling: bool,
    /// Grammar is checked by the LanguageTool server at [`ProofSettings::grammar_url`].
    pub check_grammar: bool,
    /// Checking happens as the owner types, not only when asked (Review › Spelling & Grammar).
    pub as_you_type: bool,
    /// The dictionaries that are on, at once: a word is right if any of them says so.
    pub languages: Vec<Language>,
    /// The words the owner added, as typed, in no order in particular.
    pub personal: Vec<String>,
    /// Grammar rules the owner told to be quiet (LanguageTool's rule ids).
    pub ignored_rules: Vec<String>,
    /// Where the grammar server is.
    pub grammar_url: String,
    /// Use the native host's explicitly installed local server. Kept separately so choosing it
    /// never overwrites a manual server address. Older settings leave this off.
    pub managed_server: bool,
    /// An older settings file's `allowRemote`. The field is still read so that a file written by
    /// an earlier version opens without an error, and it **never** grants anything: the grammar
    /// server proofing speaks to is on this computer or there is no server at all
    /// ([`ProofSettings::grammar_allowed`]).
    pub allow_remote: bool,
    /// The folder the owner keeps dictionaries in. Empty: the usual places
    /// (`/usr/share/hunspell`, `~/.local/share/hunspell`, `/Library/Spelling`…).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub dictionary_dir: String,
}

impl Default for ProofSettings {
    fn default() -> Self {
        ProofSettings {
            check_spelling: true,
            check_grammar: false,
            as_you_type: true,
            languages: vec![Language::EnCa, Language::FrCa],
            personal: Vec::new(),
            ignored_rules: Vec::new(),
            grammar_url: "http://127.0.0.1:8081".into(),
            managed_server: false,
            allow_remote: false,
            dictionary_dir: String::new(),
        }
    }
}

impl ProofSettings {
    /// The languages in effect, without repeats and never empty: the two Canadian ones when the
    /// settings name none that proofing knows.
    pub fn active(&self) -> Vec<Language> {
        let mut out: Vec<Language> = Vec::new();
        for l in &self.languages {
            if !out.contains(l) && out.len() < Language::ALL.len() {
                out.push(*l);
            }
        }
        if out.is_empty() {
            out.push(Language::EnCa);
        }
        out
    }

    /// Whether checking anything is on at all.
    pub fn any(&self) -> bool {
        self.check_spelling || self.check_grammar
    }

    /// Whether the grammar server may be spoken to at the address in the settings: only this
    /// computer's. [`ProofSettings::allow_remote`] is an older settings file's field and never
    /// grants anything, so a message never leaves this computer because of a settings file.
    pub fn grammar_allowed(&self) -> bool {
        crate::proof::address_allowed(&self.grammar_url, self.allow_remote)
    }

    /// The words of the personal dictionary, in the order they were added, without repeats and no
    /// more than [`MAX_PERSONAL`]. Only what is one word is kept: an entry with a space or a
    /// control character inside it is not a word a token could ever be, so it is left out.
    pub fn personal_words(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for w in &self.personal {
            if out.len() >= MAX_PERSONAL || !personal_word_ok(w) {
                continue;
            }
            let w = w.trim();
            if !out.iter().any(|kept| same_word(kept, w)) {
                out.push(w.to_string());
            }
        }
        out
    }

    /// Adds a word to the personal dictionary; `false` when it is already there, is not one word,
    /// or the dictionary is full.
    pub fn add_word(&mut self, word: &str) -> bool {
        let w = word.trim();
        if !personal_word_ok(w) {
            return false;
        }
        if self.personal_words().iter().any(|kept| same_word(kept, w)) || self.personal.len() >= MAX_PERSONAL {
            return false;
        }
        self.personal.push(w.to_string());
        true
    }

    /// Takes a word out of the personal dictionary; whether it was there.
    pub fn remove_word(&mut self, word: &str) -> bool {
        let before = self.personal.len();
        self.personal.retain(|w| !same_word(w, word.trim()));
        self.personal.len() != before
    }
}

/// Whether `word` is one word the personal dictionary can hold: letters, and only what joins them
/// inside a word (an apostrophe, a hyphen). A space, a tab or a control character inside it is not
/// part of a word — no token ever holds one — so an entry with one could never be a match and is
/// refused instead of kept where it would quietly do nothing.
pub fn personal_word_ok(word: &str) -> bool {
    let word = word.trim();
    !word.is_empty()
        && word.chars().count() <= MAX_PERSONAL_WORD
        && word.chars().any(char::is_alphabetic)
        && !word.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// Whether two words are the same word, whatever their case and whichever apostrophe: what the
/// personal dictionary has no two of.
pub fn same_word(a: &str, b: &str) -> bool {
    key(a) == key(b)
}

/// A word as it is looked up: lower case, with the typographic apostrophe folded to the plain one.
pub fn key(word: &str) -> String {
    let mut out = String::with_capacity(word.len());
    for c in word.chars() {
        match c {
            '\u{2019}' | '\u{2018}' | '\u{02bc}' => out.push('\''),
            c => out.extend(c.to_lowercase()),
        }
    }
    out.nfc().collect()
}

/// Most characters of a grammar address that are read; a longer one is no address.
const MAX_GRAMMAR_URL: usize = 2_048;

/// Whether an address is one the grammar server may be spoken at: a plain `http://` or `https://`
/// address to this computer, and nothing else.
///
/// The address is read by the same reader a link in a message is ([`mailcraft_link::WebLink`]),
/// which refuses a host that is not one and a port that is not one; on top of that it must be a
/// plain address, with no user information in front of the host (`user@host`), no query and no
/// fragment, so that what is said to the server is the server and nothing else.
///
/// The two schemes are held to different proofs that the server is this computer:
///
/// - `http://` may name it (`localhost`, `ip6-localhost`, or a loopback address). The plain
///   connection filters every address a name resolves to, so a look-up cannot take it elsewhere.
/// - `https://` must write the address out as a loopback IP literal (`https://127.0.0.1`,
///   `https://[::1]`). A secure connection in this program has no resolved-address filter, so only
///   a host the transport dials without a look-up is provably this computer. A name there
///   (`localhost` in any case, `ip6-localhost`) and a legacy numeric form (`2130706433`, `127.1`,
///   `0177.0.0.1`) are refused, however the link reader would normalize the host.
///
/// `allow_remote` is an older settings file's field. It is accepted so a caller's signature does
/// not change, and it **never** grants anything: a server that is not on this computer is refused
/// however it is asked for, so a message never leaves this computer because of a settings file.
pub fn address_allowed(url: &str, allow_remote: bool) -> bool {
    let _ = allow_remote;
    let url = url.trim();
    if url.is_empty() || url.len() > MAX_GRAMMAR_URL || url.contains('?') || url.contains('#') {
        return false;
    }
    let authority = url.split_once("://").map_or("", |(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or_default());
    if authority.is_empty() || authority.contains('@') || authority.ends_with(':') {
        return false;
    }
    let Some(link) = mailcraft_link::WebLink::parse(url) else { return false };
    if link.is_https() {
        // The host exactly as the transport will read it, never the link reader's normal form: a
        // name here is resolved by somebody else, and this program's secure dial does not filter
        // what it resolves to.
        raw_host(authority).and_then(|host| host.parse::<std::net::IpAddr>().ok()).is_some_and(|ip| ip.is_loopback())
    } else {
        is_this_machine(link.host())
    }
}

/// The host of an authority `host[:port]` or `[v6][:port]`, exactly as it is written. The port has
/// already been read and refused by the link reader; this only takes the host out of the authority.
fn raw_host(authority: &str) -> Option<&str> {
    if let Some(v6) = authority.strip_prefix('[') {
        let (host, after) = v6.split_once(']')?;
        return match after {
            "" => Some(host),
            after => after.strip_prefix(':').map(|_| host),
        };
    }
    Some(authority.split_once(':').map_or(authority, |(host, _)| host))
}

/// Whether `host` is this computer: loopback by name or by address (and nothing that merely ends
/// the same way, such as `localhost.example`). An IPv6 address is written in brackets by the link
/// reader (`[::1]`), and either way is this computer when it is loopback.
fn is_this_machine(host: &str) -> bool {
    let host = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
    if host.eq_ignore_ascii_case("localhost") || host.eq_ignore_ascii_case("ip6-localhost") || host == "::1" {
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => false,
    }
}

/// A run of letters that proofing looks at, with where it is in the text (bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Word<'a> {
    pub text: &'a str,
    pub span: Span,
}

/// The words of `text`, in order: runs of letters (and the apostrophe and hyphen inside them),
/// leaving out what is not a word to check: an address, an e-mail address, a number, a word longer
/// than [`MAX_WORD`], a picture (the object character), and anything in a script with no space
/// between words. Only the first [`MAX_CHECKED`] characters are looked at.
pub fn words(text: &str) -> Vec<Word<'_>> {
    let text = match text.char_indices().nth(MAX_CHECKED) {
        Some((cut, _)) => text.get(..cut).unwrap_or_default(),
        None => text,
    };
    let skip = skipped(text);
    let mut out: Vec<Word<'_>> = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in text.char_indices() {
        let part = is_word_char(c);
        match (part, start) {
            (true, None) => start = Some(i),
            (false, Some(from)) => {
                push_word(text, from, i, &skip, &mut out);
                start = None;
            }
            _ => {}
        }
    }
    if let Some(from) = start {
        push_word(text, from, text.len(), &skip, &mut out);
    }
    out
}

/// Adds the word `from..to`, trimmed of the punctuation at its ends, when it is one to check.
fn push_word<'a>(text: &'a str, from: usize, to: usize, skip: &[(usize, usize)], out: &mut Vec<Word<'a>>) {
    let Some(piece) = text.get(from..to) else {
        return;
    };
    let trimmed = piece.trim_matches(|c: char| !c.is_alphanumeric() && !is_combining_mark(c));
    if trimmed.is_empty() {
        return;
    }
    let lead = piece.len() - piece.trim_start_matches(|c: char| !c.is_alphanumeric() && !is_combining_mark(c)).len();
    let start = from + lead;
    let end = start + trimmed.len();
    let Some(span) = Span::of(text, start, end) else {
        return;
    };
    if skip.iter().any(|(a, b)| start < *b && *a < end) {
        return;
    }
    if trimmed.chars().count() > MAX_WORD || !trimmed.chars().any(char::is_alphabetic) {
        return;
    }
    // A word written in a script that puts no space between words (Chinese, Japanese, Thai…): the
    // run is a sentence, and checking it as one word would flag every one of them.
    if trimmed.chars().any(no_spaces_between_words) {
        return;
    }
    out.push(Word { text: trimmed, span });
}

/// A character a word is made of: a letter or a digit, or the apostrophe or hyphen joining two.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || is_combining_mark(c) || matches!(c, '\'' | '\u{2019}' | '\u{2018}' | '\u{02bc}' | '-' | '_' | '.')
}

/// Scripts written without spaces between words: a run of them is not a word.
fn no_spaces_between_words(c: char) -> bool {
    matches!(c as u32,
        0x0E00..=0x0EFF | 0x1000..=0x109F | 0x1780..=0x17FF | 0x2E80..=0x9FFF | 0xA000..=0xA4CF
        | 0xAC00..=0xD7AF | 0xF900..=0xFAFF | 0xFF65..=0xFF9F | 0x20000..=0x3FFFF)
}

/// The places in `text` that are looked at as a whole and never as words: a web address, an
/// e-mail address, a path, and what a name of a file or a variable looks like.
///
/// A run of text between white space and punctuation is looked at once, whatever it holds: every
/// `@`, `/` or `www.` inside one run is part of the same address, and looking at the run from each
/// of them is the square of its length. A message is bounded by [`MAX_CHECKED`], so this is too.
fn skipped(text: &str) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in text.char_indices() {
        let edge = c.is_whitespace() || matches!(c, '(' | ')' | '[' | ']' | '<' | '>' | '"' | '\'' | '«' | '»' | ',' | ';');
        match (edge, start) {
            (false, None) => start = Some(i),
            (true, Some(from)) => {
                skip_run(text, from, i, &mut out);
                start = None;
            }
            _ => {}
        }
    }
    if let Some(from) = start {
        skip_run(text, from, text.len(), &mut out);
    }
    out.sort_unstable();
    out
}

/// Adds the span of one white-space-free, punctuation-free run when it is an address: a web address
/// (`://`, `www.`, a path marker), or an e-mail address or path — an `@` or a slash with a name of
/// more than one letter on more than one side of it.
fn skip_run(text: &str, from: usize, to: usize, out: &mut Vec<(usize, usize)>) {
    let Some(piece) = text.get(from..to) else { return };
    if piece.contains("://") || piece.contains("www.") || piece.contains("\\\\") {
        out.push((from, to));
        return;
    }
    if !piece.contains('@') && !piece.contains('/') {
        return;
    }
    let named = piece.split(['@', '/']).filter(|p| p.chars().filter(|c| c.is_alphabetic()).count() > 1).count();
    if named >= 2 {
        out.push((from, to));
    }
}
