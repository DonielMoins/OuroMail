// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Grammar from a LanguageTool server on this computer (#954): one `POST /v2/check`, asked and
//! read with the types of the `languagetool-rust` crate (3.0.1), never with that crate's client.
//! That crate is a third-party binding of LanguageTool's JSON API, not a library the LanguageTool
//! project maintains; only the shapes it defines are used here, and none of its own client code.
//!
//! The window opens no connection of its own: the host asks here on a thread that is not the
//! window's. [`check_multilingual`] is what that host asks with, the legacy host included: it
//! asks the server to work each sentence's language out, reads the answer with the
//! [`ltrs::api::check::Response`], [`ltrs::api::check::Match`] and
//! [`ltrs::api::check::LanguageResponse`] types that crate defines, and returns diagnostics
//! already placed in the draft. [`check`] is the connection-level call beside it, giving back the
//! answer's bytes for `mailcraft_model::proof::parse_answer`. [`ready`] is the probe the host
//! makes while it starts a server: one `GET /v2/languages`, and no text of any kind.
//!
//! - **Only this computer.** A grammar server on the internet would be told the message being
//!   written. An address that is not `http(s)`, or that is not this machine's, is refused before
//!   a connection is made — refused here, on this module's own account, whatever the caller has
//!   checked ([`mailcraft_model::proof::address_allowed`], with no remote server allowed) — and
//!   the crate's HTTP client refuses `http://` to anything but loopback as well. A secure address
//!   is accepted only when its host is a loopback IP literal (`https://127.0.0.1`,
//!   `https://[::1]`): the shared secure dial has no resolved-address filter, so a name
//!   (`localhost`, `ip6-localhost`) or a legacy numeric spelling is refused here rather than
//!   looked up. An address with a user name, a query or a fragment is not one a check is posted
//!   to, nor a readiness probe asked of.
//! - **The answer is hostile**: at most [`MAX_ANSWER`] bytes of it, no redirect followed, no
//!   proxy read from the environment, no cookie and no credential sent, and one patience for
//!   connecting and for the answer alike.
//! - **Nothing of the text is said.** An error says what happened in the owner's words and
//!   carries neither the text nor the server's address; [`CheckOutcome`]'s `Debug` carries only
//!   counts, never a diagnostic's message or suggestion.
//! - **Asked once.** Nothing here tries again: the window asks again when it is ready to, and the
//!   supervisor probes a starting server again when it is ready to.
//!
//! **More than one language at once.** [`check_multilingual`] splits a draft into sentences and
//! asks the server to work each sentence's language out (`language=auto`, with the owner's
//! configured variants as `preferredVariants`). A sentence is only believed when the server is
//! sure which language it is in (at least 0.95 confidence) and that language is one the owner
//! enabled, so a sentence that mixes two languages where the server's own answer is uncertain —
//! its matches are often false there — is left alone rather than corrected. A sentence the server
//! checked as one tongue but detected as another is dropped whole; there is no fallback to the
//! explicitly enabled language, because that would correct the other tongue's text in this
//! tongue's rules. Spelling matches are never taken from the server at all, the dictionaries
//! being the ones that know a word, and a grammar match that touches a word the server could not
//! spell in the language it chose is dropped too (that word is the other language's).
//!
//! **That is a guess, not a proof.** The confidence is the server's own detector's, and a sentence
//! that changes language half way can be detected confidently all the same: a real server answers
//! 0.9999 for `Bonjour, can you send me le document?` as French, and a French typography rule then
//! asks for a space its English words do not want. Nothing here can tell a word borrowed into the
//! other tongue from a mistake — the owner silences the rule, or turns grammar off.

use std::fmt;
use std::time::{Duration, Instant};

use ltrs::api::check::{LanguageResponse, Level, Match, Request, Response};
use mailcraft_model::proof::{self, Diagnostic, Kind, Language, MAX_ANSWER, MAX_CHECKED, MAX_DIAGNOSTICS, Source, Span};
use serde_json::Value;

use crate::NetError;
use crate::http::{self, Request as HttpRequest, Url};

/// How long connecting may take, and so may the answer. A check of a paragraph on a server on
/// this computer takes a fraction of a second; a server that has to think longer than this is
/// one the window asks again rather than waits for.
pub const PATIENCE: Duration = Duration::from_secs(8);

/// Longest sentence sent as one check, in bytes. A longer piece is not a sentence but a pasted
/// document: it is left alone rather than sent whole.
const SEGMENT_BYTES: usize = 16 * 1024;

/// Most sentences one check asks the server about, however long the draft is.
const MAX_SEGMENTS: usize = 32;

/// Least confidence, from the server's own language detector, that a sentence is in the language
/// it says it is in. Below this the sentence is taken to mix languages and is left alone: the
/// matches a server finds in a sentence it is unsure of are often false.
const CONFIDENT: f64 = 0.95;

/// Most rule ids from the owner's settings that are looked at when a match is read.
pub(crate) const MAX_QUIET: usize = 256;

/// Longest rule id looked for among the owner's settings, in bytes. A rule's id is a short word of
/// capitals, digits and underscores; anything longer is not one, and is not worth holding on to
/// because the settings are a file like any other.
const MAX_QUIET_RULE_ID: usize = 128;

/// Longest entry of the owner's settings that is looked at, in bytes — before it is trimmed. What
/// trimming may leave is [`MAX_QUIET_RULE_ID`], and one id may be written with any amount of white
/// space around it; an entry longer than this is refused for its own length, so that a settings
/// file holding a very long string costs a length check and not a scan of it.
const MAX_QUIET_RAW: usize = MAX_QUIET_RULE_ID * 4;

