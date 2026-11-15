// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Spelling and grammar while writing (#954), through the commands: what is found, what a
//! correction does to the message and its undo, and what Review › Spelling & Grammar walks
//! through.
//!
//! The dictionaries here are invented ones written into a folder of this test's own, so that what
//! a test says is a word never depends on what this computer has installed.

use mailcraft_model::proof::{Kind, Language};
use serde_json::json;
use tempdir::Dir;

use crate::Session;
use crate::sample;

/// The instant these tests are at: the sample's mailbox, as its commands see it.
fn now() -> mailcraft_time::Timestamp {
    1_750_000_000
}

/// Two dictionaries of a few words each, in a folder of their own, and a session told to use it.
fn session(name: &str) -> (Session, Dir) {
    let dir = Dir::new(name);
    dir.write("en_CA.dic", "6\ncolour\ncentre\ndocument\ndocuments\nthe\ntomorrow\n");
    dir.write("fr_CA.dic", "5\nbonjour\nfrançais\naujourd'hui\nMontréal\ndocument\n");
    let mut s = Session::at(sample::mailbox(now(), 0), now(), 0);
    s.prefs.proof.dictionary_dir = dir.path().to_string_lossy().to_string();
    (s, dir)
}

/// A message being written with `text` in it, and nothing else: the signature a new message is
/// given is taken out again, so that what a test counts is what it wrote.
fn composing(s: &mut Session, text: &str) {
    s.run("mail.new", &json!({})).expect("a message");
    s.run("compose.type", &json!({ "text": text })).expect("the text");
    let len = s.doc().map(|(b, _)| b.text().len()).unwrap_or(0);
    let end = text.chars().count();
    if len > end {
        s.run("compose.delete", &json!({ "from": end, "to": len })).expect("the signature taken out");
    }
}

