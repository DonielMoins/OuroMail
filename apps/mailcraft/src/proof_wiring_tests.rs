// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! The desktop must install the provider the asynchronous proofreading UI actually asks (#954).

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use mailcraft_model::proof::ProofSettings;
use mailcraft_ui_egui::proof_service::{Failure, SpellingRequest};

#[test]
fn desktop_services_install_a_passive_managed_proofing_host() {
    use mailcraft_ui_egui::proof_server::Phase;
    let ctx = egui::Context::default();
    let (file_dialog, _launcher) = crate::file_dialogs::hook(&ctx);
    let services = crate::services(file_dialog, ctx, None, None);
    let host = services.proof_server.as_ref().expect("the desktop must offer the actual managed host, not just its Options controls");
    let before = host.status();
    assert!(matches!(before.phase, Phase::Unchecked | Phase::Unavailable));
    for _ in 0..16 {
        assert_eq!(host.status(), before, "status must never discover, download or start anything");
    }
    assert!(before.endpoint.is_none());
    assert_eq!(before.epoch, 0);
}

#[test]
fn desktop_services_install_the_native_proofing_provider() {
    let ctx = egui::Context::default();
    let (file_dialog, _launcher) = crate::file_dialogs::hook(&ctx);
    let services = crate::services(file_dialog, ctx, None, None);
    assert!(services.proof_native.is_some(), "the UI asks proof_native, not the legacy grammar-only hook");
    let provider = services.proof_native.unwrap();
    // An already-cancelled synthetic request exercises the installed worker without reading any
    // dictionary or making a request. Merely constructing services must not start a grammar check.
    let reply = provider
        .spelling(SpellingRequest {
            text: "synthetic proofing wiring canary".into(),
            settings: ProofSettings::default(),
            cancelled: Arc::new(AtomicBool::new(true)),
        })
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    assert_eq!(reply.error, Some(Failure::Cancelled));
    assert!(reply.diagnostics.is_empty());
}
