// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Proofing's own tests: the words it looks at, the settings it keeps, and a dictionary it reads.
//!
//! The dictionaries here are invented ones (`example` words), so that what a test says a word is
//! never changes with a dictionary's version or with what this computer has installed.

use super::*;

/// A small dictionary of two languages, as a file would say it, with affixes.
fn english() -> &'static str {
    "5\ndocument/S\ndocuments/S\ncolour\ncentre\nneighbour\n"
}

fn french() -> &'static str {
    "4\ndocument/S\nfrançais\naujourd'hui\nMontréal\nQuébec\n"
}

const AFFIXES: &str = "SET UTF-8\nFLAG long\nSFX S Y 1\nSFX S 0 s .\n";

fn both() -> Speller {
    let mut s = Speller::new();
    if let Ok(d) = Dictionary::from_dic(Language::EnCa, english()) {
        s.put(d);
    }
    if let Ok(d) = Dictionary::from_dic(Language::FrCa, french()) {
        s.put(d);
    }
    s
}

#[test]
fn keys_fold_case_and_apostrophes() {
    assert_eq!(key("Aujourd’hui"), key("aujourd'hui"));
    assert_eq!(key("MONTRÉAL"), "montréal");
}

#[test]
fn canonically_equivalent_accents_keep_their_original_ranges() {
    let decomposed = "Montre\u{301}al";
    let composed = "Montréal";
    assert_eq!(key(decomposed), key(composed));
    let s = both();
    assert!(s.is_word(decomposed));
    let text = format!("{decomposed} franc\u{327}ais");
    let tokens = words(&text);
    assert_eq!(tokens.len(), 2);
    assert_eq!(tokens[0].text, decomposed);
    assert!(s.check(&text).is_empty());
    for token in tokens {
        assert_eq!(text.get(token.span.start..token.span.end), Some(token.text));
    }
    let mut settings = ProofSettings::default();
    assert!(settings.add_word(composed));
    assert!(!settings.add_word(decomposed));
}

#[test]
fn codes_round_trip() {
    for l in Language::ALL {
        assert_eq!(Language::parse(l.code()), Some(l), "{l:?}");
    }
    assert_eq!(Language::parse("en_CA"), Some(Language::EnCa));
    assert_eq!(Language::parse("fr"), Some(Language::FrCa));
    assert_eq!(Language::parse("en-GB"), None);
    assert_eq!(Language::parse(""), None);
}

#[test]
fn legacy_settings_do_not_select_a_managed_server_or_replace_the_manual_address() {
    let manual = "http://127.0.0.1:18081";
    let settings: ProofSettings = serde_json::from_value(serde_json::json!({"grammarUrl": manual, "checkGrammar": true})).unwrap();
    let encoded = serde_json::to_value(&settings).unwrap();
    assert_eq!(encoded.get("managedServer"), Some(&serde_json::json!(false)));
    assert_eq!(settings.grammar_url, manual);
    let managed: ProofSettings = serde_json::from_value(serde_json::json!({"grammarUrl": manual, "managedServer": true})).unwrap();
    assert_eq!(serde_json::to_value(&managed).unwrap().get("managedServer"), Some(&serde_json::json!(true)));
    assert_eq!(managed.grammar_url, manual);
    assert!(serde_json::from_value::<ProofSettings>(serde_json::json!({"managedServer": "yes"})).is_err());
}

#[test]
fn only_this_machine_however_asked() {
    assert!(address_allowed("http://127.0.0.1:8081", false));
    assert!(address_allowed("http://localhost:8081/v2/check", false));
    assert!(address_allowed("http://[::1]:8081", false));
    assert!(!address_allowed("http://localhost.example.com:8081", false));
    assert!(!address_allowed("http://192.168.1.9:8081", false));
    assert!(!address_allowed("https://languagetool.example", false));
    // A secure connection has no resolved-address filter: only a literal loopback address is
    // this machine's there, and a name is resolved by somebody else.
    assert!(address_allowed("https://127.0.0.1:8081", false));
    assert!(address_allowed("https://[::1]:8081/v2/check", false));
    assert!(!address_allowed("https://localhost:8081", false));
    assert!(!address_allowed("https://ip6-localhost:8081", false));
    // An older settings file's `allowRemote` is read and grants nothing: a server that is not this
    // computer is refused however it is asked for.
    assert!(!address_allowed("https://languagetool.example", true));
    assert!(address_allowed("http://127.0.0.1:8081", true), "this computer is still allowed");
    assert!(!address_allowed("file:///etc/passwd", true));
    assert!(!address_allowed("ftp://example.org", true));
    assert!(!address_allowed("", false));
}