/// The words of `sentence` the answer says are wrong, in order (the body also holds the
/// signature a new message is given, whose words are nobody's business here).
fn flagged(v: &serde_json::Value) -> Vec<String> {
    let in_it = |w: &String, whole: &str| whole.contains(w.as_str());
    v.get("diagnostics")
        .and_then(|d| d.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|d| {
                    let span = d.get("span")?;
                    span.get("start").and_then(|x| x.as_u64()).zip(span.get("end").and_then(|x| x.as_u64()))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(start, end)| v.get("text").and_then(|t| t.as_str()).and_then(|t| t.get(start as usize..end as usize)).map(str::to_string))
        .filter(|w| in_it(w, v.get("text").and_then(|t| t.as_str()).unwrap_or_default()))
        .collect()
}

#[test]
fn a_word_of_either_language_is_right() {
    let (mut s, _dir) = session("either");
    composing(&mut s, "Bonjour Pierre, the colour of the centre is right.");
    let answer = s.run("proof.check", &json!({})).expect("a check");
    let wrong = flagged(&answer);
    assert!(!wrong.contains(&"colour".to_string()), "a Canadian dictionary knows it: {wrong:?}");
    assert!(!wrong.contains(&"centre".to_string()));
    assert!(!wrong.contains(&"bonjour".to_string()), "the French dictionary knows it: {wrong:?}");
    assert!(wrong.contains(&"Pierre".to_string()), "no dictionary here knows the name: {wrong:?}");
    assert_eq!(answer.get("loaded").and_then(|v| v.as_bool()), Some(true));
}

#[test]
fn the_two_languages_are_both_read() {
    let (mut s, _dir) = session("both");
    let answer = s.run("proof.status", &json!({})).expect("the settings");
    let read = answer.get("read").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    assert!(read.len() >= 2, "both dictionaries were read: {read:?}");
    assert_eq!(answer.get("loaded").and_then(|v| v.as_bool()), Some(true));
}

#[test]
fn no_dictionary_means_no_places_rather_than_every_word() {
    let mut s = Session::at(sample::mailbox(now(), 0), now(), 0);
    s.prefs.proof.dictionary_dir = "/nonexistent/folder/of/dictionaries".into();
    composing(&mut s, "Bonjour, this is a message with words in it.");
    let answer = s.run("proof.check", &json!({})).expect("a check");
    assert!(flagged(&answer).is_empty(), "nothing is flagged when there is nothing to check against");
    assert_eq!(answer.get("loaded").and_then(|v| v.as_bool()), Some(false));
    assert!(answer.get("note").and_then(|n| n.as_str()).is_some(), "and it says why");
}

#[test]
fn a_word_the_owner_added_is_a_word() {
    let (mut s, _dir) = session("personal");
    composing(&mut s, "Hochelaga is where the mountain is.");
    assert!(flagged(&s.run("proof.check", &json!({})).expect("a check")).contains(&"Hochelaga".to_string()));
    let answer = s.run("proof.addWord", &json!({ "word": "Hochelaga" })).expect("a word added");
    assert_eq!(answer.get("added").and_then(|v| v.as_bool()), Some(true));
    assert!(!flagged(&s.run("proof.check", &json!({})).expect("a check")).contains(&"Hochelaga".to_string()), "it is a word now");
    let again = s.run("proof.addWord", &json!({ "word": "hochelaga" })).expect("the same word again");
    assert_eq!(again.get("added").and_then(|v| v.as_bool()), Some(false), "but only once");
    let removed = s.run("proof.removeWord", &json!({ "word": "HOCHELAGA" })).expect("taken out");
    assert_eq!(removed.get("removed").and_then(|v| v.as_bool()), Some(true));
    assert!(flagged(&s.run("proof.check", &json!({})).expect("a check")).contains(&"Hochelaga".to_string()));
}

#[test]
fn the_personal_dictionary_is_kept_in_the_settings() {
    let (mut s, _dir) = session("kept");
    s.run("proof.addWord", &json!({ "word": "Hochelaga" })).expect("a word added");
    let text = serde_json::to_string(&s.prefs).expect("settings");
    let back: crate::Prefs = serde_json::from_str(&text).expect("the same settings");
    assert_eq!(back.proof.personal_words(), ["Hochelaga"]);
}

#[test]
fn suggestions_are_words_and_a_correction_is_one_step_of_undo() {
    let (mut s, _dir) = session("change");
    composing(&mut s, "the documet is here");
    let answer = s.run("proof.check", &json!({})).expect("a check");
    let places = flagged(&answer);
    assert_eq!(places, ["documet", "is", "here"], "the test dictionary knows only its own words");
    let suggest = s.run("proof.suggest", &json!({ "word": "documet" })).expect("suggestions");
    let list = suggest.get("suggestions").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    assert!(list.iter().any(|s| s == "document"), "{list:?}");

    // The place, in characters of the body: `documet` is the second word.
    let start = 4;
    let end = start + "documet".chars().count();
    let snapshot = s.doc().unwrap().0.plain();
    s.run("proof.change", &json!({ "text": "document", "from": start, "to": end, "expectedText": snapshot, "composeId": s.compose_id() }))
        .expect("changed");
    let body = s.doc().map(|(b, _)| b.plain()).unwrap_or_default();
    assert_eq!(body.trim_end(), "the document is here");
    // One step back, and the message is as it was: a correction is one step of its undo.
    s.run("compose.undo", &json!({})).expect("undone");
    let body = s.doc().map(|(b, _)| b.plain()).unwrap_or_default();
    assert_eq!(body.trim_end(), "the documet is here");
    assert!(s.run("compose.undo", &json!({})).is_ok(), "and the typing before it is another step");
}

#[test]
fn a_correction_keeps_the_look_of_the_word() {
    let (mut s, _dir) = session("look");
    composing(&mut s, "the documet is here");
    s.run("compose.select", &json!({ "from": 4, "to": 11 })).expect("selected");
    s.run("format.bold", &json!({})).expect("bold");
    // A right click's correction names the place it was made at.
    let snapshot = s.doc().unwrap().0.plain();
    s.run("proof.change", &json!({ "text": "document", "from": 4, "to": 11, "expectedText": snapshot, "composeId": s.compose_id() }))
        .expect("changed");
    let (body, _) = s.doc().expect("a message");
    let bold: String = body.text().paragraphs().iter().flat_map(|p| p.runs.iter()).filter(|r| r.style.bold).map(|r| r.text.clone()).collect();
    assert_eq!(bold, "document", "what is put in is bold as what it replaced was, and nothing else is");
}

#[test]
fn review_walks_the_places_and_changes_one() {
    let (mut s, _dir) = session("walk");
    composing(&mut s, "the documet and the sentance");
    let opened = s.run("review.spelling", &json!({ "again": true })).expect("the walk");
    assert!(opened.get("total").and_then(|v| v.as_u64()).unwrap_or(0) >= 2, "the places of the message and of its signature");
    let at = opened.get("at").cloned().unwrap_or_default();
    assert_eq!(at.get("word").and_then(|v| v.as_str()), Some("documet"));
    s.run("proof.change", &json!({ "text": "document" })).expect("changed");
    let body = s.doc().map(|(b, _)| b.plain()).unwrap_or_default();
    assert!(body.starts_with("the document and the sentance"), "{body:?}");
    // The walk goes on with what is left, and ends.
    let rest = s.run("review.spelling", &json!({})).expect("the walk again");
    let next = rest.get("at").and_then(|a| a.get("word")).and_then(|v| v.as_str()).unwrap_or_default().to_string();
    assert_ne!(next, "documet", "the corrected word is not looked at again");
    assert!(["is", "here", "and", "the", "sentance"].contains(&next.as_str()), "{next}");
    // Whatever is left is left alone, one at a time, until the walk is over.
    let mut done = false;
    for _ in 0..50 {
        let answer = s.run("proof.ignore", &json!({})).expect("left alone");
        if answer.get("done").and_then(|v| v.as_bool()) == Some(true) {
            done = true;
            break;
        }
    }
    assert!(done, "the walk ends");
}

#[test]
fn a_walk_of_a_message_that_changed_is_not_applied_to_it() {
    let (mut s, _dir) = session("stale");
    composing(&mut s, "the documet is here");
    s.run("review.spelling", &json!({ "again": true })).expect("the walk");
    // The owner types somewhere else: the place no longer belongs to this text.
    s.run("compose.type", &json!({ "text": "more " })).expect("typed");
    assert!(s.run("proof.change", &json!({ "text": "document" })).is_err(), "a stale place is refused");
    let body = s.doc().map(|(b, _)| b.plain()).unwrap_or_default();
    assert!(body.contains("the documet is here"), "the text is as it was: {body:?}");
    assert!(body.contains("heremore"), "and the typing was all that happened: {body:?}");
}

#[test]
fn settings_are_changed_and_read_back() {
    let (mut s, _dir) = session("set");
    let answer = s
        .run("proof.set", &json!({ "checkSpelling": false, "checkGrammar": true, "languages": ["fr-CA"], "grammarUrl": "http://127.0.0.1:8081" }))
        .expect("set");
    let settings = answer.get("settings").cloned().unwrap_or_default();
    assert_eq!(settings.get("checkSpelling").and_then(|v| v.as_bool()), Some(false));
    assert_eq!(settings.get("checkGrammar").and_then(|v| v.as_bool()), Some(true));
    assert_eq!(settings.get("languages"), Some(&json!(["fr-CA"])));
    // Grammar is local-only; legacy settings never grant remote transmission.
    assert!(s.run("proof.set", &json!({ "grammarUrl": "https://languagetool.example" })).is_err());
    assert!(s.run("proof.set", &json!({ "allowRemote": true, "grammarUrl": "https://languagetool.example" })).is_err());
    assert_eq!(s.prefs.proof.grammar_url, "http://127.0.0.1:8081");
    assert!(s.run("proof.set", &json!({ "grammarUrl": "file:///etc/passwd" })).is_err(), "and only http(s) addresses");
}

#[test]
fn grammar_places_from_the_host_are_taken_only_where_they_fit() {
    let (mut s, _dir) = session("grammar");
    composing(&mut s, "She have sent the documents.");
    let walk = s
        .run(
            "review.spelling",
            &json!({
                "again": true,
                "grammar": [
                    { "start": 0, "end": 8, "kind": "grammar", "message": "The verb does not agree.", "suggestions": ["She has"], "rule": "HE_VERB_AGR" },
                    { "start": 20, "end": 900, "kind": "grammar", "message": "not this text's" },
                    { "start": 7, "end": 7, "kind": "grammar", "message": "empty" }
                ]
            }),
        )
        .expect("the walk");
    let rules = walk.get("rules").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    assert_eq!(rules, vec![json!("HE_VERB_AGR")], "the two that do not fit are left out");
    assert!(walk.get("at").and_then(|a| a.get("word")).is_some(), "there is something to look at");
}

#[test]
fn a_quiet_rule_is_left_out() {
    let (mut s, _dir) = session("quiet");
    composing(&mut s, "She have sent the documents.");
    s.run("proof.ignoreRule", &json!({ "rule": "HE_VERB_AGR" })).expect("quiet");
    let walk = s
        .run("review.spelling", &json!({ "again": true, "grammar": [{ "start": 0, "end": 8, "kind": "grammar", "rule": "HE_VERB_AGR" }] }))
        .expect("the walk");
    assert!(walk.get("rules").and_then(|v| v.as_array()).is_none_or(|r| r.is_empty()), "the rule is the owner's to leave quiet");
    s.run("proof.forgetRules", &json!({})).expect("forgotten");
    assert!(s.prefs.proof.ignored_rules.is_empty());
}

#[test]
fn a_language_can_be_turned_off() {
    let (mut s, _dir) = session("off");
    // A draft snapshots the defaults when it opens; set them before opening this one.
    // Changing the defaults later must not change a draft that is already open.
    s.run("proof.set", &json!({ "languages": ["en-CA"] })).expect("English alone");
    composing(&mut s, "Bonjour le monde");
    let wrong = flagged(&s.run("proof.check", &json!({})).expect("a check"));
    assert!(wrong.contains(&"Bonjour".to_string()), "French is off, so a French word is not a word: {wrong:?}");
    assert_eq!(s.prefs.proof.languages, vec![Language::EnCa]);
}

#[test]
fn a_place_is_never_outside_the_text() {
    let (mut s, _dir) = session("fits");
    for text in ["Bonjour 🙂 Montréal", "a\u{300}bc documet", "", "   ", "\u{fffc}"] {
        composing(&mut s, text);
        let answer = s.run("proof.check", &json!({})).expect("a check");
        let span_text = answer.get("text").and_then(|t| t.as_str()).unwrap_or_default().to_string();
        for d in answer.get("diagnostics").and_then(|d| d.as_array()).cloned().unwrap_or_default() {
            let span = d.get("span").cloned().unwrap_or_default();
            let (Some(start), Some(end)) = (span.get("start").and_then(|v| v.as_u64()), span.get("end").and_then(|v| v.as_u64())) else {
                panic!("a place with no ends: {d:?}");
            };
            let (start, end) = (start as usize, end as usize);
            assert!(end > start && end <= span_text.len(), "{d:?} does not fit");
            assert!(span_text.is_char_boundary(start) && span_text.is_char_boundary(end), "{d:?} splits a character");
        }
        let _ = s.run("compose.discard", &json!({}));
    }
}

#[test]
fn the_kind_of_a_place_says_what_found_it() {
    let (mut s, _dir) = session("kind");
    composing(&mut s, "the documet");
    let answer = s.run("proof.check", &json!({})).expect("a check");
    let d = answer.get("diagnostics").and_then(|d| d.as_array()).and_then(|a| a.first()).cloned().unwrap_or_default();
    assert_eq!(d.get("kind").and_then(|v| v.as_str()), Some("spelling"));
    assert_eq!(d.get("source").and_then(|v| v.as_str()), Some("local"));
    assert_eq!(d.get("kind").and_then(|v| v.as_str()), Some("spelling"));
    assert_eq!(Kind::Spelling, Kind::Spelling);
}

#[test]
fn an_inline_correction_refuses_the_snapshot_of_an_edited_body() {
    let (mut s, _dir) = session("inline-stale");
    composing(&mut s, "the documet is here");
    let expected = s.doc().unwrap().0.plain();
    let compose_id = s.compose_id().unwrap();
    s.run("compose.select", &json!({"from": 0})).unwrap();
    s.run("compose.type", &json!({"text": "new "})).unwrap();
    let before = s.doc().unwrap().0.plain();
    assert!(
        s.run(
            "proof.change",
            &json!({
                "from": 4, "to": 11, "text": "document", "expectedText": expected,
                "composeId": compose_id
            })
        )
        .is_err()
    );
    assert_eq!(s.doc().unwrap().0.plain(), before);
}

#[test]
fn an_inline_correction_cannot_move_to_an_identical_new_draft() {
    let (mut s, _dir) = session("inline-owner");
    composing(&mut s, "the documet is here");
    let expected = s.doc().unwrap().0.plain();
    let compose_id = s.compose_id().unwrap();
    composing(&mut s, &expected);
    assert_ne!(s.compose_id(), Some(compose_id));
    assert!(
        s.run(
            "proof.change",
            &json!({
                "from": 4, "to": 11, "text": "document", "expectedText": expected,
                "composeId": compose_id
            })
        )
        .is_err()
    );
    assert_eq!(s.doc().unwrap().0.plain(), expected);
}

#[test]
fn a_review_walk_is_owned_by_one_draft_even_when_text_is_identical() {
    let (mut s, _dir) = session("walk-owner");
    composing(&mut s, "the documet is here");
    s.run("review.spelling", &json!({"again": true})).unwrap();
    composing(&mut s, "the documet is here");
    let before = s.doc().unwrap().0.plain();
    assert!(s.run("proof.change", &json!({"text": "document"})).is_err());
    assert_eq!(s.doc().unwrap().0.plain(), before);
}

#[test]
fn a_personal_word_without_installed_dictionaries_does_not_flag_everything_else() {
    let dir = Dir::new("personal-without-dictionaries");
    let mut s = Session::at(sample::mailbox(now(), 0), now(), 0);
    s.prefs.proof.dictionary_dir = dir.path().to_string_lossy().to_string();
    composing(&mut s, "hello world Hochelaga");
    s.run("proof.addWord", &json!({"word": "Hochelaga"})).unwrap();
    let answer = s.run("proof.check", &json!({})).unwrap();
    assert!(answer["diagnostics"].as_array().unwrap().is_empty());
    assert_eq!(answer["loaded"], false, "personal entries are not a loaded language dictionary");
}

#[test]
fn a_named_proofreading_correction_requires_the_checked_snapshot() {
    let (mut s, _dir) = session("named-correction-needs-snapshot");
    composing(&mut s, "the documet");
    let before = s.doc().unwrap().0.plain();
    assert!(s.run("proof.change", &json!({"from": 4, "to": 11, "text": "document"})).is_err());
    assert_eq!(s.doc().unwrap().0.plain(), before);
    assert!(
        s.run("proof.change", &json!({"from": 4, "to": 11, "text": "document", "expectedText": before})).is_err(),
        "a snapshot alone cannot identify an identical draft"
    );
    s.run("proof.change", &json!({"from": 4, "to": 11, "text": "document", "expectedText": before, "composeId": s.compose_id()})).unwrap();
    assert_eq!(s.doc().unwrap().0.plain(), "the document");
}

#[test]
fn check_uses_selected_characters_and_returns_relative_byte_offsets() {
    let (mut s, _dir) = session("check-selection");
    composing(&mut s, "Montréal documet bonjor");
    s.run("compose.select", &json!({"from": 9, "to": 16})).unwrap();
    let answer = s.run("proof.check", &json!({})).unwrap();
    assert_eq!(answer["text"], "documet");
    assert_eq!(answer["from"], 9);
    assert_eq!(answer["to"], 16);
    assert_eq!(answer["diagnostics"][0]["span"], json!({"start": 0, "end": 7}));
    let explicit = s.run("proof.check", &json!({"text": "é bonjor", "from": 2, "to": 8})).unwrap();
    assert_eq!(explicit["text"], "bonjor");
    assert_eq!(explicit["diagnostics"][0]["span"], json!({"start": 0, "end": 6}));
    assert!(s.run("proof.check", &json!({"text": false})).is_err());
    assert!(s.run("proof.check", &json!({"from": -1})).is_err());
    assert!(s.run("proof.check", &json!({"from": 9000})).is_err());
}

#[test]
fn a_named_correction_does_not_load_or_recheck_dictionaries() {
    let (mut s, _dir) = session("inline-no-recheck");
    composing(&mut s, "the documet");
    let snapshot = s.doc().unwrap().0.plain();
    assert!(s.proof.read.is_empty());
    s.run("proof.change", &json!({"from": 4, "to": 11, "text": "document", "expectedText": snapshot, "composeId": s.compose_id()})).unwrap();
    assert!(s.proof.read.is_empty(), "inline corrections must not expand dictionaries during a UI frame");
    assert!(s.proof.walk.is_none(), "the async providers will supply fresh diagnostics");
}

#[test]
fn corrupt_preferred_dictionary_does_not_prevent_a_valid_alternative() {
    let dir = Dir::new("dictionary-fallback");
    dir.write("en_CA.dic", "0\n");
    dir.write("en-CA.dic", "1\ncolour\n");
    let mut s = Session::at(sample::mailbox(now(), 0), now(), 0);
    s.prefs.proof.dictionary_dir = dir.path().to_string_lossy().to_string();
    composing(&mut s, "colour colur");
    let answer = s.run("proof.check", &json!({})).unwrap();
    assert_eq!(answer["loaded"], true);
    assert_eq!(flagged(&answer), ["colur"]);
}

#[test]
fn review_correction_invalidates_grammar_instead_of_shifting_it() {
    let (mut s, _dir) = session("review-grammar-recheck");
    composing(&mut s, "the documet tomorrow");
    s.run("review.spelling", &json!({"again": true, "grammar": [{"start": 12, "end": 20, "kind": "grammar", "rule": "TEST_AGREEMENT"}]})).unwrap();
    let answer = s.run("proof.change", &json!({"text": "document"})).unwrap();
    assert_eq!(answer["grammarPending"], true, "the host must obtain fresh grammar after any semantic edit");
    assert!(answer["rules"].as_array().unwrap().is_empty());
}

#[test]
fn a_host_review_uses_guarded_spelling_results_without_loading_dictionaries() {
    let (mut s, _dir) = session("host-review");
    composing(&mut s, "the documet");
    let snapshot = s.doc().unwrap().0.plain();
    let places = json!([{"start": 4, "end": 11, "kind": "spelling", "suggestions": ["document"]}]);
    assert!(s.run("review.spelling", &json!({"again": true, "spelling": places})).is_err());
    let walk = s.run("review.spelling", &json!({"again": true, "spelling": places, "expectedText": snapshot, "composeId": s.compose_id()})).unwrap();
    assert_eq!(walk["at"]["word"], "documet");
    assert_eq!(walk["at"]["suggestions"], json!(["document"]));
    assert!(s.proof.read.is_empty(), "a UI frame must use the host's prepared results");
}

#[test]
fn editing_proofing_settings_does_not_load_dictionaries_on_the_ui_thread() {
    let (mut s, dir) = session("settings-no-io");
    let folder = dir.path().to_string_lossy().to_string();
    s.run("proof.set", &json!({"dictionaryDir": folder})).unwrap();
    assert!(s.proof.read.is_empty());
    s.run("proof.addWord", &json!({"word": "Hochelaga"})).unwrap();
    s.run("proof.removeWord", &json!({"word": "Hochelaga"})).unwrap();
    s.run("proof.ignoreRule", &json!({"rule": "TEST_RULE"})).unwrap();
    assert!(s.proof.read.is_empty(), "settings commands update metadata without loading dictionaries");
    let status = s.run("proof.status", &json!({})).unwrap();
    assert_eq!(status["loaded"], true, "explicit synchronous status remains available to headless clients");
}

#[test]
fn proofing_commands_preserve_agent_settings_and_draft_boundaries() {
    use crate::agent::{Access, Class, Verdict};
    let (mut s, _dir) = session("proof-agent-boundaries");
    for command in ["proof.set", "proof.addWord", "proof.removeWord", "proof.ignoreRule", "proof.forgetRules"] {
        assert_eq!(crate::agent::access(command), Some(Access::Owner(Class::Settings)), "{command}");
        assert!(matches!(crate::agent::check(&s, command, &json!({"word": "Hochelaga"}), &[]), Verdict::Ask(Class::Settings, _)), "{command}");
    }
    for command in ["proof.check", "proof.suggest", "proof.status", "review.spelling", "proof.change", "proof.ignore", "proof.close"] {
        assert_eq!(crate::agent::access(command), Some(Access::Safe), "{command}");
    }
    assert_eq!(crate::agent::classify(&s, "proof.ignore", &json!({"rule": "TEST_RULE"})), Access::Owner(Class::Settings));
    composing(&mut s, "the documet");
    let snapshot = s.doc().unwrap().0.plain();
    let params = json!({"from": 4, "to": 11, "text": "document", "expectedText": snapshot, "composeId": s.compose_id()});
    s.agent = Some(Vec::new());
    let result = s.run("proof.change", &params);
    s.agent = None;
    assert!(result.is_err(), "an agent cannot silently correct the owner's draft");
    assert_eq!(s.doc().unwrap().0.plain(), snapshot);
}

#[test]
fn proofing_a_signature_requires_settings_permission_before_changing_its_text() {
    use crate::agent::{Access, Class, Verdict};
    let (mut s, _dir) = session("proof-signature-agent-boundaries");
    s.run("signature.set", &json!({"name": "Proofing test", "text": "the documet"})).unwrap();
    s.run("signature.open", &json!({"name": "Proofing test"})).unwrap();
    let id = s.signature_id().expect("the open signature editor");
    let snapshot = s.doc().unwrap().0.plain();
    let params = json!({"from": 4, "to": 11, "text": "document", "expectedText": snapshot, "signature": true, "signatureId": id});
    assert_eq!(crate::agent::classify(&s, "proof.change", &params), Access::Owner(Class::Settings));
    assert!(matches!(crate::agent::check(&s, "proof.change", &params, &[]), Verdict::Ask(Class::Settings, _)));
    s.agent = Some(Vec::new());
    let denied = s.run("proof.change", &params);
    s.agent = None;
    assert!(denied.is_err());
    assert_eq!(s.doc().unwrap().0.plain(), snapshot);
    assert!(matches!(crate::agent::check(&s, "proof.change", &params, &[Class::Settings]), Verdict::Run));
    // Even with the permission, another editor's id is not this one's.
    let elsewhere = params
        .clone()
        .as_object_mut()
        .map(|o| {
            o.insert("signatureId".into(), json!(id.saturating_add(1)));
            serde_json::Value::Object(o.clone())
        })
        .unwrap_or_default();
    s.agent = Some(vec![Class::Settings]);
    let wrong_editor = s.run("proof.change", &elsewhere);
    s.agent = None;
    assert!(wrong_editor.is_err(), "a correction of another signature editor is refused");
    assert_eq!(s.doc().unwrap().0.plain(), snapshot, "and nothing changed");
    s.agent = Some(vec![Class::Settings]);
    let allowed = s.run("proof.change", &params);
    s.agent = None;
    allowed.unwrap();
    assert_eq!(s.doc().unwrap().0.plain(), "the document");
}

/// A signature of `text` in the mailbox, opened as the editor the commands work on.
fn editing_signature(s: &mut Session, name: &str, text: &str) {
    s.run("signature.set", &json!({ "name": name, "text": text })).expect("a signature");
    s.run("signature.open", &json!({ "name": name })).expect("the signature opened");
}

/// A named correction of the open signature's `documet` (characters 4..11), carrying the editor
/// instance it was checked in.
fn signature_correction(s: &Session, id: u64) -> serde_json::Value {
    let snapshot = s.doc().map(|(b, _)| b.plain()).unwrap_or_default();
    json!({
        "from": 4, "to": 11, "text": "document", "expectedText": snapshot,
        "signature": true, "signatureId": id
    })
}

#[test]
fn two_signatures_of_the_same_words_are_still_two_editors() {
    let (mut s, _dir) = session("signature-identical-text");
    editing_signature(&mut s, "Standard", "the documet");
    let first = s.signature_id().expect("the open signature editor");
    assert!(first > 0, "an identity is never 0");
    s.run("signature.close", &json!({ "discard": true })).expect("closed");
    editing_signature(&mut s, "Second", "the documet");
    let second = s.signature_id().expect("the open signature editor");
    assert_eq!(s.doc().map(|(b, _)| b.plain()), Some("the documet".to_string()), "the same words");
    assert_ne!(second, first, "the same words are not the same editor");
    assert!(second > first, "identities are given in order: {first} then {second}");

    // The first editor's correction, its complete text unchanged, is refused on the second.
    assert!(s.run("proof.change", &signature_correction(&s, first)).is_err(), "another editor's correction");
    assert_eq!(s.doc().unwrap().0.plain(), "the documet", "nothing changed");
    // The editor it belongs to takes it.
    s.run("proof.change", &signature_correction(&s, second)).expect("changed");
    assert_eq!(s.doc().unwrap().0.plain(), "the document");
}

#[test]
fn a_signature_keeps_its_identity_while_it_is_open_and_saved_in_place() {
    let (mut s, _dir) = session("signature-identity-kept");
    editing_signature(&mut s, "Standard", "the documet");
    let id = s.signature_id().expect("the open signature editor");
    s.run("signature.save", &json!({})).expect("saved");
    assert_eq!(s.signature_id(), Some(id), "saving the same editor in place is not another one");
    s.run("signature.set", &json!({ "name": "Standard", "text": "the documet to" })).expect("replaced");
    assert_eq!(s.signature_id(), Some(id), "the open editor is the one that was changed");
    assert_eq!(s.doc().unwrap().0.plain(), "the documet to");
}

#[test]
fn closing_a_signature_and_opening_it_again_is_another_editor() {
    let (mut s, _dir) = session("signature-close-reopen");
    editing_signature(&mut s, "Standard", "the documet");
    let first = s.signature_id().expect("the open signature editor");
    let stale = signature_correction(&s, first);
    s.run("signature.close", &json!({ "discard": true })).expect("closed");
    s.run("signature.open", &json!({ "name": "Standard" })).expect("opened again");
    let second = s.signature_id().expect("the open signature editor");
    assert_ne!(second, first, "a reopened signature is a new editor instance");
    assert!(s.run("proof.change", &stale).is_err(), "the closed editor's correction is late");
    assert_eq!(s.doc().unwrap().0.plain(), "the documet");
    s.run("proof.change", &signature_correction(&s, second)).expect("changed");
    assert_eq!(s.doc().unwrap().0.plain(), "the document");
}

#[test]
fn a_walk_of_a_closed_signature_is_not_applied_to_its_successor() {
    let (mut s, _dir) = session("signature-walk-closed");
    editing_signature(&mut s, "Standard", "the documet");
    let walk = s.run("review.spelling", &json!({ "again": true })).expect("the walk");
    assert_eq!(walk["at"]["word"], "documet");
    s.run("signature.close", &json!({ "discard": true })).expect("closed");
    s.run("signature.open", &json!({ "name": "Standard" })).expect("opened again");
    // The walk was the closed editor's: it is not this one's, walk-based or named.
    assert!(s.run("proof.change", &json!({ "text": "document" })).is_err(), "the closed editor's walk is refused");
    assert_eq!(s.doc().unwrap().0.plain(), "the documet");
    let id = s.signature_id().expect("the open signature editor");
    let late = json!({ "from": 4, "to": 11, "text": "document", "expectedText": "the documet", "signature": true, "signatureId": id });
    assert!(s.run("proof.change", &late).is_ok(), "the current editor's own correction is taken");
    assert_eq!(s.doc().unwrap().0.plain(), "the document");
    // And its own walk is a new one.
    let now = s.run("review.spelling", &json!({ "again": true })).expect("a new walk");
    assert_eq!(now["text"], "the document");
}

#[test]
fn a_current_signature_correction_is_one_step_of_undo() {
    let (mut s, _dir) = session("signature-one-step-undo");
    editing_signature(&mut s, "Standard", "the documet");
    let id = s.signature_id().expect("the open signature editor");
    s.run("proof.change", &signature_correction(&s, id)).expect("changed");
    assert_eq!(s.doc().unwrap().0.plain(), "the document");
    s.run("compose.undo", &json!({})).expect("undone");
    assert_eq!(s.doc().unwrap().0.plain(), "the documet", "one step back is the mistake");
    s.run("compose.redo", &json!({})).expect("redone");
    assert_eq!(s.doc().unwrap().0.plain(), "the document", "one step forward is the correction");
    assert_eq!(s.signature_id(), Some(id), "undoing and redoing kept the same open editor");
}

#[test]
fn a_signature_correction_without_its_identity_is_refused() {
    let (mut s, _dir) = session("signature-needs-identity");
    editing_signature(&mut s, "Standard", "the documet");
    let snapshot = s.doc().unwrap().0.plain();
    for params in [
        json!({ "from": 4, "to": 11, "text": "document", "expectedText": snapshot }),
        json!({ "from": 4, "to": 11, "text": "document", "expectedText": snapshot, "signature": true }),
        json!({ "from": 4, "to": 11, "text": "document", "expectedText": snapshot, "signatureId": 1 }),
        json!({ "from": 4, "to": 11, "text": "document", "expectedText": snapshot, "signature": true, "composeId": s.compose_id() }),
    ] {
        assert!(s.run("proof.change", &params.clone()).is_err(), "refused: {params}");
        assert_eq!(s.doc().unwrap().0.plain(), snapshot, "and nothing changed: {params}");
    }
    // The complete checked text is still required, never the words alone.
    let id = s.signature_id().expect("the open signature editor");
    assert!(s.run("proof.change", &json!({ "from": 4, "to": 11, "text": "document", "signature": true, "signatureId": id })).is_err());
    assert_eq!(s.doc().unwrap().0.plain(), snapshot);
}

#[test]
fn the_managed_server_switch_is_strict_and_keeps_the_grammar_address() {
    let (mut s, _dir) = session("managed-server");
    let answer = s.run("proof.set", &json!({ "grammarUrl": "http://127.0.0.1:9091", "managedServer": true })).expect("set");
    assert_eq!(answer["settings"]["managedServer"], true);
    assert_eq!(s.prefs.proof.grammar_url, "http://127.0.0.1:9091", "the manual address is kept as it was");
    assert!(s.proof.read.is_empty(), "no dictionary is read by a setting");

    let before = serde_json::to_value(&s.prefs.proof).expect("settings");
    for wrong in [json!("true"), json!(1), json!([]), json!(null)] {
        let result = s.run("proof.set", &json!({ "managedServer": wrong, "checkSpelling": false }));
        assert!(result.is_err(), "`managedServer` is true or false, not {wrong}");
        assert_eq!(serde_json::to_value(&s.prefs.proof).expect("settings"), before, "nothing changed: {wrong}");
    }
    assert_eq!(s.prefs.proof.grammar_url, "http://127.0.0.1:9091");
    assert!(s.prefs.proof.check_spelling, "a refused value changes no other setting either");

    // Kept between runs, and turned off again without touching the address.
    let saved = serde_json::to_string(&s.prefs).expect("settings");
    let read: crate::Prefs = serde_json::from_str(&saved).expect("the same settings");
    assert!(read.proof.managed_server);
    s.run("proof.set", &json!({ "managedServer": false })).expect("off");
    assert!(!s.prefs.proof.managed_server);
    assert_eq!(s.prefs.proof.grammar_url, "http://127.0.0.1:9091");
    assert!(s.proof.read.is_empty(), "neither switch reads a dictionary");
    assert_eq!(s.run("proof.status", &json!({})).expect("status")["settings"]["managedServer"], false);
}

#[test]
fn proofing_settings_mutations_are_guarded_even_for_direct_agent_dispatch() {
    let (mut s, _dir) = session("proof-direct-agent-settings");
    composing(&mut s, "the documet");
    let cases = [
        ("proof.set", json!({"checkSpelling": false})),
        ("proof.addWord", json!({"word": "Hochelaga"})),
        ("proof.removeWord", json!({"word": "Hochelaga"})),
        ("proof.ignoreRule", json!({"rule": "TEST_RULE"})),
        ("proof.forgetRules", json!({})),
        ("proof.ignore", json!({"rule": "TEST_RULE"})),
    ];
    for (command, params) in cases {
        let before = serde_json::to_value(&s.prefs.proof).unwrap();
        s.agent = Some(Vec::new());
        let result = s.run(command, &params);
        s.agent = None;
        assert!(result.is_err(), "{command} must require settings permission at the mutation boundary");
        assert_eq!(serde_json::to_value(&s.prefs.proof).unwrap(), before, "{command}");
    }
}

/// A folder of this test's own, taken away when the test ends.
pub(crate) mod tempdir {
    use std::path::{Path, PathBuf};

    /// A folder under the system's temporary folder, named for the test that asked for it.
    pub struct Dir(PathBuf);

    impl Dir {
        /// Makes the folder.
        pub fn new(name: &str) -> Dir {
            let path = std::env::temp_dir().join(format!("mailcraft-proof-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            let _ = std::fs::create_dir_all(&path);
            Dir(path)
        }

        /// Writes `text` into `file`.
        pub fn write(&self, file: &str, text: &str) {
            let _ = std::fs::write(self.0.join(file), text);
        }

        /// The folder.
        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
