// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! The options fields must keep their input until the owner leaves the field, and the built-in
//! LanguageTool controls must ask the host only for the button the owner pressed.

use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use egui::{Context, Event, Id, RawInput};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use mailcraft_engine::Session;
use serde_json::json;

use crate::proof::{Reply as LegacyReply, Request as LegacyRequest};
use crate::proof_server::{Action, Issue, Java, ManagedServer, Phase, Status};
use crate::proof_service::{Failure, GrammarReply, GrammarRequest, ProofService, SpellingReply, SpellingRequest, SuggestionReply, SuggestionRequest};
use crate::{MailApp, Services};

/// The address the owner typed: choosing the built-in server must never rewrite it.
const MANUAL: &str = "http://127.0.0.1:8123";

/// Wednesday 2026-10-14 16:00 UTC, the sample mailbox's clock.
const NOW: i64 = 1_791_993_600;

fn frame(ctx: &Context, events: Vec<Event>, saved: &mut String) {
    let mut output = ctx.run_ui(RawInput { events, ..RawInput::default() }, |ui| {
        let (_, changed) = super::proofing_field(ui, "proofFieldTest", saved, "Server");
        if let Some(value) = changed {
            *saved = value;
        }
    });
    output.textures_delta.clear();
}

fn pending(ctx: &Context) -> (String, String) {
    ctx.data(|data| data.get_temp::<(String, String)>(Id::unique("proofFieldTest"))).unwrap()
}

#[test]
fn settings_input_survives_intervening_frames_and_commits_on_focus_loss() {
    let ctx = Context::default();
    let id = Id::unique("proofFieldTest");
    let mut saved = String::new();
    frame(&ctx, Vec::new(), &mut saved);
    ctx.memory_mut(|memory| memory.request_focus(id));
    frame(&ctx, vec![Event::Text("http://127.0.0.1:".into())], &mut saved);
    assert_eq!(pending(&ctx).1, "http://127.0.0.1:");
    frame(&ctx, Vec::new(), &mut saved);
    frame(&ctx, vec![Event::Text("8081".into())], &mut saved);
    assert_eq!(pending(&ctx).1, "http://127.0.0.1:8081");
    assert!(saved.is_empty(), "typing must not commit partially entered settings");
    frame(&ctx, Vec::new(), &mut saved);
    assert_eq!(pending(&ctx).1, "http://127.0.0.1:8081");
    ctx.memory_mut(|memory| memory.surrender_focus(id));
    frame(&ctx, Vec::new(), &mut saved);
    assert_eq!(saved, "http://127.0.0.1:8081");
}

#[test]
fn a_setting_changed_by_a_command_replaces_the_old_edit_buffer() {
    let ctx = Context::default();
    let mut saved = "old".to_string();
    frame(&ctx, Vec::new(), &mut saved);
    saved = "new setting".to_string();
    frame(&ctx, Vec::new(), &mut saved);
    assert_eq!(pending(&ctx), ("new setting".into(), "new setting".into()));
}

// ---------------------------------------------------------------------------------------------
// The built-in server's controls, driven by real frames and pointer clicks against a host of
// this test's own.
// ---------------------------------------------------------------------------------------------

type Window = Harness<'static, MailApp>;

/// A host that records every admitted operation and moves to the phase a real host would be in
/// while it works, so the next frame sees pending work. A snapshot's phase can be changed
/// between frames to stand for whatever the real download, extraction, or reaping reports.
#[derive(Default)]
struct Host {
    state: Mutex<Status>,
    actions: Mutex<Vec<Action>>,
    /// When set, every request is refused with this kind and the phase does not move.
    refuse: Mutex<Option<Issue>>,
    /// When set, a request is admitted but the reported phase does not move yet: a host that has
    /// taken the work but not published it, which leaves the same button on screen and enabled.
    hold: Mutex<bool>,
}

impl Host {
    fn new(status: Status) -> Arc<Host> {
        Arc::new(Host { state: Mutex::new(status), ..Default::default() })
    }

    fn actions(&self) -> Vec<Action> {
        self.actions.lock().unwrap().clone()
    }

    fn set(&self, status: Status) {
        *self.state.lock().unwrap() = status;
    }
}

impl ManagedServer for Host {
    fn status(&self) -> Status {
        self.state.lock().unwrap().clone()
    }

    fn request(&self, action: Action) -> Result<(), Issue> {
        self.actions.lock().unwrap().push(action);
        if let Some(issue) = *self.refuse.lock().unwrap() {
            return Err(issue);
        }
        if *self.hold.lock().unwrap() {
            return Ok(());
        }
        self.state.lock().unwrap().phase = match action {
            Action::Check => Phase::Checking,
            Action::Download => Phase::Downloading,
            Action::Cancel => Phase::Cancelling,
            Action::Start => Phase::Starting,
            Action::Stop => Phase::Stopping,
        };
        Ok(())
    }
}

/// File › Options on the sample mailbox, with `services` as its host and the built-in server
/// chosen (or not) as `managed`. The window is taller than the page so no control is clipped out
/// of the pointer's reach.
fn window_with(services: Services, managed: bool) -> Window {
    window_in(services, managed, "en")
}

