// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! A dictionary folder is a filesystem path, not prose: `proof.set` keeps every byte it is given,
//! `dictionary_dirs` looks in exactly that folder, and only an exact empty string means the usual
//! places. A folder whose name has spaces at either end — or is nothing but spaces — is read as
//! it was named, including from a real little dictionary written to a folder of this test's own.
//!
//! Nothing here reads a real dictionary, a real mailbox, or opens a connection: the fixture is two
//! invented files in a scratch folder, and the sample mailbox and a fixed clock are enough.

use std::path::PathBuf;

use mailcraft_engine::proof::dictionary_dirs;
use mailcraft_engine::{Session, sample};
use mailcraft_time::Timestamp;
use serde_json::{Value, json};

/// The instant these tests are at.
const NOW: Timestamp = 1_750_000_000;

/// A session on the sample mailbox, with the defaults the program ships.
fn session() -> Session {
    Session::at(sample::mailbox(NOW, 0), NOW, 0)
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

/// A scratch folder this test made itself, with a space at the end of its name, taken away when the
/// test ends. Created exclusively: an existing folder is never touched, and no folder is removed
/// before it is known to be ours.
struct Fixture {
    path: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Fixture {
        let base = std::env::temp_dir();
        // A unique name plus the loop: `create_dir` fails rather than adopting a folder another
        // run (or a stale one) already made, so the folder removed in `drop` is only ever ours.
        for attempt in 0..1_000u32 {
            let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
            let candidate = base.join(format!("mailcraft-proof-paths-{}-{label}-{stamp}-{attempt} ", std::process::id()));
            match std::fs::create_dir(&candidate) {
                Ok(()) => return Fixture { path: candidate },
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("could not make a scratch folder {candidate:?}: {error}"),
            }
        }
        panic!("could not make an unused scratch folder under {base:?}");
    }

    fn write(&self, file: &str, text: &str) {
        let path = self.path.join(file);
        std::fs::write(&path, text).unwrap_or_else(|error| panic!("could not write {path:?}: {error}"));
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Both the setter and the reader keep a named folder exactly: leading and trailing spaces, a path
/// of spaces alone, and a UTF-8 name. Only the exact empty string restores the usual places.
#[test]
fn a_named_folder_is_kept_and_looked_in_exactly_as_it_was_given() {
    let mut s = session();
    let usual = dictionary_dirs(&s.prefs.proof);
    assert!(!usual.is_empty(), "the defaults name some usual places");

    for path in [" /synthetic/path ", " ", "/synthetic/dictionnaires/é ", "  /synthetic/two levels/dir  "] {
        let answer = s.run("proof.set", &json!({ "dictionaryDir": path })).expect("a folder");
        assert_eq!(answer["settings"]["dictionaryDir"].as_str(), Some(path), "{path:?} is answered back");
        assert_eq!(s.prefs.proof.dictionary_dir, path, "{path:?} is kept byte for byte");
        assert_eq!(dictionary_dirs(&s.prefs.proof), vec![PathBuf::from(path)], "{path:?} is the one folder looked in");
    }

    // A path of spaces alone is a folder, not a way of saying "the usual places".
    assert_ne!(dictionary_dirs(&s.prefs.proof), usual);

    // Only the exact empty string means no folder was named.
    s.run("proof.set", &json!({ "dictionaryDir": "" })).expect("the usual places");
    assert_eq!(s.prefs.proof.dictionary_dir, "");
    assert_eq!(dictionary_dirs(&s.prefs.proof), usual, "an exact empty string restores the usual places");
}

/// A NUL or a path past the limit is refused before anything is kept: the refusals are atomic, so
/// the other settings sent in the same call are untouched too.
#[test]
fn a_nul_or_an_overlong_folder_is_refused_and_changes_nothing() {
    let mut s = session();
    s.run(
        "proof.set",
        &json!({
            "checkSpelling": false,
            "checkGrammar": false,
            "asYouType": false,
            "languages": ["en-CA"],
            "dictionaryDir": "/synthetic/named",
        }),
    )
    .expect("the starting settings");
    let before = s.prefs.proof.clone();

    let nul = s.run(
        "proof.set",
        &json!({
            "dictionaryDir": "/synthetic/named\0hidden",
            "checkSpelling": true,
            "checkGrammar": true,
            "asYouType": true,
            "languages": ["fr-CA"],
        }),
    );
    let why = nul.expect_err("a folder holding a NUL is refused").to_string();
    assert!(why.contains("cannot be used"), "the refusal says why: {why}");
    assert_eq!(s.prefs.proof, before, "a refused NUL leaves every setting as it was");

    // 1,025 bytes: one past what a setting keeps. Refused whole, never shortened.
    let long = format!("/synthetic/{}", "x".repeat(1_025 - "/synthetic/".len()));
    assert_eq!(long.len(), 1_025);
    let overlong = s.run("proof.set", &json!({ "dictionaryDir": long, "asYouType": true }));
    let why = overlong.expect_err("a folder past the limit is refused").to_string();
    assert!(why.contains("too long"), "the refusal says why: {why}");
    assert_eq!(s.prefs.proof, before, "a refused overlong folder leaves every setting as it was");
}

/// A real little dictionary, in a folder whose name ends in a space, is found and read: its word is
/// accepted by `proof.check`, and a word it does not have is flagged.
#[test]
fn a_dictionary_in_a_folder_ending_in_a_space_is_read() {
    let fixture = Fixture::new("read");
    fixture.write("en_CA.aff", "SET UTF-8\n");
    fixture.write("en_CA.dic", "3\nthe\ncolour\ndocument\n");
    let dir = fixture.path().to_string_lossy().to_string();
    assert!(dir.ends_with(' '), "the fixture's folder ends in a space: {dir:?}");

    let mut s = session();
    s.run("proof.set", &json!({ "languages": ["en-CA"], "dictionaryDir": dir })).expect("the folder");
    assert_eq!(s.prefs.proof.dictionary_dir, dir, "the space at the end is kept");

    let status = s.run("proof.status", &json!({})).expect("the settings");
    assert_eq!(status["settings"]["dictionaryDir"].as_str(), Some(dir.as_str()));
    assert_eq!(status["loaded"].as_bool(), Some(true), "the dictionary was read");
    let read: Vec<&str> = status["read"].as_array().map(|list| list.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    assert!(read.contains(&"en-CA"), "Canadian English was read: {read:?}");

    // The reader really used the fixture's words, not a dictionary installed on this computer.
    s.run("mail.new", &json!({})).expect("a message");
    let answer = s.run("proof.check", &json!({ "text": "colour Zorblax" })).expect("a check");
    assert_eq!(flagged(&answer), ["Zorblax"], "the fixture's word is a word; the one it lacks is not");
}
