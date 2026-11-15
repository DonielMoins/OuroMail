// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! The UI's managed-server query is readable, but an agent never operates its lifecycle. These
//! public-API checks stay small enough to run without compiling the engine's whole unit suite.

use mailcraft_engine::Session;
use mailcraft_engine::agent::{Access, Class, Verdict, access, check, classify};
use serde_json::{Value, json};

fn sample() -> Session {
    Session::new(mailcraft_engine::sample::mailbox(1_791_993_600, 0))
}

#[test]
fn managed_proof_server_status_is_safe_for_agents() {
    let s = sample();
    assert_eq!(access("ui.proofServer"), Some(Access::Safe));
    for params in [Value::Null, json!({}), json!({"action": null})] {
        assert_eq!(classify(&s, "ui.proofServer", &params), Access::Safe, "{params}");
        assert_eq!(check(&s, "ui.proofServer", &params, &[]), Verdict::Run, "{params}");
    }
}

#[test]
fn managed_proof_server_actions_are_never_available_to_agents() {
    let s = sample();
    for action in [json!("check"), json!("download"), json!("start"), json!("stop"), json!("cancel"), json!(""), json!(false), json!([])] {
        let params = json!({"action": action});
        assert_eq!(classify(&s, "ui.proofServer", &params), Access::Never, "{params}");
        assert!(matches!(check(&s, "ui.proofServer", &params, &Class::ALL), Verdict::Refuse(_)), "{params}");
    }
}