/// Most languages of the owner's enabled list that one check looks at, and most entries of it that
/// are read. The list is the settings', which are outside input like any other file. A tongue is
/// one of four [`Language`]s, so reading this many entries loses no language the owner enabled:
/// what the cap leaves out can only be a repeat of one already read, or an entry past a list no
/// owner writes. The same normalized list decides which variants are asked for and which answer is
/// believed, so the two never disagree about what the owner enabled.
const MAX_LANGUAGES: usize = 32;

/// Longest rule id kept from an answer, in characters. LanguageTool's own are short names
/// (`FR_ACCORD`, `EN_AGREEMENT`); a longer one is a server's mistake or a hostile answer, and it
/// is not carried into a diagnostic.
const MAX_RULE_ID: usize = 64;

/// Longest replacement kept from an answer, in characters. A suggestion is text that may be put
/// into the draft in one step, so it is bounded as a personal word is.
const MAX_REPLACEMENT: usize = 300;

/// LanguageTool's name for "work the language out yourself".
const AUTO: &str = "auto";

/// Longest a whole batch may go on asking, whatever patience the caller asks for. A patience of
/// years is capped at this, so that no window is left asking a server for ever.
pub(crate) const MAX_BATCH: Duration = Duration::from_secs(300);

/// What is posted to, under the server's address.
const CHECK: &str = "/v2/check";

/// What a readiness probe asks for, under the server's address: the languages the server can
/// check, which says it is up and answering without being told anything of a message.
const LANGUAGES: &str = "/v2/languages";

/// Longest answer a readiness probe reads, in bytes. LanguageTool 6.6's `/v2/languages` answer is
/// 3.3 KB; anything past this is not one, and is not read.
pub(crate) const READY_MAX_BODY: usize = 16 * 1024;

/// Most languages one `/v2/languages` answer may name before it is not believed. LanguageTool 6.6
/// names 60; the cap leaves room for variants without letting a hostile answer hold an unbounded
/// list of them.
pub(crate) const MAX_READY_LANGUAGES: usize = 256;

/// Longest name of one language in a readiness answer, in characters (it may hold any script), and
/// longest code and long code, in bytes. A code is a short word of letters, digits, `-` and `_`:
/// LanguageTool 6.6's longest is `de-DE-x-simple-language`, 23 bytes.
pub(crate) const MAX_READY_NAME: usize = 128;
pub(crate) const MAX_READY_CODE: usize = 64;

/// Why a check has no answer. Each says which of the things that can go wrong did, and none of
/// them holds the text that was sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LanguageToolError {
    /// The address is not one a grammar server is spoken to at: it is no address at all, or it is
    /// not this computer's.
    #[error("grammar is checked by a LanguageTool server on this computer, and that address is not one of this computer's")]
    Refused,
    /// The language asked for is not one proofing knows.
    #[error("that is not a language a grammar check can be asked for")]
    Language,
    /// The text is longer than one check may ask about. A longer text is refused rather than
    /// cut: the multiline caller segments its own text and says in its outcome how much it left
    /// alone, and a single check has no outcome to say that in.
    #[error("the text is longer than one grammar check may ask about")]
    UnitTextTooLong,
    /// No server answered at that address: nothing listens there.
    #[error("no grammar server on this computer answered")]
    Unreachable,
    /// The server did not answer in the time it had.
    #[error("the grammar server took too long")]
    TooSlow,
    /// The answer is longer than a grammar server's answers may be.
    #[error("the grammar server's answer is too long to read")]
    TooLong,
    /// The server hung up without an answer.
    #[error("the grammar server hung up without an answer")]
    Closed,
    /// The server answered, and said it could not check the text.
    #[error("the grammar server could not check the text")]
    Failed,
    /// What came back is not HTTP, or not the shape of an answer.
    #[error("the grammar server answered with something that is not a grammar check")]
    Garbled,
}

/// What one multilingual check did: the diagnostics that fit the draft, which sentences were
/// actually checked, which were not, and whether the draft was only partly looked at.
///
/// `checked_segments` counts sentences that were sent and whose answer named a language the owner
/// enabled with at least [`CONFIDENT`] certainty. `skipped_segments` counts sentences deliberately
/// left alone (a language the server was unsure of, a mixed sentence, a sentence over
/// [`SEGMENT_BYTES`]) and sentences that were not asked about because the batch stopped at its
/// deadline, at [`MAX_SEGMENTS`], or after a later request failed.
///
/// `partial` is true when the outcome is not a complete look at the draft: the text was longer
/// than [`MAX_CHECKED`], a sentence was too long to send, there were more sentences than
/// [`MAX_SEGMENTS`], the diagnostics cap was reached, a later request failed or ran out of
/// time, or the server's own answer said its results were incomplete
/// (`warnings.incompleteResults`, the field of the crate's `Warnings` type). A sentence
/// deliberately dropped because its language was uncertain does **not** make the outcome
/// partial; it is the conservative answer, not an interrupted one.
///
/// The `Debug` rendering says only the counts. It never holds a message or a suggestion from the
/// server, so a log line cannot leak the text being written.
#[derive(Clone, PartialEq, Eq)]
pub struct CheckOutcome {
    /// The diagnostics that fit the draft, in the order the server found them.
    pub diagnostics: Vec<Diagnostic>,
    /// Sentences sent and answered in a language the owner enabled, with certainty.
    pub checked_segments: usize,
    /// Sentences not checked: uncertain, mixed, too long, or never reached.
    pub skipped_segments: usize,
    /// Whether the draft was only partly looked at (see the type's notes).
    pub partial: bool,
}

