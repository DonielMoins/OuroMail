// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Local-only per-email language metadata, separate from global defaults and wire formats.

use mailcraft_engine::Compose;
use mailcraft_model::Message;
use serde_json::{Value, json};

#[test]
fn a_saved_draft_round_trips_its_local_proofing_languages() {
    let chosen = json!(["en-CA", "fr-FR"]);
    let draft: Message = serde_json::from_value(json!({"draft": true, "proofLanguages": chosen})).unwrap();
    let saved = serde_json::to_value(draft).unwrap();
    assert_eq!(saved.get("proofLanguages"), Some(&chosen));
}

#[test]
fn a_compose_round_trips_its_own_languages_without_global_settings() {
    let chosen = json!(["fr-CA"]);
    let compose: Compose = serde_json::from_value(json!({"proofLanguages": chosen})).unwrap();
    let saved = serde_json::to_value(compose).unwrap();
    assert_eq!(saved.get("proofLanguages"), Some(&chosen));
}

#[test]
fn older_drafts_and_composes_need_no_language_override() {
    let draft: Message = serde_json::from_value(json!({"draft": true})).unwrap();
    let compose: Compose = serde_json::from_value(json!({})).unwrap();
    for saved in [serde_json::to_value(draft).unwrap(), serde_json::to_value(compose).unwrap()] {
        assert!(saved.get("proofLanguages").is_none_or(Value::is_null));
    }
}