#[test]
fn a_secure_grammar_address_is_a_literal_loopback_address() {
    // HTTPS is dialled without the plain connection's resolved-address filter, so a name never
    // proves the connection stays on this computer, and a legacy numeric spelling is normalized
    // only by the link reader. What is left is an address written as a loopback IP literal, which
    // the transport dials without looking anything up.
    for url in [
        "https://127.0.0.1",
        "https://127.0.0.1:8081",
        "https://127.0.0.1:8081/v2/check",
        "https://127.0.0.2:8081",
        "https://[::1]",
        "https://[::1]:8081/v2/check",
        "HTTPS://127.0.0.1:8081",
        "https://127.0.0.1:8081/",
    ] {
        assert!(address_allowed(url, false), "{url:?} is a literal loopback address");
    }
    for url in [
        // The names this computer is called by are not addresses: each is whatever a look-up says.
        "https://localhost",
        "https://localhost:8081",
        "https://LOCALHOST:8081/v2/check",
        "https://ip6-localhost",
        "https://ip6-localhost:8081",
        // Forms the link reader would normalize to `127.0.0.1`; the transport keeps them as names.
        "https://2130706433:8081",
        "https://127.1:8081",
        "https://0177.0.0.1:8081",
        "https://127.0.0.1.:8081",
        // Other machines, by address or by name, secure or not.
        "https://192.168.1.9:8081",
        "https://10.0.0.1:8081",
        "https://localhost.example.com:8081",
        "https://languagetool.example",
        // Credentials, a query and a fragment are no more an address over HTTPS.
        "https://user@127.0.0.1:8081",
        "https://user:secret@127.0.0.1:8081",
        "https://127.0.0.1:8081/?x=1",
        "https://127.0.0.1:8081/#top",
        "https://[::1]:8081/?x=1",
        // A port that is not one, however it is written.
        "https://127.0.0.1:notaport",
        "https://127.0.0.1:99999",
        "https://127.0.0.1:",
    ] {
        assert!(!address_allowed(url, false), "{url:?} is not a literal loopback HTTPS address");
        assert!(!address_allowed(url, true), "{url:?} is refused even when a legacy setting asks");
    }
}

#[test]
fn words_are_runs_of_letters_with_their_places() {
    let text = "Bonjour Pierre, I reviewed les documents hier soir.";
    let got: Vec<&str> = words(text).iter().map(|w| w.text).collect();
    assert_eq!(got, ["Bonjour", "Pierre", "I", "reviewed", "les", "documents", "hier", "soir"]);
    for w in words(text) {
        assert_eq!(text.get(w.span.start..w.span.end), Some(w.text), "a word's place is its text");
    }
}

#[test]
fn addresses_are_left_alone() {
    let text = "Write to pierre@example.org about https://example.org/a today, or http://x.example/y.";
    let got: Vec<&str> = words(text).iter().map(|w| w.text).collect();
    assert_eq!(got, ["Write", "to", "about", "today", "or"], "the addresses are left out, the words around them are not");
}

#[test]
fn a_picture_and_a_long_run_are_not_words() {
    let long = "a".repeat(MAX_WORD + 1);
    let text = format!("ok \u{fffc} {long} 12345");
    let got: Vec<&str> = words(&text).iter().map(|w| w.text).collect();
    assert_eq!(got, ["ok"]);
}

