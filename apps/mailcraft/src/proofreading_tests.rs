// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Grammar checking's host half, against servers of this test's own on loopback: an answer with a
//! place in it, the languages and the quiet rules the owner enabled, an answer that says it is
//! incomplete, a server that is not there, and a server that is not a grammar server.
//!
//! Every answer here is a whole `POST /v2/check` response as LanguageTool 6.6 writes one, the
//! shape the `languagetool-rust` crate types: a `software` block, the language the server checked
//! as and the one it detected with a confidence, and matches with their context, sentence, type
//! and rule. A fixture that only resembles one would prove the reader lax rather than faithful.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use mailcraft_model::proof::{Kind, Language};
use mailcraft_ui_egui::proof::Request;

use crate::proofreading::*;

/// What this host says when only part of the text was looked at: never a clean answer.
const INCOMPLETE_SAID: &str = "only part of the text";

/// A match as LanguageTool 6.6 writes it: every field of the crate's `Match` type, including the
/// unstable ones. `segment` is the sentence that was sent, so the context is the server's own.
fn a_match(segment: &str, message: &str, rule: &str, issue_type: &str, offset: usize, length: usize, replacement: &str) -> String {
    format!(
        r#"{{"message":"{message}","shortMessage":"","replacements":[{{"value":"{replacement}"}}],"offset":{offset},"length":{length},"context":{{"text":"{segment}","offset":{offset},"length":{length}}},"sentence":"{segment}","type":{{"typeName":"Other"}},"rule":{{"id":"{rule}","description":"A rule","issueType":"{issue_type}","category":{{"id":"GRAMMAR","name":"Grammar"}}}},"ignoreForIncompleteSentence":false,"contextForSureMatch":0}}"#
    )
}

/// A whole answer, as LanguageTool 6.6 writes it, with the server's own word on whether it looked
/// at everything. `detected` is what its detector says, which a check must trust or drop.
fn response(code: &str, name: &str, detected: &str, confidence: f64, incomplete: bool, matches: &str) -> String {
    format!(
        r#"{{"software":{{"name":"LanguageTool","version":"6.6","buildDate":"2025-03-27 20:50:25 +0100","apiVersion":1,"premium":false,"premiumHint":"","status":""}},"warnings":{{"incompleteResults":{incomplete}}},"language":{{"name":"{name}","code":"{code}","detectedLanguage":{{"name":"{name}","code":"{detected}","confidence":{confidence},"source":"ngram"}}}},"matches":[{matches}]}}"#
    )
}

/// The French answer used by most tests: `document` is wrong in "Les document sont important.".
fn french_answer(matches: &str) -> String {
    response("fr-CA", "French (Canada)", "fr", 0.99, false, matches)
}

/// An HTTP answer with `body`.
fn answered(body: &str) -> String {
    format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
}

/// A server on this machine that answers up to `most` requests with `body`, keeping the last
/// request it read and counting how many it took. Its address.
///
/// The count is what proves a request was bounded before any server work: a check that must not
/// reach a server leaves the count at zero.
struct Fake {
    url: String,
    asked: Arc<AtomicUsize>,
    last: Arc<Mutex<Vec<u8>>>,
}

fn fake(body: &str, most: usize) -> Fake {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
    let at = listener.local_addr().expect("an address");
    let answer = answered(body);
    let asked = Arc::new(AtomicUsize::new(0));
    let last = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&asked);
    let kept = Arc::clone(&last);
    std::thread::spawn(move || {
        for _ in 0..most {
            let Ok((mut socket, _)) = listener.accept() else { break };
            let request = read_request(&mut socket);
            seen.fetch_add(1, Ordering::SeqCst);
            if let Ok(mut kept) = kept.lock() {
                kept.clear();
                kept.extend_from_slice(&request);
            }
            let _ = socket.write_all(answer.as_bytes());
        }
    });
    Fake { url: format!("http://{at}"), asked, last }
}

impl Fake {
    fn asked(&self) -> usize {
        self.asked.load(Ordering::SeqCst)
    }

    /// The `x-www-form-urlencoded` fields of the last request, percent-decoded.
    fn fields(&self) -> Vec<(String, String)> {
        let bytes = self.last.lock().expect("the last request").clone();
        let request = String::from_utf8_lossy(&bytes);
        let Some((_, form)) = request.split_once("\r\n\r\n") else { return Vec::new() };
        form.split('&')
            .filter_map(|pair| {
                let (name, value) = pair.split_once('=')?;
                Some((percent_decode(name), percent_decode(value)))
            })
            .collect()
    }

    fn field(&self, name: &str) -> Option<String> {
        self.fields().into_iter().find(|(field, _)| field == name).map(|(_, value)| value)
    }
}