impl fmt::Debug for CheckOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CheckOutcome")
            .field("diagnostics", &self.diagnostics.len())
            .field("checked_segments", &self.checked_segments)
            .field("skipped_segments", &self.skipped_segments)
            .field("partial", &self.partial)
            .finish()
    }
}

/// Asks the grammar server at `url` to check `text` as `language` (`fr-CA`), waiting `patience`
/// to connect and as long again for the answer, and gives back the bytes of its answer — for
/// [`proof::parse_answer`], which is the only thing that reads them.
///
/// The request is the [`ltrs::api::check::Request`] the crate defines, written as a form, with
/// `level=picky`; nothing of the client that crate ships is used, and no proxy, cookie or
/// credential is sent. A text longer than [`MAX_CHECKED`] characters is refused as
/// [`LanguageToolError::UnitTextTooLong`] before the form is built, so no part of a pasted
/// document is sent and nothing is silently cut: the multiline caller segments its own text and
/// says in its outcome what it left alone.
///
/// `url` is the server's address, which may end in a `/`; `/v2/check` is posted under it. An
/// address with a query or a fragment is not one a check is posted to.
pub fn check(url: &str, text: &str, language: &str, patience: Duration) -> Result<Vec<u8>, LanguageToolError> {
    let at = endpoint(url).ok_or(LanguageToolError::Refused)?;
    // This module's own refusal, never the caller's alone: a live server on another machine is
    // told nothing of the message.
    if !proof::address_allowed(&at, false) {
        return Err(LanguageToolError::Refused);
    }
    let language = Language::parse(language).ok_or(LanguageToolError::Language)?;
    // The client's own rule too: `https://` anywhere, plain `http://` to this machine alone.
    let at = Url::parse(&at).map_err(|_| LanguageToolError::Refused)?;
    if !transport_allowed(&at) {
        return Err(LanguageToolError::Refused);
    }
    ask_once(&at, text, language.code(), None, patience)
}

/// Asks whether a LanguageTool server is ready at `url`, for the supervisor that has just started
/// one: one `GET /v2/languages`, read as the `[{"name","code","longCode"}]` list that the
/// `languagetool-rust` crate's `languages::Language` type defines, and nothing else.
///
/// The probe has no text and no credential to send: it is a `GET` with an `Accept` header, no
/// body, no cookie, no proxy and no redirect followed. `patience` is capped at [`PATIENCE`], so
/// that a caller cannot leave a supervisor waiting on a starting server for longer than a check
/// waits. The answer is at most [`READY_MAX_BODY`] bytes and [`MAX_READY_LANGUAGES`] entries, each
/// one a language with a name and a code LanguageTool writes. `Ok(())` means exactly that: a
/// server answered `200` with a language list a check can be asked of. Nothing is started,
/// stopped or retried here, and an error carries neither the answer nor the address.
///
/// `url` is the server's address, which may end in a `/`; `/v2/languages` is asked under it. An
/// address that is not this computer's, or that has a user name, a query or a fragment, is
/// refused before a connection is made, as [`check`] refuses one.
pub fn ready(url: &str, patience: Duration) -> Result<(), LanguageToolError> {
    let at = endpoint_at(url, LANGUAGES).ok_or(LanguageToolError::Refused)?;
    // This module's own refusal, never the caller's alone: a server on another machine learns
    // nothing about this one, not even that it is up.
    if !proof::address_allowed(&at, false) {
        return Err(LanguageToolError::Refused);
    }
    let at = Url::parse(&at).map_err(|_| LanguageToolError::Refused)?;
    if !transport_allowed(&at) {
        return Err(LanguageToolError::Refused);
    }
    let headers = [("Accept", "application/json")];
    let request = HttpRequest { method: "GET", headers: &headers, body: None, patience: ready_patience(patience), max_body: READY_MAX_BODY };
    let reply = http::request(&at, &request).map_err(told)?;
    if reply.status != 200 {
        return Err(LanguageToolError::Failed);
    }
    read_languages(&reply.body)
}

/// The patience a readiness probe is given: what the caller asked for, never more than
/// [`PATIENCE`].
pub(crate) fn ready_patience(patience: Duration) -> Duration {
    patience.min(PATIENCE)
}

/// Reads a `/v2/languages` answer with the types the `languagetool-rust` crate defines: an array
/// of its `languages::Language` entries, non-empty, at most [`MAX_READY_LANGUAGES`] of them and
/// each one a language a check can be asked for. The body cap is checked again here, so a caller
/// that reads bytes from somewhere else is held to it too.
fn read_languages(body: &[u8]) -> Result<(), LanguageToolError> {
    if body.len() > READY_MAX_BODY {
        return Err(LanguageToolError::TooLong);
    }
    // The endpoint answers one array; an object or a bare string is not a language list.
    if body.trim_ascii_start().first() != Some(&b'[') {
        return Err(LanguageToolError::Garbled);
    }
    let languages: ltrs::api::languages::Response = serde_json::from_slice(body).map_err(|_| LanguageToolError::Garbled)?;
    if languages.is_empty() || languages.len() > MAX_READY_LANGUAGES {
        return Err(LanguageToolError::Garbled);
    }
    if !languages.iter().all(language_is_one) {
        return Err(LanguageToolError::Garbled);
    }
    Ok(())
}