#[test]
fn personal_words_never_repeat() {
    let mut s = ProofSettings::default();
    assert!(s.add_word("Hochelaga"));
    assert!(!s.add_word("hochelaga"), "the same word in another case is the same word");
    assert!(s.add_word("aujourd’hui"));
    assert_eq!(s.personal_words().len(), 2);
    assert!(s.remove_word("HOCHELAGA"));
    assert_eq!(s.personal_words(), ["aujourd’hui"]);
    assert!(!s.add_word(""), "nothing is not a word");
    assert!(!s.add_word("12345"), "a number is not a word");
}

#[test]
fn personal_words_refuse_internal_whitespace_and_control_characters() {
    // A word is what a token is: a run of letters. A space or a control character inside one is
    // not part of a word, and a personal entry that holds one could never match a token, so it is
    // refused rather than stored where it would quietly do nothing.
    let mut s = ProofSettings::default();
    for bad in ["Hoc helaga", "Hoc\thelaga", "Hoc\u{0}helaga", "Hoc\nhelaga", "Hoc\u{85}helaga"] {
        assert!(!s.add_word(bad), "{bad:?} is not one word");
    }
    assert!(s.personal.is_empty());
    // A settings file written by hand, or by a version that did not check: the bad entries are
    // left out of the words that are used, and the good one is kept.
    s.personal = vec!["Hoc helaga".into(), "Hoc\u{7}helaga".into(), "Hochelaga".into()];
    assert_eq!(s.personal_words(), ["Hochelaga"]);
    // A hyphen and an apostrophe join the parts of one word, and are still words.
    assert!(s.add_word("rendez-vous"));
    assert!(s.add_word("aujourd’hui"));
}

#[test]
fn a_grammar_address_is_this_computer_and_nothing_else() {
    // The legacy field is read from an older settings file and never grants anything: an address
    // that is not this computer's is refused whatever it says.
    let mut s = ProofSettings { allow_remote: true, grammar_url: "https://languagetool.example".into(), ..ProofSettings::default() };
    assert!(!s.grammar_allowed(), "a legacy allowRemote does not let a message out");
    s.grammar_url = "http://192.168.1.9:8081".into();
    assert!(!s.grammar_allowed());
    s.grammar_url = "http://127.0.0.1:8081".into();
    assert!(s.grammar_allowed(), "this computer is still this computer");

    // Only a plain address to this machine: no user information, no query, no fragment, no
    // broken brackets and no broken port. Over HTTPS only a literal loopback address is one.
    let allowed =
        ["http://127.0.0.1:8081", "http://localhost:8081/v2/check", "http://[::1]:8081", "https://127.0.0.1:8081", "https://[::1]:8081/v2/check"];
    for url in allowed {
        assert!(address_allowed(url, false), "{url:?} is this computer");
    }
    let refused = [
        "http://user@127.0.0.1:8081",
        "http://user:secret@localhost:8081/v2/check",
        "http://127.0.0.1:8081/?language=auto",
        "http://127.0.0.1:8081/#answer",
        "http://[::1:8081",
        "http://[::1]x:8081",
        "http://[127.0.0.1]:8081",
        "http://127.0.0.1:notaport",
        "http://127.0.0.1:99999",
        "http://127.0.0.1:",
        "http://::1:8081",
        "http://127.0.0.1:8081:9",
        "https://languagetool.example",
        "https://LOCALHOST:8081/v2/check",
        "https://localhost:8081",
        "https://ip6-localhost:8081",
        "https://2130706433:8081",
        "https://127.1:8081",
        "https://user@127.0.0.1:8081",
        "https://127.0.0.1:8081/?language=auto",
        "https://127.0.0.1:8081/#answer",
        "http://192.168.1.9:8081",
        "http://localhost.example.com:8081",
    ];
    for url in refused {
        assert!(!address_allowed(url, false), "{url:?} is not a plain loopback address");
        assert!(!address_allowed(url, true), "{url:?} is refused even when asked");
    }
}