/// As [`window_with`], with the interface in `language`. The choice is the window's own
/// (`ui.language`, the command File › Options runs), not a global set outside it: the window puts
/// its language in force at every frame, so a test that changed the global alone would be undone.
fn window_in(services: Services, managed: bool, language: &str) -> Window {
    let mut app = MailApp::new(Session::at(mailcraft_engine::sample::mailbox(NOW, 0), NOW, 0), services);
    app.autosave = false;
    app.ui.backstage = true;
    app.ui.backstage_page = "options".into();
    app.session.prefs.proof.check_spelling = false;
    app.session.prefs.proof.check_grammar = true;
    app.session.prefs.proof.managed_server = managed;
    app.session.prefs.proof.grammar_url = MANUAL.into();
    app.execute("ui.language", json!({ "language": language })).expect("the interface language");
    let mut window = Harness::builder()
        .with_accessibility_check(false)
        .allow_missing_glyphs()
        .with_size(egui::vec2(1280.0, 4000.0))
        .with_step_dt(1.0 / 60.0)
        .build_ui_state(
            |ui, app: &mut MailApp| {
                let ctx = ui.ctx().clone();
                app.logic(&ctx);
                app.ui(ui);
            },
            app,
        );
    window.run_steps(5);
    window
}

/// The page with a host reporting `status` and the built-in server chosen.
fn window(status: Status) -> (Window, Arc<Host>) {
    let host = Host::new(status);
    let services = Services { proof_server: Some(host.clone()), ..Services::default() };
    (window_with(services, true), host)
}

fn rect(window: &Window, key: &str) -> egui::Rect {
    window.state().hit.rect(key, 0).unwrap_or_else(|| panic!("{key} is not on screen"))
}

fn optional_rect(window: &Window, key: &str) -> Option<egui::Rect> {
    window.state().hit.rect(key, 0)
}

/// A press and release on `pos`, one pointer event per frame.
fn click(window: &mut Window, pos: egui::Pos2) {
    window.input_mut().events.push(Event::PointerMoved(pos));
    window.step();
    for pressed in [true, false] {
        window.input_mut().events.push(Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE });
        window.step();
    }
    window.step();
}

/// Click the control recorded under `key`.
fn click_key(window: &mut Window, key: &str) {
    let pos = rect(window, key).center();
    click(window, pos);
}

/// Whether anything on screen says `text`.
fn says(window: &Window, text: &str) -> bool {
    window.query_all_by_label_contains(text).next().is_some()
}

fn prefs(window: &Window) -> mailcraft_model::proof::ProofSettings {
    window.state().session.prefs.proof.clone()
}

const CONTROLS: [&str; 5] = ["proofManagedCheck", "proofManagedDownload", "proofManagedCancel", "proofManagedStart", "proofManagedStop"];

/// One press on a control, and what that same control is afterwards: still on screen but disabled
/// while its own operation is the one in flight, or gone once the phase no longer offers it. The
/// two are different, and a test that accepts either is not testing the disabled case.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum After {
    Disabled(&'static str),
    Absent,
}

#[test]
fn what_the_controls_offer_is_decided_by_the_cached_phase() {
    use super::managed_options as options;
    use crate::proof_server::Phase;

    // Nothing inspected yet is a question, not a verdict: the read-only check, and nothing else.
    let unchecked = options(Phase::Unchecked, Java::Unknown, None);
    assert!(unchecked.check && !unchecked.download && !unchecked.start && !unchecked.stop);
    assert_eq!(unchecked.pending, None);

    let checking = options(Phase::Checking, Java::Unknown, None);
    assert_eq!(checking.pending, Some(Action::Check), "a check in flight admits nothing else");

    let absent = options(Phase::NotInstalled, Java::Unknown, None);
    assert!(absent.download && absent.check && !absent.start);

    // Cancellation and stopping are pending until the host has finished cleaning up.
    assert_eq!(options(Phase::Cancelling, Java::Available, None).pending, Some(Action::Cancel));
    assert_eq!(options(Phase::Stopping, Java::Available, None).pending, Some(Action::Stop));

    // Starting needs Java, which is only ever discovered, never installed by MailCraft.
    assert!(options(Phase::Installed, Java::Available, None).start);
    assert!(!options(Phase::Installed, Java::Missing, None).start);
    assert!(!options(Phase::Installed, Java::TooOld, None).start);

    // A copy that did not verify is left alone: no download, no repair.
    assert!(!options(Phase::Failed, Java::Available, Some(Issue::IntegrityFailed)).download);
    assert!(!options(Phase::Failed, Java::Available, Some(Issue::InvalidArchive)).download);
    // A download that never arrived may be asked for again, by a press.
    assert!(options(Phase::Failed, Java::Available, Some(Issue::DownloadFailed)).download);
}