/// Whether one entry of a language list is a language a check may be asked for: a name that is not
/// blank and is no longer than [`MAX_READY_NAME`] characters, and a code and long code that are
/// short words of letters, digits, `-` and `_`, as LanguageTool writes them (`en-US`, `fr`,
/// `de-DE-x-simple-language`). Anything else is a server's mistake or a hostile answer.
fn language_is_one(language: &ltrs::api::languages::Language) -> bool {
    let sound_code =
        |code: &str| !code.is_empty() && code.len() <= MAX_READY_CODE && code.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'));
    let name = language.name.trim();
    !name.is_empty()
        && name.len() <= MAX_READY_NAME.saturating_mul(4)
        && name.chars().count() <= MAX_READY_NAME
        && sound_code(&language.code)
        && sound_code(&language.long_code)
}

/// Asks the grammar server at `url` about each sentence of `text`, letting the server work out
/// which of `languages` (the owner's enabled ones) the sentence is in, and gives back the
/// diagnostics that fit `text` itself — grammar and style, never spelling.
///
/// - The draft is split into bounded sentences on `.`, `?` and `!`, and into paragraphs on a blank
///   line, using the original bytes. A stop inside a number or a word (`3.14`, `mail.example`) is
///   not a sentence's end, and a line break inside a paragraph is not one either: the server's own
///   reader splits sentences better than a search for a full stop, and a piece cut mid-sentence
///   makes it report a sentence that starts in lower case. At most [`MAX_SEGMENTS`] sentences are
///   asked about, each of at most [`SEGMENT_BYTES`], and only the first [`MAX_CHECKED`] characters
///   are looked at: a longer piece, or anything past the thirty-second sentence, is left alone
///   rather than sent.
/// - `language=auto` is posted, with one `preferredVariants` entry for each tongue among
///   `languages`, so that the server's answer uses a variant the owner enabled.
/// - A sentence the server is less than 0.95 confident about, whose answer names no language
///   proofing knows, whose detected tongue differs from the tongue it was checked as, or whose
///   language the owner did not enable, is skipped: nothing is said about it, and there is no
///   fallback to another enabled language. That confidence is a guess: a sentence that mixes two
///   languages can be detected with certainty, and then its matches are believed (the module notes
///   say why, and what is done about the words of the other tongue).
/// - The answer is decoded with the types the `languagetool-rust` crate defines. A whole
///   [`ltrs::api::check::Response`] is read first; when one malformed match would make that decode
///   fail, the bounded JSON object's matches are taken out and the rest is decoded again as the
///   same [`ltrs::api::check::Response`] with an empty `matches`, so the header — including
///   `warnings` — is still validated by that crate's types, and each match is decoded on its own
///   as a [`ltrs::api::check::Match`], so a good match beside a broken one is not lost. Nothing
///   of the wire is re-declared here. Offsets are UTF-16 code units, as LanguageTool writes them;
///   each is converted to a byte offset of the sentence that was sent, and a match whose offset
///   does not land on a character is dropped rather than cut into one. Spans are then moved by the
///   sentence's byte offset and checked against the draft; a span that does not fit is dropped,
///   never clamped. Spelling matches are dropped here, as is a match that overlaps a word the
///   server could not spell in the language it chose.
/// - The server's own word on its results is believed: when an answer's `warnings` says
///   `incompleteResults`, that sentence's diagnostics are kept as usual and
///   [`CheckOutcome::partial`] is set, so a look at part of a sentence is never reported as a
///   complete one. An answer without `warnings` says nothing, and is read as complete.
/// - A rule the owner silenced in the settings is left out, whether they typed its id in capitals
///   or with a space around it: matching is on the id, trimmed and without case. Only the first
///   [`MAX_QUIET`] entries of the settings list are looked at — at most [`MAX_QUIET_RAW`] bytes
///   each, refused for their own length before they are trimmed — so a hostile list of ids, or one
///   of very long entries, cannot make a check scan more than a check.
/// - The batch stops asking when its deadline passes. The whole batch is measured from one start:
///   each request gets what is left of that deadline, at most [`PATIENCE`], and once nothing is
///   left no further sentence is asked about. A later request that fails (or runs out of time)
///   keeps the good results already found and returns them with [`CheckOutcome::partial`] true;
///   if the very first request fails, that first error is returned and there is no outcome. When
///   the first sentence's language is uncertain and the next request fails, the failure is still
///   returned, since nothing was actually checked. Nothing here follows a redirect, and a server
///   that is not on this computer is refused as [`check`] refuses one.
pub fn check_multilingual(
    url: &str,
    text: &str,
    languages: &[Language],
    ignored: &[String],
    patience: Duration,
) -> Result<CheckOutcome, LanguageToolError> {
    let at = endpoint(url).ok_or(LanguageToolError::Refused)?;
    if !proof::address_allowed(&at, false) {
        return Err(LanguageToolError::Refused);
    }
    let at = Url::parse(&at).map_err(|_| LanguageToolError::Refused)?;
    if !transport_allowed(&at) {
        return Err(LanguageToolError::Refused);
    }
    // One bounded, de-duplicated list decides both what is asked for as a variant and what answer
    // is believed, so the two cannot disagree about what the owner enabled.
    let enabled = enabled_languages(languages);
    let variants = preferred_variants(&enabled);
    if variants.is_empty() {
        return Err(LanguageToolError::Language);
    }
    let variants: Vec<String> = variants.into_iter().map(str::to_string).collect();
    let whole = text;
    let text = checked_prefix(whole);
    let quiet = quiet_rules(ignored);
    let deadline = batch_deadline(patience);
    let segmented = segments(text);
    let total = segmented.pieces.len();
    let mut skipped = segmented.too_long.saturating_add(segmented.beyond_cap);
    let mut partial = skipped > 0 || text.len() < whole.len();
    let mut checked = 0usize;
    let mut diagnostics: Vec<Diagnostic> = Vec::new();
    for (index, &(base, segment)) in segmented.pieces.iter().enumerate() {
        if diagnostics.len() >= MAX_DIAGNOSTICS {
            skipped = skipped.saturating_add(total.saturating_sub(index));
            partial = true;
            break;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            if checked == 0 {
                return Err(LanguageToolError::TooSlow);
            }
            skipped = skipped.saturating_add(total.saturating_sub(index));
            partial = true;
            break;
        }
        // Never more than one check's patience, and never more than the batch has left.
        let per_request = left.min(PATIENCE);
        let body = match ask_once(&at, segment, AUTO, Some(&variants), per_request) {
            Ok(body) => body,
            Err(e) => {
                if checked == 0 {
                    return Err(e);
                }
                skipped = skipped.saturating_add(total.saturating_sub(index));
                partial = true;
                break;
            }
        };
        let answer = match read_answer(&body) {
            Ok(answer) => answer,
            Err(e) => {
                if checked == 0 {
                    return Err(e);
                }
                skipped = skipped.saturating_add(total.saturating_sub(index));
                partial = true;
                break;
            }
        };
        // The server said, in its own answer, that it had not looked at everything: whatever else
        // happens to this sentence, the look at the draft is not a complete one.
        if answer.incomplete {
            partial = true;
        }
        let Some(language) = trusted_language(&answer.language, &enabled) else {
            skipped = skipped.saturating_add(1);
            continue;
        };
        checked = checked.saturating_add(1);
        collect_segment(&answer.matches, segment, language, &quiet, base, text, &mut diagnostics);
        if diagnostics.len() >= MAX_DIAGNOSTICS {
            skipped = skipped.saturating_add(total.saturating_sub(index + 1));
            partial = true;
            break;
        }
    }
    Ok(CheckOutcome { diagnostics, checked_segments: checked, skipped_segments: skipped, partial })
}

