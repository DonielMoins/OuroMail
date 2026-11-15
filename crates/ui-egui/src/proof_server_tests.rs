// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! A cached managed-server snapshot is never permission to download or start anything (#954).

use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use mailcraft_engine::Session;
use serde_json::json;

use crate::proof_server::{Action, Issue, ManagedServer, Phase, Status};
use crate::proof_service::{GrammarReply, GrammarRequest, ProofService, SpellingReply, SpellingRequest, SuggestionReply, SuggestionRequest};
use crate::{MailApp, Services};

#[derive(Default)]
struct Managed {
    state: Mutex<Status>,
    actions: Mutex<Vec<Action>>,
}

impl ManagedServer for Managed {
    fn status(&self) -> Status {
        self.state.lock().unwrap().clone()
    }

    fn request(&self, action: Action) -> Result<(), Issue> {
        self.actions.lock().unwrap().push(action);
        Ok(())
    }
}

#[derive(Default)]
struct Provider {
    grammar: Mutex<Vec<(GrammarRequest, Sender<GrammarReply>)>>,
}

impl ProofService for Provider {
    fn spelling(&self, _: SpellingRequest) -> Receiver<SpellingReply> {
        panic!("spelling is off in this fixture")
    }

    fn grammar(&self, request: GrammarRequest) -> Receiver<GrammarReply> {
        let (tx, rx) = channel();
        self.grammar.lock().unwrap().push((request, tx));
        rx
    }

    fn suggest(&self, _: SuggestionRequest) -> Receiver<SuggestionReply> {
        panic!("no suggestion was requested")
    }
}

fn app(status: Status) -> (MailApp, Arc<Managed>, Arc<Provider>) {
    let server = Arc::new(Managed { state: Mutex::new(status), ..Default::default() });
    let provider = Arc::new(Provider::default());
    let services = Services { proof_server: Some(server.clone()), proof_native: Some(provider.clone()), ..Services::default() };
    let now = 1_791_993_600;
    let mut app = MailApp::new(Session::at(mailcraft_engine::sample::mailbox(now, 0), now, 0), services);
    app.autosave = false;
    app.execute("mail.new", json!({})).unwrap();
    let len = app.session.doc().unwrap().0.plain().chars().count();
    app.execute("compose.select", json!({"from": 0, "to": len})).unwrap();
    app.execute("compose.type", json!({"text": "She have sent the documents."})).unwrap();
    app.session.prefs.proof.check_spelling = false;
    app.session.prefs.proof.check_grammar = true;
    app.session.prefs.proof.as_you_type = true;
    app.session.prefs.proof.managed_server = true;
    // A configured manual server must not become a fallback when the selected managed server
    // is unavailable, unverified, stopping, or still being called off.
    app.session.prefs.proof.grammar_url = "http://127.0.0.1:8123".into();
    (app, server, provider)
}

fn frame(ctx: &egui::Context, app: &mut MailApp, seconds: f64) {
    let mut output = ctx.run_ui(egui::RawInput { time: Some(seconds), ..Default::default() }, |_| {
        let body = crate::proof::body_of(app).unwrap();
        let text = app.session.doc().unwrap().0.plain();
        crate::proof::run(app, ctx, body, &text);
    });
    output.textures_delta.clear();
}

#[test]
fn a_managed_server_that_is_not_running_never_sends_text_or_starts_work() {
    for phase in [
        Phase::Unchecked,
        Phase::Unavailable,
        Phase::Checking,
        Phase::NotInstalled,
        Phase::Downloading,
        Phase::Extracting,
        Phase::Cancelling,
        Phase::Installed,
        Phase::Starting,
        Phase::Stopping,
        Phase::Stopped,
        Phase::Cancelled,
        Phase::Failed,
    ] {
        let (mut app, server, provider) = app(Status { phase, endpoint: Some("http://127.0.0.1:18081".into()), ..Default::default() });
        let ctx = egui::Context::default();
        frame(&ctx, &mut app, 1.0);
        frame(&ctx, &mut app, 1.5);
        crate::proof::test(&mut app);
        assert!(provider.grammar.lock().unwrap().is_empty(), "no text for {phase:?}, even with a stale endpoint or a manual URL");
        assert!(server.actions.lock().unwrap().is_empty(), "a frame or a connection test is not consent to operate the server");
    }
}

#[test]
fn even_a_running_managed_snapshot_cannot_authorize_a_remote_endpoint() {
    let (mut app, server, provider) = app(Status { phase: Phase::Running, endpoint: Some("https://grammar.example".into()), ..Default::default() });
    let ctx = egui::Context::default();
    frame(&ctx, &mut app, 1.0);
    frame(&ctx, &mut app, 1.5);
    crate::proof::test(&mut app);
    assert!(provider.grammar.lock().unwrap().is_empty());
    assert!(server.actions.lock().unwrap().is_empty());
}

#[test]
fn only_a_changed_managed_instance_invalidates_the_pending_grammar_check() {
    let (mut app, server, provider) =
        app(Status { phase: Phase::Running, endpoint: Some("http://127.0.0.1:18765".into()), epoch: 7, ..Default::default() });
    let ctx = egui::Context::default();
    frame(&ctx, &mut app, 1.0);
    frame(&ctx, &mut app, 1.4);
    assert_eq!(provider.grammar.lock().unwrap().len(), 1);
    let cancelled = provider.grammar.lock().unwrap().first().unwrap().0.cancelled.clone();
    server.state.lock().unwrap().downloaded_bytes = 1234;
    frame(&ctx, &mut app, 1.5);
    frame(&ctx, &mut app, 2.6);
    assert_eq!(provider.grammar.lock().unwrap().len(), 1, "progress is not another server instance");
    assert!(!cancelled.load(Ordering::Relaxed));
    server.state.lock().unwrap().epoch = 8;
    frame(&ctx, &mut app, 3.0);
    frame(&ctx, &mut app, 3.4);
    assert!(cancelled.load(Ordering::Relaxed), "the old instance's answer is no longer this draft's");
    let requests = provider.grammar.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests.last().unwrap().0.url, "http://127.0.0.1:18765");
    assert!(server.actions.lock().unwrap().is_empty());
}