#[test]
fn every_phase_offers_exactly_the_operations_its_state_allows() {
    use super::managed_options as options;
    use crate::proof_server::Phase;

    // Every phase, including the ones with no operation at all and the ones still finishing:
    // (phase, java, issue, check, download, cancel, start, stop, pending)
    let cases = [
        (Phase::Unavailable, Java::Unknown, None, false, false, false, false, false, None),
        (Phase::Unchecked, Java::Unknown, None, true, false, false, false, false, None),
        (Phase::Checking, Java::Unknown, None, false, false, false, false, false, Some(Action::Check)),
        (Phase::NotInstalled, Java::Unknown, None, true, true, false, false, false, None),
        (Phase::Downloading, Java::Unknown, None, false, false, true, false, false, None),
        (Phase::Extracting, Java::Unknown, None, false, false, true, false, false, None),
        (Phase::Cancelling, Java::Available, None, false, false, false, false, false, Some(Action::Cancel)),
        (Phase::Installed, Java::Available, None, true, false, false, true, false, None),
        (Phase::Installed, Java::Missing, None, true, false, false, false, false, None),
        (Phase::Installed, Java::TooOld, None, true, false, false, false, false, None),
        (Phase::Starting, Java::Available, None, false, false, false, false, false, Some(Action::Start)),
        (Phase::Running, Java::Available, None, false, false, false, false, true, None),
        (Phase::Stopping, Java::Available, None, false, false, false, false, false, Some(Action::Stop)),
        (Phase::Stopped, Java::Available, None, true, false, false, true, false, None),
        (Phase::Stopped, Java::Missing, None, true, false, false, false, false, None),
        (Phase::Cancelled, Java::Unknown, None, true, true, false, false, false, None),
        // A failure with no kind leaves the read-only check and no retry: nothing is guessed at.
        (Phase::Failed, Java::Available, None, true, false, false, false, false, None),
        (Phase::Failed, Java::Available, Some(Issue::DownloadFailed), true, true, false, false, false, None),
        (Phase::Failed, Java::Available, Some(Issue::IntegrityFailed), true, false, false, false, false, None),
        (Phase::Failed, Java::Available, Some(Issue::InvalidArchive), true, false, false, false, false, None),
        (Phase::Failed, Java::Available, Some(Issue::StartFailed), true, false, false, false, false, None),
        (Phase::Failed, Java::Missing, Some(Issue::JavaMissing), true, false, false, false, false, None),
    ];
    for (phase, java, issue, has_check, has_download, has_cancel, has_start, has_stop, pending) in cases {
        let got = options(phase, java, issue);
        let expect = (has_check, has_download, has_cancel, has_start, has_stop, pending);
        assert_eq!((got.check, got.download, got.cancel, got.start, got.stop, got.pending), expect, "{phase:?}/{java:?}/{issue:?}");
    }
}

#[test]
fn frames_and_mode_toggles_never_check_download_start_or_stop_anything() {
    let (mut window, host) = window(Status { phase: Phase::Unchecked, ..Default::default() });
    window.run_steps(10);
    assert!(host.actions().is_empty(), "drawing the page is not consent to operate the server");
    assert_eq!(prefs(&window).grammar_url, MANUAL);

    // Turning the built-in server off and on again is a setting, not an operation.
    click_key(&mut window, "proofManagedMode");
    assert!(!prefs(&window).managed_server);
    assert_eq!(prefs(&window).grammar_url, MANUAL, "turning the built-in server off rewrites no address");
    window.run_steps(5);
    click_key(&mut window, "proofManagedMode");
    assert!(prefs(&window).managed_server);
    window.run_steps(10);
    assert!(host.actions().is_empty(), "choosing the built-in server is not permission to download or run it");
    assert_eq!(prefs(&window).grammar_url, MANUAL);
}

#[test]
fn each_owner_operation_is_asked_once_when_its_own_button_is_clicked() {
    let cases = [
        (Phase::Unchecked, Java::Unknown, None, "proofManagedCheck", Action::Check, After::Disabled("Checking…")),
        (Phase::NotInstalled, Java::Unknown, None, "proofManagedDownload", Action::Download, After::Absent),
        (Phase::Downloading, Java::Unknown, None, "proofManagedCancel", Action::Cancel, After::Disabled("Cancelling…")),
        (Phase::Installed, Java::Available, None, "proofManagedStart", Action::Start, After::Disabled("Starting…")),
        (Phase::Stopped, Java::Available, None, "proofManagedStart", Action::Start, After::Disabled("Starting…")),
        (Phase::Cancelled, Java::Unknown, None, "proofManagedDownload", Action::Download, After::Absent),
        (Phase::Running, Java::Available, None, "proofManagedStop", Action::Stop, After::Disabled("Stopping…")),
    ];
    for (phase, java, issue, key, action, after) in cases {
        let (mut window, host) = window(Status { phase, java, issue, ..Default::default() });
        window.run_steps(3);
        assert!(host.actions().is_empty(), "{phase:?}: frames alone ask nothing");
        click_key(&mut window, key);
        assert_eq!(host.actions(), vec![action], "{phase:?}: only the clicked button's operation");
        // The host now reports the work in flight. Either the same control stays on screen and is
        // disabled, or the phase no longer offers it at all: a disabled one must take no second
        // press, and a missing one must not be mistaken for a disabled one.
        window.run_steps(3);
        match after {
            After::Absent => assert!(optional_rect(&window, key).is_none(), "{phase:?}: {key} is not drawn once its operation started"),
            After::Disabled(said) => {
                let shown = optional_rect(&window, key).unwrap_or_else(|| panic!("{phase:?}: {key} stays on screen while its operation runs"));
                assert!(says(&window, said), "{phase:?}: the pending operation is said in words");
                click(&mut window, shown.center());
                assert_eq!(host.actions(), vec![action], "{phase:?}: a disabled control admits nothing");
            }
        }
    }
}