/// The body of one check, as the crate's [`Request`] writes it. A text longer than [`MAX_CHECKED`]
/// characters is refused here, before the form is built: the count stops at the first character
/// past the cap, so the work of refusing is bounded too, and nothing of the text is sent or cut.
fn ask_once(at: &Url, text: &str, language: &str, variants: Option<&[String]>, patience: Duration) -> Result<Vec<u8>, LanguageToolError> {
    if text.char_indices().nth(MAX_CHECKED).is_some() {
        return Err(LanguageToolError::UnitTextTooLong);
    }
    let mut request = Request::new().with_text(text).with_language(language.to_string());
    request.preferred_variants = variants.map(|values| values.to_vec());
    request.level = Level::Picky;
    let body = serde_urlencoded::to_string(&request).map_err(|_| LanguageToolError::Garbled)?;
    let headers = [("Content-Type", "application/x-www-form-urlencoded"), ("Accept", "application/json")];
    let request = HttpRequest { method: "POST", headers: &headers, body: Some(body.as_bytes()), patience, max_body: MAX_ANSWER };
    let reply = http::request(at, &request).map_err(told)?;
    if !(200..300).contains(&reply.status) {
        return Err(LanguageToolError::Failed);
    }
    Ok(reply.body)
}

/// One answer, read with the types the `languagetool-rust` crate defines: the sentence's language,
/// its matches (the good ones only when a neighbour is malformed), and whether the server itself
/// said its results were incomplete.
struct Answer {
    language: LanguageResponse,
    matches: Vec<Match>,
    incomplete: bool,
}

/// Reads an answer with the types the `languagetool-rust` crate defines. A whole [`Response`] is
/// tried first. If that fails only because one match is malformed, the matches are taken out raw
/// and the rest is decoded again as the same [`Response`] with an empty `matches`, so the header —
/// its `language` and its `warnings` alike — is validated by the crate's own types; each match is
/// then decoded on its own as a [`Match`], so a neighbour that is good is not lost and no parallel
/// wire type is invented. `warnings.incompleteResults` is carried out either way.
fn read_answer(body: &[u8]) -> Result<Answer, LanguageToolError> {
    if body.len() > MAX_ANSWER {
        return Err(LanguageToolError::TooLong);
    }
    // An answer is one object: `[]`, a bare string or a number is not one, whatever it parses to.
    if body.trim_ascii_start().first() != Some(&b'{') {
        return Err(LanguageToolError::Garbled);
    }
    if let Ok(answer) = serde_json::from_slice::<Response>(body) {
        return Ok(Answer { incomplete: incomplete(&answer), language: answer.language, matches: answer.matches });
    }
    let mut answer: Value = serde_json::from_slice(body).map_err(|_| LanguageToolError::Garbled)?;
    let items = match answer.get_mut("matches").and_then(Value::as_array_mut) {
        Some(items) => std::mem::take(items),
        None => return Err(LanguageToolError::Garbled),
    };
    // The empty list left behind is what `Response` requires; every other field is the server's,
    // so the header is read as strictly as a whole answer is.
    let header: Response = serde_json::from_value(answer).map_err(|_| LanguageToolError::Garbled)?;
    let mut matches = Vec::with_capacity(items.len().min(MAX_DIAGNOSTICS.saturating_mul(4)));
    for item in items.into_iter().take(MAX_DIAGNOSTICS.saturating_mul(4)) {
        if let Ok(found) = serde_json::from_value::<Match>(item) {
            matches.push(found);
        }
    }
    Ok(Answer { incomplete: incomplete(&header), language: header.language, matches })
}

