// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Asking a grammar server for a check, against stand-in servers on this machine: one that
//! answers a LanguageTool 6.6-shaped response, one that refuses, one that hangs up, one that is
//! not there at all, one that is too slow, one whose answer is too long, and one that is a
//! redirect. The real-provider tests reach a real LanguageTool only when the owner names its
//! address in `MAILCRAFT_LT_TEST_URL`; no port is guessed at, and no server is ever started or
//! stopped by these tests.

use std::time::{Duration, Instant};

use mailcraft_model::proof::{Diagnostic, Kind, Language, MAX_ANSWER, MAX_CHECKED, Source, Span, parse_answer};

use crate::fake::{FakeHttp, json_answer};
use crate::http::pairs;
use crate::languagetool::*;

/// Long enough for anything on this machine.
const SOON: Duration = Duration::from_secs(5);

const TEXT: &str = "Les document sont important.";

/// A match as LanguageTool 6.6 writes it: every required field of the `languagetool-rust` crate's
/// `Match` type, including the unstable ones (`contextForSureMatch`, `ignoreForIncompleteSentence`,
/// `type`). `segment` is the text that was checked, so the context offsets are the ones the server
/// writes.
fn a_match(segment: &str, message: &str, rule: &str, issue_type: &str, offset: usize, length: usize, replacement: &str) -> String {
    format!(
        r#"{{"message":"{message}","shortMessage":"","replacements":[{{"value":"{replacement}"}}],"offset":{offset},"length":{length},"context":{{"text":"{segment}","offset":{offset},"length":{length}}},"sentence":"{segment}","type":{{"typeName":"Other"}},"rule":{{"id":"{rule}","description":"A rule","issueType":"{issue_type}","category":{{"id":"GRAMMAR","name":"Grammar"}}}},"ignoreForIncompleteSentence":false,"contextForSureMatch":0}}"#
    )
}

/// The same match without its `context`, which is what an answer from a broken or hostile server
/// may hold beside a match that is perfectly good.
fn a_match_without_context(message: &str, rule: &str, offset: usize, length: usize) -> String {
    format!(
        r#"{{"message":"{message}","shortMessage":"","replacements":[],"offset":{offset},"length":{length},"sentence":"","type":{{"typeName":"Other"}},"rule":{{"id":"{rule}","description":"A rule","issueType":"grammar","category":{{"id":"GRAMMAR","name":"Grammar"}}}},"ignoreForIncompleteSentence":false,"contextForSureMatch":0}}"#
    )
}

/// A whole `POST /v2/check` answer, as LanguageTool 6.6 writes it. `detected` is what the
/// detector says, which is what a caller must trust or drop; `confidence` is its own measure.
fn response(language_code: &str, language_name: &str, detected: &str, confidence: f64, matches: &str) -> String {
    format!(
        r#"{{"software":{{"name":"LanguageTool","version":"6.6","buildDate":"2025-03-27 20:50:25 +0100","apiVersion":1,"premium":false,"premiumHint":"","status":""}},"warnings":{{"incompleteResults":false}},"language":{{"name":"{language_name}","code":"{language_code}","detectedLanguage":{{"name":"{language_name}","code":"{detected}","confidence":{confidence},"source":"ngram"}}}},"matches":[{matches}]}}"#
    )
}

/// The French answer used by most tests: `document` is wrong in "Les document sont important.".
fn french_answer() -> String {
    response("fr-CA", "French (Canada)", "fr", 0.99, &a_match(TEXT, "Accord", "FR_ACCORD", "grammar", 4, 8, "documents"))
}

/// The same answer with the server's own word that it did not look at everything
/// (`warnings.incompleteResults`, the field of the `languagetool-rust` crate's `Warnings` type).
fn marked_incomplete(answer: &str) -> String {
    answer.replace("\"incompleteResults\":false", "\"incompleteResults\":true")
}

#[test]
fn a_check_is_asked_and_its_answer_given_back() {
    let fake = FakeHttp::answering(json_answer("200 OK", &french_answer()));
    let got = check(&fake.url(""), TEXT, "fr-CA", SOON).expect("an answer");
    assert_eq!(got, french_answer().as_bytes());

    let request = fake.request(0);
    let (head, form) = request.split_once("\r\n\r\n").expect("a head and a body");
    assert!(head.starts_with("POST /v2/check HTTP/1.1\r\n"), "{head}");
    assert!(head.contains("Content-Type: application/x-www-form-urlencoded\r\n"), "{head}");
    assert!(head.contains("Host: 127.0.0.1:"), "{head}");
    // Nothing of this program but its name, and no cookie or credential of any kind.
    for never in ["cookie:", "authorization:", "referer:", "accept-encoding:"] {
        assert!(!head.to_ascii_lowercase().contains(never), "{never} in {head}");
    }
    // The form is the crate's `Request` own fields: the text, the language, and the level. That
    // type writes `enabledOnly` only when it is true, and this check does not enable it.
    let asked_for = pairs(form);
    assert_eq!(asked_for.len(), 3, "{form}");
    assert_eq!(asked_for.first().map(|(name, value)| (name.as_str(), value.as_str())), Some(("text", TEXT)));
    assert_eq!(asked_for.get(1).map(|(name, value)| (name.as_str(), value.as_str())), Some(("language", "fr-CA")));
    assert_eq!(asked_for.get(2).map(|(name, value)| (name.as_str(), value.as_str())), Some(("level", "picky")));
    // And what came back is what the model's reader makes a diagnostic of.
    let got = parse_answer(&got, TEXT, Language::FrCa, &[]).expect("an answer");
    assert_eq!(got.len(), 1);
    let span = got.first().expect("one").span;
    assert_eq!(TEXT.get(span.start..span.end), Some("document"));
}

#[test]
fn the_text_goes_whole_whatever_it_holds() {
    // A `&`, an `=`, a `%` and a space in the message never change what the request says.
    let text = "Bonjour & à bientôt = 100%";
    let fake = FakeHttp::answering(json_answer("200 OK", &response("fr-CA", "French (Canada)", "fr", 0.99, "")));
    check(&fake.url("/"), text, "fr-CA", SOON).expect("an answer");
    let request = fake.request(0);
    let (_, form) = request.split_once("\r\n\r\n").expect("a head and a body");
    assert_eq!(form, "text=Bonjour+%26+%C3%A0+bient%C3%B4t+%3D+100%25&language=fr-CA&level=picky");
    assert_eq!(pairs(form).first().map(|(name, value)| (name.as_str(), value.as_str())), Some(("text", text)));
}