#[test]
fn a_pending_cancellation_or_stop_disables_every_conflicting_control() {
    for phase in [Phase::Checking, Phase::Starting, Phase::Cancelling, Phase::Stopping] {
        let (mut window, host) = window(Status { phase, java: Java::Available, ..Default::default() });
        window.run_steps(3);
        let mut on_screen = false;
        for key in CONTROLS {
            if let Some(rect) = optional_rect(&window, key) {
                on_screen = true;
                click(&mut window, rect.center());
            }
        }
        assert!(on_screen, "{phase:?}: the operation still in flight is on screen");
        assert!(host.actions().is_empty(), "{phase:?}: nothing is admitted while cleanup is pending");
        let pending = match phase {
            Phase::Checking => "Checking…",
            Phase::Starting => "Starting…",
            Phase::Cancelling => "Cancelling…",
            _ => "Stopping…",
        };
        assert!(says(&window, pending), "{phase:?}: said in words, not shown as done");
    }
}

#[test]
fn without_java_start_is_not_offered_and_java_is_never_installed() {
    for java in [Java::Missing, Java::TooOld] {
        let (mut window, host) = window(Status { phase: Phase::Installed, java, ..Default::default() });
        window.run_steps(3);
        assert!(optional_rect(&window, "proofManagedStart").is_none(), "{java:?}: no Start without Java 17");
        for key in CONTROLS {
            if let Some(rect) = optional_rect(&window, key) {
                click(&mut window, rect.center());
            }
        }
        assert_eq!(host.actions(), vec![Action::Check], "{java:?}: the read-only check is all that is offered, and nothing was started");
        assert!(says(&window, "Java 17 or newer"), "{java:?}: said in words");
        // The words never offer to install Java: only the owner does that.
        assert!(!says(&window, "Install Java"), "{java:?}: no automatic Java install");
    }
}

#[test]
fn a_build_without_the_service_says_so_and_offers_no_operation() {
    let mut window = window_with(Services::default(), true);
    window.run_steps(5);
    assert!(says(&window, "cannot run a built-in LanguageTool server"));
    // The advice is actionable: the switch above is what makes the kept address editable, and the
    // field it names is on screen but disabled while the switch is on.
    assert!(says(&window, "Turn this switch off"), "the advice names the control that frees the address");
    assert!(!says(&window, "address above"), "the address above cannot be edited while this is shown");
    assert!(optional_rect(&window, "proofServer").is_some(), "the kept address is still shown");
    for key in CONTROLS {
        assert!(optional_rect(&window, key).is_none(), "{key} must not be offered by a serverless build");
    }
    assert_eq!(prefs(&window).grammar_url, MANUAL);
}

#[test]
fn the_manual_address_is_kept_and_cannot_be_typed_while_the_built_in_server_is_chosen() {
    let (mut window, _host) = window(Status { phase: Phase::Unchecked, ..Default::default() });
    window.run_steps(3);
    assert!(prefs(&window).managed_server);
    assert_eq!(prefs(&window).grammar_url, MANUAL);

    // The field is drawn disabled. A click and text go nowhere, and leaving it cannot commit.
    click_key(&mut window, "proofServer");
    window.input_mut().events.push(Event::Text("http://127.0.0.1:9999".into()));
    window.run_steps(2);
    click_key(&mut window, "proofManagedMode");
    window.run_steps(3);
    assert!(!prefs(&window).managed_server);
    assert_eq!(prefs(&window).grammar_url, MANUAL, "the address the owner typed survives the built-in server");

    // And back on: the manual address is not cleared by choosing the built-in server.
    click_key(&mut window, "proofManagedMode");
    window.run_steps(3);
    assert!(prefs(&window).managed_server);
    assert_eq!(prefs(&window).grammar_url, MANUAL);
}

#[test]
fn download_progress_is_shown_but_extracting_has_no_percentage() {
    let before = Status {
        phase: Phase::Downloading,
        downloaded_bytes: 512 * 1024,
        total_bytes: 2 * 1024 * 1024,
        version: "6.4".into(),
        epoch: 4,
        ..Default::default()
    };
    let (mut window, host) = window(before.clone());
    window.run_steps(3);
    assert!(says(&window, "25%"), "half a megabyte of two is a quarter");
    assert!(says(&window, "Version 6.4"));
    assert!(prefs(&window).managed_server);
    assert_eq!(host.state.lock().unwrap().epoch, 4, "drawing progress is not a new instance");

    // Unpacking: the download's byte counts are stale, and no percentage is invented.
    host.set(Status { phase: Phase::Extracting, epoch: 4, ..before.clone() });
    window.run_steps(3);
    assert!(says(&window, "Extracting"));
    assert!(!says(&window, "of 2.0 MB"), "no download percentage while extracting");
    assert_eq!(host.state.lock().unwrap().epoch, 4);
    assert_eq!(prefs(&window).grammar_url, MANUAL);
    assert!(host.actions().is_empty());
}

#[test]
fn java_unknown_is_neutral_and_advice_never_names_an_operation_that_is_not_offered() {
    // A host that cannot run the server, with a Java it has not looked for: nothing is offered,
    // so no advice may point at a button this state does not draw.
    let (unavailable, host) = window(Status { phase: Phase::Unavailable, java: Java::Unknown, ..Default::default() });
    assert!(says(&unavailable, "not available on this computer"));
    for key in CONTROLS {
        assert!(optional_rect(&unavailable, key).is_none(), "{key} is not offered by an unavailable host");
    }
    assert!(!says(&unavailable, "Check installation"), "no advice may name a button this state does not draw");
    assert!(!says(&unavailable, "was not looked for yet"), "Unknown is not a claim that Java was never searched for");
    assert!(says(&unavailable, "not known"), "Unknown is said as an unknown, not as an absence");
    assert!(host.actions().is_empty());

    // A download in flight offers cancellation only: the words may not send the owner to Check.
    let (downloading, _host) =
        window(Status { phase: Phase::Downloading, java: Java::Unknown, downloaded_bytes: 1, total_bytes: 10, ..Default::default() });
    assert!(optional_rect(&downloading, "proofManagedCancel").is_some(), "cancellation is offered while downloading");
    for key in CONTROLS {
        let offered = key == "proofManagedCancel";
        assert_eq!(optional_rect(&downloading, key).is_some(), offered, "{key} while downloading");
    }
    assert!(!says(&downloading, "Check installation"), "a download in flight draws no Check button");
}