/// Whether an answer's own `warnings` say its results are incomplete. An answer without `warnings`
/// says nothing of the kind, and is read as a complete one; the field is the crate's
/// `Warnings::incomplete_results`.
fn incomplete(answer: &Response) -> bool {
    answer.warnings.as_ref().is_some_and(|warnings| warnings.incomplete_results)
}

/// Which enabled language `answer` says the sentence was checked as, when the answer may be
/// trusted: the server detected the sentence in a tongue the owner enabled, with at least
/// [`CONFIDENT`] confidence, and checked it as that same tongue. The region preferred is the
/// owner's configured one for that tongue — a server may name another (`en-GB` for an English
/// sentence, `fr-BE` for a French one) and the tongue is still the one the owner enabled.
fn trusted_language(answer: &LanguageResponse, enabled: &[Language]) -> Option<Language> {
    let checked_as = tongue_of(&answer.code)?;
    let detected = &answer.detected_language;
    let confidence = detected.confidence?;
    if !confidence.is_finite() || confidence < CONFIDENT {
        return None;
    }
    let detected_tongue = tongue_of(&detected.code)?;
    // The language the server checked as and the one it worked out must be the same tongue: a
    // sentence it is not sure of is checked in neither.
    if checked_as != detected_tongue {
        return None;
    }
    if !enabled.iter().any(|language| language.is_english() == detected_tongue) {
        return None;
    }
    // The server's own region when the owner enabled it, else the owner's first variant of the
    // tongue: what was asked for as `preferredVariants` is what is believed.
    enabled
        .iter()
        .copied()
        .find(|language| language.code().eq_ignore_ascii_case(&detected.code))
        .or_else(|| enabled.iter().copied().find(|language| language.is_english() == detected_tongue))
}

/// Which tongue a language code is in, as a server may write it: `en` and every `en-*` are
/// English, `fr` and every `fr-*` French. `None` for a code that is not one of those, or is not a
/// code at all, so that an answer about a tongue proofing does not know is never believed.
fn tongue_of(code: &str) -> Option<bool> {
    let code = code.trim();
    if code.is_empty() || code.len() > 16 || !code.is_ascii() {
        return None;
    }
    let mut parts = code.split(['-', '_']);
    let tongue = parts.next()?;
    let region = parts.next();
    if parts.next().is_some() {
        return None;
    }
    if region.is_some_and(|region| region.is_empty() || !region.chars().all(|c| c.is_ascii_alphanumeric())) {
        return None;
    }
    match tongue.to_ascii_lowercase().as_str() {
        "en" => Some(true),
        "fr" => Some(false),
        _ => None,
    }
}

/// Turns one sentence's matches, as the `languagetool-rust` crate types them, into diagnostics
/// placed in the whole draft.
fn collect_segment(matches: &[Match], segment: &str, language: Language, quiet: &[String], base: usize, text: &str, out: &mut Vec<Diagnostic>) {
    let index = utf16_index(segment);
    let mut found: Vec<Diagnostic> = Vec::new();
    // The spans of the words the server could not spell in the tongue it chose. In a sentence
    // that mixes two languages these are the other tongue's words, and a grammar rule of this
    // tongue has no business correcting them.
    let mut unsaid: Vec<Span> = Vec::new();
    for found_match in matches.iter().take(MAX_DIAGNOSTICS.saturating_mul(4)) {
        let Some(span) = utf16_span(segment, &index, found_match.offset, found_match.length) else { continue };
        let rule = (found_match.rule.id.chars().count() <= MAX_RULE_ID).then(|| found_match.rule.id.clone());
        if let Some(rule) = &rule
            && quiet.iter().any(|kept| kept.eq_ignore_ascii_case(rule.trim()))
        {
            continue;
        }
        let kind = match found_match.rule.issue_type.as_str() {
            "style" => Kind::Style,
            "misspelling" => Kind::Spelling,
            _ => Kind::Grammar,
        };
        let replacements = found_match
            .replacements
            .iter()
            .filter(|replacement| replacement.value.chars().count() <= MAX_REPLACEMENT)
            .map(|replacement| replacement.value.clone());
        let mut diagnostic = Diagnostic::new(span, kind, Source::Server).saying(&found_match.message).suggesting(replacements);
        diagnostic.language = Some(language);
        diagnostic.rule = rule;
        if !diagnostic.is_sound() {
            continue;
        }
        if kind == Kind::Spelling {
            unsaid.push(span);
        }
        found.push(diagnostic);
    }
    for mut diagnostic in found {
        // Spelling belongs to the dictionaries, and a spelling match from a grammar server would
        // overrule them.
        if diagnostic.kind == Kind::Spelling {
            continue;
        }
        if unsaid.iter().any(|word| overlaps(word, &diagnostic.span)) {
            continue;
        }
        let (Some(start), Some(end)) = (base.checked_add(diagnostic.span.start), base.checked_add(diagnostic.span.end)) else {
            continue;
        };
        // Never clamped: a span that does not land in the draft is not this draft's.
        let Some(span) = Span::of(text, start, end) else { continue };
        diagnostic.span = span;
        if !diagnostic.is_sound() {
            continue;
        }
        out.push(diagnostic);
        if out.len() >= MAX_DIAGNOSTICS {
            break;
        }
    }
}

