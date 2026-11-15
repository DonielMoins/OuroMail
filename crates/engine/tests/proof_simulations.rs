// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Proofing simulations A–N (#954): a suite that drives a [`Session`] exactly as a script or an
//! agent would — two invented dictionaries of this test's own, the sample mailbox in memory, and
//! no network — and watches what spelling, grammar and the message's undo do with mixed languages,
//! Canadian spellings, a host's grammar places, hostile answers, large drafts and long runs.
//!
//! What is a word never depends on what this computer has installed: [`Fixture`] writes `en_CA.dic`
//! and `fr_CA.dic` (a few dozen invented words each) into a folder of its own and names that
//! folder in the settings, and the tests remove it when they end. Nothing here touches a real
//! mailbox or opens a socket.
//!
//! The numbers a test measures (I, F, N) are printed to stderr, so
//! `cargo test -p mailcraft-engine --test proof_simulations -- --nocapture` shows them, and written
//! to `/tmp/proof-sim-report.md` as a markdown table (rows are appended; delete the file before a
//! run so a report is one run's).
//!
//! The letters A–N label the tests below and nothing else: they are this suite's own inventory, not
//! the mission's original A–N acceptance scenarios, and a green run here is no evidence that those
//! are covered. Each test says what it does in its own words; the original scenarios are mapped to
//! real evidence, letter by letter, in the acceptance map kept beside the handoff.
//!
//! A correction named by `from`/`to` (a right click's, a script's) now carries the complete checked
//! text and the id of the draft it was checked in — or, while a signature is being edited, the
//! `signature: true` flag and the id of that signature editor instance (`signatureId`) — and the
//! engine refuses one without them ([`guards`]). Every named correction here sends them, and the
//! tests that mean to be refused for their ends send them too, so that what is refused is the place
//! and not a missing guard.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use mailcraft_engine::{Prefs, Session, sample};
use mailcraft_model::RichBody;
use mailcraft_model::proof::MAX_DIAGNOSTICS;
use mailcraft_model::rich::{ListKind, MAX_UNDO};
use mailcraft_time::Timestamp;
use serde_json::{Value, json};

/// The instant these tests are at.
const NOW: Timestamp = 1_750_000_000;

/// Canadian English words: enough for every sentence in this file, and no more.
const EN_WORDS: [&str; 61] = [
    "a",
    "all",
    "an",
    "and",
    "are",
    "at",
    "b",
    "be",
    "but",
    "centre",
    "colour",
    "correct",
    "day",
    "do",
    "document",
    "documents",
    "error",
    "everything",
    "favour",
    "for",
    "from",
    "good",
    "has",
    "have",
    "he",
    "help",
    "here",
    "honour",
    "i",
    "in",
    "is",
    "it",
    "look",
    "looks",
    "message",
    "more",
    "morning",
    "mountain",
    "neighbour",
    "no",
    "not",
    "of",
    "on",
    "or",
    "review",
    "reviewed",
    "right",
    "send",
    "sent",
    "sentence",
    "she",
    "text",
    "that",
    "the",
    "this",
    "to",
    "today",
    "tomorrow",
    "was",
    "you",
    "your",
];

/// Canadian French words: the same.
const FR_WORDS: [&str; 46] = [
    "aujourd'hui",
    "avoir",
    "beaucoup",
    "bien",
    "bonjour",
    "correct",
    "dans",
    "de",
    "demain",
    "des",
    "document",
    "documents",
    "du",
    "elle",
    "envoie",
    "envoyer",
    "et",
    "fait",
    "français",
    "hier",
    "il",
    "je",
    "la",
    "le",
    "les",
    "mais",
    "matin",
    "merci",
    "montréal",
    "nous",
    "ou",
    "pierre",
    "pour",
    "quand",
    "quebec",
    "québec",
    "rendez",
    "soir",
    "sous",
    "sur",
    "très",
    "tu",
    "un",
    "une",
    "voir",
    "vous",
];

/// A folder of this test's own, with the two dictionaries in it, taken away when the test ends.
struct Fixture {
    path: PathBuf,
}