#[test]
fn a_refused_request_is_said_in_words_and_never_retried_by_frames() {
    let (mut window, host) = window(Status { phase: Phase::Unchecked, ..Default::default() });
    *host.refuse.lock().unwrap() = Some(Issue::Busy);
    window.run_steps(3);
    click_key(&mut window, "proofManagedCheck");
    window.run_steps(2);
    assert_eq!(host.actions(), vec![Action::Check], "asked once, because the owner pressed once");
    assert!(says(&window, "busy"), "the refusal is said in words");
    window.run_steps(10);
    assert_eq!(host.actions(), vec![Action::Check], "a refusal is not retried by later frames");
    assert!(host.state.lock().unwrap().phase == Phase::Unchecked, "nothing was admitted");
}

#[test]
fn a_refusal_note_does_not_outlive_the_state_that_produced_it() {
    let (mut window, host) = window(Status { phase: Phase::Unchecked, ..Default::default() });
    *host.refuse.lock().unwrap() = Some(Issue::Busy);
    window.run_steps(3);
    click_key(&mut window, "proofManagedCheck");
    window.run_steps(2);
    assert!(says(&window, "busy"), "the refusal is said while the state it was refused in still stands");

    // The host reports a different phase: the old refusal is not news about this one.
    host.set(Status { phase: Phase::NotInstalled, ..Default::default() });
    window.run_steps(2);
    assert!(!says(&window, "busy"), "a refusal is cleared when the phase moves");

    // A change of mode clears it too, and coming back does not bring it back.
    *host.refuse.lock().unwrap() = Some(Issue::Busy);
    host.set(Status { phase: Phase::Unchecked, ..Default::default() });
    window.run_steps(2);
    click_key(&mut window, "proofManagedCheck");
    window.run_steps(2);
    assert!(says(&window, "busy"), "refused again in the new state");
    *host.refuse.lock().unwrap() = None;
    click_key(&mut window, "proofManagedMode");
    window.run_steps(2);
    assert!(!prefs(&window).managed_server);
    assert!(!says(&window, "busy"), "leaving the built-in server clears its refusal");
    click_key(&mut window, "proofManagedMode");
    window.run_steps(2);
    assert!(prefs(&window).managed_server);
    assert!(!says(&window, "busy"), "and choosing it again does not resurrect an old refusal");
}

#[test]
fn the_download_words_keep_the_licences_in_the_installed_copy() {
    let (window, _host) = window(Status { phase: Phase::NotInstalled, ..Default::default() });
    assert!(says(&window, "licences and notices"), "the licences are said to come with it");
    assert!(says(&window, "stay in the installed copy"), "they remain in the installed distribution");
    assert!(!says(&window, "kept beside"), "nothing is claimed about a kept archive beside the install");
}

#[test]
fn a_failed_check_and_a_failed_download_are_described_without_an_automatic_repair() {
    // What did not verify: this build's own pinned checksum is named, not one the archive's author
    // supplies, and nothing is repaired, replaced or removed by MailCraft. Temporary files may be
    // cleaned up, but an installed copy is never overwritten.
    let (mismatch, _host) = window(Status { phase: Phase::Failed, java: Java::Available, issue: Some(Issue::IntegrityFailed), ..Default::default() });
    assert!(says(&mismatch, "checksum pinned in this MailCraft build"), "the pin is this build's, not the author's");
    assert!(says(&mismatch, "computed from the official HTTPS archive when the pin was prepared"), "said where the pin comes from");
    assert!(says(&mismatch, "not signed or supplied by upstream"), "no upstream-supplied checksum is claimed");
    assert!(!says(&mismatch, "archive's own"), "no upstream-supplied checksum is claimed");
    assert!(says(&mismatch, "existing installation is never overwritten"));
    assert!(says(&mismatch, "temporary files are cleaned up when possible"), "staging removal is conditional, not absolute");
    assert!(!says(&mismatch, "left exactly as it is"), "cannot promise the temporary archive survives");
    assert!(!says(&mismatch, "Remove it yourself to download again"), "no removal is presented as the fix");
    assert!(optional_rect(&mismatch, "proofManagedDownload").is_none(), "nothing is downloaded over what did not verify");
    assert!(optional_rect(&mismatch, "proofManagedCheck").is_some(), "the read-only check is what is offered");

    // An archive that could not be read is about the download: nothing was installed, a copy
    // already here is untouched, and a partly written staging tree is cleaned up when possible.
    let (archive, _host) = window(Status { phase: Phase::Failed, java: Java::Available, issue: Some(Issue::InvalidArchive), ..Default::default() });
    assert!(says(&archive, "downloaded archive could not be read"), "a bad download is named as a download");
    assert!(says(&archive, "nothing was installed"), "installation, not unpacking, is what can be promised");
    assert!(!says(&archive, "nothing was unpacked"), "extraction may have written members before it failed");
    assert!(says(&archive, "existing installation is never overwritten"));
    assert!(says(&archive, "temporary files are cleaned up when possible"));
    assert!(optional_rect(&archive, "proofManagedDownload").is_none());
}