#[test]
fn a_trailing_slash_does_not_double_the_path() {
    let fake = FakeHttp::answering(json_answer("200 OK", &french_answer()));
    for base in [fake.url(""), fake.url("/"), format!("{}//", fake.url(""))] {
        check(&base, "Bonjour.", "fr-CA", SOON).expect("an answer");
    }
    assert_eq!(fake.asked(), 3);
    for n in 0..3 {
        assert!(fake.request(n).starts_with("POST /v2/check HTTP/1.1\r\n"), "{}", fake.request(n));
    }
}

#[test]
fn a_server_on_another_machine_is_not_spoken_to_at_all() {
    let fake = FakeHttp::answering(json_answer("200 OK", &french_answer()));
    for bad in [
        // Plain http to a machine that is not this one, by address or by name.
        "http://192.0.2.7:8081",
        "http://languagetool.example",
        // Secure or not, a server on another machine is told nothing of the message here.
        "https://languagetool.example/v2/check",
        // What is no web address at all.
        "ftp://127.0.0.1:8081",
        "file:///etc/passwd",
        "127.0.0.1:8081",
        "//127.0.0.1:8081",
        "",
        "   ",
        // An address with a query or a fragment is not where a check is posted.
        "http://127.0.0.1:8081/?x=1",
        "http://127.0.0.1:8081/#top",
        // A name and a password in the address are nobody's here.
        "http://user:secret@127.0.0.1:8081",
    ] {
        assert_eq!(check(bad, TEXT, "fr-CA", SOON), Err(LanguageToolError::Refused), "{bad}");
    }
    // The stand-in server itself, through an address that is not one: not a byte of a request
    // reaches it, so the refusal happened before any connection.
    assert_eq!(check(&format!("http://user@{}/", fake.url("")), TEXT, "fr-CA", SOON), Err(LanguageToolError::Refused));
    assert_eq!(fake.asked(), 0);
}

#[test]
fn a_secure_address_that_is_not_a_literal_loopback_is_refused_before_a_connection() {
    // A listener holds a port open. If any of these had been dialled, the connection would be
    // waiting in its queue for `accept`; none is, because each was refused before a look-up.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
    listener.set_nonblocking(true).expect("a non-blocking listener");
    let port = listener.local_addr().expect("an address").port();
    let bad = [
        // The names this computer is called by: a secure connection follows the look-up.
        format!("https://localhost:{port}"),
        format!("https://LOCALHOST:{port}"),
        format!("https://ip6-localhost:{port}"),
        // Legacy numeric forms, which the model's link reader would normalize to `127.0.0.1` but
        // the transport keeps as names.
        format!("https://2130706433:{port}"),
        format!("https://127.1:{port}"),
        format!("https://127.0.0.1.:{port}"),
        // Credentials, a query and a fragment are not a place a check is posted either.
        format!("https://user@{port}"),
        format!("https://user:secret@127.0.0.1:{port}"),
        format!("https://127.0.0.1:{port}/?x=1"),
        format!("https://127.0.0.1:{port}/#top"),
    ];
    for url in &bad {
        assert_eq!(check(url, TEXT, "fr-CA", SOON), Err(LanguageToolError::Refused), "{url}");
        assert_eq!(check_multilingual(url, TEXT, &[Language::FrCa], &[], SOON), Err(LanguageToolError::Refused), "{url}");
        assert_eq!(ready(url, SOON), Err(LanguageToolError::Refused), "{url}");
    }
    match listener.accept() {
        Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock, "{error}"),
        Ok((_, from)) => panic!("a refused address connected from {from}"),
    }
}

#[test]
fn a_secure_literal_loopback_reaches_the_transport() {
    // A port with nobody on it: bound once, so that it was free, and let go. The literal address
    // is allowed through the policy, so the connection is attempted and refused — the error is
    // `Unreachable`, never `Refused`, which is what a refused-before-connecting address gives.
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
        listener.local_addr().expect("an address").port()
    };
    let url = format!("https://127.0.0.1:{port}");
    assert_eq!(check(&url, TEXT, "fr-CA", SOON), Err(LanguageToolError::Unreachable), "{url}");
    assert_eq!(check_multilingual(&url, TEXT, &[Language::FrCa], &[], SOON), Err(LanguageToolError::Unreachable), "{url}");
    assert_eq!(ready(&url, SOON), Err(LanguageToolError::Unreachable), "{url}");
}

#[test]
fn the_transport_host_policy_allows_only_a_literal_loopback_over_https() {
    // The model reads a host the way a link does, normalizing a legacy number to a dotted address.
    // The transport keeps the host as written and then resolves it, so the module holds the host
    // the transport will really dial to its own rule: HTTPS is a literal loopback address only.
    let allowed = ["https://127.0.0.1:8081", "https://127.0.0.2:8081", "https://[::1]:8081", "http://127.0.0.1:8081", "http://localhost:8081"];
    for url in allowed {
        let at = crate::http::Url::parse(url).expect("a transport address");
        assert!(transport_allowed(&at), "{url}");
    }
    let refused = [
        "https://localhost:8081",
        "https://LOCALHOST:8081",
        "https://ip6-localhost:8081",
        "https://2130706433:8081",
        "https://127.1:8081",
        "https://127.0.0.1.:8081",
    ];
    for url in refused {
        let at = crate::http::Url::parse(url).expect("a transport address");
        assert!(!transport_allowed(&at), "{url}");
    }
}

#[test]
fn plain_http_to_this_machine_by_name_still_works() {
    // The plain connection keeps its resolved-address filter, so a name is still this computer's
    // there; moving the secure rule must not take that away.
    let fake = FakeHttp::answering(json_answer("200 OK", &french_answer()));
    let by_name = format!("http://localhost:{}", fake.port);
    check(&by_name, TEXT, "fr-CA", SOON).expect("a check by name");
    let outcome = check_multilingual(&by_name, TEXT, &[Language::FrCa], &[], SOON).expect("a multilingual check by name");
    assert_eq!(outcome.checked_segments, 1);
    let fake = FakeHttp::answering(json_answer("200 OK", &languages_answer()));
    ready(&format!("http://localhost:{}", fake.port), SOON).expect("a readiness probe by name");
}

#[test]
fn a_language_that_is_not_one_is_refused_before_a_connection() {
    let fake = FakeHttp::answering(json_answer("200 OK", &french_answer()));
    for bad in ["", "xx-YY", "tlh", "en-CA-et-plus"] {
        assert_eq!(check(&fake.url(""), TEXT, bad, SOON), Err(LanguageToolError::Language), "{bad}");
    }
    assert_eq!(fake.asked(), 0);
}

