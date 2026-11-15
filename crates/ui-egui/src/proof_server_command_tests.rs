// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! `ui.proofServer`: one owner/script seam over the managed server, with a passive cached query
//! and exactly one admitted request per action (#954). No window or frame is involved.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use mailcraft_engine::Session;
use mailcraft_engine::agent::Class;
use serde_json::{Value, json};

use crate::proof_server::{Action, Issue, Java, ManagedServer, Phase, Status};
use crate::{MailApp, Services};

#[derive(Default)]
struct Fake {
    state: Mutex<Status>,
    reads: AtomicUsize,
    requests: Mutex<Vec<Action>>,
    refuse: Mutex<Option<Issue>>,
}

impl Fake {
    fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }

    fn requests(&self) -> Vec<Action> {
        self.requests.lock().unwrap().clone()
    }

    fn refuse(&self, issue: Issue) {
        *self.refuse.lock().unwrap() = Some(issue);
    }
}

impl ManagedServer for Fake {
    fn status(&self) -> Status {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.state.lock().unwrap().clone()
    }

    fn request(&self, action: Action) -> Result<(), Issue> {
        self.requests.lock().unwrap().push(action);
        match *self.refuse.lock().unwrap() {
            Some(issue) => Err(issue),
            None => Ok(()),
        }
    }
}

const NOW: i64 = 1_791_993_600;

fn app(status: Status) -> (MailApp, Arc<Fake>) {
    let host = Arc::new(Fake { state: Mutex::new(status), ..Default::default() });
    let services = Services { proof_server: Some(host.clone()), ..Services::default() };
    let mut app = MailApp::new(Session::at(mailcraft_engine::sample::mailbox(NOW, 0), NOW, 0), services);
    app.autosave = false;
    (app, host)
}

fn without_a_host() -> MailApp {
    let mut app = MailApp::new(Session::at(mailcraft_engine::sample::mailbox(NOW, 0), NOW, 0), Services::default());
    app.autosave = false;
    app
}

#[rustfmt::skip]
const ACTIONS: [(&str, Action); 5] = [
    ("check", Action::Check),
    ("download", Action::Download),
    ("start", Action::Start),
    ("stop", Action::Stop),
    ("cancel", Action::Cancel),
];

#[test]
fn a_query_returns_the_cached_snapshot_without_a_frame_and_asks_for_no_work() {
    let (mut app, host) = app(Status {
        phase: Phase::Installed,
        java: Java::Available,
        version: "6.6".into(),
        downloaded_bytes: 0,
        total_bytes: 0,
        endpoint: None,
        epoch: 3,
        issue: None,
    });
    // No egui context, no frame: the command must answer from the host's cached snapshot.
    let v = app.execute("ui.proofServer", json!({})).unwrap();
    assert_eq!(v["available"].as_bool(), Some(true));
    assert_eq!(v["phase"].as_str(), Some("Installed"));
    assert_eq!(v["java"].as_str(), Some("Available"));
    assert_eq!(v["version"].as_str(), Some("6.6"));
    assert_eq!(v["progress"]["downloaded"].as_u64(), Some(0));
    assert_eq!(v["progress"]["total"].as_u64(), Some(0));
    assert_eq!(v["issue"], Value::Null);
    assert_eq!(v["epoch"].as_u64(), Some(3));
    assert_eq!(v["endpoint"], Value::Null);
    assert_eq!(host.reads(), 1, "one cached read");
    assert!(host.requests().is_empty(), "a query never probes, downloads, spawns or queues");
    assert!(app.dialog.is_none());
}

