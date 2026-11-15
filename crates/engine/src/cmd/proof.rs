// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Spelling and grammar while writing (#954): the checks, Review › Spelling & Grammar's walk, and
//! the settings that say what is checked and where the words come from.
//!
//! Spelling is checked here, on this computer, against every dictionary the owner turned on at
//! once: a word is right if any of them knows it, so an English word in a French sentence is not a
//! mistake and neither is the other way round ([`mailcraft_model::proof::Speller`]). Grammar is
//! the host's (a LanguageTool server, asked by the window or the CLI with `mailcraft_net`); what
//! these commands do with it is take its places in, check that they still fit the text, and walk
//! the owner through them.
//!
//! Nothing here writes to the mailbox, and nothing here opens a connection: `proof.check` and
//! `proof.suggest` change nothing at all, and a correction is one step of the message's own undo
//! ([`mailcraft_model::rich::RichBody::replace_range`]).
//!
//! Message text is data: it is not logged, and no answer of these commands carries anything of a
//! message that was not asked for (the places found, their words and what could stand in them).

use mailcraft_model::proof::{Diagnostic, Kind, Language, ProofSettings, Span};

use serde_json::{Value, json};

use super::bad;
use super::format::{needs_doc, state};
use crate::proof::{Editor, Proofing, is_sound};
use crate::{CmdResult, CommandSpec, Session, p};