#[test]
fn a_server_that_says_it_cannot_is_an_error() {
    for status in ["500 Internal Server Error", "400 Bad Request", "429 Too Many Requests", "503 Service Unavailable"] {
        let fake = FakeHttp::answering(json_answer(status, "no"));
        assert_eq!(check(&fake.url(""), TEXT, "fr-CA", SOON), Err(LanguageToolError::Failed), "{status}");
        assert_eq!(fake.asked(), 1, "{status}");
    }
}

#[test]
fn a_server_that_hangs_up_without_an_answer_is_told_apart() {
    let fake = FakeHttp::answering(Vec::new());
    assert_eq!(check(&fake.url(""), TEXT, "fr-CA", SOON), Err(LanguageToolError::Closed));
    assert_eq!(fake.asked(), 1);
    // Half an answer is no answer either.
    let fake = FakeHttp::answering(b"HTTP/1.1 200 OK\r\nContent-Length: 99\r\n\r\n{\"matches\"".to_vec());
    assert!(matches!(check(&fake.url(""), TEXT, "fr-CA", SOON), Err(LanguageToolError::Closed | LanguageToolError::Garbled)));
}

#[test]
fn an_answer_that_is_not_json_is_given_back_for_the_reader_to_refuse() {
    // Asking worked; what came back is the model's to read, and it refuses it there.
    let fake = FakeHttp::answering(json_answer("200 OK", "<html>not a check</html>"));
    let body = check(&fake.url(""), TEXT, "fr-CA", SOON).expect("bytes");
    assert!(parse_answer(&body, TEXT, Language::FrCa, &[]).is_err());
    // The multilingual caller reads its own answer, and refuses the same bytes there.
    assert_eq!(check_multilingual(&fake.url(""), TEXT, &[Language::FrCa], &[], SOON), Err(LanguageToolError::Garbled));
}