#[test]
fn an_older_settings_file_with_allow_remote_still_refuses_a_remote_server() {
    let legacy: ProofSettings =
        serde_json::from_str(r#"{"grammarUrl":"https://languagetool.example","allowRemote":true}"#).expect("an older settings file");
    assert!(legacy.allow_remote, "the field is still read, for compatibility");
    assert!(!legacy.grammar_allowed(), "and it never lets anything out");
    let loopback: ProofSettings =
        serde_json::from_str(r#"{"grammarUrl":"http://127.0.0.1:8081","allowRemote":true}"#).expect("an older settings file");
    assert!(loopback.grammar_allowed());
}

#[test]
fn a_server_suggestion_is_cut_before_it_is_kept() {
    // An answer from outside is hostile: a suggestion or a message of megabytes must not be kept
    // whole, and must not be measured whole either.
    let huge = "x".repeat(1_000_000);
    let d = Diagnostic::new(Span { start: 0, end: 1 }, Kind::Grammar, Source::Server).suggesting([huge.clone(), huge.clone()]).saying(&huge);
    assert_eq!(d.suggestions.len(), 1, "the same suggestion twice is one");
    assert!(d.suggestions[0].chars().count() <= MAX_TEXT, "a suggestion is cut: {}", d.suggestions[0].chars().count());
    assert!(d.message.chars().count() <= 300, "a message is cut: {}", d.message.chars().count());
}

#[test]
fn long_runs_of_address_marks_are_skipped_quickly() {
    // Forty-five thousand `@` and as many `/`, one run each, inside the first `MAX_CHECKED`
    // characters (so the words around them are looked at too): each character of a run belongs to
    // the same address, so looking at the run once is enough. Looking at it from every character
    // is the square of its length — billions of steps — which a message may not spend on one line.
    let text = format!("ok {} word {} done", "@".repeat(45_000), "/".repeat(45_000));
    assert!(text.chars().count() < MAX_CHECKED, "the words around the runs are inside the checked text");
    let started = std::time::Instant::now();
    let got: Vec<&str> = words(&text).iter().map(|w| w.text).collect();
    let spent = started.elapsed();
    assert_eq!(got, ["ok", "word", "done"], "the words around the runs are still words");
    assert!(spent < std::time::Duration::from_secs(2), "a run of address marks is looked at once: {spent:?}");
}

#[test]
fn several_markers_in_one_address_are_one_place() {
    // An address that is a web address and an e-mail address at once: one run, one place left
    // alone, whatever it contains.
    let text = "see https://www.example.org/a@b.example/c and write to pierre@example.org now";
    let got: Vec<&str> = words(text).iter().map(|w| w.text).collect();
    assert_eq!(got, ["see", "and", "write", "to", "now"]);
}

#[test]
fn settings_survive_a_round_trip_and_unknown_fields_are_kept_kindly() {
    let mut s = ProofSettings::default();
    assert!(s.add_word("Hochelaga"));
    s.check_grammar = true;
    s.languages = vec![Language::FrCa, Language::EnUs];
    let text = serde_json::to_string(&s).expect("a settings file");
    let back: ProofSettings = serde_json::from_str(&text).expect("the same settings");
    assert_eq!(back, s);
    // A settings file of an older version, or one edited by hand: what is missing is default.
    let old: ProofSettings = serde_json::from_str(r#"{"checkSpelling":false}"#).expect("older settings");
    assert!(!old.check_spelling);
    assert_eq!(old.languages, ProofSettings::default().languages);
}

#[test]
fn a_word_is_right_when_either_language_says_so() {
    let s = both();
    assert_eq!(s.accept("colour"), Some(Language::EnCa));
    assert_eq!(s.accept("couleur"), None, "not a word of either dictionary here");
    assert_eq!(s.accept("Montréal"), Some(Language::FrCa));
    assert_eq!(s.accept("français"), Some(Language::FrCa));
    assert_eq!(s.accept("aujourd'hui"), Some(Language::FrCa));
    assert_eq!(s.accept("aujourd’hui"), Some(Language::FrCa), "the typographic apostrophe is the same word");
    assert_eq!(s.accept("centre"), Some(Language::EnCa));
    assert_eq!(s.accept("neighbour"), Some(Language::EnCa));
    assert_eq!(s.accept("quebec"), None, "a missing accent is another spelling");
}

#[test]
fn a_mixed_sentence_is_flagged_only_where_it_is_wrong() {
    let s = both();
    let text = "Bonjour Pierre, I reviewed les documents hier soir and everything looks correct.";
    let got = s.check(text);
    let flagged: Vec<&str> = got.iter().filter_map(|d| text.get(d.span.start..d.span.end)).collect();
    assert_eq!(
        flagged,
        ["Bonjour", "Pierre", "I", "reviewed", "les", "hier", "soir", "and", "everything", "looks", "correct"],
        "the test dictionary is small; what matters is that no word of either language is flagged for its language"
    );
}

#[test]
fn a_hyphenated_word_is_checked_by_its_halves() {
    let mut s = Speller::new();
    if let Ok(d) = Dictionary::from_dic(Language::FrCa, "2\nrendez\nvous\n") {
        s.put(d);
    }
    assert!(s.is_word("rendez-vous"));
    let text = "rendez-vuos";
    let got = s.check(text);
    assert_eq!(got.len(), 1);
    let d = got.first().expect("one");
    assert_eq!(text.get(d.span.start..d.span.end), Some("vuos"), "only the half that is wrong is flagged");
}

#[test]
fn suggestions_are_words_of_the_dictionary() {
    let s = both();
    let got = s.suggest("documant", 5);
    assert!(got.contains(&"document".to_string()), "{got:?}");
    let got = s.suggest("montrél", 5);
    assert!(got.iter().any(|w| w.eq_ignore_ascii_case("montréal")), "{got:?}");
    // A word written with a capital comes back with one.
    let got = s.suggest("Montrél", 5);
    assert!(got.iter().any(|w| w.starts_with("Montréal") || w.starts_with("Montréal,")), "{got:?}");
}

#[test]
fn affixes_make_words_of_stems() {
    let d = Dictionary::from_hunspell(Language::EnCa, AFFIXES, "2\ndocument/S\ndocument\n").expect("a dictionary");
    assert!(d.holds("document"));
    assert!(d.holds("documents"), "the suffix's word is a word");
    assert!(!d.holds("documented"));
}

#[test]
fn a_dictionary_that_is_not_one_is_an_error() {
    assert!(Dictionary::from_dic(Language::EnCa, "").is_err());
    assert!(Dictionary::from_dic(Language::EnCa, "0\n").map_or(true, |d| d.is_empty()), "a dictionary of no words is none");
    // A line that cannot be read is skipped, not fatal.
    let d = Dictionary::from_dic(Language::FrCa, "2\nfrançais\n\u{0}//\n").expect("a dictionary with a bad line");
    assert_eq!(d.len(), 1);
}

#[test]
fn a_real_dictionary_reads_within_its_caps() {
    // A dictionary as a package installs one: a count, stems with flags, and affixes.
    let aff = "SET UTF-8\nFLAG num\nNEEDAFFIX 500\nSFX 101 Y 2\nSFX 101 0 s [^y]\nSFX 101 y ies y\nPFX 102 Y 1\nPFX 102 0 re .\n";
    let dic = "4\ncolour/101\ndocument/101\nhappy/101\nwrite/102\n";
    let d = Dictionary::from_hunspell(Language::EnCa, aff, dic).expect("a dictionary");
    for w in ["colour", "colours", "document", "documents", "happy", "happies", "rewrite"] {
        assert!(d.holds(w), "{w} should be a word of it");
    }
    assert!(!d.holds("happys"), "the condition keeps the `y` rules apart");
}

#[test]
fn a_word_check_is_not_a_sentence_check() {
    // The text is longer than the cap: only its first characters are looked at, and nothing panics.
    let text = "bonjour ".repeat(MAX_CHECKED);
    let s = both();
    let got = s.check(&text);
    assert!(got.len() <= MAX_DIAGNOSTICS);
}

#[test]
fn every_span_of_a_diagnostic_fits_its_text() {
    let s = both();
    for text in ["Bonjour, I reviewed les documents hier soir.", "a\u{300}b\u{fffd}\u{1F600} 🙂 ok", "«Ça va?» dit-il, en français.", "  ", ""]
    {
        for d in s.check(text) {
            assert!(d.span.fits(text), "{d:?} does not fit {text:?}");
        }
    }
}