/// Whether two spans of one text touch: the same character, or a character of each.
fn overlaps(a: &Span, b: &Span) -> bool {
    a.start < b.end && b.start < a.end
}

/// Where each character of `text` is, in UTF-16 code units and in bytes: only the starts of
/// characters, in order, with the UTF-16 count of each (`2` for a character outside the basic
/// plane). LanguageTool counts its offsets this way (Java's `String`), and a MailCraft text is
/// counted in bytes, so this is the one conversion between them.
fn utf16_index(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::with_capacity(text.len().min(4_096));
    let mut units = 0usize;
    for (at, c) in text.char_indices() {
        out.push((units, at));
        units = units.saturating_add(c.len_utf16());
    }
    out.push((units, text.len()));
    out
}

/// The byte offset of the character starting at UTF-16 code unit `units`: `None` when no
/// character starts there (past the end, or between the two halves of a character outside the
/// basic plane), which is where a correction would cut a character in two.
fn utf16_to_byte(index: &[(usize, usize)], units: usize) -> Option<usize> {
    let at = index.binary_search_by_key(&units, |(unit, _)| *unit).ok()?;
    index.get(at).map(|(_, byte)| *byte)
}

/// The span of `text` that a match's UTF-16 `offset` and `length` name, checked: both
/// ends must be the start of characters and the span must be non-empty.
fn utf16_span(text: &str, index: &[(usize, usize)], offset: usize, length: usize) -> Option<Span> {
    let start = utf16_to_byte(index, offset)?;
    let end = utf16_to_byte(index, offset.checked_add(length)?)?;
    Span::of(text, start, end)
}

/// The rule ids the owner told to be quiet, trimmed and without repeats, at most [`MAX_QUIET`] of
/// them: the list is the owner's, and the settings holding it are outside input. Only the first
/// [`MAX_QUIET`] entries are looked at, whatever the list's length, so a hostile file cannot make a
/// check read more entries than a check reads; an entry past the cap is not looked for, and a rule
/// it names is not quieted. An entry longer than [`MAX_QUIET_RAW`] is refused for its own length
/// before it is trimmed, so a very long string is never scanned; after that, an id that is not
/// printable ASCII, or is longer than [`MAX_QUIET_RULE_ID`], is no rule's id and is not looked for.
fn quiet_rules(ignored: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for rule in ignored.iter().take(MAX_QUIET) {
        if rule.len() > MAX_QUIET_RAW {
            continue;
        }
        let rule = rule.trim();
        if rule.is_empty() || rule.len() > MAX_QUIET_RULE_ID || !rule.chars().all(|c| c.is_ascii_graphic()) {
            continue;
        }
        if !out.iter().any(|kept: &String| kept.eq_ignore_ascii_case(rule)) {
            out.push(rule.to_string());
        }
    }
    out
}

/// The owner's enabled languages, once, bounded and without repeats: at most [`MAX_LANGUAGES`]
/// entries of the list are read, and one already read is not read again. This one list is what the
/// check asks for as `preferredVariants` and what it believes an answer about, so a language that
/// steers the question is the same one that makes an answer trusted. A tongue is one of four
/// [`Language`]s, so nothing the owner enabled is lost: a repeat says nothing new, and an entry
/// past the cap can only be one.
fn enabled_languages(languages: &[Language]) -> Vec<Language> {
    let mut out: Vec<Language> = Vec::new();
    for language in languages.iter().copied().take(MAX_LANGUAGES) {
        if !out.contains(&language) {
            out.push(language);
        }
    }
    out
}

/// The instant a batch's patience runs out: at most [`MAX_BATCH`] from now, whatever the caller
/// asked for, and never an instant this machine's clock cannot express.
pub(crate) fn batch_deadline(patience: Duration) -> Instant {
    let now = Instant::now();
    now.checked_add(patience.min(MAX_BATCH)).unwrap_or(now)
}

/// The first [`MAX_CHECKED`] characters of `text`, on a character boundary.
fn checked_prefix(text: &str) -> &str {
    match text.char_indices().nth(MAX_CHECKED) {
        Some((at, _)) => text.get(..at).unwrap_or(text),
        None => text,
    }
}

/// One `preferredVariants` entry for each tongue among `languages`, in the order the owner
/// enabled them: the region the owner chose, so the server answers in a variant they enabled.
fn preferred_variants(languages: &[Language]) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for language in languages {
        if !out.iter().any(|kept| tongue_of(kept) == tongue_of(language.code())) {
            out.push(language.code());
        }
    }
    out
}

/// The sentences of `text`, as byte ranges into it: up to [`MAX_SEGMENTS`] of them, each trimmed
/// of white space, no longer than [`SEGMENT_BYTES`] and holding a letter (an empty piece between
/// two stops, or a run of punctuation, is not a sentence to ask about). The counts of the
/// sentences left out (too long, or past the cap) are kept so the caller can say the look was
/// partial and how many were skipped.
struct Segments<'a> {
    pieces: Vec<(usize, &'a str)>,
    too_long: usize,
    beyond_cap: usize,
}

