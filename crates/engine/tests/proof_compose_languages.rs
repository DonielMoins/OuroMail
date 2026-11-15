// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! A message being written may be proofread in languages of its own, chosen in the composer's
//! footer (#954): the choice belongs to that draft, is snapshotted from the owner's defaults when
//! it opens, is kept with a locally saved draft, and never changes another open message or the
//! global defaults in File › Options.
//!
//! The dictionaries are two tiny invented pairs written to a folder of this test's own and named
//! in the settings, as `proof_simulations.rs` does; nothing here reads a real dictionary, a real
//! mailbox, or opens a connection. The sample mailbox and a fixed clock are enough.

use std::path::{Path, PathBuf};

use mailcraft_engine::{Session, sample};
use mailcraft_time::Timestamp;
use serde_json::{Value, json};

/// The instant these tests are at.
const NOW: Timestamp = 1_750_000_000;

/// Canadian English: a handful of words, one of them English-only in this pair.
const EN_WORDS: [&str; 3] = ["the", "colour", "document"];

/// Canadian French: the same, with one French-only word.
const FR_WORDS: [&str; 3] = ["le", "courriel", "document"];

/// A folder of this test's own with the two tiny dictionaries in it, taken away when it ends.
struct Fixture {
    path: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        let path = std::env::temp_dir().join(format!("mailcraft-proof-lang-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        let _ = std::fs::create_dir_all(&path);
        let fixture = Fixture { path };
        fixture.dictionary("en_CA.dic", &EN_WORDS);
        fixture.dictionary("fr_CA.dic", &FR_WORDS);
        fixture
    }

    /// A Hunspell `.dic` of `words` (the first line is only a hint).
    fn dictionary(&self, file: &str, words: &[&str]) {
        let mut text = format!("{}\n", words.len());
        for word in words {
            text.push_str(word);
            text.push('\n');
        }
        let _ = std::fs::write(self.path.join(file), text);
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A session on the sample mailbox whose dictionaries are this test's own, and whose defaults are
/// English only (so that a French word is a mistake until a draft says otherwise).
fn session(name: &str) -> (Session, Fixture) {
    let fixture = Fixture::new(name);
    let mut s = Session::at(sample::mailbox(NOW, 0), NOW, 0);
    let dir = fixture.path().to_string_lossy().to_string();
    s.run("proof.set", &json!({ "languages": ["en-CA"], "dictionaryDir": dir })).expect("the defaults");
    (s, fixture)
}

/// A message being written, empty: its id.
fn open(s: &mut Session) -> u64 {
    s.run("mail.new", &json!({})).expect("a message")["compose"].as_u64().expect("an id")
}

/// The words an answer flagged, in the text it checked.
fn flagged(answer: &Value) -> Vec<String> {
    let text = answer["text"].as_str().unwrap_or_default();
    answer["diagnostics"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|d| {
                    let start = usize::try_from(d["span"]["start"].as_u64()?).ok()?;
                    let end = usize::try_from(d["span"]["end"].as_u64()?).ok()?;
                    text.get(start..end).map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// One draft's languages are its own: a French word is a mistake in the draft that was left with
/// the English defaults, and is a word in the draft that chose French; the global defaults and the
/// other draft are untouched.
#[test]
fn a_drafts_languages_are_its_own() {
    let (mut s, _fixture) = session("isolation");
    let english = open(&mut s);
    let french = open(&mut s);
    s.run("compose.set", &json!({ "compose": french, "proofLanguages": ["fr-CA"] })).expect("the draft's French");

    let in_french = s.run("proof.check", &json!({ "compose": french, "text": "le courriel" })).expect("a check");
    assert_eq!(in_french["loaded"], json!(true), "a dictionary was read: {in_french}");
    assert!(flagged(&in_french).is_empty(), "the French draft reads French: {:?}", flagged(&in_french));

    let in_english = s.run("proof.check", &json!({ "compose": english, "text": "le courriel" })).expect("a check");
    assert_eq!(flagged(&in_english), ["le", "courriel"], "the other draft stays English: {in_english}");

    let status = s.run("proof.status", &json!({})).expect("the settings");
    assert_eq!(status["settings"]["languages"], json!(["en-CA"]), "the global defaults did not move");
}

/// A message keeps the defaults it was opened with: changing File › Options while it is being
/// written changes the defaults for the next message, never this one; and the next message opened
/// takes the new defaults.
#[test]
fn a_new_message_keeps_the_defaults_it_was_opened_with() {
    let (mut s, _fixture) = session("snapshot");
    let older = open(&mut s);
    s.run("proof.set", &json!({ "languages": ["fr-CA"] })).expect("new defaults");
    let newer = open(&mut s);

    let kept = s.run("proof.check", &json!({ "compose": older, "text": "le courriel" })).expect("a check");
    assert_eq!(flagged(&kept), ["le", "courriel"], "the message opened first stays English: {kept}");

    let fresh = s.run("proof.check", &json!({ "compose": newer, "text": "le courriel" })).expect("a check");
    assert!(flagged(&fresh).is_empty(), "the message opened after the change is French: {:?}", flagged(&fresh));
}

/// A locally saved draft keeps the languages it was written with: saved, closed, the defaults
/// changed, and opened again from Drafts, it is still read in the language chosen for it — and the
/// choice is in the local draft alone, never in what would be sent.
#[test]
fn a_saved_draft_keeps_its_own_languages() {
    let (mut s, _fixture) = session("saved");
    let draft = open(&mut s);
    s.run("compose.set", &json!({ "compose": draft, "proofLanguages": ["fr-CA"] })).expect("the draft's French");
    let saved = s.run("compose.saveDraft", &json!({ "compose": draft })).expect("saved")["draft"].as_u64().expect("a draft id");
    s.run("compose.close", &json!({ "compose": draft })).expect("closed");
    s.run("proof.set", &json!({ "languages": ["en-US"] })).expect("other defaults");

    let again = s.run("mail.editDraft", &json!({ "id": saved })).expect("opened again")["compose"].as_u64().expect("an id");
    let check = s.run("proof.check", &json!({ "compose": again, "text": "le courriel" })).expect("a check");
    assert!(flagged(&check).is_empty(), "the reopened draft is still French: {:?}", flagged(&check));
}

/// A language list that is not one is refused before anything changes: not a list, a code that is
/// no language, one that is not a string, an empty list, or a list longer than any choice could be.
/// Duplicates are folded the way the global `proof.set` folds them.
#[test]
fn a_language_list_that_cannot_be_one_changes_nothing() {
    let (mut s, _fixture) = session("validation");
    let draft = open(&mut s);
    s.run("compose.set", &json!({ "compose": draft, "proofLanguages": ["fr-CA"] })).expect("French");

    for bad in [
        json!({ "proofLanguages": "fr-CA" }),
        json!({ "proofLanguages": ["xx-XX"] }),
        json!({ "proofLanguages": [2] }),
        json!({ "proofLanguages": [] }),
        json!({ "proofLanguages": null }),
        json!({ "proofLanguages": vec!["en-CA"; 64] }),
    ] {
        let mut params = bad.as_object().cloned().unwrap_or_default();
        params.insert("compose".into(), json!(draft));
        params.insert("subject".into(), json!("changed with it"));
        let refused = s.run("compose.set", &Value::Object(params));
        assert!(refused.is_err(), "{bad} is refused");
        assert_eq!(s.composing_by_id(draft).map(|c| c.subject.as_str()), Some(""), "nothing else changed: {bad}");
    }

    let check = s.run("proof.check", &json!({ "compose": draft, "text": "le courriel" })).expect("a check");
    assert!(flagged(&check).is_empty(), "the refused lists left French in place: {:?}", flagged(&check));

    s.run("compose.set", &json!({ "compose": draft, "proofLanguages": ["fr-CA", "fr-CA", "en-CA"] })).expect("both, once each");
    let both = s.run("proof.check", &json!({ "compose": draft, "text": "le courriel colour" })).expect("a check");
    assert!(flagged(&both).is_empty(), "duplicates fold, and both languages are on: {:?}", flagged(&both));
}

/// Everything that reads the dictionaries follows the draft in front: suggestions come from its
/// languages, and Review › Spelling & Grammar walks what they find — not the global defaults.
#[test]
fn suggestions_and_the_review_follow_the_draft() {
    let (mut s, _fixture) = session("effective");
    let french = open(&mut s);
    s.run("compose.set", &json!({ "compose": french, "proofLanguages": ["fr-CA"] })).expect("French");
    s.run("compose.type", &json!({ "compose": french, "text": "le courriel colour" })).expect("its text");
    // The signature a new message starts with is taken out again, so what is counted is the text.
    let typed = "le courriel colour".chars().count();
    let len = s.doc().map(|(b, _)| b.text().len()).unwrap_or(0);
    if len > typed {
        s.run("compose.delete", &json!({ "compose": french, "from": typed, "to": len })).expect("the signature taken out");
    }

    let said = s.run("proof.suggest", &json!({ "compose": french, "word": "courriell", "limit": 8 })).expect("suggestions");
    let offered: Vec<&str> = said["suggestions"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    assert!(offered.contains(&"courriel"), "the French dictionary answers: {offered:?}");

    let walk = s.run("review.spelling", &json!({ "compose": french, "again": true })).expect("the walk");
    assert_eq!(walk["total"], json!(1), "only the English word is a mistake in French: {walk}");
    assert_eq!(walk["at"]["word"], json!("colour"), "the place is the English word: {walk}");
}

/// A signature being edited is proofread with the global defaults: it has no languages of its own,
/// and an open message's choice is no part of it.
#[test]
fn a_signature_is_proofread_with_the_global_defaults() {
    let (mut s, _fixture) = session("signature");
    let draft = open(&mut s);
    s.run("compose.set", &json!({ "compose": draft, "proofLanguages": ["fr-CA"] })).expect("the draft's French");
    s.run("signature.open", &json!({ "name": "Standard" })).expect("the signature");

    let check = s.run("proof.check", &json!({ "text": "courriel" })).expect("a check");
    assert_eq!(flagged(&check), ["courriel"], "the global English defaults check a signature: {check}");

    s.run("signature.close", &json!({ "discard": true })).expect("closed");
}

/// When the global dictionaries or their folder change, what was read and said about them no
/// longer describes the settings: it is cleared at once, without reading anything on the frame that
/// saved them, and the next status reads the new ones.
#[test]
fn a_global_dictionary_change_clears_the_old_report_without_reading() {
    let (mut s, fixture) = session("cache");
    let status = s.run("proof.status", &json!({})).expect("status");
    assert_eq!(status["read"], json!(["en-CA"]), "English was read");

    s.run("proof.set", &json!({ "languages": ["fr-CA"] })).expect("other languages");
    assert!(s.proof.read.is_empty(), "the old languages are no longer reported: {:?}", s.proof.read);
    assert!(s.proof.note.is_none(), "nor the old note: {:?}", s.proof.note);
    assert_eq!(status["settings"]["languages"], json!(["en-CA"]), "the earlier answer is what it was");

    let after = s.run("proof.status", &json!({})).expect("status again");
    assert_eq!(after["read"], json!(["fr-CA"]), "the next status reads French: {after}");
    assert_eq!(after["settings"]["dictionaryDir"], json!(fixture.path().to_string_lossy().to_string()));
}

#[test]
fn a_language_change_refuses_a_correction_from_the_previous_language_walk() {
    let (mut s, _fixture) = session("stale-language-walk");
    let draft = open(&mut s);
    s.run("compose.set", &json!({"compose": draft, "body": "courriel"})).unwrap();
    let walk = s.run("review.spelling", &json!({"compose": draft, "again": true})).unwrap();
    assert_eq!(walk["at"]["word"], json!("courriel"));
    s.run("compose.set", &json!({"compose": draft, "proofLanguages": ["fr-CA"]})).unwrap();
    let before = s.composing_by_id(draft).unwrap().body.plain();
    let changed = s.run("proof.change", &json!({"compose": draft, "text": "colour"}));
    assert!(changed.is_err(), "the old English walk must not correct a now-valid French word: {changed:?}");
    assert_eq!(s.composing_by_id(draft).unwrap().body.plain(), before);
}

#[test]
fn opening_untrusted_draft_metadata_bounds_languages_before_deduplicating() {
    use mailcraft_engine::model::proof::Language;
    let (mut s, _fixture) = session("bounded-saved-language-metadata");
    let draft = open(&mut s);
    let saved = s.run("compose.saveDraft", &json!({"compose": draft})).unwrap()["draft"].as_u64().unwrap();
    s.run("compose.close", &json!({"compose": draft})).unwrap();
    // Saved files are hostile too. Rejected duplicates must count towards the traversal budget;
    // a different valid language beyond the command's 32-entry limit must not sneak into it.
    let mut hostile = vec![Language::EnCa; 32];
    hostile.push(Language::FrCa);
    s.mailbox.message_mut(saved).unwrap().proof_languages = Some(hostile);
    let reopened = s.run("mail.editDraft", &json!({"id": saved})).unwrap()["compose"].as_u64().unwrap();
    assert_eq!(s.composing_by_id(reopened).unwrap().proof_languages, Some(vec![Language::EnCa]));
    assert_eq!(s.proof_settings_for(reopened).languages, vec![Language::EnCa]);
}