/// One request read off a socket: its head, and as much of its form as the head's `Content-Length`
/// promises. Bounded, so a hostile or broken client cannot make a test server read for ever.
fn read_request(socket: &mut TcpStream) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = socket.read(&mut chunk).unwrap_or(0);
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(head_end) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
            let length =
                head.lines().find_map(|line| line.strip_prefix("content-length:")).and_then(|value| value.trim().parse::<usize>().ok()).unwrap_or(0);
            if buf.len() >= head_end + 4 + length {
                break;
            }
        }
        if buf.len() > 256 * 1024 {
            break;
        }
    }
    buf
}

/// `text/plain` percent-decoding, with `+` read as a space, as a form field is written.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while let Some(&byte) = bytes.get(at) {
        match byte {
            b'+' => {
                out.push(b' ');
                at += 1;
            }
            b'%' => {
                let hex = |index: usize| bytes.get(index).and_then(|byte| (*byte as char).to_digit(16));
                match (hex(at + 1), hex(at + 2)) {
                    (Some(high), Some(low)) => {
                        out.push((high * 16 + low) as u8);
                        at += 3;
                    }
                    _ => {
                        out.push(byte);
                        at += 1;
                    }
                }
            }
            other => {
                out.push(other);
                at += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn request(url: &str, text: &str, languages: Vec<Language>, ignored: Vec<String>) -> Request {
    let language = languages.first().copied().unwrap_or(Language::FrCa);
    Request { url: url.to_string(), text: text.to_string(), language, languages, ignored }
}

const TEXT: &str = "Les document sont important.";

#[test]
fn a_place_the_server_found_comes_back_with_its_bytes() {
    let server = fake(&french_answer(&a_match(TEXT, "Accord", "FR_ACCORD", "grammar", 4, 8, "documents")), 1);
    let reply = answer(&request(&server.url, TEXT, vec![Language::FrCa], Vec::new()));
    assert!(reply.error.is_none(), "{:?}", reply.error);
    assert_eq!(reply.text, TEXT);
    let place = reply.diagnostics.first().expect("one place");
    assert_eq!(TEXT.get(place.span.start..place.span.end), Some("document"));
    assert_eq!(place.suggestions.first().map(String::as_str), Some("documents"));
    assert_eq!(place.kind, Kind::Grammar);
    assert_eq!(place.rule.as_deref(), Some("FR_ACCORD"));
    assert_eq!(place.language, Some(Language::FrCa));
    assert_eq!(server.asked(), 1);
}

#[test]
fn every_enabled_language_is_asked_for_as_a_variant() {
    let server = fake(&french_answer(&a_match(TEXT, "Accord", "FR_ACCORD", "grammar", 4, 8, "documents")), 1);
    let reply = answer(&request(&server.url, TEXT, vec![Language::FrCa, Language::EnCa], Vec::new()));
    assert!(reply.error.is_none(), "{:?}", reply.error);
    // One check, asked for every language the owner turned on: a server that can check them
    // together is never asked for the first of them alone.
    assert_eq!(server.asked(), 1);
    assert_eq!(server.field("language").as_deref(), Some("auto"));
    assert_eq!(server.field("preferredVariants").as_deref(), Some("fr-CA,en-CA"));
    // And a request that promised a language list never quietly asks about one language.
    assert!(!server.fields().iter().any(|(name, value)| name == "language" && value != "auto"));
}

#[test]
fn an_english_place_is_believed_in_english_when_english_is_enabled() {
    let text = "I will send the document tomorrow.";
    let server = fake(
        &response("en-CA", "English (Canada)", "en", 0.99, false, &a_match(text, "Agreement", "EN_AGREEMENT", "grammar", 16, 8, "documents")),
        1,
    );
    let reply = answer(&request(&server.url, text, vec![Language::FrCa, Language::EnCa], Vec::new()));
    assert!(reply.error.is_none(), "{:?}", reply.error);
    let place = reply.diagnostics.first().expect("one place");
    assert_eq!(text.get(place.span.start..place.span.end), Some("document"));
    assert_eq!(place.language, Some(Language::EnCa), "the language it was checked as, not the first the settings list");
}

#[test]
fn a_language_the_owner_did_not_enable_is_never_believed() {
    let text = "I will send the document tomorrow.";
    // The server answers about English while the owner enabled French alone: no match is this
    // draft's, and no other enabled language is fallen back to.
    let server = fake(
        &response("en-CA", "English (Canada)", "en", 0.99, false, &a_match(text, "Agreement", "EN_AGREEMENT", "grammar", 16, 8, "documents")),
        1,
    );
    let reply = answer(&request(&server.url, text, vec![Language::FrCa], Vec::new()));
    assert!(reply.diagnostics.is_empty(), "{:?}", reply.diagnostics);
    assert!(reply.error.is_none(), "a sentence left alone is a complete look, not a failed one: {:?}", reply.error);
}

#[test]
fn a_rule_the_owner_silenced_is_left_out() {
    let server = fake(&french_answer(&a_match(TEXT, "Accord", "FR_ACCORD", "grammar", 4, 8, "documents")), 1);
    // As the settings keep it: padded with spaces and in another case.
    let reply = answer(&request(&server.url, TEXT, vec![Language::FrCa], vec![" fr_accord ".to_string()]));
    assert!(reply.error.is_none(), "{:?}", reply.error);
    assert!(reply.diagnostics.is_empty(), "a quiet rule came back: {:?}", reply.diagnostics);
}

#[test]
fn a_spelling_match_from_the_server_is_left_to_the_dictionaries() {
    let server = fake(&french_answer(&a_match(TEXT, "Possible spelling mistake found.", "MORFOLOGIK_RULE_FR", "misspelling", 4, 8, "documents")), 1);
    let reply = answer(&request(&server.url, TEXT, vec![Language::FrCa], Vec::new()));
    assert!(reply.error.is_none(), "{:?}", reply.error);
    assert!(reply.diagnostics.is_empty(), "spelling belongs to the dictionaries: {:?}", reply.diagnostics);
}

#[test]
fn an_answer_that_says_it_is_incomplete_is_not_clean() {
    let text = "Les document sont important.";
    let body = response("fr-CA", "French (Canada)", "fr", 0.99, true, &a_match(text, "Accord", "FR_ACCORD", "grammar", 4, 8, "documents"));
    let server = fake(&body, 1);
    let reply = answer(&request(&server.url, text, vec![Language::FrCa], Vec::new()));
    // The place the server did find is kept, and the answer is still never reported as a whole
    // look at the text.
    assert_eq!(reply.diagnostics.len(), 1);
    let said = reply.error.expect("the incompleteness is said");
    assert!(said.to_ascii_lowercase().contains(INCOMPLETE_SAID), "{said}");
}

#[test]
fn a_text_too_long_to_send_is_never_reported_clean() {
    // One sentence past every bound the adapter has: nothing of it can be sent, and saying so is
    // not the same as finding nothing wrong with it.
    let text = "a ".repeat(60_000);
    let server = fake(&french_answer(""), 1);
    let reply = answer(&request(&server.url, &text, vec![Language::FrCa], Vec::new()));
    assert_eq!(server.asked(), 0, "an oversized text is bounded before any server work");
    assert!(reply.diagnostics.is_empty());
    assert_eq!(reply.text, text, "the text comes back with its answer, so that a stale one is dropped");
    let said = reply.error.expect("an incomplete look is said").to_ascii_lowercase();
    assert!(said.contains(INCOMPLETE_SAID), "{said}");
}

#[test]
fn an_empty_language_list_is_refused_without_asking_anybody() {
    let server = fake(&french_answer(&a_match(TEXT, "Accord", "FR_ACCORD", "grammar", 4, 8, "documents")), 1);
    let reply =
        answer(&Request { url: server.url.clone(), text: TEXT.to_string(), language: Language::FrCa, languages: Vec::new(), ignored: Vec::new() });
    // Consent is the list: the single-language field is not a fallback for it.
    assert_eq!(server.asked(), 0);
    assert!(reply.diagnostics.is_empty());
    let said = reply.error.expect("a list nobody enabled is refused");
    assert!(!said.contains("document"), "the message's text is never in an error: {said}");
}

#[test]
fn a_server_that_is_not_there_says_so_and_says_nothing_else() {
    // A port nobody listens on: bound and let go, so the number is one this machine has free.
    let port = TcpListener::bind("127.0.0.1:0").and_then(|l| l.local_addr()).expect("a port").port();
    let url = format!("http://127.0.0.1:{port}");
    let text = "the document is here";
    let reply = answer(&request(&url, text, vec![Language::EnCa], Vec::new()));
    let error = reply.error.clone().expect("an error");
    assert!(!error.contains("document"), "the message's text is never in an error: {error}");
    assert!(reply.diagnostics.is_empty());
    assert_eq!(reply.text, text, "the text comes back with its answer, so that a stale one is dropped");
}

#[test]
fn a_server_that_is_not_a_grammar_server_is_read_as_no_places() {
    let server = fake("this is not JSON", 1);
    let reply = answer(&request(&server.url, "Bonjour", vec![Language::FrCa], Vec::new()));
    assert!(reply.diagnostics.is_empty());
    assert!(reply.error.is_some());
}

#[test]
fn a_server_on_another_computer_is_never_asked() {
    let reply = answer(&request("https://languagetool.example", "Bonjour le monde", vec![Language::FrCa], Vec::new()));
    assert!(reply.diagnostics.is_empty());
    assert!(reply.error.is_some(), "refused before anything is sent");
}

#[test]
fn the_service_answers_through_a_channel() {
    let server = fake(&french_answer(&a_match(TEXT, "Accord", "R", "grammar", 0, 3, "")), 1);
    let service = service();
    let channel = service(request(&server.url, TEXT, vec![Language::FrCa], Vec::new()));
    let reply = channel.recv_timeout(std::time::Duration::from_secs(10)).expect("an answer");
    assert_eq!(reply.diagnostics.len(), 1);
    assert!(reply.error.is_none(), "{:?}", reply.error);
}