/// The sentences of `text`, bounded. A stop ends a sentence only where a sentence can: see
/// [`sentence_end`]. A newline ends one only where a paragraph does: see [`ends_paragraph`].
/// Everything else is the server's own reader's to split.
fn segments(text: &str) -> Segments<'_> {
    let mut out = Segments { pieces: Vec::new(), too_long: 0, beyond_cap: 0 };
    let mut start = 0usize;
    for (at, c) in text.char_indices() {
        let after = at + c.len_utf8();
        let end = match c {
            '.' | '?' | '!' => sentence_end(text, after),
            '\n' => ends_paragraph(text, after).then_some(after),
            _ => None,
        };
        if let Some(end) = end {
            keep_segment(&mut out, text, start, end);
            start = end;
        }
    }
    keep_segment(&mut out, text, start, text.len());
    out
}

/// Where a sentence ends, when the stop ending at byte `after` ends one: at the stop itself, or
/// past the closing quote or bracket written right after it (that mark belongs to the sentence it
/// closes, not to the next one). `None` when the stop is inside a number (`3.14`) or an address
/// (`mail.example`): cutting there would hide the sentence's real mistakes from the server's
/// reader, which knows a number from a sentence's end.
fn sentence_end(text: &str, after: usize) -> Option<usize> {
    match text.get(after..).and_then(|rest| rest.chars().next()) {
        None => Some(after),
        Some(next) if next.is_whitespace() => Some(after),
        Some(next) if matches!(next, '"' | '\'' | '\u{2019}' | '\u{201d}' | '\u{00bb}' | ')' | ']' | '}') => Some(after + next.len_utf8()),
        Some(_) => None,
    }
}

/// Whether the newline ending at byte `after` ends a paragraph: the next line, past spaces, tabs
/// and carriage returns, is another newline — a blank line between two paragraphs. A line break
/// inside a paragraph is not one: a message is hard-wrapped, and a sentence is not ended by the
/// width of the window it was written in.
fn ends_paragraph(text: &str, after: usize) -> bool {
    text.get(after..).and_then(|rest| rest.chars().find(|c| !matches!(c, ' ' | '\t' | '\r'))).is_some_and(|next| next == '\n')
}

/// Adds the trimmed piece `start..end` of `text` when it is a sentence worth asking about.
fn keep_segment<'a>(out: &mut Segments<'a>, text: &'a str, start: usize, end: usize) {
    let mut end = end.min(text.len());
    let mut start = start.min(end);
    while start < end {
        let Some(c) = text.get(start..).and_then(|rest| rest.chars().next()) else { break };
        if !c.is_whitespace() {
            break;
        }
        start += c.len_utf8();
    }
    while end > start {
        let Some(c) = text.get(..end).and_then(|head| head.chars().next_back()) else { break };
        if !c.is_whitespace() {
            break;
        }
        end -= c.len_utf8();
    }
    if start >= end {
        return;
    }
    let Some(segment) = text.get(start..end) else { return };
    if !segment.chars().any(char::is_alphabetic) {
        return;
    }
    if segment.len() > SEGMENT_BYTES {
        out.too_long = out.too_long.saturating_add(1);
        return;
    }
    if out.pieces.len() >= MAX_SEGMENTS {
        out.beyond_cap = out.beyond_cap.saturating_add(1);
        return;
    }
    out.pieces.push((start, segment));
}

/// The address a check is posted to: the server's own, with one `/` between it and [`CHECK`],
/// however many trailing ones it was written with. `None` for what no check is posted to.
fn endpoint(url: &str) -> Option<String> {
    endpoint_at(url, CHECK)
}

/// The address `path` is asked under: [`endpoint`]'s rule for a readiness probe's [`LANGUAGES`]
/// too, so both refuse the same addresses — an empty one, and one with a query or a fragment — and
/// neither doubles a trailing `/`.
fn endpoint_at(url: &str, path: &str) -> Option<String> {
    let base = url.trim().trim_end_matches('/');
    // A query or a fragment after the address would be read as part of the path: neither is one.
    if base.is_empty() || base.contains(['?', '#']) {
        return None;
    }
    Some(format!("{base}{path}"))
}

/// Whether the address the transport will actually dial is one a check may be sent to.
///
/// The model's [`proof::address_allowed`] reads a host the way a link reader does, normalizing a
/// legacy number (`2130706433`) to dotted decimal; the transport keeps the host exactly as it was
/// written and then resolves it. The two disagree about names in the one place that matters here:
/// a secure connection is dialled without the resolved-address filter the plain one has, so an
/// `https://` name would follow whatever a look-up says. A grammar server is on this computer or
/// not at all, so a secure address is accepted only when its literal host is a loopback IP
/// address, which is dialled with no look-up. A plain address keeps the filter below the transport
/// (`http://` is accepted only for a loopback name or address, and every resolved address is
/// filtered), so nothing more is needed here.
pub(crate) fn transport_allowed(at: &Url) -> bool {
    !at.secure || at.host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// What an error of the crate's HTTP client means for a check: one of the owner's sentences,
/// with nothing of the text and nothing of the answer in it.
fn told(e: NetError) -> LanguageToolError {
    if http::is_too_slow(&e) {
        return LanguageToolError::TooSlow;
    }
    if http::is_too_long(&e) {
        return LanguageToolError::TooLong;
    }
    if http::is_closed(&e) {
        return LanguageToolError::Closed;
    }
    match e {
        // No connection was made at all: the address, the name, or the secure connection.
        NetError::Connect { .. } | NetError::Tls { .. } | NetError::Certificate(_) | NetError::Config(_) => LanguageToolError::Unreachable,
        _ => LanguageToolError::Garbled,
    }
}