#[test]
fn the_manual_connection_test_is_neither_drawn_nor_kept_while_the_built_in_server_is_chosen() {
    let (mut manual, host) = legacy_window(false);
    let said = "the manual server answered";
    *host.answer.lock().unwrap() = Some(legacy_answer(said));
    crate::proof::test(manual.state_mut());
    manual.run_steps(3);
    assert!(says(&manual, "the manual server answered"), "in manual mode the answer is said on the page");

    click_key(&mut manual, "proofManagedMode");
    manual.run_steps(3);
    assert!(prefs(&manual).managed_server);
    assert!(!says(&manual, said), "the manual answer is not shown as news of the built-in server");

    click_key(&mut manual, "proofManagedMode");
    manual.run_steps(3);
    assert!(!prefs(&manual).managed_server);
    assert!(!says(&manual, said), "and it does not come back as a stale answer");
}

#[test]
fn the_manual_address_field_is_drawn_disabled_and_takes_no_input_until_the_mode_changes() {
    let (mut window, _host) = window(Status { phase: Phase::Unchecked, ..Default::default() });
    window.run_steps(3);
    assert!(prefs(&window).managed_server);

    // Drawn, so the kept address can be read — but disabled, which is not the same as absent.
    let field = rect(&window, "proofServer");
    click(&mut window, field.center());
    window.input_mut().events.push(Event::Text("http://127.0.0.1:9999".into()));
    window.run_steps(2);
    let buffer = window.ctx.data(|data| data.get_temp::<(String, String)>(Id::unique("proofServer")));
    assert_eq!(buffer, Some((MANUAL.to_string(), MANUAL.to_string())), "a disabled field takes no input");
    assert_eq!(prefs(&window).grammar_url, MANUAL, "and commits nothing");

    // Turned off, the same field takes input: the silence above was disablement, not a dead field.
    click_key(&mut window, "proofManagedMode");
    window.run_steps(2);
    assert!(!prefs(&window).managed_server);
    let field = rect(&window, "proofServer");
    click(&mut window, field.center());
    window.input_mut().events.push(Event::Text("/typed".into()));
    window.run_steps(2);
    let edited = window.ctx.data(|data| data.get_temp::<(String, String)>(Id::unique("proofServer"))).expect("the field keeps its buffer");
    assert_ne!(edited.1, MANUAL, "the field takes input once the mode allows it");
    assert_eq!(prefs(&window).grammar_url, MANUAL, "typing alone commits nothing");
}

#[test]
fn a_download_press_is_admitted_once_and_frames_never_ask_again() {
    let (mut window, host) = window(Status { phase: Phase::NotInstalled, ..Default::default() });
    *host.hold.lock().unwrap() = true;
    window.run_steps(3);
    click_key(&mut window, "proofManagedDownload");
    assert_eq!(host.actions(), vec![Action::Download], "one press, one admitted operation");

    // The host has taken the work but not published it: the same button is still drawn, and
    // drawing frames must not press it again.
    window.run_steps(30);
    assert_eq!(host.actions(), vec![Action::Download], "drawing frames never repeat a request");
    assert!(optional_rect(&window, "proofManagedDownload").is_some(), "the host has not moved, so it is still offered");

    // A second press is a second owner decision, and reaches the host exactly once.
    let download = rect(&window, "proofManagedDownload");
    click(&mut window, download.center());
    assert_eq!(host.actions(), vec![Action::Download, Action::Download], "each press is admitted once");

    // Once the host reports the download, Download is gone rather than disabled: a duplicate
    // cannot be drawn from the old place, and only cancellation remains.
    *host.hold.lock().unwrap() = false;
    host.set(Status { phase: Phase::Downloading, ..Default::default() });
    window.run_steps(3);
    assert!(optional_rect(&window, "proofManagedDownload").is_none(), "a download in flight offers no second Download");
    assert!(optional_rect(&window, "proofManagedCancel").is_some());
    window.run_steps(5);
    assert_eq!(host.actions().len(), 2, "drawing the download asks for nothing more");
}

// ---------------------------------------------------------------------------------------------
// The connection test belongs to one exact configuration. A command can change the configuration
// between frames just as a click can, so these drive `proof.set` and then frames, and a request
// still in flight is made to answer late.
// ---------------------------------------------------------------------------------------------

/// A native grammar host that records each request and keeps the sender of its answer, so a test
/// can answer after the configuration that asked has gone.
#[derive(Default)]
struct GrammarHost {
    requests: Mutex<Vec<GrammarRequest>>,
    senders: Mutex<Vec<Sender<GrammarReply>>>,
    /// An answer to hand over as soon as the request is made.
    answer: Mutex<Option<GrammarReply>>,
}

impl ProofService for GrammarHost {
    fn spelling(&self, _request: SpellingRequest) -> Receiver<SpellingReply> {
        let (_sender, receiver) = std::sync::mpsc::channel();
        receiver
    }

