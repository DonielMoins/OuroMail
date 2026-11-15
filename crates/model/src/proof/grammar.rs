// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! The grammar server: LanguageTool's `POST /v2/check`, asked and read.
//!
//! Nothing here opens a connection — the host does that (`mailcraft_net::languagetool`, on a
//! thread of the window's own) — and nothing here trusts an answer: every length and offset in it
//! is checked against the text that was sent, a match that does not fit is dropped rather than
//! guessed at, and the count is capped ([`super::MAX_DIAGNOSTICS`]).
//!
//! **Offsets.** LanguageTool counts its offsets in UTF-16 code units (Java's `String`), while a
//! MailCraft text is counted in bytes (a Rust `&str`). The two agree only for plain ASCII, which
//! is exactly where a French sentence with `é`, `à` and `œ` stops being ASCII, so every offset is
//! converted through [`utf16_to_byte`] and a match whose offset does not land on a character is
//! dropped. There is no other conversion anywhere: a diagnostic's [`Span`] is always bytes.

use serde::Deserialize;

use super::{Diagnostic, Kind, Language, MAX_DIAGNOSTICS, ProofError, Source, Span};

/// Longest rule id kept from an answer, in characters. LanguageTool's own are short names
/// (`FR_ACCORD`, `EN_AGREEMENT`); a longer one is a server's mistake or a hostile answer, and it
/// is not carried into a diagnostic (nor into the settings when the owner silences it).
const MAX_RULE_ID: usize = 64;

/// Longest replacement kept from an answer, in characters. A suggestion is text that may be put
/// into the draft in one step, so it is bounded as a personal word is; LanguageTool's are a word
/// or a phrase, never a paragraph.
const MAX_REPLACEMENT: usize = 300;

/// How the text is asked to be checked: LanguageTool's own language, and no rule switched off by
/// the request (the owner's quiet rules are dropped from the answer here instead, so that one
/// answer is the same for every window).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckRequest<'a> {
    /// The text, which is the message being written or one paragraph of it.
    pub text: &'a str,
    /// Which language to check it as.
    pub language: Language,
}

impl CheckRequest<'_> {
    /// The body of the request, `application/x-www-form-urlencoded`.
    pub fn form(&self) -> String {
        let mut out = String::with_capacity(self.text.len() + 64);
        out.push_str("text=");
        encode_into(&mut out, self.text);
        out.push_str("&language=");
        encode_into(&mut out, self.language.code());
        out.push_str("&enabledOnly=false&level=picky");
        out
    }
}

/// `text` written for a form: everything but an unreserved letter or digit as `%XX`, a space as
/// `+`. A `&`, a `=` or a `%` in the message never changes what the request says.
fn encode_into(out: &mut String, text: &str) {
    for byte in text.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(*byte as char),
            b' ' => out.push('+'),
            other => {
                out.push('%');
                out.push(hex(other >> 4));
                out.push(hex(other & 0x0f));
            }
        }
    }
}

fn hex(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        _ => (b'A' + (nibble - 10)) as char,
    }
}

/// The answer, as far as this program reads it.
#[derive(Debug, Deserialize)]
struct Answer {
    #[serde(default)]
    matches: Vec<Match>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Match {
    #[serde(default)]
    message: String,
    #[serde(default)]
    offset: i64,
    #[serde(default)]
    length: i64,
    #[serde(default)]
    replacements: Vec<Replacement>,
    #[serde(default)]
    rule: Option<Rule>,
    #[serde(default, alias = "rule_id")]
    rule_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Replacement {
    #[serde(default)]
    value: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Rule {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    issue_type: Option<String>,
}

/// The diagnostics in an answer to a check of `text` in `language`, in the answer's order, each
/// with what could stand in its place.
///
/// An answer that is not JSON at all, or that does not have the shape of one, is [`ProofError::Answer`];
/// a match that the text does not hold is left out (it is not this text's, whatever the server
/// says). `ignored` names the rule ids the owner told to be quiet.
pub fn parse_answer(body: &[u8], text: &str, language: Language, ignored: &[String]) -> Result<Vec<Diagnostic>, ProofError> {
    if body.len() > super::MAX_ANSWER {
        return Err(ProofError::Answer);
    }
    // An answer is one object: `[]`, a bare string or a number is not one, whatever it parses to.
    if !body.trim_ascii_start().starts_with(b"{") {
        return Err(ProofError::Answer);
    }
    let answer: Answer = serde_json::from_slice(body).map_err(|_| ProofError::Answer)?;
    let lines = utf16_index(text);
    let mut out: Vec<Diagnostic> = Vec::new();
    for m in answer.matches.iter().take(MAX_DIAGNOSTICS * 4) {
        if out.len() >= MAX_DIAGNOSTICS {
            break;
        }
        let id = m.rule.as_ref().and_then(|r| r.id.clone()).or_else(|| m.rule_id.clone()).filter(|id| id.chars().count() <= MAX_RULE_ID);
        if let Some(id) = &id
            && ignored.iter().any(|quiet| quiet == id)
        {
            continue;
        }
        let Some(start) = utf16_to_byte(text, &lines, m.offset) else { continue };
        let Some(end) = utf16_to_byte(text, &lines, m.offset.saturating_add(m.length.max(0))) else { continue };
        let Some(span) = Span::of(text, start, end) else { continue };
        let kind = match m.rule.as_ref().and_then(|r| r.issue_type.as_deref()) {
            Some("style") => Kind::Style,
            Some("misspelling") => Kind::Spelling,
            _ => Kind::Grammar,
        };
        let replacements = m.replacements.iter().filter(|r| r.value.chars().count() <= MAX_REPLACEMENT).map(|r| r.value.clone());
        let mut d = Diagnostic::new(span, kind, Source::Server).saying(&m.message).suggesting(replacements);
        d.language = Some(language);
        d.rule = id;
        if d.is_sound() {
            out.push(d);
        }
    }
    Ok(out)
}

/// Where each character of `text` is, in UTF-16 code units and in bytes: only the starts of
/// characters, in order, with the UTF-16 count of each (`2` for a character outside the basic
/// plane).
pub(super) fn utf16_index(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::with_capacity(text.len().min(4_096));
    let mut units = 0usize;
    for (at, c) in text.char_indices() {
        out.push((units, at));
        units += c.len_utf16();
    }
    out.push((units, text.len()));
    out
}

/// The byte offset of the character starting at UTF-16 code unit `units` of `text`: `None` when
/// no character starts there (past the end, or between the two halves of a character outside the
/// basic plane), which is where a correction would cut a character in two.
pub fn utf16_to_byte(text: &str, index: &[(usize, usize)], units: i64) -> Option<usize> {
    let units = usize::try_from(units).ok()?;
    let at = index.binary_search_by_key(&units, |(u, _)| *u).ok()?;
    let byte = index.get(at).map(|(_, b)| *b)?;
    (byte <= text.len() && text.is_char_boundary(byte)).then_some(byte)
}