#[test]
fn every_action_is_forwarded_exactly_once_and_the_cached_status_comes_back() {
    for (name, action) in ACTIONS {
        let (mut app, host) = app(Status { phase: Phase::Stopped, ..Default::default() });
        let v = app.execute("ui.proofServer", json!({ "action": name })).unwrap();
        assert_eq!(host.requests(), vec![action], "{name}");
        assert_eq!(host.reads(), 1, "{name}: the answer is the cached snapshot after admission");
        assert_eq!(v["phase"].as_str(), Some("Stopped"), "{name}: no completion is invented");
        assert!(app.dialog.is_none(), "{name}: a programmatic command opens no dialog");
    }
}

#[test]
fn an_agent_with_every_permission_may_read_but_never_operate_the_server() {
    let (mut app, host) = app(Status { phase: Phase::Installed, ..Default::default() });
    // Every permission `--allow` can name: still not consent to install or run a program.
    app.session.agent = Some(Class::ALL.to_vec());
    let v = app.execute("ui.proofServer", json!({})).unwrap();
    assert_eq!(v["phase"].as_str(), Some("Installed"));
    assert_eq!(host.reads(), 1);
    for (name, _) in ACTIONS {
        let said = app.execute("ui.proofServer", json!({ "action": name })).unwrap_err();
        assert!(said.contains("owner"), "{name}: {said}");
        assert!(!said.contains(name), "{name}: the refusal echoes no parameter");
    }
    assert!(host.requests().is_empty(), "no agent action reaches the host");
    assert_eq!(host.reads(), 1, "a refused action reads no status");
    // The same command from the owner's or a script's side is admitted once.
    app.session.agent = None;
    app.execute("ui.proofServer", json!({ "action": "check" })).unwrap();
    assert_eq!(host.requests(), vec![Action::Check]);
}

#[test]
fn without_a_host_the_query_is_unavailable_and_every_action_is_refused() {
    let mut app = without_a_host();
    let v = app.execute("ui.proofServer", Value::Null).unwrap();
    assert_eq!(v["available"].as_bool(), Some(false));
    assert_eq!(v["phase"].as_str(), Some("Unavailable"));
    assert_eq!(v["java"].as_str(), Some("Unknown"));
    assert_eq!(v["version"].as_str(), Some(""));
    assert_eq!(v["progress"]["downloaded"].as_u64(), Some(0));
    assert_eq!(v["progress"]["total"].as_u64(), Some(0));
    assert_eq!(v["issue"], Value::Null);
    assert_eq!(v["epoch"].as_u64(), Some(0));
    assert_eq!(v["endpoint"], Value::Null);
    for (name, _) in ACTIONS {
        let said = app.execute("ui.proofServer", json!({ "action": name })).unwrap_err();
        assert!(said.contains("not available"), "{name}: {said}");
    }
    assert!(app.dialog.is_none());
}

#[test]
fn invalid_actions_and_stray_parameters_are_refused_before_the_host_is_touched() {
    let bad = [
        json!({ "action": "install" }),
        json!({ "action": "Check" }),
        json!({ "action": 1 }),
        json!({ "action": true }),
        json!({ "action": {} }),
        json!({ "action": [] }),
        json!({ "action": "check", "path": "/tmp/tool.jar" }),
        json!({ "action": "download", "url": "https://example.org/x" }),
        json!("check"),
        json!(["check"]),
        json!(7),
    ];
    for params in bad {
        let (mut app, host) = app(Status::default());
        let said = app.execute("ui.proofServer", params.clone()).unwrap_err();
        assert!(!said.is_empty(), "{params}");
        assert_eq!(host.reads(), 0, "{params}: no status is read");
        assert!(host.requests().is_empty(), "{params}: no request is made");
        assert!(app.dialog.is_none(), "{params}: no dialog");
    }
    // An explicit null action, and no params at all, are both a plain query.
    for params in [json!({ "action": null }), json!({}), Value::Null] {
        let (mut app, host) = app(Status { phase: Phase::Running, ..Default::default() });
        app.execute("ui.proofServer", params.clone()).unwrap();
        assert_eq!(host.reads(), 1, "{params}");
        assert!(host.requests().is_empty(), "{params}");
    }
}