#[test]
fn an_answer_longer_than_a_grammar_servers_may_be_is_not_taken() {
    let full = format!(r#"{{"matches":[],"pad":"{}"}}"#, "x".repeat(MAX_ANSWER));
    let fake = FakeHttp::answering(json_answer("200 OK", &full));
    assert_eq!(check(&fake.url(""), TEXT, "fr-CA", SOON), Err(LanguageToolError::TooLong));
    assert_eq!(check_multilingual(&fake.url(""), TEXT, &[Language::FrCa], &[], SOON), Err(LanguageToolError::TooLong));
    // As much as one may be is read whole.
    let fits = "x".repeat(MAX_ANSWER);
    let fake = FakeHttp::answering(json_answer("200 OK", &fits));
    assert_eq!(check(&fake.url(""), TEXT, "fr-CA", SOON).map(|body| body.len()), Ok(MAX_ANSWER));
}

#[test]
fn nothing_listening_is_a_clean_error_at_once() {
    // A port with nobody on it: bound once, so that it was free, and let go.
    let port = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port").local_addr().expect("an address").port();
    let started = Instant::now();
    assert_eq!(check(&format!("http://127.0.0.1:{port}"), TEXT, "fr-CA", SOON), Err(LanguageToolError::Unreachable));
    assert!(started.elapsed() < Duration::from_secs(2), "took {:?}", started.elapsed());
    let started = Instant::now();
    assert_eq!(check_multilingual(&format!("http://127.0.0.1:{port}"), TEXT, &[Language::FrCa], &[], SOON), Err(LanguageToolError::Unreachable));
    assert!(started.elapsed() < Duration::from_secs(2), "took {:?}", started.elapsed());
}

#[test]
fn a_slow_server_is_not_waited_for() {
    let fake = FakeHttp::start(|_, _| {
        std::thread::sleep(Duration::from_secs(3));
        json_answer("200 OK", &french_answer())
    });
    let started = Instant::now();
    assert_eq!(check(&fake.url(""), TEXT, "fr-CA", Duration::from_millis(200)), Err(LanguageToolError::TooSlow));
    assert!(started.elapsed() < Duration::from_secs(3), "took {:?}", started.elapsed());
}

#[test]
fn the_errors_say_what_happened_and_nothing_of_the_text() {
    let fake = FakeHttp::answering(json_answer("200 OK", &french_answer()));
    let text = "Le mot de passe est hunter2.";
    let told = |e: LanguageToolError| e.to_string();
    // Not one of the owner's sentences holds a word of the message or the server's address.
    for error in [
        LanguageToolError::Refused,
        LanguageToolError::Language,
        LanguageToolError::UnitTextTooLong,
        LanguageToolError::Unreachable,
        LanguageToolError::TooSlow,
        LanguageToolError::TooLong,
        LanguageToolError::Closed,
        LanguageToolError::Failed,
        LanguageToolError::Garbled,
    ] {
        let said = told(error);
        assert!(!said.contains("hunter2") && !said.contains("mot de passe"), "{said}");
        assert!(!said.contains("127.0.0.1") && !said.contains("http"), "{said}");
        assert!(!said.is_empty(), "{error:?}");
    }
    // And a real failure is one of them, with nothing of the message in it.
    let said = check(&format!("http://127.0.0.1:{}/", "1"), text, "fr-CA", Duration::from_millis(50)).expect_err("no server").to_string();
    assert!(!said.contains("hunter2") && !said.contains("1"), "{said}");
    assert_eq!(fake.asked(), 0);
}

#[test]
fn the_multilingual_check_asks_with_automatic_language_and_preferred_variants() {
    let fake = FakeHttp::answering(json_answer("200 OK", &french_answer()));
    let outcome = check_multilingual(&fake.url(""), TEXT, &[Language::FrCa, Language::EnCa], &[], SOON).expect("an answer");
    assert_eq!(outcome.checked_segments, 1);
    assert_eq!(outcome.skipped_segments, 0);
    assert!(!outcome.partial);
    assert_eq!(outcome.diagnostics.len(), 1);
    let diagnostic = outcome.diagnostics.first().expect("one");
    assert_eq!(TEXT.get(diagnostic.span.start..diagnostic.span.end), Some("document"));
    assert_eq!(diagnostic.language, Some(Language::FrCa));
    assert_eq!(diagnostic.rule.as_deref(), Some("FR_ACCORD"));
    assert_eq!(diagnostic.source, Source::Server);

    let request = fake.request(0);
    let (head, form) = request.split_once("\r\n\r\n").expect("a head and a body");
    assert!(head.starts_with("POST /v2/check HTTP/1.1\r\n"), "{head}");
    let asked_for = pairs(form);
    assert_eq!(asked_for.len(), 4, "{form}");
    let value = |name: &str| asked_for.iter().find(|(field, _)| field == name).map(|(_, value)| value.as_str());
    assert_eq!(value("text"), Some(TEXT));
    assert_eq!(value("language"), Some("auto"));
    assert_eq!(value("preferredVariants"), Some("fr-CA,en-CA"));
    assert_eq!(value("level"), Some("picky"));
    // The crate's `Request` omits a false `enabledOnly`, and never writes a credential.
    assert_eq!(value("enabledOnly"), None);
    for never in ["username", "apiKey", "api_key", "cookie", "authorization", "referer"] {
        assert_eq!(value(never), None, "{never} in {form}");
    }
}

#[test]
fn both_languages_are_believed_when_the_detector_is_sure() {
    let french = response(
        "fr-CA",
        "French (Canada)",
        "fr",
        0.99,
        &a_match("Les document sont important.", "Accord", "FR_ACCORD", "grammar", 4, 8, "documents"),
    );
    let english = response("en-CA", "English (Canada)", "en", 0.98, &a_match("This are a test.", "Agreement", "EN_AGREEMENT", "grammar", 5, 3, "is"));
    let fake = FakeHttp::start(move |n, _| json_answer("200 OK", if n == 0 { &french } else { &english }));
    let text = "Les document sont important. This are a test.";
    let outcome = check_multilingual(&fake.url(""), text, &[Language::FrCa, Language::EnCa], &[], SOON).expect("an answer");
    assert_eq!(outcome.checked_segments, 2);
    assert_eq!(outcome.skipped_segments, 0);
    assert!(!outcome.partial);
    assert_eq!(outcome.diagnostics.len(), 2);
    let spans: Vec<&str> = outcome.diagnostics.iter().map(|d| text.get(d.span.start..d.span.end).unwrap_or("")).collect();
    assert_eq!(spans, ["document", "are"]);
    let languages: Vec<Option<Language>> = outcome.diagnostics.iter().map(|d| d.language).collect();
    assert_eq!(languages, [Some(Language::FrCa), Some(Language::EnCa)]);
}

#[test]
fn an_uncertain_or_mixed_sentence_is_left_alone() {
    // The detector is not sure: the answer's matches are not believed.
    let unsure = response("fr-CA", "French (Canada)", "fr", 0.94, &a_match(TEXT, "Accord", "FR_ACCORD", "grammar", 4, 8, "documents"));
    let fake = FakeHttp::answering(json_answer("200 OK", &unsure));
    let outcome = check_multilingual(&fake.url(""), TEXT, &[Language::FrCa, Language::EnCa], &[], SOON).expect("an answer");
    assert!(outcome.diagnostics.is_empty());
    assert_eq!(outcome.checked_segments, 0);
    assert_eq!(outcome.skipped_segments, 1);
    assert!(!outcome.partial);

    // The server checked it as one tongue and detected another: a mixed sentence, left alone.
    let mixed = response("en-CA", "English (Canada)", "fr", 0.99, &a_match(TEXT, "Accord", "FR_ACCORD", "grammar", 4, 8, "documents"));
    let fake = FakeHttp::answering(json_answer("200 OK", &mixed));
    let outcome = check_multilingual(&fake.url(""), TEXT, &[Language::FrCa, Language::EnCa], &[], SOON).expect("an answer");
    assert!(outcome.diagnostics.is_empty());
    assert_eq!(outcome.checked_segments, 0);
    assert_eq!(outcome.skipped_segments, 1);

    // A language the owner did not enable is not believed either.
    let unenabled = response("fr-CA", "French (Canada)", "fr", 0.99, &a_match(TEXT, "Accord", "FR_ACCORD", "grammar", 4, 8, "documents"));
    let fake = FakeHttp::answering(json_answer("200 OK", &unenabled));
    let outcome = check_multilingual(&fake.url(""), TEXT, &[Language::EnCa], &[], SOON).expect("an answer");
    assert!(outcome.diagnostics.is_empty());
    assert_eq!(outcome.skipped_segments, 1);

    // The crate's type makes `confidence` optional; an answer without one is not a sure one.
    let without = french_answer().replace("\"confidence\":0.99,", "");
    let fake = FakeHttp::answering(json_answer("200 OK", &without));
    let outcome = check_multilingual(&fake.url(""), TEXT, &[Language::FrCa], &[], SOON).expect("an answer");
    assert!(outcome.diagnostics.is_empty());
    assert_eq!(outcome.skipped_segments, 1);
}

#[test]
fn a_spelling_match_from_the_server_is_not_a_diagnostic() {
    // Spelling belongs to the local dictionaries; a server's misspelling match must not become a
    // second, duplicate underline under the same word.
    let body = response("fr-CA", "French (Canada)", "fr", 0.99, &a_match(TEXT, "Faute", "FR_SPELL", "misspelling", 4, 8, "document"));
    let fake = FakeHttp::answering(json_answer("200 OK", &body));
    let outcome = check_multilingual(&fake.url(""), TEXT, &[Language::FrCa], &[], SOON).expect("an answer");
    assert_eq!(outcome.checked_segments, 1);
    assert!(outcome.diagnostics.is_empty());
    assert_eq!(outcome.skipped_segments, 0);
    assert!(!outcome.partial);
}

#[test]
fn offsets_are_utf16_counted_and_checked_against_the_emoji() {
    let text = "Bonjour 😀 document est ici.";
    // "Bonjour " is 8 UTF-16 units, the emoji two more (8 and 9), the space one (10), so
    // `document` starts at unit 11 — byte 13, because the emoji is four bytes in UTF-8.
    let good = a_match(text, "Accord", "FR_ACCORD", "grammar", 11, 8, "documents");
    // Unit 9 is the low half of the emoji: no character starts there, so this match is dropped
    // rather than cut into a character.
    let between_surrogates = a_match(text, "Impossible", "BAD_OFFSET", "grammar", 9, 2, "x");
    let body = response("fr-CA", "French (Canada)", "fr", 0.99, &format!("{good},{between_surrogates}"));
    let fake = FakeHttp::answering(json_answer("200 OK", &body));
    let outcome = check_multilingual(&fake.url(""), text, &[Language::FrCa], &[], SOON).expect("an answer");
    assert_eq!(outcome.checked_segments, 1);
    assert_eq!(outcome.diagnostics.len(), 1);
    let diagnostic = outcome.diagnostics.first().expect("one");
    assert_eq!(text.get(diagnostic.span.start..diagnostic.span.end), Some("document"));
    assert_eq!(diagnostic.span.start, 13);
    assert_eq!(diagnostic.span.end, 21);
}

#[test]
fn an_ignored_rule_is_left_out_whatever_the_case_and_spacing() {
    let document = a_match(TEXT, "Accord", "QUIET_RULE", "grammar", 4, 8, "documents");
    let important = a_match(TEXT, "Accord", "LOUD_RULE", "grammar", 18, 9, "importants");
    let body = response("fr-CA", "French (Canada)", "fr", 0.99, &format!("{document},{important}"));
    let fake = FakeHttp::answering(json_answer("200 OK", &body));
    let outcome = check_multilingual(&fake.url(""), TEXT, &[Language::FrCa], &[" quiet_rule ".to_string()], SOON).expect("an answer");
    assert_eq!(outcome.diagnostics.len(), 1);
    assert_eq!(outcome.diagnostics.first().and_then(|d| d.rule.as_deref()), Some("LOUD_RULE"));
}

#[test]
fn a_server_that_says_its_results_are_incomplete_makes_the_outcome_partial() {
    // The whole answer decodes as the crate's `Response`, whose `warnings` says the server had
    // not looked at everything. The good diagnostic is kept, and the look is not called complete.
    let body = marked_incomplete(&french_answer());
    let fake = FakeHttp::answering(json_answer("200 OK", &body));
    let outcome = check_multilingual(&fake.url(""), TEXT, &[Language::FrCa], &[], SOON).expect("an answer");
    assert_eq!(outcome.checked_segments, 1);
    assert_eq!(outcome.diagnostics.len(), 1);
    assert_eq!(outcome.diagnostics.first().and_then(|d| d.rule.as_deref()), Some("FR_ACCORD"));
    assert!(outcome.partial, "the server said its results were incomplete");
}

#[test]
fn incompleteness_survives_a_malformed_neighbour() {
    let broken = a_match_without_context("Broken", "BROKEN_RULE", 0, 3);
    let good = a_match(TEXT, "Accord", "FR_ACCORD", "grammar", 4, 8, "documents");
    let body = response("fr-CA", "French (Canada)", "fr", 0.99, &format!("{broken},{good}"));
    // The split answer's header is the crate's `Response` with an empty `matches`, so its `warnings`
    // is read too: the good match is kept, and the server's own word still makes the look partial.
    let fake = FakeHttp::answering(json_answer("200 OK", &marked_incomplete(&body)));
    let outcome = check_multilingual(&fake.url(""), TEXT, &[Language::FrCa], &[], SOON).expect("an answer");
    assert_eq!(outcome.checked_segments, 1);
    assert_eq!(outcome.diagnostics.len(), 1);
    assert_eq!(outcome.diagnostics.first().and_then(|d| d.rule.as_deref()), Some("FR_ACCORD"));
    assert!(outcome.partial, "the server said its results were incomplete");
    // The same answer without that word is a complete look: the flag is the server's.
    let fake = FakeHttp::answering(json_answer("200 OK", &body));
    let outcome = check_multilingual(&fake.url(""), TEXT, &[Language::FrCa], &[], SOON).expect("an answer");
    assert_eq!(outcome.checked_segments, 1);
    assert_eq!(outcome.diagnostics.len(), 1);
    assert!(!outcome.partial);
}

#[test]
fn a_text_longer_than_one_check_may_ask_about_is_refused_before_a_connection() {
    let fake = FakeHttp::answering(json_answer("200 OK", &french_answer()));
    // One character over the cap, counted in characters and not bytes: the text is refused before
    // the form is built, so the server is asked nothing at all.
    let text = "é".repeat(MAX_CHECKED + 1);
    assert_eq!(check(&fake.url(""), &text, "fr-CA", SOON), Err(LanguageToolError::UnitTextTooLong));
    assert_eq!(fake.asked(), 0);
}

#[test]
fn the_last_text_one_check_may_ask_about_is_asked_whole() {
    let fake = FakeHttp::answering(json_answer("200 OK", &french_answer()));
    // Exactly the cap, counted in characters: its last character takes two bytes of UTF-8, so a
    // bound counted in bytes would refuse a text that is within the cap.
    let text = format!("{}é", "a".repeat(MAX_CHECKED - 1));
    check(&fake.url(""), &text, "fr-CA", SOON).expect("an answer");
    assert_eq!(fake.asked(), 1);
    let request = fake.request(0);
    let (_, form) = request.split_once("\r\n\r\n").expect("a head and a body");
    assert_eq!(pairs(form).first().map(|(name, value)| (name.as_str(), value.as_str())), Some(("text", text.as_str())));
}

#[test]
fn only_the_first_quiet_entries_are_scanned() {
    let loud = a_match(TEXT, "Accord", "LOUD_RULE", "grammar", 4, 8, "documents");
    let body = response("fr-CA", "French (Canada)", "fr", 0.99, &loud);
    // As many entries as the scan may look at -- repeats of one id, and entries that are no rule's
    // id at all -- and the rule the answer names as the next one: past the cap, so the scan never
    // reaches it and the match is not quieted.
    let mut quiet: Vec<String> = Vec::new();
    for n in 0..MAX_QUIET {
        quiet.push(if n % 2 == 0 { " quiet_rule ".to_string() } else { format!("not a rule {n}") });
    }
    quiet.push("LOUD_RULE".to_string());
    assert_eq!(quiet.len(), MAX_QUIET + 1);
    let fake = FakeHttp::answering(json_answer("200 OK", &body));
    let outcome = check_multilingual(&fake.url(""), TEXT, &[Language::FrCa], &quiet, SOON).expect("an answer");
    assert_eq!(MAX_QUIET, 256);
    assert_eq!(outcome.diagnostics.len(), 1, "the rule is named past the cap of the scan");
    assert_eq!(outcome.diagnostics.first().and_then(|d| d.rule.as_deref()), Some("LOUD_RULE"));
}

#[test]
fn a_quiet_entry_longer_than_any_rule_id_is_refused_before_it_is_trimmed() {
    let document = a_match(TEXT, "Accord", "QUIET_RULE", "grammar", 4, 8, "documents");
    let body = response("fr-CA", "French (Canada)", "fr", 0.99, &document);
    // Four thousand spaces either side of a rule's id: the entry is no rule's id, and it is refused
    // for its own length before the trim reads it, rather than trimmed down into one.
    let padded = format!("{}QUIET_RULE{}", " ".repeat(4_096), " ".repeat(4_096));
    let fake = FakeHttp::answering(json_answer("200 OK", &body));
    let outcome = check_multilingual(&fake.url(""), TEXT, &[Language::FrCa], &[padded], SOON).expect("an answer");
    assert_eq!(outcome.diagnostics.len(), 1, "a padded entry is not a rule id the owner typed");
}

#[test]
fn a_fallback_header_is_validated_as_the_crates_response() {
    // A body whose one match is malformed cannot be read whole. Without the answer's `software` its
    // header is not the crate's `Response` either, so the split is not trusted and no match is
    // believed: the fallback validates the header with the same type, it does not skip it.
    let broken = a_match_without_context("Broken", "BROKEN_RULE", 0, 3);
    let good = a_match(TEXT, "Accord", "FR_ACCORD", "grammar", 4, 8, "documents");
    let body = response("fr-CA", "French (Canada)", "fr", 0.99, &format!("{broken},{good}")).replace("\"software\":", "\"notSoftware\":");
    let fake = FakeHttp::answering(json_answer("200 OK", &body));
    assert_eq!(check_multilingual(&fake.url(""), TEXT, &[Language::FrCa], &[], SOON), Err(LanguageToolError::Garbled));
}

#[test]
fn a_malformed_neighbour_does_not_lose_a_good_match() {
    let broken = a_match_without_context("Broken", "BROKEN_RULE", 0, 3);
    let good = a_match(TEXT, "Accord", "FR_ACCORD", "grammar", 4, 8, "documents");
    let body = response("fr-CA", "French (Canada)", "fr", 0.99, &format!("{broken},{good}"));
    let fake = FakeHttp::answering(json_answer("200 OK", &body));
    let outcome = check_multilingual(&fake.url(""), TEXT, &[Language::FrCa], &[], SOON).expect("an answer");
    assert_eq!(outcome.checked_segments, 1);
    assert_eq!(outcome.diagnostics.len(), 1);
    let diagnostic = outcome.diagnostics.first().expect("one");
    assert_eq!(TEXT.get(diagnostic.span.start..diagnostic.span.end), Some("document"));
    assert_eq!(diagnostic.rule.as_deref(), Some("FR_ACCORD"));
}

#[test]
fn a_redirect_is_not_followed() {
    let answer = b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:9/v2/check\r\nContent-Length: 0\r\n\r\n".to_vec();
    let fake = FakeHttp::answering(answer);
    assert_eq!(check(&fake.url(""), TEXT, "fr-CA", SOON), Err(LanguageToolError::Failed));
    assert_eq!(fake.asked(), 1);
    assert_eq!(check_multilingual(&fake.url(""), TEXT, &[Language::FrCa], &[], SOON), Err(LanguageToolError::Failed));
    assert_eq!(fake.asked(), 2);
}

#[test]
fn a_partial_timeout_keeps_the_sentences_already_checked() {
    let french = french_answer();
    let fake = FakeHttp::start(move |n, _| {
        if n == 0 {
            return json_answer("200 OK", &french);
        }
        std::thread::sleep(Duration::from_secs(3));
        json_answer("200 OK", &french)
    });
    let text = "Les document sont important. Les document sont important.";
    let started = Instant::now();
    let outcome = check_multilingual(&fake.url(""), text, &[Language::FrCa], &[], Duration::from_millis(600)).expect("partial");
    assert!(started.elapsed() < Duration::from_secs(3), "took {:?}", started.elapsed());
    assert!(outcome.partial);
    assert_eq!(outcome.checked_segments, 1);
    assert_eq!(outcome.skipped_segments, 1);
    assert_eq!(outcome.diagnostics.len(), 1);
    assert_eq!(outcome.diagnostics.first().map(|d| d.span.start), Some(4));
}

#[test]
fn the_batch_cap_leaves_the_rest_unchecked() {
    let text = (0..40).map(|n| format!("Sentence {n}.")).collect::<Vec<_>>().join(" ");
    let fake = FakeHttp::answering(json_answer("200 OK", &response("en-CA", "English (Canada)", "en", 0.99, "")));
    let outcome = check_multilingual(&fake.url(""), &text, &[Language::EnCa], &[], SOON).expect("an answer");
    assert_eq!(fake.asked(), 32);
    assert_eq!(outcome.checked_segments, 32);
    assert_eq!(outcome.skipped_segments, 8);
    assert!(outcome.partial);
}

#[test]
fn a_sentence_longer_than_a_segment_is_left_alone() {
    let text = format!("{} end.", "a".repeat(16 * 1024 + 16));
    let fake = FakeHttp::answering(json_answer("200 OK", &response("en-CA", "English (Canada)", "en", 0.99, "")));
    let outcome = check_multilingual(&fake.url(""), &text, &[Language::EnCa], &[], SOON).expect("an answer");
    assert_eq!(fake.asked(), 0);
    assert_eq!(outcome.diagnostics.len(), 0);
    assert_eq!(outcome.checked_segments, 0);
    assert_eq!(outcome.skipped_segments, 1);
    assert!(outcome.partial);
}

#[test]
fn a_language_list_with_nothing_enabled_is_refused_before_a_connection() {
    let fake = FakeHttp::answering(json_answer("200 OK", &french_answer()));
    assert_eq!(check_multilingual(&fake.url(""), TEXT, &[], &[], SOON), Err(LanguageToolError::Language));
    assert_eq!(fake.asked(), 0);
}

#[test]
fn the_outcome_debug_never_holds_the_message_or_a_suggestion() {
    let diagnostic = Diagnostic::new(Span { start: 0, end: 1 }, Kind::Grammar, Source::Server)
        .saying("hunter2 is the secret message")
        .suggesting(["hunter2".to_string(), "letmein".to_string()]);
    let outcome = CheckOutcome { diagnostics: vec![diagnostic], checked_segments: 1, skipped_segments: 0, partial: false };
    let debug = format!("{outcome:?}");
    for never in ["hunter2", "letmein", "secret message"] {
        assert!(!debug.contains(never), "{debug}");
    }
    assert!(debug.contains("checked_segments"), "{debug}");
}

/// The child half of [`a_proxy_environment_is_not_read`]: it runs in a process whose environment
/// names a proxy, and asks a stand-in server on this machine. If the check honoured the proxy it
/// would never reach `target`, and the parent would see a failed child.
#[test]
fn a_proxy_environment_is_not_read_by_a_child() {
    let target = FakeHttp::answering(json_answer("200 OK", &french_answer()));
    let got = check(&target.url(""), TEXT, "fr-CA", SOON);
    assert!(got.is_ok(), "{got:?}");
    assert_eq!(target.asked(), 1);
}

/// The HTTP client has no proxy support at all; this proves a process whose environment names one
/// still speaks to the server it was given, and only to it. `std::env::set_var` is unsafe in this
/// edition (and forbidden here), so the variables are set on a child process instead.
#[test]
fn a_proxy_environment_is_not_read() {
    let exe = std::env::current_exe().expect("the test binary");
    let output = std::process::Command::new(exe)
        .args(["--exact", "languagetool_tests::a_proxy_environment_is_not_read_by_a_child", "--nocapture"])
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .env("ALL_PROXY", "http://127.0.0.1:9")
        .env("http_proxy", "http://127.0.0.1:9")
        .env("https_proxy", "http://127.0.0.1:9")
        .env("all_proxy", "http://127.0.0.1:9")
        .env("NO_PROXY", "")
        .output()
        .expect("a child test process");
    assert!(output.status.success(), "child failed: {}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
}

/// The explicit opt-in for a real LanguageTool server: an address the owner deliberately names
/// for a test run, never a port these tests guess at or probe on their own. When it is absent (the
/// usual case) the real-provider tests say why they are skipped and ask nothing. Only this test
/// file reads it: no production code routes through an environment variable.
const REAL_SERVER_ENV: &str = "MAILCRAFT_LT_TEST_URL";

/// The address named by [`REAL_SERVER_ENV`], held to the same grammar-address policy as every
/// entrypoint. `None`, with the reason said on stderr, when it is not set.
fn real_server() -> Option<String> {
    let url = match std::env::var(REAL_SERVER_ENV) {
        Ok(url) => url.trim().to_string(),
        Err(_) => {
            eprintln!("skipped: {REAL_SERVER_ENV} is not set; a real LanguageTool server is asked only by explicit opt-in");
            return None;
        }
    };
    assert!(!url.is_empty(), "{REAL_SERVER_ENV} is set but names no address");
    assert!(mailcraft_model::proof::address_allowed(&url, false), "{REAL_SERVER_ENV} is not an address a grammar check may be sent to");
    Some(url)
}

/// Not a stand-in: a real LanguageTool server, asked only when the owner named its address in
/// [`REAL_SERVER_ENV`]. Nothing is started or stopped, and no port is guessed at.
#[test]
fn a_real_language_tool_at_the_opt_in_address_answers_a_check() {
    let Some(url) = real_server() else { return };
    let outcome = check_multilingual(&url, TEXT, &[Language::FrCa, Language::EnCa], &[], Duration::from_secs(12))
        .expect("the explicitly named server answered a check");
    assert!(outcome.checked_segments >= 1, "{outcome:?}");
    assert!(!outcome.diagnostics.is_empty(), "{outcome:?}");
    for diagnostic in &outcome.diagnostics {
        assert!(diagnostic.span.fits(TEXT), "{diagnostic:?}");
    }
    // The connection-level call answers too, and the model reads its bytes into diagnostics.
    let body = check(&url, TEXT, "fr-CA", Duration::from_secs(12)).expect("a check answer");
    let diagnostics = parse_answer(&body, TEXT, Language::FrCa, &[]).expect("an answer the model reads");
    assert!(!diagnostics.is_empty(), "the explicitly named server answered a check with no diagnostics");
}

/// A whole `GET /v2/languages` answer as LanguageTool 6.6 writes it: the `[{"name","code",
/// "longCode"}]` array that the `languagetool-rust` crate's `languages::Language` type reads.
fn languages_answer() -> String {
    r#"[{"name":"Arabic","code":"ar","longCode":"ar"},{"name":"English (US)","code":"en-US","longCode":"en-US"},{"name":"French","code":"fr","longCode":"fr-FR"}]"#.to_string()
}

#[test]
fn readiness_asks_only_for_the_language_list() {
    let fake = FakeHttp::answering(json_answer("200 OK", &languages_answer()));
    ready(&fake.url(""), SOON).expect("a server that is ready");
    assert_eq!(fake.asked(), 1);
    let request = fake.request(0);
    let (head, body) = request.split_once("\r\n\r\n").expect("a head and an empty body");
    assert!(head.starts_with("GET /v2/languages HTTP/1.1\r\n"), "{head}");
    assert!(head.contains("Host: 127.0.0.1:"), "{head}");
    assert!(head.contains("Accept: application/json\r\n"), "{head}");
    // A readiness probe asks, it does not send: no text, no form, no body of any kind.
    assert!(body.is_empty(), "a GET carries no body: {body}");
    for never in ["content-type:", "content-length:", "text=", "language=", "preferredvariants"] {
        assert!(!request.to_ascii_lowercase().contains(never), "{never} in {request}");
    }
    // And nothing that would sign it in, mark it, or point it at a proxy.
    for never in ["cookie:", "authorization:", "proxy-authorization:", "referer:", "x-api-key"] {
        assert!(!head.to_ascii_lowercase().contains(never), "{never} in {head}");
    }
}

#[test]
fn readiness_puts_one_slash_between_the_server_and_the_list() {
    let fake = FakeHttp::answering(json_answer("200 OK", &languages_answer()));
    for base in [fake.url(""), fake.url("/"), format!("{}//", fake.url(""))] {
        ready(&base, SOON).expect("a server that is ready");
    }
    assert_eq!(fake.asked(), 3);
    for n in 0..3 {
        assert!(fake.request(n).starts_with("GET /v2/languages HTTP/1.1\r\n"), "{}", fake.request(n));
    }
}

#[test]
fn readiness_refuses_anything_but_this_computers_server_before_a_connection() {
    let fake = FakeHttp::answering(json_answer("200 OK", &languages_answer()));
    for bad in [
        // No server on another machine is asked whether it is ready.
        "http://192.0.2.7:8081",
        "http://languagetool.example",
        "https://languagetool.example/v2/languages",
        // What is no web address at all.
        "ftp://127.0.0.1:8081",
        "file:///etc/passwd",
        "127.0.0.1:8081",
        "//127.0.0.1:8081",
        "",
        "   ",
        // A query or a fragment is not where a language list is asked for.
        "http://127.0.0.1:8081/?x=1",
        "http://127.0.0.1:8081/#top",
        // A name and a password in the address are nobody's here.
        "http://user:secret@127.0.0.1:8081",
    ] {
        assert_eq!(ready(bad, SOON), Err(LanguageToolError::Refused), "{bad}");
    }
    // The stand-in server itself, through addresses that are not ones: not a byte of a request
    // reaches it, so the refusal happened before any connection.
    let at = fake.url("");
    for bad in [format!("http://user@{at}/"), format!("{at}?x=1"), format!("{at}#top")] {
        assert_eq!(ready(&bad, SOON), Err(LanguageToolError::Refused), "{bad}");
    }
    assert_eq!(fake.asked(), 0);
}

#[test]
fn readiness_requires_http_200() {
    for status in
        ["204 No Content", "400 Bad Request", "404 Not Found", "429 Too Many Requests", "500 Internal Server Error", "503 Service Unavailable"]
    {
        let fake = FakeHttp::answering(json_answer(status, ""));
        assert_eq!(ready(&fake.url(""), SOON), Err(LanguageToolError::Failed), "{status}");
        assert_eq!(fake.asked(), 1, "{status}");
    }
}

#[test]
fn readiness_does_not_follow_a_redirect() {
    let answer = b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:9/v2/languages\r\nContent-Length: 0\r\n\r\n".to_vec();
    let fake = FakeHttp::answering(answer);
    assert_eq!(ready(&fake.url(""), SOON), Err(LanguageToolError::Failed));
    assert_eq!(fake.asked(), 1, "the redirect was not followed");
}

#[test]
fn readiness_refuses_what_is_not_a_language_list() {
    for body in [
        "",
        "not json",
        "{}",
        r#"{"languages":[]}"#,
        // No language at all is not a server that can check anything.
        "[]",
        // The crate's `languages::Language` needs all three fields, and a real one beside a
        // broken one does not make the list a list of languages.
        r#"[{"name":"French","code":"fr"}]"#,
        r#"[{"name":"French","code":"","longCode":"fr"}]"#,
        r#"[{"name":"   ","code":"fr","longCode":"fr"}]"#,
        r#"[{"name":"French","code":"fr FR","longCode":"fr"}]"#,
        r#"[{"name":"French","code":"fr","longCode":""}]"#,
    ] {
        let fake = FakeHttp::answering(json_answer("200 OK", body));
        assert_eq!(ready(&fake.url(""), SOON), Err(LanguageToolError::Garbled), "{body:?}");
    }
}

#[test]
fn readiness_refuses_a_language_list_beyond_its_bounds() {
    // One entry past the cap the probe believes, each entry small enough that the answer itself is
    // well within the body cap: the list is refused for its length, not for its bytes.
    let one = r#"{"name":"x","code":"x","longCode":"x"}"#;
    let many = format!("[{}]", vec![one; MAX_READY_LANGUAGES + 1].join(","));
    assert!(many.len() < READY_MAX_BODY, "{} bytes", many.len());
    let fake = FakeHttp::answering(json_answer("200 OK", &many));
    assert_eq!(ready(&fake.url(""), SOON), Err(LanguageToolError::Garbled));
    // A code no LanguageTool writes (an absurdly long one) is refused too, and so is a name.
    let long_code = format!(r#"[{{"name":"x","code":"{}","longCode":"x"}}]"#, "a".repeat(MAX_READY_CODE + 1));
    let fake = FakeHttp::answering(json_answer("200 OK", &long_code));
    assert_eq!(ready(&fake.url(""), SOON), Err(LanguageToolError::Garbled));
    let long_name = format!(r#"[{{"name":"{}","code":"x","longCode":"x"}}]"#, "n".repeat(MAX_READY_NAME + 1));
    let fake = FakeHttp::answering(json_answer("200 OK", &long_name));
    assert_eq!(ready(&fake.url(""), SOON), Err(LanguageToolError::Garbled));
}

#[test]
fn readiness_refuses_an_answer_past_its_cap() {
    let full = format!(r#"[{{"name":"x","code":"x","longCode":"x","pad":"{}"}}]"#, "x".repeat(READY_MAX_BODY));
    assert!(full.len() > READY_MAX_BODY);
    let fake = FakeHttp::answering(json_answer("200 OK", &full));
    assert_eq!(ready(&fake.url(""), SOON), Err(LanguageToolError::TooLong));
}

#[test]
fn readiness_refuses_a_truncated_answer() {
    let fake = FakeHttp::answering(b"HTTP/1.1 200 OK\r\nContent-Length: 99\r\n\r\n[{\"name\"".to_vec());
    assert!(matches!(ready(&fake.url(""), SOON), Err(LanguageToolError::Closed | LanguageToolError::Garbled)));
}

#[test]
fn readiness_does_not_wait_out_a_hung_server() {
    let fake = FakeHttp::start(|_, _| {
        std::thread::sleep(Duration::from_secs(3));
        json_answer("200 OK", &languages_answer())
    });
    let started = Instant::now();
    assert_eq!(ready(&fake.url(""), Duration::from_millis(200)), Err(LanguageToolError::TooSlow));
    assert!(started.elapsed() < Duration::from_secs(3), "took {:?}", started.elapsed());
}

#[test]
fn readiness_patience_is_never_more_than_a_checks() {
    // The caller cannot leave a supervisor waiting on a server for longer than a check waits.
    assert_eq!(ready_patience(Duration::from_secs(3_600)), PATIENCE);
    assert_eq!(ready_patience(PATIENCE), PATIENCE);
    assert_eq!(ready_patience(Duration::from_millis(25)), Duration::from_millis(25));
}

#[test]
fn readiness_errors_say_nothing_of_the_address_or_the_answer() {
    let fake = FakeHttp::answering(json_answer("500 Internal Server Error", "hunter2 in the body"));
    let error = ready(&fake.url(""), SOON).expect_err("not ready");
    for said in [error.to_string(), format!("{error:?}")] {
        assert!(!said.contains("hunter2"), "{said}");
        assert!(!said.contains("127.0.0.1"), "{said}");
        assert!(!said.contains("http"), "{said}");
        assert!(!said.is_empty(), "{said}");
    }
    // And a real connection failure, likewise: nothing of where it tried.
    let error = ready("http://127.0.0.1:9", Duration::from_millis(50)).expect_err("nothing listens");
    for said in [error.to_string(), format!("{error:?}")] {
        assert!(!said.contains("127.0.0.1") && !said.contains("http"), "{said}");
    }
}

/// Not a stand-in either: the same explicitly named real server, asked whether it is ready.
#[test]
fn a_real_language_tool_at_the_opt_in_address_is_ready() {
    let Some(url) = real_server() else { return };
    ready(&url, Duration::from_secs(12)).expect("the explicitly named server said it was ready");
}
