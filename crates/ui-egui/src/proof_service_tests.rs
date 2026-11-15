// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use mailcraft_model::proof::{Diagnostic, Kind, Language, ProofSettings, Source, Span};

use crate::proof_service::*;

#[test]
fn provider_request_debug_never_contains_draft_settings_or_suggestion_word() {
    let secret = "synthetic-private-canary-954";
    let cancelled = Arc::new(AtomicBool::new(false));
    let settings = ProofSettings {
        dictionary_dir: secret.to_string(),
        personal: vec![secret.to_string()],
        grammar_url: format!("http://127.0.0.1:18081/{secret}"),
        ..Default::default()
    };
    let spelling = SpellingRequest { text: secret.to_string(), settings: settings.clone(), cancelled: cancelled.clone() };
    let grammar = GrammarRequest {
        url: settings.grammar_url.clone(),
        text: secret.to_string(),
        languages: vec![Language::EnCa],
        ignored_rules: vec![secret.to_string()],
        cancelled: cancelled.clone(),
    };
    let suggestion = SuggestionRequest { word: secret.to_string(), settings, limit: 5, cancelled };
    for debug in [format!("{spelling:?}"), format!("{grammar:?}"), format!("{suggestion:?}")] {
        assert!(!debug.contains(secret), "request Debug leaked the synthetic canary");
        assert!(!debug.contains("127.0.0.1"), "request Debug leaked an endpoint");
    }
}

#[test]
fn provider_reply_debug_never_contains_diagnostics_notes_or_suggestions() {
    let secret = "synthetic-private-canary-954";
    let diagnostic = Diagnostic::new(Span { start: 0, end: 1 }, Kind::Grammar, Source::Server).saying(secret).suggesting([secret.to_string()]);
    let spelling = SpellingReply { diagnostics: vec![diagnostic.clone()], note: Some(secret.to_string()), error: None };
    let grammar = GrammarReply { diagnostics: vec![diagnostic], partial: true, checked_segments: 1, ..Default::default() };
    let suggestion = SuggestionReply { suggestions: vec![secret.to_string()], error: None };
    for debug in [format!("{spelling:?}"), format!("{grammar:?}"), format!("{suggestion:?}")] {
        assert!(!debug.contains(secret), "reply Debug leaked the synthetic canary");
    }
}

#[test]
fn cached_ui_place_debug_never_exposes_draft_or_server_words() {
    let canaries = ["private-draft-word-954", "private-rule-954", "private-explanation-954", "private-replacement-954"];
    let place = crate::proof::Place {
        from: 1,
        to: 3,
        kind: Kind::Grammar,
        source: Source::Server,
        language: Some(Language::FrCa),
        word: canaries[0].into(),
        rule: Some(canaries[1].into()),
        message: canaries[2].into(),
        suggestions: vec![canaries[3].into()],
    };
    let direct = format!("{place:?}");
    let wrapped = format!("{:?}", Some(vec![place]));
    for debug in [direct, wrapped] {
        assert!(debug.contains("Place"), "Debug remains useful without private content");
        for canary in canaries {
            assert!(!debug.contains(canary), "cached diagnostic Debug leaked private content");
        }
    }
}