    fn grammar(&self, request: GrammarRequest) -> Receiver<GrammarReply> {
        let (sender, receiver) = std::sync::mpsc::channel();
        if let Some(answer) = self.answer.lock().unwrap().take() {
            let _ = sender.send(answer);
        }
        self.requests.lock().unwrap().push(request);
        self.senders.lock().unwrap().push(sender);
        receiver
    }

    fn suggest(&self, _request: SuggestionRequest) -> Receiver<SuggestionReply> {
        let (_sender, receiver) = std::sync::mpsc::channel();
        receiver
    }
}

/// The older host interface, with the same late-answer seam.
#[derive(Default)]
struct LegacyHost {
    requests: Mutex<Vec<LegacyRequest>>,
    senders: Mutex<Vec<Sender<LegacyReply>>>,
    answer: Mutex<Option<LegacyReply>>,
}

fn native_window(managed: bool) -> (Window, Arc<GrammarHost>) {
    let host = Arc::new(GrammarHost::default());
    (window_with(Services { proof_native: Some(host.clone()), ..Services::default() }, managed), host)
}

fn legacy_window(managed: bool) -> (Window, Arc<LegacyHost>) {
    let host = Arc::new(LegacyHost::default());
    let answering = host.clone();
    let service: Box<dyn Fn(LegacyRequest) -> Receiver<LegacyReply>> = Box::new(move |request| {
        let (sender, receiver) = std::sync::mpsc::channel();
        if let Some(answer) = answering.answer.lock().unwrap().take() {
            let _ = sender.send(answer);
        }
        answering.requests.lock().unwrap().push(request);
        answering.senders.lock().unwrap().push(sender);
        receiver
    });
    (window_with(Services { proof_check: Some(service), ..Services::default() }, managed), host)
}

fn native_answer(failure: Failure) -> GrammarReply {
    GrammarReply { error: Some(failure), ..Default::default() }
}

fn legacy_answer(said: &str) -> LegacyReply {
    LegacyReply { text: String::new(), diagnostics: Vec::new(), error: Some(said.to_string()) }
}

/// `proof.set` as a command between frames, not a click: the page did not see the control change.
fn command(window: &mut Window, params: serde_json::Value) {
    window.state_mut().execute("proof.set", params).expect("proof.set");
}

#[test]
fn a_command_changing_mode_clears_the_manual_connection_test_answer_for_good() {
    let (mut window, host) = legacy_window(false);
    *host.answer.lock().unwrap() = Some(legacy_answer("the manual server answered"));
    crate::proof::test(window.state_mut());
    window.run_steps(3);
    assert!(says(&window, "the manual server answered"));

    // The command changes the mode without the checkbox seeing it.
    command(&mut window, json!({"managedServer": true}));
    window.run_steps(3);
    command(&mut window, json!({"managedServer": false}));
    window.run_steps(3);
    assert!(!prefs(&window).managed_server);
    assert!(!says(&window, "the manual server answered"), "the answer is not news of the mode chosen now");
}

#[test]
fn a_command_changing_mode_clears_a_refusal_note_for_good() {
    let (mut window, host) = window(Status { phase: Phase::Unchecked, ..Default::default() });
    *host.refuse.lock().unwrap() = Some(Issue::Busy);
    window.run_steps(3);
    click_key(&mut window, "proofManagedCheck");
    window.run_steps(2);
    assert!(says(&window, "busy"), "the refusal is said while the mode it was refused in stands");

    // Both changes are commands: the pointer never touches the mode checkbox.
    command(&mut window, json!({"managedServer": false}));
    window.run_steps(2);
    command(&mut window, json!({"managedServer": true}));
    window.run_steps(2);
    assert!(prefs(&window).managed_server);
    assert!(!says(&window, "busy"), "a command-driven mode change clears the refusal, and it does not come back");
}

#[test]
fn a_mode_change_cancels_a_native_connection_test_in_flight() {
    let (mut window, host) = native_window(false);
    window.run_steps(2);
    crate::proof::test(window.state_mut());
    let cancelled = host.requests.lock().unwrap().first().expect("the test asked the native host").cancelled.clone();
    assert!(!cancelled.load(Ordering::Relaxed), "a live request is not cancelled yet");

    command(&mut window, json!({"managedServer": true}));
    window.run_steps(3);
    assert!(cancelled.load(Ordering::Relaxed), "leaving the configuration calls the request off");
    assert_eq!(host.requests.lock().unwrap().len(), 1, "clearing state does not make another request");
}

#[test]
fn a_late_native_connection_test_answer_cannot_reappear_after_a_mode_change() {
    let (mut window, host) = native_window(false);
    window.run_steps(2);
    crate::proof::test(window.state_mut());
    let sender = host.senders.lock().unwrap().first().expect("the test asked the native host").clone();

    command(&mut window, json!({"managedServer": true}));
    window.run_steps(3);
    let _ = sender.send(native_answer(Failure::Refused));
    command(&mut window, json!({"managedServer": false}));
    window.run_steps(4);
    assert!(!says(&window, "The grammar server refused the message."), "an answer for the mode that is gone is never drawn");
}

#[test]
fn a_late_legacy_connection_test_answer_cannot_reappear_after_a_mode_change() {
    let (mut window, host) = legacy_window(false);
    window.run_steps(2);
    crate::proof::test(window.state_mut());
    assert!(host.requests.lock().unwrap().len() == 1, "the test asked the older host interface");
    let sender = host.senders.lock().unwrap().first().expect("the test asked the older host").clone();

    command(&mut window, json!({"managedServer": true}));
    window.run_steps(3);
    let _ = sender.send(legacy_answer("the late manual server answered"));
    command(&mut window, json!({"managedServer": false}));
    window.run_steps(4);
    assert!(!says(&window, "the late manual server answered"), "the older interface's late answer is dropped too");
}