impl Fixture {
    /// Makes the folder and writes `en_CA.dic` and `fr_CA.dic` into it.
    fn new(name: &str) -> Fixture {
        let path = std::env::temp_dir().join(format!("mailcraft-proof-sim-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        let _ = std::fs::create_dir_all(&path);
        let fixture = Fixture { path };
        fixture.dictionary("en_CA.dic", &EN_WORDS);
        fixture.dictionary("fr_CA.dic", &FR_WORDS);
        fixture
    }

    /// Writes a Hunspell `.dic` of `words` (the first line is the count, which is only a hint).
    fn dictionary(&self, file: &str, words: &[&str]) {
        let mut text = format!("{}\n", words.len());
        for word in words {
            text.push_str(word);
            text.push('\n');
        }
        let _ = std::fs::write(self.path.join(file), text);
    }

    /// The folder.
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A session on the sample mailbox, told to keep its dictionaries in a folder of this test's own.
fn session(name: &str) -> (Session, Fixture) {
    let fixture = Fixture::new(name);
    let mut s = Session::at(sample::mailbox(NOW, 0), NOW, 0);
    s.prefs.proof.dictionary_dir = fixture.path().to_string_lossy().to_string();
    (s, fixture)
}

/// A message being written with `text` in it, and nothing else: the signature a new message is
/// given is taken out again, so that what a test counts is what it wrote. Answers its id.
fn open(s: &mut Session, text: &str) -> u64 {
    let id = s.run("mail.new", &json!({})).expect("a message")["compose"].as_u64().expect("an id");
    s.run("compose.type", &json!({ "text": text })).expect("the text");
    let typed = text.chars().count();
    let len = s.doc().map(|(b, _)| b.text().len()).unwrap_or(0);
    if len > typed {
        s.run("compose.delete", &json!({ "from": typed, "to": len })).expect("the signature taken out");
    }
    id
}

/// The same, when the test does not need the message's id.
fn composing(s: &mut Session, text: &str) {
    let _ = open(s, text);
}

/// The message being written, as plain text.
fn body(s: &Session) -> String {
    s.doc().map(|(b, _)| b.plain()).unwrap_or_default()
}

/// The guards a named correction carries now: the complete text that was checked, and the id of
/// the draft it was checked in. The engine refuses a named correction without both (or, while a
/// signature is being edited, without `signature: true` and that editor instance's `signatureId`
/// instead of a draft id).
fn guards(s: &Session) -> (String, u64) {
    (body(s), s.compose_id().expect("a draft is being written"))
}

/// Every place an answer found, as (start, end, word) in characters of the text checked.
fn places(answer: &Value) -> Vec<(usize, usize, String)> {
    let text = answer.get("text").and_then(Value::as_str).unwrap_or_default();
    let list = answer.get("diagnostics").and_then(Value::as_array).cloned().unwrap_or_default();
    list.iter()
        .filter_map(|d| {
            let span = d.get("span")?;
            let start = usize::try_from(span.get("start")?.as_u64()?).ok()?;
            let end = usize::try_from(span.get("end")?.as_u64()?).ok()?;
            let piece = text.get(start..end)?;
            let from = text.get(..start)?.chars().count();
            Some((from, from + piece.chars().count(), piece.to_string()))
        })
        .collect()
}

/// The words an answer flagged, in order.
fn flagged(answer: &Value) -> Vec<String> {
    places(answer).into_iter().map(|(_, _, word)| word).collect()
}

/// What an answer says could stand in for `word`.
fn suggestions_for(answer: &Value, word: &str) -> Vec<String> {
    let text = answer.get("text").and_then(Value::as_str).unwrap_or_default();
    let list = answer.get("diagnostics").and_then(Value::as_array).cloned().unwrap_or_default();
    list.iter()
        .find(|d| {
            let span = d.get("span");
            let start = span.and_then(|s| s.get("start")).and_then(Value::as_u64).and_then(|n| usize::try_from(n).ok());
            let end = span.and_then(|s| s.get("end")).and_then(Value::as_u64).and_then(|n| usize::try_from(n).ok());
            matches!((start, end), (Some(a), Some(b)) if text.get(a..b) == Some(word))
        })
        .map(|d| {
            d.get("suggestions")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                .unwrap_or_default()
        })
        .unwrap_or_default()
}

/// Every place of an answer is a stretch of its own text: inside it and on character boundaries.
fn assert_spans_fit(answer: &Value) {
    let text = answer.get("text").and_then(Value::as_str).unwrap_or_default();
    let list = answer.get("diagnostics").and_then(Value::as_array).cloned().unwrap_or_default();
    for d in &list {
        let span = d.get("span").cloned().unwrap_or_default();
        let start = usize::try_from(span.get("start").and_then(Value::as_u64).expect("a start")).expect("a start");
        let end = usize::try_from(span.get("end").and_then(Value::as_u64).expect("an end")).expect("an end");
        assert!(end > start, "an empty place: {d:?}");
        assert!(end <= text.len(), "a place past the text: {d:?}");
        assert!(text.is_char_boundary(start) && text.is_char_boundary(end), "a place splitting a character: {d:?}");
    }
}

/// How many steps of undo a message being written has (the stack itself is private; this is the
/// cheap proxy the long-running simulation compares: clone it and walk back until there is none).
fn undo_steps(body: &RichBody) -> usize {
    let mut probe = body.clone();
    let mut steps = 0;
    while probe.undo() {
        steps += 1;
        if steps > 10_000 {
            break;
        }
    }
    steps
}

/// A draft of about `words` words, mostly known ones with a name no dictionary has every 97.
fn passage(words: usize) -> String {
    const SENTENCE: [&str; 12] = ["The", "colour", "of", "the", "centre", "is", "correct", "and", "the", "document", "is", "here"];
    let mut out: Vec<&str> = Vec::with_capacity(words + words / 97 + 1);
    for i in 0..words {
        if i > 0 && (i % 97 == 0 || i == words / 2) {
            out.push("Zorblax");
        } else {
            out.push(SENTENCE[i % SENTENCE.len()]);
        }
    }
    out.join(" ")
}

/// The report's lock: one row at a time, from the tests that run side by side.
static REPORT: Mutex<()> = Mutex::new(());

/// Writes one row of the measured numbers to `/tmp/proof-sim-report.md` (and says it on stderr, so
/// `--nocapture` shows it too).
fn record(test: &str, what: &str, value: &str, pass: bool) {
    let Ok(_guard) = REPORT.lock() else { return };
    let path = "/tmp/proof-sim-report.md";
    let fresh = std::fs::metadata(path).map(|m| m.len() == 0).unwrap_or(true);
    let mut row = String::new();
    if fresh {
        row.push_str("# Proofing simulations A–N — the measured numbers (M7.6, #954)\n\n");
        row.push_str("Rows are appended by `crates/engine/tests/proof_simulations.rs`; times are this machine's.\n\n");
        row.push_str("| test | what it measures | value | pass |\n|---|---|---|---|\n");
    }
    row.push_str(&format!("| {test} | {what} | {value} | {} |\n", if pass { "pass" } else { "FAIL" }));
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(row.as_bytes());
    }
    eprintln!("{test} — {what}: {value} ({})", if pass { "pass" } else { "FAIL" });
}

/// Milliseconds, to one place, for the report.
fn ms(d: Duration) -> String {
    format!("{:.1} ms", d.as_secs_f64() * 1_000.0)
}

/// A: one paragraph in English and in French. Nothing is flagged for being in the other language —
/// both dictionaries are consulted for every word, always — and only a word neither knows is.
#[test]
fn a_mixed_language_composition_flags_only_unknown_words() {
    let (mut s, _fixture) = session("a-mixed");
    composing(&mut s, "Bonjour Pierre, I reviewed the documents hier soir and everything looks correct. Merci beaucoup for your help.");
    let answer = s.run("proof.check", &json!({})).expect("a check");
    assert_eq!(answer.get("loaded").and_then(Value::as_bool), Some(true), "a dictionary was read");
    let read: Vec<String> = answer
        .get("languages")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    assert!(read.contains(&"en-CA".to_string()) && read.contains(&"fr-CA".to_string()), "{read:?}");
    let wrong = flagged(&answer);
    assert!(wrong.is_empty(), "nothing of either language is a mistake: {wrong:?}");

    // The proof that it is the words and not the mixing: one word neither dictionary knows.
    s.run("compose.type", &json!({ "text": " Zorblax." })).expect("typed");
    let after = s.run("proof.check", &json!({})).expect("a check");
    assert_eq!(flagged(&after), ["Zorblax"], "only the word neither dictionary knows");
    assert_spans_fit(&after);
}

/// B: a sentence written half in English and half in French, with mistakes in both. Each misspelling
/// is flagged and carries the correction that fits it — the dictionaries are all read for it.
#[test]
fn b_mixed_language_mistakes_carry_their_corrections() {
    let (mut s, _fixture) = session("b-mistakes");
    composing(&mut s, "Je vouis envoyer le documant tomorow morning.");
    let answer = s.run("proof.check", &json!({})).expect("a check");
    assert_eq!(flagged(&answer), ["vouis", "documant", "tomorow"], "exactly the three mistakes");
    for (word, right) in [("vouis", "vous"), ("documant", "document"), ("tomorow", "tomorrow")] {
        let said = suggestions_for(&answer, word);
        assert!(said.iter().any(|s| s == right), "{word} should offer {right}: {said:?}");
    }
    assert_spans_fit(&answer);
}

/// C: Canadian spellings and names are words because the dictionaries that are on say so — the
/// engine holds no list of its own — and a language turned off makes its words unknown.
#[test]
fn c_canadian_spellings_and_names_are_words() {
    let (mut s, _fixture) = session("c-canadian");
    composing(&mut s, "The colour of the centre is right, neighbour, and Montréal, Québec, français, aujourd'hui are all correct.");
    let answer = s.run("proof.check", &json!({})).expect("a check");
    assert!(flagged(&answer).is_empty(), "all of them are words: {:?}", flagged(&answer));
    // And it is the dictionary that said so: with French off, its words are not.
    s.run("proof.set", &json!({ "languages": ["en-CA"] })).expect("English alone");
    composing(&mut s, "Montréal and français and aujourd'hui.");
    let english_only = flagged(&s.run("proof.check", &json!({})).expect("a check"));
    assert!(english_only.contains(&"Montréal".to_string()) && english_only.contains(&"français".to_string()), "{english_only:?}");
}

/// D: a host's grammar places (what the window or the CLI got from its LanguageTool server) are
/// taken into the walk, applied only when the owner says so, and walked to the end. The engine
/// itself never asks anything of a server.
#[test]
fn d_grammar_places_from_the_host_are_walked_and_applied() {
    let (mut s, _fixture) = session("d-grammar");
    composing(&mut s, "She have sent the documents.");
    let before = body(&s);
    let walk = s
        .run(
            "review.spelling",
            &json!({
                "again": true,
                "grammar": [
                    {
                        "start": 0,
                        "end": 8,
                        "kind": "grammar",
                        "message": "The verb does not agree with its subject.",
                        "suggestions": ["She has"],
                        "rule": "HE_VERB_AGR",
                        "language": "en-CA"
                    },
                    {
                        "start": 14,
                        "end": 17,
                        "kind": "style",
                        "message": "A definite article here reads oddly.",
                        "suggestions": ["these"],
                        "rule": "STYLE_ARTICLE",
                        "language": "en-CA"
                    }
                ]
            }),
        )
        .expect("the walk");
    assert_eq!(walk.get("total").and_then(Value::as_u64), Some(2), "both places are in the walk");
    assert_eq!(walk.get("at").and_then(|a| a.get("word")).and_then(Value::as_str), Some("She have"));
    assert_eq!(walk.get("at").and_then(|a| a.get("kind")).and_then(Value::as_str), Some("grammar"));
    assert_eq!(body(&s), before, "nothing is applied until the owner says so");

    // The owner takes the host's suggestion: one step of the message's undo, and the text is right.
    let changed = s.run("proof.change", &json!({ "text": "She has" })).expect("changed");
    assert_eq!(changed.get("changed").and_then(Value::as_str), Some("She has"));
    assert_eq!(body(&s).trim_end(), "She has sent the documents.");

    // The second place was for the text as it was before the correction; the host answers for the
    // new text, and the place that is left is the one the owner walks next.
    let next = s
        .run(
            "review.spelling",
            &json!({
                "again": true,
                "grammar": [{ "start": 13, "end": 16, "kind": "style", "message": "A definite article here reads oddly.", "suggestions": ["these"], "rule": "STYLE_ARTICLE" }]
            }),
        )
        .expect("the walk again");
    assert_eq!(next.get("at").and_then(|a| a.get("word")).and_then(Value::as_str), Some("the"), "{next:?}");
    s.run("proof.change", &json!({ "text": "these" })).expect("changed");
    assert_eq!(body(&s).trim_end(), "She has sent these documents.");

    // Nothing of this went over a network: the engine's own manifest names no network crate.
    let manifest = include_str!("../Cargo.toml");
    assert!(!manifest.contains("mailcraft-net") && !manifest.contains("reqwest"), "the engine asks no server");
}

/// E: a place whose text changed under it is refused and changes nothing (a correction is never
/// put in the wrong place), and so are ends that are not a stretch of the body — sent with the
/// complete checked text and the draft's id, so what is refused is the ends themselves and not a
/// missing guard.
#[test]
fn e_a_stale_place_and_impossible_ends_are_refused() {
    let (mut s, _fixture) = session("e-stale");
    composing(&mut s, "the documet is here");
    s.run("review.spelling", &json!({ "again": true })).expect("the walk");
    // The owner types somewhere else: the walk's text is no longer the message's.
    s.run("compose.type", &json!({ "text": "more " })).expect("typed");
    let before = body(&s);
    assert!(s.run("proof.change", &json!({ "text": "document" })).is_err(), "a stale place is refused");
    assert_eq!(body(&s), before, "and nothing changed");

    // Ends that are not a stretch of the message: past its end, empty, and absurdly far. The
    // complete text and the draft's id are right, so the ends are what is refused.
    let (snapshot, compose) = guards(&s);
    for (from, to) in [(0u64, 9_999u64), (4, 4), (0, 1_000_000), (0, 0)] {
        let params = json!({
            "text": "document",
            "from": from,
            "to": to,
            "expectedText": snapshot.as_str(),
            "composeId": compose
        });
        let error = s.run("proof.change", &params).expect_err("refused");
        assert!(error.to_string().contains("stretch of the message"), "the ends are refused: {error}");
        assert_eq!(body(&s), before, "the message is as it was: {params}");
    }
    // The place that does fit is still correctable, so it was the place that was refused.
    s.run("proof.change", &json!({ "text": "document", "from": 4, "to": 11, "expectedText": snapshot.as_str(), "composeId": compose }))
        .expect("changed");
    assert!(body(&s).starts_with("the document is here"));
}

/// F: typing fast — three hundred keystrokes, a hundred backspaces, fifty replacements and twenty
/// undos and redos — leaves the body exactly where it should be, and a check afterwards still
/// answers places that fit it.
#[test]
fn f_rapid_typing_leaves_the_body_where_it_should_be() {
    let (mut s, _fixture) = session("f-typing");
    s.run("mail.new", &json!({})).expect("a message");
    if !body(&s).is_empty() {
        let len = body(&s).chars().count();
        s.run("compose.delete", &json!({ "from": 0, "to": len })).expect("a body of nothing");
    }

    let typed: String = (0..300).map(|i| ["a", "b", " "][i % 3]).collect();
    let started = Instant::now();
    for ch in typed.chars() {
        s.run("compose.type", &json!({ "text": ch.to_string() })).expect("a keystroke");
    }
    let typing = started.elapsed();
    assert_eq!(body(&s).chars().count(), 300);

    let started = Instant::now();
    for _ in 0..100 {
        s.run("compose.delete", &json!({})).expect("a backspace");
    }
    let backspacing = started.elapsed();
    assert_eq!(body(&s).chars().count(), 200);
    let mut expected = typed.get(..200).expect("the first 200 characters").to_string();

    let started = Instant::now();
    for i in 0..50 {
        s.run("compose.select", &json!({ "from": 0, "to": 1 })).expect("selected");
        s.run("compose.type", &json!({ "text": if i % 2 == 0 { "b" } else { "a" } })).expect("a replacement");
    }
    let replacing = started.elapsed();
    // Fifty replacements, the last of which puts back the `a` that was there: the body as it was.
    expected.replace_range(0..1, "a");
    assert_eq!(body(&s), expected);

    let started = Instant::now();
    for _ in 0..20 {
        s.run("compose.undo", &json!({})).expect("undone");
    }
    for _ in 0..20 {
        s.run("compose.redo", &json!({})).expect("redone");
    }
    let undoing = started.elapsed();
    assert_eq!(body(&s), expected, "twenty back and twenty forward is where it was");

    let started = Instant::now();
    let answer = s.run("proof.check", &json!({})).expect("a check");
    let checking = started.elapsed();
    assert_spans_fit(&answer);
    assert_eq!(answer.get("text").and_then(Value::as_str), Some(expected.as_str()));
    let mut said = flagged(&answer);
    said.sort();
    said.dedup();
    assert_eq!(said, ["ab"], "the words are all `ab`, which no dictionary here has");

    record("F rapid typing", "300 keystrokes", &ms(typing), typing < Duration::from_secs(2));
    record("F rapid typing", "100 backspaces", &ms(backspacing), backspacing < Duration::from_secs(2));
    record("F rapid typing", "50 replacements", &ms(replacing), replacing < Duration::from_secs(2));
    record("F rapid typing", "20 undos and 20 redos", &ms(undoing), undoing < Duration::from_secs(2));
    record("F rapid typing", "proof.check of the final 200 characters", &ms(checking), checking < Duration::from_secs(1));
}

/// G: a correction keeps the look of what it replaced: bold, italic, the link it was, and the list
/// the paragraph was in are the corrected word's too.
#[test]
fn g_a_correction_keeps_the_formatting_round_it() {
    let (mut s, _fixture) = session("g-look");
    composing(&mut s, "the documet is here");
    s.run("compose.select", &json!({ "from": 4, "to": 11 })).expect("selected");
    s.run("format.bold", &json!({})).expect("bold");
    s.run("format.italic", &json!({})).expect("italic");
    s.run("format.bullets", &json!({ "from": 4, "to": 11 })).expect("a bullet");
    s.run("compose.link", &json!({ "url": "https://example.org/docs", "from": 4, "to": 11 })).expect("a link");
    let (snapshot, compose) = guards(&s);
    s.run("proof.change", &json!({ "text": "document", "from": 4, "to": 11, "expectedText": snapshot, "composeId": compose })).expect("changed");

    let (rich, _) = s.doc().expect("a message");
    let paragraphs = rich.text().paragraphs();
    assert_eq!(paragraphs.len(), 1, "still one paragraph");
    assert_eq!(paragraphs[0].list, ListKind::Bullet, "the list it was in is the list it is in");
    let corrected: Vec<_> = paragraphs[0].runs.iter().filter(|r| r.text.contains("document")).collect();
    assert_eq!(corrected.len(), 1, "one run holds the corrected word: {:?}", paragraphs[0].runs);
    let style = &corrected[0].style;
    assert!(style.bold, "bold survived: {style:?}");
    assert!(style.italic, "italic survived: {style:?}");
    assert_eq!(style.link.as_deref(), Some("https://example.org/docs"), "the link survived: {style:?}");
    let bold: String = paragraphs[0].runs.iter().filter(|r| r.style.bold).map(|r| r.text.as_str()).collect();
    assert_eq!(bold, "document", "and nothing else became bold");
    assert_eq!(paragraphs[0].text(), "the document is here", "and the text is right");
}

/// H: a correction is exactly one step of the message's own undo — one back has the mistake, one
/// forward has the correction, and nothing in between.
#[test]
fn h_a_correction_is_exactly_one_step_of_undo() {
    let (mut s, _fixture) = session("h-undo");
    composing(&mut s, "the documet is here");
    let before = undo_steps(s.doc().map(|(b, _)| b).expect("a message"));
    let (snapshot, compose) = guards(&s);
    s.run("proof.change", &json!({ "text": "document", "from": 4, "to": 11, "expectedText": snapshot, "composeId": compose })).expect("changed");
    let after = undo_steps(s.doc().map(|(b, _)| b).expect("a message"));
    assert_eq!(after, (before + 1).min(MAX_UNDO), "one step, and no more: {before} → {after}");

    s.run("compose.undo", &json!({})).expect("undone");
    assert_eq!(body(&s).trim_end(), "the documet is here", "one back is the mistake itself");
    s.run("compose.redo", &json!({})).expect("redone");
    assert_eq!(body(&s).trim_end(), "the document is here", "one forward is the correction");
}

/// I: large drafts — fifty words to ten thousand — are checked on this computer in a time that is
/// measured and printed (and asserted under a generous ceiling), with places that still fit.
#[test]
fn i_large_drafts_are_checked_in_a_time_that_is_measured() {
    let (mut s, _fixture) = session("i-large");
    s.run("mail.new", &json!({})).expect("a message");
    for words in [50usize, 250, 1_000, 5_000, 10_000] {
        let text = passage(words);
        s.run("compose.set", &json!({ "body": text })).expect("a body");
        assert_eq!(body(&s).chars().count(), text.chars().count(), "{words}: the body is what was written");
        let started = Instant::now();
        let answer = s.run("proof.check", &json!({})).expect("a check");
        let elapsed = started.elapsed();
        assert_spans_fit(&answer);
        let found = flagged(&answer).len();
        assert!(found > 0, "{words}: the name no dictionary knows is found");
        let ceiling = Duration::from_secs(2);
        record("I large drafts", &format!("proof.check of {words} words ({} characters)", text.chars().count()), &ms(elapsed), elapsed < ceiling);
        assert!(elapsed < ceiling, "{words} words took {elapsed:?}");
        eprintln!("I: {words} words ({} characters), {found} places: {elapsed:?}", text.chars().count());
    }
}

/// J: three messages written in turn on one session. A walk started on one is never applied to
/// another, and each correction lands in the message it was made in.
#[test]
fn j_three_messages_in_turn_never_take_each_others_places() {
    let (mut s, _fixture) = session("j-three");

    let first = open(&mut s, "the documet is here");
    let walk = s.run("review.spelling", &json!({ "again": true })).expect("the walk");
    assert_eq!(walk.get("at").and_then(|a| a.get("word")).and_then(Value::as_str), Some("documet"));

    // A second message, put in front: the first one's walk belongs to the first one's text.
    let second = open(&mut s, "the sentance is wrong");
    assert!(s.run("proof.change", &json!({ "text": "document" })).is_err(), "a place of another message is refused");
    assert_eq!(body(&s).trim_end(), "the sentance is wrong", "the second message is untouched");
    // Nor by naming it: this draft's complete text with the first draft's id is refused, so a
    // correction can never be put into an identical draft by a stale id.
    let (snapshot, _) = guards(&s);
    assert!(
        s.run("proof.change", &json!({ "text": "document", "from": 4, "to": 12, "expectedText": snapshot, "composeId": first })).is_err(),
        "a correction named for another draft is refused"
    );
    assert_eq!(body(&s).trim_end(), "the sentance is wrong", "and the second message is still untouched");
    let second_walk = s.run("review.spelling", &json!({ "again": true })).expect("the walk");
    assert_eq!(second_walk.get("text").and_then(Value::as_str), Some("the sentance is wrong"));
    assert_eq!(second_walk.get("at").and_then(|a| a.get("word")).and_then(Value::as_str), Some("sentance"));
    s.run("proof.change", &json!({ "text": "sentence" })).expect("changed");
    assert_eq!(body(&s).trim_end(), "the sentence is wrong");

    // A third, corrected by the place a right click names.
    let third = open(&mut s, "the colur is right");
    let checked = s.run("proof.check", &json!({})).expect("a check");
    assert_eq!(checked.get("text").and_then(Value::as_str), Some("the colur is right"), "only this message is checked");
    let (from, to, _word) = places(&checked).into_iter().find(|(_, _, w)| w == "colur").expect("the mistake");
    assert_eq!((from, to), (4, 9));
    let (snapshot, compose) = guards(&s);
    s.run("proof.change", &json!({ "text": "colour", "from": from, "to": to, "expectedText": snapshot, "composeId": compose })).expect("changed");
    assert_eq!(body(&s).trim_end(), "the colour is right");

    // Each message holds its own text, and only its own correction.
    let text_of = |s: &Session, id: u64| s.composing_by_id(id).map(|c| c.body.plain()).unwrap_or_default();
    assert_eq!(text_of(&s, first).trim_end(), "the documet is here", "the first was never corrected");
    assert_eq!(text_of(&s, second).trim_end(), "the sentence is wrong");
    assert_eq!(text_of(&s, third).trim_end(), "the colour is right");
}

/// K: a hostile `grammar` answer to `review.spelling` — negative and absurd offsets, empty and
/// backwards spans, ten thousand entries, control characters in suggestions, a rule id of ten
/// thousand characters — is dropped where it does not fit, never panics, and the walk stays bounded.
#[test]
fn k_hostile_grammar_answers_never_panic_and_stay_bounded() {
    let (mut s, _fixture) = session("k-hostile");
    let text = "The colour of the centre is correct and the document is here. ".repeat(20);
    s.run("mail.new", &json!({})).expect("a message");
    s.run("compose.set", &json!({ "body": text })).expect("a body");
    let written = body(&s);
    let len = written.chars().count();

    let mut grammar: Vec<Value> = vec![
        json!({ "start": -3, "end": 5 }),
        json!({ "start": "0", "end": 5 }),
        json!({ "start": 0, "end": u64::MAX }),
        json!({ "start": 5, "end": 5 }),
        json!({ "start": 9, "end": 3 }),
        json!({ "start": u64::MAX - 1, "end": u64::MAX }),
        json!({
            "start": 0,
            "end": 3,
            "kind": "??",
            "message": "a\nb\u{0}c",
            "suggestions": ["\u{0}\u{7}\u{1b}", "\u{2028}x", "ok"],
            "rule": "R".repeat(10_000)
        }),
    ];
    for i in 0..9_990usize {
        let start = (i * 13) % len.saturating_sub(4).max(1);
        grammar.push(json!({
            "start": start,
            "end": start + 3,
            "kind": "style",
            "message": format!("place {i}"),
            "suggestions": ["the"],
            "rule": format!("R{i}")
        }));
    }

    let started = Instant::now();
    let walk = s.run("review.spelling", &json!({ "again": true, "grammar": grammar })).expect("the walk, whatever came in");
    let elapsed = started.elapsed();
    let total = walk.get("total").and_then(Value::as_u64).unwrap_or(u64::MAX);
    assert!(total <= MAX_DIAGNOSTICS as u64, "the walk is bounded: {total}");
    assert_eq!(walk.get("text").and_then(Value::as_str), Some(written.as_str()), "the text is the message's");
    assert!(elapsed < Duration::from_secs(2), "the hostile answer is read quickly: {elapsed:?}");

    // Every place the owner walks through fits the text, and what it says is one line of text.
    let mut walked = 0;
    loop {
        let at = walk.get("at").cloned().unwrap_or_default();
        if at.is_null() {
            break;
        }
        let start = at.get("start").and_then(Value::as_u64).and_then(|n| usize::try_from(n).ok()).expect("a start");
        let end = at.get("end").and_then(Value::as_u64).and_then(|n| usize::try_from(n).ok()).expect("an end");
        assert!(end > start && end <= written.len(), "a place that does not fit: {at:?}");
        assert!(written.is_char_boundary(start) && written.is_char_boundary(end));
        assert!(at.get("word").and_then(Value::as_str).is_some_and(|w| !w.is_empty()), "a place with a word: {at:?}");
        for said in at.get("suggestions").and_then(Value::as_array).cloned().unwrap_or_default() {
            let said = said.as_str().unwrap_or_default();
            assert!(!said.chars().any(char::is_control), "a suggestion is one line: {said:?}");
        }
        if let Some(message) = at.get("message").and_then(Value::as_str) {
            assert!(!message.chars().any(char::is_control), "a message is one line: {message:?}");
        }
        let next = s.run("proof.ignore", &json!({})).expect("left alone");
        if next.get("done").and_then(Value::as_bool) == Some(true) {
            break;
        }
        walked += 1;
        if walked >= 60 {
            break;
        }
        // Keep walking the same walk.
        let at_next = next.get("at").cloned().unwrap_or_default();
        let start = at_next.get("start").and_then(Value::as_u64).and_then(|n| usize::try_from(n).ok()).expect("a start");
        let end = at_next.get("end").and_then(Value::as_u64).and_then(|n| usize::try_from(n).ok()).expect("an end");
        assert!(end > start && end <= written.len(), "a place that does not fit: {at_next:?}");
    }
    assert!(walked > 0, "the walk went somewhere");
    record("K hostile grammar", "10,000-entry hostile answer read and walked", &ms(elapsed), elapsed < Duration::from_secs(2));
}

/// L: offline. Grammar is on and a grammar server is named, but nothing was brought back and
/// nothing is asked: no check blocks, a check without grammar is the same as with it, and the
/// engine has no network of its own at all.
#[test]
fn l_offline_nothing_is_asked_and_a_check_is_unaffected() {
    let (mut s, _fixture) = session("l-offline");
    composing(&mut s, "the documet and the sentance");
    let settings = s.run("proof.set", &json!({ "grammarUrl": "http://127.0.0.1:8081", "checkGrammar": true, "asYouType": true })).expect("set");
    assert_eq!(settings.get("settings").and_then(|s| s.get("checkGrammar")).and_then(Value::as_bool), Some(true));
    assert_eq!(settings.get("grammarAllowed").and_then(Value::as_bool), Some(true));

    let with = s.run("proof.check", &json!({})).expect("a check");
    s.run("proof.set", &json!({ "checkGrammar": false })).expect("grammar off");
    let started = Instant::now();
    let without = s.run("proof.check", &json!({})).expect("a check");
    let elapsed = started.elapsed();
    assert_eq!(flagged(&with), flagged(&without), "grammar on changed nothing of the spelling");
    assert_eq!(flagged(&without), ["documet", "sentance"]);
    assert!(elapsed < Duration::from_secs(1), "a check with no server to ask does not wait on one: {elapsed:?}");

    // No host brought any place back, so the walk has none: the engine invents none.
    let walk = s.run("review.spelling", &json!({ "again": true })).expect("the walk");
    let rules = walk.get("rules").and_then(Value::as_array).cloned().unwrap_or_default();
    assert!(rules.is_empty(), "no grammar place was made up: {rules:?}");
    assert_eq!(walk.get("total").and_then(Value::as_u64), Some(2), "the two spelling places, and nothing else");

    // The engine really has nothing to send with: its manifest names no network crate.
    let manifest = include_str!("../Cargo.toml");
    for crate_name in ["mailcraft-net", "reqwest", "hyper", "ureq"] {
        assert!(!manifest.contains(crate_name), "the engine names no network crate: {crate_name}");
    }
    record("L offline", "two proof.check calls, grammar on, no host", &ms(elapsed), elapsed < Duration::from_secs(1));
}

/// M: the personal dictionary. Adding a word changes what is flagged, taking it out changes it
/// back, the list never repeats, and the settings survive being saved and read in again.
#[test]
fn m_the_personal_dictionary_changes_what_is_flagged_and_is_saved() {
    let (mut s, _fixture) = session("m-personal");
    composing(&mut s, "Hochelaga is where the mountain is, and Zorblax too");
    let before = flagged(&s.run("proof.check", &json!({})).expect("a check"));
    assert!(before.contains(&"Hochelaga".to_string()) && before.contains(&"Zorblax".to_string()), "{before:?}");

    let added = s.run("proof.addWord", &json!({ "word": "Hochelaga" })).expect("a word");
    assert_eq!(added.get("added").and_then(Value::as_bool), Some(true));
    let after = flagged(&s.run("proof.check", &json!({})).expect("a check"));
    assert!(!after.contains(&"Hochelaga".to_string()) && after.contains(&"Zorblax".to_string()), "{after:?}");

    // Never twice, whatever its case or the spaces round it.
    for word in ["hochelaga", "  HOCHELAGA  ", "Hochelaga"] {
        let again = s.run("proof.addWord", &json!({ "word": word })).expect("the same word again");
        assert_eq!(again.get("added").and_then(Value::as_bool), Some(false), "{word}");
    }
    let list: Vec<String> = s
        .run("proof.status", &json!({}))
        .expect("the settings")
        .get("settings")
        .and_then(|s| s.get("personal"))
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    assert_eq!(list, ["Hochelaga"], "one word, once: {list:?}");

    // Nothing that is not a word goes in, and only a word that is there comes out.
    let too_long = "x".repeat(65);
    for word in ["", "   ", too_long.as_str()] {
        assert_eq!(s.run("proof.addWord", &json!({ "word": word })).expect("refused")["added"].as_bool(), Some(false), "{word:?}");
    }
    assert_eq!(s.run("proof.removeWord", &json!({ "word": "nothere" })).expect("nothing")["removed"].as_bool(), Some(false));
    assert_eq!(s.run("proof.removeWord", &json!({ "word": "HOCHELAGA" })).expect("taken out")["removed"].as_bool(), Some(true));
    let back = flagged(&s.run("proof.check", &json!({})).expect("a check"));
    assert!(back.contains(&"Hochelaga".to_string()), "a word taken out is a mistake again: {back:?}");

    // The settings as they are survive being saved and read in.
    s.run("proof.addWord", &json!({ "word": "Hochelaga" })).expect("a word");
    s.run("proof.addWord", &json!({ "word": "Zorblax" })).expect("a word");
    let saved = serde_json::to_string(&s.prefs).expect("the settings");
    let read: Prefs = serde_json::from_str(&saved).expect("the same settings");
    assert_eq!(read.proof.personal_words(), s.prefs.proof.personal_words());
    assert_eq!(read.proof.personal_words().len(), read.proof.personal.len(), "no repeats");
    assert_eq!(read.proof.dictionary_dir, s.prefs.proof.dictionary_dir);
    assert_eq!(read.proof.check_spelling, s.prefs.proof.check_spelling);
}

/// N: two thousand rounds of typing, checking, correcting, undoing and changing the settings on one
/// session: nothing panics, the body stays within a bound, and the undo history stays capped.
#[test]
fn n_a_long_run_stays_within_its_bounds() {
    let (mut s, _fixture) = session("n-long");
    s.run("mail.new", &json!({})).expect("a message");
    let mut checks = 0usize;
    let mut corrections = 0usize;
    let mut undos = 0usize;
    let mut settings_changes = 0usize;
    let mut largest_body = 0usize;
    let mut largest_history = 0usize;
    let started = Instant::now();
    for i in 0..2_000usize {
        let word = match i % 3 {
            0 => "documet ",
            1 => "the ",
            _ => "is ",
        };
        s.run("compose.type", &json!({ "text": word })).expect("typing");
        if i % 5 == 0 {
            s.run("compose.delete", &json!({})).expect("a backspace");
        }
        if i % 7 == 0 {
            let answer = s.run("proof.check", &json!({})).expect("a check");
            checks += 1;
            assert_spans_fit(&answer);
            // A right click's correction, on a place that is a mistake and fits the body.
            if let Some((from, to, _)) = places(&answer).into_iter().find(|(_, _, w)| w == "documet")
                && to > from
            {
                let (snapshot, compose) = guards(&s);
                s.run("proof.change", &json!({ "text": "document", "from": from, "to": to, "expectedText": snapshot, "composeId": compose }))
                    .expect("changed");
                corrections += 1;
            }
        }
        if i % 11 == 0 {
            let _ = s.run("compose.undo", &json!({}));
            undos += 1;
        }
        if i % 13 == 0 {
            let on = i % 26 != 0;
            s.run("proof.set", &json!({ "checkSpelling": on, "asYouType": on })).expect("the settings");
            settings_changes += 1;
        }
        if i % 97 == 0 {
            s.run("proof.addWord", &json!({ "word": format!("own{i}") })).expect("a word");
        }
        // Starting again every so often: a draft that is cleared, as an owner retyping one would.
        if i % 200 == 199 {
            largest_body = largest_body.max(body(&s).chars().count());
            largest_history = largest_history.max(undo_steps(s.doc().map(|(b, _)| b).expect("a message")));
            s.run("compose.set", &json!({ "body": "" })).expect("a fresh body");
        }
        if i % 100 == 0 {
            largest_body = largest_body.max(body(&s).chars().count());
            largest_history = largest_history.max(undo_steps(s.doc().map(|(b, _)| b).expect("a message")));
        }
    }
    let elapsed = started.elapsed();
    largest_body = largest_body.max(body(&s).chars().count());
    largest_history = largest_history.max(undo_steps(s.doc().map(|(b, _)| b).expect("a message")));

    assert_eq!(checks, 286, "a check every seventh round");
    assert!(corrections > 100, "the corrections happened: {corrections}");
    assert!(undos > 100 && settings_changes > 100);
    assert!(largest_body < 2_000, "the body is bounded: {largest_body}");
    assert!(largest_history <= MAX_UNDO, "the undo history is capped: {largest_history}");
    // Still a working session at the end of it.
    let answer = s.run("proof.check", &json!({})).expect("a check");
    assert_spans_fit(&answer);

    record("N long run", "2,000 rounds of typing, checks, corrections, undos, settings", &ms(elapsed), elapsed < Duration::from_secs(30));
    record("N long run", "largest body held", &format!("{largest_body} characters"), largest_body < 2_000);
    record("N long run", "largest undo history", &format!("{largest_history} steps (cap {MAX_UNDO})"), largest_history <= MAX_UNDO);
    record("N long run", "checks and corrections", &format!("{checks} checks, {corrections} corrections, {undos} undos"), checks > 0);
}

/// Beyond the letters above — what the letters do not measure. A draft is saved, the mailbox is
/// written to a file of this test's own and read back by the store, and proofreading carries on in
/// the draft that came back: the store's own file, not the serde roundtrip of one value in memory
/// that M measures. A GUI, a preferences file and a real mailbox are still not covered here.
#[test]
fn a_saved_draft_is_checked_and_corrected_after_the_mailbox_is_read_back_from_disk() {
    let (mut s, fixture) = session("persisted-draft");
    // The fixture's own folder is where the mailbox file goes, so that it goes when the test ends.
    let path = fixture.path().join("mailbox.mailcraft");
    s.run("mail.new", &json!({})).expect("a message");
    let typed = "Merci Pierre pour le documet, Montréal 🙂";
    s.run("compose.set", &json!({ "body": typed })).expect("a body");
    let saved = s.run("compose.saveDraft", &json!({})).expect("a draft");
    let draft = saved.get("draft").and_then(Value::as_u64).expect("a draft's id");
    let before = s.run("proof.check", &json!({})).expect("a check");
    assert_eq!(flagged(&before), ["documet"], "the mistake before the draft is saved");

    mailcraft_engine::store::save(&path, &s.mailbox).expect("the mailbox written to its file");
    assert!(path.is_file(), "the file is on disk: {}", path.display());
    drop(s);

    let opened = mailcraft_engine::store::open(&path).expect("the mailbox read back from its file");
    assert!(opened.mailbox.message(draft).is_some(), "the draft is in the file");
    let mut s = Session::at(opened.mailbox, NOW, 0);
    s.prefs.proof.dictionary_dir = fixture.path().to_string_lossy().to_string();
    s.run("mail.editDraft", &json!({ "id": draft })).expect("the draft opens");
    assert_eq!(body(&s), typed, "the text read back is the text that was written, character for character");
    let checked = s.run("proof.check", &json!({})).expect("a check");
    assert_eq!(flagged(&checked), ["documet"], "the mistake is still a mistake in the draft that was read");
    let (from, to, _) = places(&checked).into_iter().next().expect("the place");
    let (snapshot, compose) = guards(&s);
    s.run("proof.change", &json!({ "text": "document", "from": from, "to": to, "expectedText": snapshot, "composeId": compose })).expect("changed");
    assert_eq!(body(&s), typed.replace("documet", "document"), "and the correction applies to the text that was read");
}

/// Beyond the letters above: places count characters, not bytes, in a draft whose words have
/// multi-byte characters, and a canonically equivalent accent is the same word for a check without
/// the draft's own bytes being changed. Nothing here normalizes what the owner wrote.
#[test]
fn characters_not_bytes_place_a_correction_among_accents_and_an_emoji() {
    let (mut s, _fixture) = session("unicode");
    // `é` written as `e` and a combining acute: two characters, three bytes.
    let typed = "Montre\u{301}al documet 🙂 merci";
    composing(&mut s, typed);
    let answer = s.run("proof.check", &json!({})).expect("a check");
    assert_eq!(flagged(&answer), ["documet"], "a canonically equivalent accent is the same word, and the emoji is no word at all");
    assert_eq!(answer.get("text").and_then(Value::as_str), Some(typed), "the checked text is the draft's own bytes, not a normalized copy");
    let (from, to, _) = places(&answer).into_iter().next().expect("the place");
    assert_eq!(
        (from, to),
        (10, 17),
        "places are characters: the accent is two characters and three bytes here, the emoji one character and four bytes"
    );
    let (snapshot, compose) = guards(&s);
    s.run("proof.change", &json!({ "text": "document", "from": from, "to": to, "expectedText": snapshot, "composeId": compose })).expect("changed");
    let after = body(&s);
    assert_eq!(after, "Montre\u{301}al document 🙂 merci", "the rest of the draft keeps the bytes it had, the decomposed accent included");
    assert_eq!(after.chars().filter(|c| *c == '🙂').count(), 1, "the emoji is still one character, and still there");
    assert_eq!(after.chars().count(), typed.chars().count() + 1, "the corrected word is one character longer, and nothing else changed length");
}

/// Beyond the letters above: a signature is proofread as a draft is, and a named correction of one
/// carries `signature: true` instead of a draft id — the draft's id is what a correction of a
/// message carries, and the engine will not take one for the other.
#[test]
fn a_signature_correction_carries_the_signature_flag_not_a_draft_id() {
    let (mut s, _fixture) = session("signature");
    s.run("mail.new", &json!({})).expect("a message");
    let message = body(&s);
    s.run("signature.open", &json!({ "name": "standard" })).expect("the signature");
    s.run("compose.select", &json!({ "from": 0 })).expect("the start of the signature");
    s.run("compose.type", &json!({ "text": "documet " })).expect("typed");
    let signature = body(&s);
    assert_eq!(signature, "documet Avery Quinn\nLumen Studio", "the editing commands work on the signature");

    // The message's id is not the signature's: refused, and the signature is as it was.
    let compose = s.compose_id().expect("a message is still being written");
    let wrong = json!({ "text": "document", "from": 0, "to": 7, "expectedText": signature, "composeId": compose });
    assert!(s.run("proof.change", &wrong).is_err(), "a signature correction is not a draft correction");
    assert_eq!(body(&s), signature, "and nothing of the signature changed");

    // With the flag and the identity of the open signature editor, and only with both, the
    // correction is put in.
    let id = s.signature_id().expect("the open signature editor");
    let right = json!({ "text": "document", "from": 0, "to": 7, "expectedText": signature, "signature": true, "signatureId": id });
    s.run("proof.change", &right).expect("changed");
    assert_eq!(body(&s), "document Avery Quinn\nLumen Studio");
    s.run("signature.close", &json!({ "discard": true })).expect("closed without keeping it");
    assert_eq!(body(&s), message, "the message being written is as it was");
}

/// Beyond the letters above: two signatures of the same words are still two editors. A correction
/// checked in one is refused on the other, and each editor keeps its own text.
#[test]
fn identical_signature_text_is_still_two_editors() {
    let (mut s, _fixture) = session("signature-identical");
    for name in ["Standard", "Second"] {
        s.run("signature.set", &json!({ "name": name, "text": "the documet is here" })).expect("a signature");
    }
    s.run("signature.open", &json!({ "name": "Standard" })).expect("the first");
    let first = s.signature_id().expect("an editor");
    let checked = body(&s);
    let first_correction = json!({ "text": "document", "from": 4, "to": 11, "expectedText": checked, "signature": true, "signatureId": first });
    s.run("signature.close", &json!({ "discard": true })).expect("closed");
    s.run("signature.open", &json!({ "name": "Second" })).expect("the second");
    let second = s.signature_id().expect("an editor");
    assert_ne!(second, first, "the same words are another editor");

    assert!(s.run("proof.change", &first_correction).is_err(), "the first editor's correction is late");
    assert_eq!(body(&s), "the documet is here", "the second signature is untouched");
    let second_correction = json!({ "text": "document", "from": 4, "to": 11, "expectedText": checked, "signature": true, "signatureId": second });
    s.run("proof.change", &second_correction).expect("changed");
    assert_eq!(body(&s), "the document is here");
}

/// Beyond the letters above: Review › Spelling & Grammar's walk belongs to the signature editor it
/// was started in. Closing that one and opening the same signature again leaves the old walk
/// behind, and the new editor's own correction is still one step of undo.
#[test]
fn a_signature_walk_ends_with_the_editor_it_began_in() {
    let (mut s, _fixture) = session("signature-walk");
    s.run("signature.open", &json!({ "name": "standard" })).expect("the signature");
    s.run("compose.select", &json!({ "from": 0 })).expect("the start");
    s.run("compose.type", &json!({ "text": "documet " })).expect("typed");
    // Reopening must have identical text: discard correctly drops unsaved edits, which would
    // otherwise test text changes rather than the identity of the closed editor's review.
    s.run("signature.save", &json!({})).expect("the typo fixture saved");
    let walk = s.run("review.spelling", &json!({ "again": true })).expect("the walk");
    assert_eq!(walk.get("at").and_then(|a| a.get("word")).and_then(Value::as_str), Some("documet"));
    s.run("signature.close", &json!({ "discard": true })).expect("closed");
    s.run("signature.open", &json!({ "name": "standard" })).expect("opened again");
    assert!(s.run("proof.change", &json!({ "text": "document" })).is_err(), "the closed editor's walk is refused");
    assert!(body(&s).starts_with("documet "), "and nothing changed: {:?}", body(&s));

    let id = s.signature_id().expect("the open signature editor");
    let correction = json!({ "text": "document", "from": 0, "to": 7, "expectedText": body(&s), "signature": true, "signatureId": id });
    s.run("proof.change", &correction).expect("the current editor's correction");
    assert!(body(&s).starts_with("document "));
    s.run("compose.undo", &json!({})).expect("undone");
    assert!(body(&s).starts_with("documet "), "one step back is the mistake");
    s.run("compose.redo", &json!({})).expect("redone");
    assert!(body(&s).starts_with("document "));
}