#[test]
fn only_a_running_validated_loopback_endpoint_is_published() {
    let local = "http://127.0.0.1:18081";
    let by_name = "http://localhost:18081";
    let cases = [
        (Status { phase: Phase::Running, endpoint: Some(local.into()), ..Default::default() }, Some(local)),
        (Status { phase: Phase::Running, endpoint: Some(by_name.into()), ..Default::default() }, Some(by_name)),
        (Status { phase: Phase::Running, endpoint: Some("https://grammar.example".into()), ..Default::default() }, None),
        (Status { phase: Phase::Running, endpoint: Some("http://192.168.1.4:18081".into()), ..Default::default() }, None),
        (Status { phase: Phase::Running, endpoint: Some("not a url".into()), ..Default::default() }, None),
        (Status { phase: Phase::Installed, endpoint: Some("http://127.0.0.1:18081".into()), ..Default::default() }, None),
        (Status { phase: Phase::Running, endpoint: None, ..Default::default() }, None),
    ];
    for (status, want) in cases {
        let phase = format!("{:?}", status.phase);
        let (mut app, _host) = app(status);
        let v = app.execute("ui.proofServer", json!({})).unwrap();
        assert_eq!(v["phase"].as_str(), Some(phase.as_str()));
        match want {
            Some(url) => assert_eq!(v["endpoint"].as_str(), Some(url)),
            None => assert_eq!(v["endpoint"], Value::Null),
        }
    }
}

#[test]
fn the_snapshot_is_fixed_labels_and_bounded_text() {
    let long = "v".repeat(200);
    let (mut app, _host) = app(Status {
        phase: Phase::Failed,
        java: Java::TooOld,
        version: long,
        downloaded_bytes: 10,
        total_bytes: 20,
        epoch: 9,
        issue: Some(Issue::IntegrityFailed),
        ..Default::default()
    });
    let v = app.execute("ui.proofServer", json!({})).unwrap();
    assert_eq!(v["phase"].as_str(), Some("Failed"));
    assert_eq!(v["java"].as_str(), Some("TooOld"));
    assert_eq!(v["version"].as_str().map(str::len), Some(64), "the host's text is bounded");
    assert_eq!(v["progress"]["downloaded"].as_u64(), Some(10));
    assert_eq!(v["progress"]["total"].as_u64(), Some(20));
    assert_eq!(v["issue"].as_str(), Some("IntegrityFailed"));
    assert_eq!(v["epoch"].as_u64(), Some(9));
}

#[test]
fn a_refused_action_answers_fixed_text_without_the_params_or_a_path() {
    let (mut app, host) = app(Status { phase: Phase::Installed, ..Default::default() });
    host.refuse(Issue::Busy);
    let said = app.execute("ui.proofServer", json!({ "action": "download" })).unwrap_err();
    assert_eq!(said, "The built-in server is busy with another request; nothing else was started.");
    assert!(!said.contains('/'), "no path");
    assert!(!said.contains("download"), "no parameter is echoed");
    assert_eq!(host.requests(), vec![Action::Download], "the one refusal came from one admission");
}

#[test]
fn the_command_is_registered_once_and_the_owner_can_run_it() {
    let rows: Vec<_> = crate::UI_COMMANDS.iter().filter(|(id, ..)| *id == "ui.proofServer").collect();
    assert_eq!(rows.len(), 1, "one registered window command");
    assert!(rows[0].3.contains("\"action\""), "its params are documented");
    let (mut app, host) = app(Status { phase: Phase::Installed, ..Default::default() });
    assert!(app.enabled("ui.proofServer"));
    assert!(app.session.registry.get("ui.proofServer").is_none(), "not an engine command");
    app.run("ui.proofServer", json!({ "action": "start" })).unwrap();
    assert_eq!(host.requests(), vec![Action::Start]);
    assert!(app.dialog.is_none(), "no dialog opens");
}