#[test]
fn a_command_changing_the_manual_address_discards_the_old_answer() {
    let (mut window, host) = legacy_window(false);
    *host.answer.lock().unwrap() = Some(legacy_answer("the manual server answered"));
    crate::proof::test(window.state_mut());
    window.run_steps(3);
    assert!(says(&window, "the manual server answered"));

    command(&mut window, json!({"grammarUrl": "http://127.0.0.1:9999"}));
    window.run_steps(3);
    assert_eq!(prefs(&window).grammar_url, "http://127.0.0.1:9999");
    assert!(!says(&window, "the manual server answered"), "the answer belongs to the address that was asked");
}

#[test]
fn an_address_committed_while_typing_discards_the_old_answer() {
    let (mut window, host) = legacy_window(false);
    *host.answer.lock().unwrap() = Some(legacy_answer("the manual server answered"));
    crate::proof::test(window.state_mut());
    window.run_steps(3);
    assert!(says(&window, "the manual server answered"));

    // The owner has typed a new address, and leaving the field commits it (the same seam the
    // pointer uses; the buffer is what the field would hold after those keystrokes).
    let id = Id::unique("proofServer");
    window.ctx.data_mut(|data| data.insert_temp(id, (MANUAL.to_string(), "http://127.0.0.1:9999".to_string())));
    window.ctx.memory_mut(|memory| memory.request_focus(id));
    window.run_steps(2);
    window.ctx.memory_mut(|memory| memory.surrender_focus(id));
    window.run_steps(3);
    assert_eq!(prefs(&window).grammar_url, "http://127.0.0.1:9999", "leaving the field commits the address");
    assert!(!says(&window, "the manual server answered"), "a newly committed address has no old answer");
}

#[test]
fn clearing_and_drawing_the_connection_test_never_asks_the_host() {
    let (mut window, host) = native_window(false);
    window.run_steps(5);
    assert!(host.requests.lock().unwrap().is_empty(), "drawing the page asks no server");

    command(&mut window, json!({"grammarUrl": "http://127.0.0.1:9999"}));
    window.run_steps(5);
    assert!(host.requests.lock().unwrap().is_empty(), "dropping a stale answer starts no new request");
}

#[test]
fn a_new_test_after_an_address_commit_is_not_discarded_as_the_old_test() {
    let (mut window, host) = native_window(false);
    window.run_steps(2);
    // In the real page this is the order when leaving Server commits its edit and the same
    // pointer press activates Test: commit, ask, then synchronize the page's configuration.
    command(&mut window, json!({"grammarUrl": "http://127.0.0.1:9999"}));
    crate::proof::test(window.state_mut());
    let cancelled = {
        let requests = host.requests.lock().unwrap();
        let request = requests.first().expect("the explicitly requested test");
        assert_eq!(request.url, "http://127.0.0.1:9999");
        request.cancelled.clone()
    };
    window.run_steps(3);
    assert!(!cancelled.load(Ordering::Relaxed), "synchronization must keep the new configuration's request");
    host.senders.lock().unwrap().first().unwrap().send(native_answer(Failure::Refused)).unwrap();
    window.run_steps(3);
    assert!(says(&window, "The grammar server refused the message."), "the new test's answer is retained");
    assert_eq!(host.requests.lock().unwrap().len(), 1, "a frame never retries a discarded request");
}

/// File › Options › Spelling & Grammar names each dictionary in the interface's language: a French
/// window offers "Français (Canada)" and never the model's English metadata, and ticking one still
/// chooses that dictionary.
#[test]
fn a_french_options_page_names_each_dictionary_in_french() {
    use crate::i18n::Lang;
    // The window's language is one value for the whole process while tests run side by side: this
    // holds it, and puts the previous one back when the test ends, however it ends.
    let _language = crate::i18n::RestoreLanguage::hold();
    let mut window = window_in(Services::default(), false, "fr");
    assert_eq!(window.state().language(), Lang::French, "the options page is in French");

    for french in ["Anglais (Canada)", "Français (Canada)", "Anglais (États-Unis)", "Français (France)"] {
        assert!(says(&window, french), "the French options page does not name {french:?}");
    }
    for english in ["English (Canada)", "French (Canada)", "English (United States)", "French (France)"] {
        assert!(!says(&window, english), "the French options page still says {english:?}");
    }

    // A tick under the French name is the checkbox it always was: from English alone, pressing
    // "Français (Canada)" adds French (Canada) — and nothing else — to the dictionaries the page
    // writes.
    window.state_mut().session.prefs.proof.languages = vec![mailcraft_model::proof::Language::EnCa];
    window.run_steps(2);
    assert_eq!(prefs(&window).languages, vec![mailcraft_model::proof::Language::EnCa], "the page offers English alone");
    window.get_by_role_and_label(egui::accesskit::Role::CheckBox, "Français (Canada)").click();
    window.run_steps(3);
    assert_eq!(
        prefs(&window).languages,
        vec![mailcraft_model::proof::Language::EnCa, mailcraft_model::proof::Language::FrCa],
        "the French name ticks French (Canada), beside English, and not English (Canada)"
    );
}