pub fn specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec::new("proof.check", "Check Spelling", "Review › Proofing", check)
            .pure()
            .params(PARAMS)
            .when_writing(needs_doc),
        CommandSpec::new("proof.suggest", "Suggestions", "Review › Proofing", suggest).pure().params(SUGGEST).when_writing(needs_doc),
        CommandSpec::new("proof.status", "Spelling & Grammar Settings", "Review › Proofing", status).pure(),
        CommandSpec::new("proof.set", "Spelling & Grammar Settings", "Review › Proofing", set).pure().params(SET),
        CommandSpec::new("proof.addWord", "Add to Dictionary", "Review › Proofing", add_word)
            .pure()
            .params(r#"{"word": string} — the owner's own word, kept in their settings; answers the settings as they are now"#),
        CommandSpec::new("proof.removeWord", "Remove from Dictionary", "Review › Proofing", remove_word)
            .pure()
            .params(r#"{"word": string} — a word of the owner's own out of the personal dictionary again"#),
        CommandSpec::new("proof.ignoreRule", "Ignore Grammar Rule", "Review › Proofing", ignore_rule)
            .pure()
            .params(r#"{"rule": string} — a LanguageTool rule id kept quiet from now on; answers the settings"#),
        CommandSpec::new("proof.forgetRules", "Check Every Rule Again", "Review › Proofing", forget_rules).pure(),
        CommandSpec::new("review.spelling", "Spelling & Grammar", "Review › Proofing", review)
            .pure()
            .params(REVIEW)
            .when_writing(needs_doc),
        CommandSpec::new("proof.change", "Change", "Review › Proofing", change)
            .params(PARAMS_CHANGE)
            .when_writing(needs_doc),
        CommandSpec::new("proof.ignore", "Ignore Once", "Review › Proofing", ignore_once)
            .pure()
            .params(r#"{"rule"?: string} — leaves the place on screen alone this time; with a rule, that grammar rule is left alone for the rest of the walk too"#)
            .when_writing(needs_doc),
        CommandSpec::new("proof.close", "Close Spelling & Grammar", "Review › Proofing", close).pure(),
    ]
}

const PARAMS: &str = r#"{"text"?: string, "from"?: number, "to"?: number} — checks text, or the body (default: its selection when nonempty); from/to count Unicode characters in the supplied text or body, diagnostics count bytes relative to the returned checked text; answers {"diagnostics": [...], "text": checked text, "from": character start, "to": character end, "languages": [...], "loaded": bool, "note"?: why no dictionary}"#;

const SUGGEST: &str =
    r#"{"word": string, "limit"?: number (1–8, default 5)} — suggestions from every enabled dictionary; answers {"suggestions": [...]}"#;

const PARAMS_CHANGE: &str = r#"{"text"?: string, "from"?: number, "to"?: number, "expectedText"?: string, "composeId"?: number, "signature"?: bool, "signatureId"?: number} — one undo step; named character ranges require the complete checked expectedText and the editor they were checked in: a draft's composeId, or signature:true with the signatureId of the open signature editor (never a composeId); without a range changes the current review place; stale corrections are refused"#;

const SET: &str = r##"{"checkSpelling"?: bool, "checkGrammar"?: bool, "asYouType"?: bool, "managedServer"?: bool (check grammar with the host's own installed local server; the grammarUrl already set is kept as it is), "languages"?: ["en-CA","fr-CA",…], "grammarUrl"?: string (http:// or https:// on loopback only), "dictionaryDir"?: string (a folder, "" for the usual places)} — grammar is local-only; legacy allowRemote:true is refused; answers the settings as they are now"##;

const REVIEW: &str = r#"{"grammar"?: [{"start": byte, "end": byte, "kind": "grammar"|"style", "message"?: string, "suggestions"?: [string], "rule"?: string, "language"?: string}], "spelling"?: [same shape, kind:"spelling"], "expectedText"?: string, "composeId"?: number, "signature"?: bool, "signatureId"?: number, "again"?: bool} — supplied spelling skips synchronous dictionary work and requires the complete expectedText and editor identity (a draft's composeId, or signature:true with the open signature editor's signatureId); again starts a new walk; answers {"done", "position", "total", "at"?, "text"}"#;

/// The characters `from..to` (in characters) of `text`.
fn chars_between(text: &str, from: usize, to: usize) -> String {
    text.chars().skip(from).take(to.saturating_sub(from)).collect()
}

/// What proofing checks while this editor is in front: the open draft's own language choice, or
/// the owner's global settings while a signature is being edited (a signature has none of its own).
fn document_settings(s: &Session) -> ProofSettings {
    match (s.signature_edit.is_some(), s.compose_id()) {
        (false, Some(id)) => s.proof_settings_for(id),
        _ => s.prefs.proof.clone(),
    }
}

/// The characters `span` covers, in a text whose positions are characters.
fn char_range(text: &str, span: Span) -> Option<(usize, usize)> {
    if !span.fits(text) {
        return None;
    }
    let start = text.get(..span.start)?.chars().count();
    let end = text.get(..span.end)?.chars().count();
    (start < end).then_some((start, end))
}

fn check(s: &mut Session, v: &Value) -> CmdResult {
    let (body, _) = s.doc().ok_or_else(|| bad("no message is being written"))?;
    let supplied = v.get("text").map(|value| value.as_str().ok_or_else(|| bad("`text` is a string"))).transpose()?;
    let whole = supplied.map(str::to_string).unwrap_or_else(|| body.plain());
    let len = whole.chars().count();
    let from = super::format::position(v, "from")?;
    let to = super::format::position(v, "to")?;
    let selection = body.selection();
    let (from, to) = if from.is_none() && to.is_none() && supplied.is_none() && !selection.is_empty() {
        (selection.start(), selection.end())
    } else {
        (from.unwrap_or(0), to.unwrap_or(len))
    };
    if from > len || to > len {
        return Err(bad("the checked range is outside the text (positions count characters)"));
    }
    let (from, to) = (from.min(to), from.max(to));
    let text = chars_between(&whole, from, to);
    let settings = document_settings(s);
    let diagnostics = if settings.check_spelling { s.proof.check(&settings, &text) } else { Vec::new() };
    let note = s.proof.note.clone();
    let loaded = !s.proof.words(&settings).is_empty();
    let languages: Vec<&str> = s.proof.words(&settings).languages().iter().map(|l| l.code()).collect();
    Ok(json!({
        "diagnostics": diagnostics,
        "text": text,
        "from": from,
        "to": to,
        "languages": languages,
        "loaded": loaded,
        "note": note,
    }))
}

fn suggest(s: &mut Session, v: &Value) -> CmdResult {
    let word = p::req_str(v, "word")?;
    let limit = p::u64(v, "limit").and_then(|n| usize::try_from(n).ok()).unwrap_or(5).clamp(1, 8);
    let settings = document_settings(s);
    Ok(json!({ "suggestions": s.proof.suggest(&settings, word, limit) }))
}

/// The settings as they are, and whether a dictionary was read.
fn settings_state(s: &Session) -> Value {
    let settings = s.prefs.proof.clone();
    let cached = s.proof.cached_words(&settings);
    let loaded = cached.is_some_and(|words| !words.is_empty());
    let note = cached.and(s.proof.note.as_ref());
    let languages: Vec<&str> = cached.map(|words| words.languages().iter().map(|l| l.code()).collect()).unwrap_or_default();
    json!({
        "settings": settings,
        "loaded": loaded,
        "read": languages,
        "note": note,
        "grammarAllowed": settings.grammar_allowed(),
    })
}

fn status(s: &mut Session, _: &Value) -> CmdResult {
    let settings = s.prefs.proof.clone();
    s.proof.words(&settings);
    Ok(settings_state(s))
}

/// The dispatcher's classification is also enforced at the settings mutation boundary.
fn settings_permission(s: &Session, command: &str) -> Result<(), crate::CmdError> {
    let class = crate::agent::Class::Settings;
    if s.agent.as_ref().is_some_and(|allow| !allow.contains(&class)) {
        return Err(bad(crate::agent::needs_owner(command, class, false, "change proofreading settings")));
    }
    Ok(())
}

fn set(s: &mut Session, v: &Value) -> CmdResult {
    settings_permission(s, "proof.set")?;
    let mut settings = s.prefs.proof.clone();
    if let Some(b) = p::opt_bool(v, "checkSpelling")? {
        settings.check_spelling = b;
    }
    if let Some(b) = p::opt_bool(v, "checkGrammar")? {
        settings.check_grammar = b;
    }
    if let Some(b) = p::opt_bool(v, "asYouType")? {
        settings.as_you_type = b;
    }
    if let Some(b) = p::opt_bool(v, "managedServer")? {
        // Which server to use, as a setting of its own: the address the owner typed stays as it
        // is, so turning this off again finds the same manual address. Nothing is asked of any
        // server here and no dictionary is read.
        settings.managed_server = b;
    }
    if let Some(b) = p::opt_bool(v, "allowRemote")? {
        if b {
            return Err(bad("grammar checking supports only a server on this computer"));
        }
        settings.check_grammar = settings.check_grammar && mailcraft_model::proof::address_allowed(&settings.grammar_url, false);
    }
    settings.allow_remote = false;
    if let Some(values) = v.get("languages") {
        let list = values.as_array().ok_or_else(|| bad("`languages` is a list of language codes"))?;
        let mut languages: Vec<Language> = Vec::new();
        for value in list {
            let code = value.as_str().ok_or_else(|| bad("`languages` is a list of language codes"))?;
            let lang = Language::parse(code).ok_or_else(|| bad(format!("`{code}` is no language proofing knows")))?;
            if !languages.contains(&lang) {
                languages.push(lang);
            }
        }
        if languages.is_empty() {
            return Err(bad("`languages` names at least one language"));
        }
        settings.languages = languages;
    }
    if let Some(url) = p::str(v, "grammarUrl") {
        let url = url.trim();
        if !url.is_empty() && !mailcraft_model::proof::address_allowed(url, settings.allow_remote) {
            return Err(bad("the grammar server's address must be http:// or https:// on this computer (loopback only)"));
        }
        if !url.is_empty() && url.len() > 2_000 {
            return Err(bad("the grammar server's address is too long"));
        }
        settings.grammar_url = url.to_string();
    }
    if let Some(dir) = p::str(v, "dictionaryDir") {
        if dir.len() > 1_024 {
            return Err(bad("`dictionaryDir` is too long"));
        }
        if dir.contains('\0') {
            return Err(bad("`dictionaryDir` names a folder that cannot be used"));
        }
        // A folder is a filesystem path, not prose: every byte the owner typed or picked is kept,
        // spaces at either end included. Only an exact empty string means the usual places.
        settings.dictionary_dir = dir.to_string();
        // Another folder: the dictionaries are read again at the next check.
    }
    s.prefs.proof = settings;
    // A change of languages or folder makes what was read (and the note about it) describe the
    // old settings; it is dropped here at once, and the next check reads the new ones. No file is
    // read on the frame that saves the settings.
    let now = s.prefs.proof.clone();
    s.proof.forget_stale_cache(&now);
    Ok(settings_state(s))
}

fn add_word(s: &mut Session, v: &Value) -> CmdResult {
    settings_permission(s, "proof.addWord")?;
    let word = p::req_str(v, "word")?.to_string();
    let mut settings = s.prefs.proof.clone();
    let added = settings.add_word(&word);
    s.prefs.proof = settings;
    let mut answer = settings_state(s);
    if let Some(map) = answer.as_object_mut() {
        map.insert("added".into(), json!(added));
    }
    Ok(answer)
}

fn remove_word(s: &mut Session, v: &Value) -> CmdResult {
    settings_permission(s, "proof.removeWord")?;
    let word = p::req_str(v, "word")?.to_string();
    let mut settings = s.prefs.proof.clone();
    let removed = settings.remove_word(&word);
    s.prefs.proof = settings;
    let mut answer = settings_state(s);
    if let Some(map) = answer.as_object_mut() {
        map.insert("removed".into(), json!(removed));
    }
    Ok(answer)
}

fn ignore_rule(s: &mut Session, v: &Value) -> CmdResult {
    settings_permission(s, "proof.ignoreRule")?;
    let rule = p::req_str(v, "rule")?.trim().to_string();
    if rule.is_empty() || rule.len() > 64 {
        return Err(bad("`rule` is a grammar rule's id"));
    }
    let mut settings = s.prefs.proof.clone();
    if !settings.ignored_rules.contains(&rule) && settings.ignored_rules.len() < 500 {
        settings.ignored_rules.push(rule);
    }
    s.prefs.proof = settings;
    Ok(settings_state(s))
}

fn forget_rules(s: &mut Session, _: &Value) -> CmdResult {
    settings_permission(s, "proof.forgetRules")?;
    s.prefs.proof.ignored_rules.clear();
    Ok(settings_state(s))
}

/// The body as it is, with what has been found in it.
fn review(s: &mut Session, v: &Value) -> CmdResult {
    bind_document(s);
    let (body, _) = s.doc().ok_or_else(|| bad("no message is being written"))?;
    let text = body.plain();
    if v.get("spelling").is_some() {
        checked_editor(s, v, &text)?;
        if !v.get("spelling").is_some_and(Value::is_array) {
            return Err(bad("`spelling` is a list of checked places"));
        }
    }
    let settings = document_settings(s);
    let again = p::bool(v, "again").unwrap_or(false) || s.proof.current_walk(&text).is_none();
    if !again {
        // The walk goes on where it was: no dictionary is read again and nothing is searched for.
        return Ok(walk_state(s, &text));
    }
    let mut found: Vec<Diagnostic> = if !settings.check_spelling {
        Vec::new()
    } else if v.get("spelling").is_some() {
        supplied_places(v, "spelling", &text)
    } else {
        s.proof.check(&settings, &text)
    };
    if settings.check_grammar || v.get("spelling").is_none() {
        found
            .extend(supplied_places(v, "grammar", &text).into_iter().filter(|d| d.rule.as_ref().is_none_or(|r| !settings.ignored_rules.contains(r))));
    }
    s.proof.start(&text, found);
    Ok(walk_state(s, &text))
}

/// The grammar places the host brought back, taken only where they still fit the text.
fn supplied_places(v: &Value, key: &str, text: &str) -> Vec<Diagnostic> {
    let Some(list) = v.get(key).and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out: Vec<Diagnostic> = Vec::new();
    for item in list.iter().take(mailcraft_model::proof::MAX_DIAGNOSTICS) {
        let Some(start) = p::u64(item, "start").and_then(|n| usize::try_from(n).ok()) else { continue };
        let Some(end) = p::u64(item, "end").and_then(|n| usize::try_from(n).ok()) else { continue };
        let Some(span) = Span::of(text, start, end) else { continue };
        let kind = match (key, p::str(item, "kind")) {
            ("spelling", _) => Kind::Spelling,
            (_, Some("style")) => Kind::Style,
            (_, Some("spelling")) => continue,
            _ => Kind::Grammar,
        };
        let source = if key == "spelling" { mailcraft_model::proof::Source::Local } else { mailcraft_model::proof::Source::Server };
        let mut d = Diagnostic::new(span, kind, source);
        if let Some(message) = p::str(item, "message") {
            d = d.saying(message);
        }
        if let Some(suggestions) = item.get("suggestions").and_then(Value::as_array) {
            d = d.suggesting(suggestions.iter().filter_map(Value::as_str).map(str::to_string));
        }
        d.rule = p::str(item, "rule").filter(|rule| rule.len() <= 64 && rule.chars().all(|c| c.is_ascii_graphic())).map(str::to_string);
        d.language = p::str(item, "language").and_then(Language::parse);
        if is_sound(&d, text) {
            out.push(d);
        }
    }
    out
}

/// What Review › Spelling & Grammar shows: the place on screen, where it is, and the body it was
/// found in (so that the window can compare it with the body as it is now).
fn walk_state(s: &mut Session, text: &str) -> Value {
    bind_document(s);
    let Some(walk) = s.proof.current_walk(text) else {
        return json!({ "done": true, "position": 0, "total": 0 });
    };
    let total = walk.items.len();
    let at = walk.current().map(|d| {
        json!({
            "start": d.span.start,
            "end": d.span.end,
            "kind": d.kind,
            "word": text.get(d.span.start..d.span.end),
            "message": d.message,
            "suggestions": d.suggestions,
            "rule": d.rule,
            "language": d.language.map(|l| l.code()),
        })
    });
    json!({
        "done": at.is_none() || walk.at >= total,
        "position": walk.at.min(total),
        "total": total,
        "at": at,
        "rules": walk.items.iter().filter_map(|d| d.rule.clone()).collect::<Vec<String>>(),
        "text": walk.text,
        "left": walk.left(),
    })
}

fn change(s: &mut Session, v: &Value) -> CmdResult {
    if s.signature_edit.is_some() {
        settings_permission(s, "proof.change")?;
    }
    bind_document(s);
    let (body, _) = s.doc().ok_or_else(|| bad("no message is being written"))?;
    let text = body.plain();
    if let Some(expected) = v.get("expectedText") {
        let expected = expected.as_str().ok_or_else(|| bad("`expectedText` is the checked body's text"))?;
        if expected != text {
            return Err(bad("the message changed after this correction was offered"));
        }
    }
    if v.get("composeId").is_some() {
        let compose = p::u64(v, "composeId").ok_or_else(|| bad("`composeId` is the checked draft's id"))?;
        if s.compose_id() != Some(compose) || s.signature_edit.is_some() {
            return Err(bad("this correction belongs to a different editor"));
        }
    }
    if s.signature_edit.is_some() {
        // A correction offered for a signature names this editor instance: with the flag and its
        // id, and never with a draft's id (refused above). One that names it not at all — a walk's
        // place, bound to this editor by `bind_document` — is this editor's own.
        if v.get("signatureId").is_some() || p::opt_bool(v, "signature")? == Some(true) {
            checked_signature(s, v)?;
        }
    } else if v.get("signatureId").is_some() {
        return Err(bad("`signatureId` names a signature editor, and none is open"));
    }
    // The place named by `from` and `to` (a right click's, a script's), else the one the walk is at.
    let named = match (super::format::position(v, "from")?, super::format::position(v, "to")?) {
        (Some(from), Some(to)) => Some((from, to)),
        (None, None) => None,
        _ => return Err(bad("a correction names both `from` and `to`, or neither")),
    };
    if named.is_some() {
        checked_editor(s, v, &text)?;
    }
    let (_place, from, to, with) = match named {
        Some((from, to)) => {
            let (from, to) = if from <= to { (from, to) } else { (to, from) };
            let with = p::str(v, "text").map(str::to_string).ok_or_else(|| bad("`text` (string) is required with `from` and `to`"))?;
            let len = text.chars().count();
            if from >= to || to > len {
                return Err(bad("`from` and `to` are not a stretch of the message (in characters)"));
            }
            (None, from, to, with)
        }
        _ => {
            let Some(walk) = s.proof.current_walk(&text) else {
                return Err(bad("Review › Spelling & Grammar was not started, or the message has changed since"));
            };
            let Some(place) = walk.current().cloned() else {
                return Err(bad("there is nothing to change"));
            };
            let with = match p::str(v, "text") {
                Some(t) => t.to_string(),
                None => place.suggestions.first().cloned().ok_or_else(|| bad("this one has no suggestion: put the right words in `text`"))?,
            };
            let (from, to) = char_range(&text, place.span).ok_or_else(|| bad("that place is no longer in the message"))?;
            (Some(place), from, to, with)
        }
    };
    let d = s.doc_mut().ok_or_else(|| bad("no message is being written"))?;
    d.body.replace_range(from, to, &with).map_err(|e| bad(e.to_string()))?;
    *d.dirty = true;
    // The message has changed, so the places found in it no longer belong to it: the walk starts
    // again, at the first place after the correction.
    let after = body_now(s);
    if named.is_some() {
        // Inline edits are followed by the host's debounced checks, never by file I/O or a
        // document-wide suggestion pass on the frame handling a click.
        s.proof.close();
    } else {
        let settings = document_settings(s);
        let mut found = if settings.check_spelling { s.proof.check(&settings, &after) } else { Vec::new() };
        let moved = from.saturating_add(with.chars().count());
        found.retain(|d| char_range(&after, d.span).is_some_and(|(a, _)| a >= moved));
        s.proof.start(&after, found);
    }
    let mut answer = walk_state(s, &after);
    if let Some(map) = answer.as_object_mut() {
        map.insert("changed".into(), json!(with));
        map.insert("state".into(), state(s));
        map.insert("grammarPending".into(), json!(true));
    }
    Ok(answer)
}

/// Prove that host results and named actions belong to this complete text and editor.
fn checked_editor(s: &Session, v: &Value, text: &str) -> Result<(), crate::CmdError> {
    let expected = v.get("expectedText").and_then(Value::as_str).ok_or_else(|| bad("the complete checked `expectedText` is required"))?;
    if expected != text {
        return Err(bad("the message changed after it was checked"));
    }
    if s.signature_edit.is_some() {
        checked_signature(s, v)
    } else if p::u64(v, "composeId") != s.compose_id() || s.compose_id().is_none() || p::opt_bool(v, "signature")? == Some(true) {
        Err(bad("the checked draft's `composeId` is required"))
    } else if v.get("signatureId").is_some() {
        Err(bad("`signatureId` names a signature editor, and none is open"))
    } else {
        Ok(())
    }
}

/// A signature's correction belongs to the editor instance it was checked in: the flag, and the
/// identity of the open editor — never a draft's id, and never its words alone.
fn checked_signature(s: &Session, v: &Value) -> Result<(), crate::CmdError> {
    if p::opt_bool(v, "signature")? != Some(true) {
        return Err(bad("a signature correction requires `signature`: true"));
    }
    if v.get("composeId").is_some() {
        return Err(bad("a signature correction carries `signatureId`, never a draft's `composeId`"));
    }
    let id = p::u64(v, "signatureId").ok_or_else(|| bad("a signature correction requires the `signatureId` of the open signature editor"))?;
    if s.signature_id() != Some(id) {
        return Err(bad("this correction belongs to a different signature editor"));
    }
    Ok(())
}

/// The body's text as it is now.
fn body_now(s: &Session) -> String {
    s.doc().map(|(b, _)| b.plain()).unwrap_or_default()
}

fn ignore_once(s: &mut Session, v: &Value) -> CmdResult {
    if p::str(v, "rule").is_some_and(|rule| !rule.is_empty()) {
        settings_permission(s, "proof.ignore")?;
    }
    bind_document(s);
    let text = body_now(s);
    if let Some(rule) = p::str(v, "rule").filter(|r| !r.is_empty() && r.chars().count() <= 64) {
        let mut settings = s.prefs.proof.clone();
        if !settings.ignored_rules.iter().any(|r| r == rule) && settings.ignored_rules.len() < 500 {
            settings.ignored_rules.push(rule.to_string());
        }
        s.prefs.proof = settings;
    }
    s.proof.ignore(&text);
    Ok(walk_state(s, &text))
}

fn close(s: &mut Session, _: &Value) -> CmdResult {
    s.proof.close();
    Ok(json!({ "done": true }))
}

/// A review's ownership is independent of its text, so two identical drafts stay distinct; and a
/// signature is owned by the editor instance it was opened as, not by the words it holds.
fn bind_document(s: &mut Session) {
    let settings = document_settings(s);
    s.proof.bind_settings(&settings);
    let editor = match (s.signature_id(), s.compose_id()) {
        (Some(id), _) => Some(Editor::Signature(id)),
        (None, Some(id)) => Some(Editor::Compose(id)),
        (None, None) => None,
    };
    s.proof.bind_document(editor);
}

/// Proofing's state for the window: whether a walk is open.
pub fn walking(proofing: &Proofing) -> bool {
    proofing.walk.is_some()
}

/// The settings, for a caller that has a session but no proofing state of its own.
pub fn settings_of(s: &Session) -> ProofSettings {
    s.prefs.proof.clone()
}
