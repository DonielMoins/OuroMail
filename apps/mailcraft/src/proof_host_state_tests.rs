// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Admission, operation generations and endpoint epochs are architectural invariants, independent of I/O.
//!
//! Every worker completion carries the generation it belongs to. A stale completion must never
//! finish, publish or promote a newer operation, and a cancelled promotion must still describe a
//! distribution that really was installed.
use super::proof_host_state::{MAX_VERSION_BYTES, State};
use mailcraft_ui_egui::proof_server::{Action, Issue, Java, Phase};

fn verified_install(java: Java) -> State {
    let mut state = State::new(true);
    assert_eq!(state.begin(Action::Check), Ok(true));
    let ticket = state.ticket();
    assert!(state.checked(ticket, true, java, "1.2.3"));
    state
}

#[test]
fn a_fresh_start_failure_updates_cached_java_and_installation_facts() {
    for (issue, java) in [(Issue::JavaMissing, Java::Missing), (Issue::JavaTooOld, Java::TooOld)] {
        let mut state = verified_install(Java::Available);
        state.begin(Action::Start).unwrap();
        assert!(state.finished(state.ticket(), Phase::Failed, Some(issue)));
        assert_eq!(state.snapshot().java, java, "the new helper result supersedes the earlier Check");
    }
    for issue in [Issue::NotInstalled, Issue::IntegrityFailed] {
        let mut state = verified_install(Java::Available);
        state.begin(Action::Start).unwrap();
        assert!(state.finished(state.ticket(), Phase::Failed, Some(issue)));
        assert!(state.snapshot().version.is_empty(), "a newly unverified installation has no verified version");
    }
}

#[test]
fn cancelling_a_start_keeps_the_verified_installation_startable() {
    let mut state = verified_install(Java::Available);
    let install = state.snapshot();
    assert_eq!(state.begin(Action::Start), Ok(true));
    let ticket = state.ticket();
    assert_eq!(state.begin(Action::Cancel), Ok(true));
    assert_eq!(state.ticket(), ticket, "a cancellation flag is not a new operation");
    assert!(state.finished(ticket, Phase::Cancelled, Some(Issue::Cancelled)));
    let after = state.snapshot();
    assert_eq!(after.phase, Phase::Stopped);
    assert_eq!(after.issue, Some(Issue::Cancelled));
    assert_eq!(after.version, install.version, "cancelling a start never removes a verified installation");
    assert_eq!(after.epoch, install.epoch, "nothing was published, so cancelling a start changes no epoch");
    assert_eq!(state.begin(Action::Start), Ok(true));
}

#[test]
fn cancellation_after_promotion_records_the_verified_distribution() {
    let mut state = State::new(true);
    assert_eq!(state.begin(Action::Check), Ok(true));
    let check = state.ticket();
    assert!(state.checked(check, false, Java::Available, ""));
    assert_eq!(state.begin(Action::Download), Ok(true));
    let download = state.ticket();
    assert!(download > check);
    assert_eq!(state.begin(Action::Cancel), Ok(true));
    assert!(state.checked(download, true, Java::Available, "9.9.9"), "the same operation may finish promotion after cancellation");
    let after = state.snapshot();
    assert_eq!(after.phase, Phase::Installed);
    assert_eq!(after.issue, Some(Issue::Cancelled));
    assert_eq!(after.version, "9.9.9");
    assert_eq!(state.begin(Action::Start), Ok(true));
}

#[test]
fn cancellation_without_promotion_offers_download_again() {
    let mut state = State::new(true);
    assert_eq!(state.begin(Action::Check), Ok(true));
    let check = state.ticket();
    assert!(state.checked(check, false, Java::Available, ""));
    assert_eq!(state.begin(Action::Download), Ok(true));
    let download = state.ticket();
    assert_eq!(state.begin(Action::Cancel), Ok(true));
    assert!(state.checked(download, false, Java::Missing, "stale"));
    let after = state.snapshot();
    assert_eq!(after.phase, Phase::Cancelled);
    assert_eq!(after.issue, Some(Issue::Cancelled));
    assert_eq!(after.version, "", "an unverified result records no version");
    assert_eq!(after.java, Java::Missing);
    assert_eq!(state.begin(Action::Download), Ok(true));
    assert!(state.ticket() > download);
}

#[test]
fn extraction_progress_never_returns_to_downloading() {
    let mut state = State::new(true);
    assert_eq!(state.begin(Action::Check), Ok(true));
    let check = state.ticket();
    assert!(state.checked(check, false, Java::Available, ""));
    assert_eq!(state.begin(Action::Download), Ok(true));
    let download = state.ticket();
    assert!(state.progress(download, 5, 10, true));
    assert_eq!(state.phase(), Phase::Extracting);
    assert!(state.progress(download, 2, 10, false));
    assert_eq!(state.phase(), Phase::Extracting, "late download progress cannot undo extraction");
    assert!(state.progress(download, 9, 10, true));
    assert_eq!(state.phase(), Phase::Extracting);
}

#[test]
fn initial_status_and_rejected_actions_are_passive() {
    let mut state = State::new(true);
    assert_eq!(state.snapshot().phase, Phase::Unchecked);
    assert_eq!(state.ticket(), 0, "a fresh host admits no generation");
    let before = state.snapshot();
    assert_eq!(state.begin(Action::Download), Err(Issue::NotInstalled));
    assert_eq!(state.begin(Action::Start), Err(Issue::NotInstalled));
    assert_eq!(state.snapshot(), before);
    assert_eq!(state.ticket(), 0);
    let mut unavailable = State::new(false);
    for action in [Action::Check, Action::Download, Action::Start, Action::Stop, Action::Cancel] {
        assert_eq!(unavailable.begin(action), Err(Issue::Unavailable));
        assert_eq!(unavailable.snapshot().phase, Phase::Unavailable);
        assert_eq!(unavailable.ticket(), 0);
    }
}

#[test]
fn one_operation_until_cleanup_and_no_endpoint_from_progress() {
    let mut state = State::new(true);
    assert_eq!(state.begin(Action::Check), Ok(true));
    let check = state.ticket();
    let before = state.snapshot();
    assert_eq!(state.begin(Action::Check), Err(Issue::Busy));
    assert_eq!(state.snapshot(), before);
    assert_eq!(state.ticket(), check);
    assert!(state.checked(check, false, Java::Missing, ""));
    assert_eq!(state.phase(), Phase::NotInstalled);
    assert_eq!(state.begin(Action::Download), Ok(true));
    let download = state.ticket();
    assert!(state.progress(download, 12, 100, false));
    assert_eq!(state.snapshot().epoch, 0);
    assert!(state.snapshot().endpoint.is_none());
    assert_eq!(state.begin(Action::Cancel), Ok(true));
    assert_eq!(state.phase(), Phase::Cancelling);
    assert_eq!(state.begin(Action::Cancel), Ok(false));
    assert_eq!(state.begin(Action::Download), Err(Issue::Busy));
    assert!(!state.progress(download, 99, 100, true), "late progress cannot undo cancellation");
    assert_eq!(state.phase(), Phase::Cancelling);
    assert!(state.finished(download, Phase::Cancelled, Some(Issue::Cancelled)));
    assert_eq!(state.begin(Action::Download), Ok(true));
}

#[test]
fn only_verified_available_start_publishes_and_revokes_an_instance() {
    let mut state = State::new(true);
    assert_eq!(state.begin(Action::Check), Ok(true));
    let first = state.ticket();
    assert!(state.checked(first, true, Java::Unknown, "1.2.3"));
    assert_eq!(state.begin(Action::Start), Err(Issue::JavaMissing));
    assert_eq!(state.ticket(), first, "a rejected start admits no generation");
    assert_eq!(state.begin(Action::Check), Ok(true));
    let second = state.ticket();
    assert!(second > first);
    assert!(state.checked(second, true, Java::Available, "1.2.3"));
    assert_eq!(state.begin(Action::Start), Ok(true));
    let start = state.ticket();
    assert!(state.publish(start, 32100));
    assert_eq!(state.phase(), Phase::Running);
    assert_eq!(state.snapshot().endpoint.as_deref(), Some("http://127.0.0.1:32100"));
    assert_eq!(state.snapshot().epoch, 1);
    assert_eq!(state.begin(Action::Check), Err(Issue::Busy));
    assert_eq!(state.begin(Action::Download), Err(Issue::Busy));
    assert_eq!(state.begin(Action::Stop), Ok(true));
    assert_eq!(state.phase(), Phase::Stopping);
    assert!(state.snapshot().endpoint.is_none(), "revoke before asynchronous pipe cleanup");
    assert_eq!(state.snapshot().epoch, 2);
    assert_eq!(state.begin(Action::Stop), Ok(false));
    assert_eq!(state.begin(Action::Start), Err(Issue::Busy));
    assert!(!state.publish(start, 32100), "late readiness cannot resurrect stopping instance");
    assert!(state.finished(start, Phase::Stopped, None));
    assert_eq!(state.snapshot().epoch, 2);
    assert_eq!(state.begin(Action::Start), Ok(true));
    let restart = state.ticket();
    assert!(state.publish(restart, 32101));
    assert_eq!(state.snapshot().epoch, 3);
    assert!(state.finished(restart, Phase::Failed, Some(Issue::StartFailed)));
    assert!(state.snapshot().endpoint.is_none());
    assert_eq!(state.snapshot().epoch, 4);
}

#[test]
fn generation_grows_only_for_admitted_check_download_and_start() {
    let mut state = State::new(true);
    assert_eq!(state.ticket(), 0);
    assert_eq!(state.begin(Action::Cancel), Ok(false));
    assert_eq!(state.begin(Action::Stop), Ok(false));
    assert_eq!(state.ticket(), 0);
    assert_eq!(state.begin(Action::Check), Ok(true));
    let check = state.ticket();
    assert_eq!(check, 1);
    assert_eq!(state.begin(Action::Check), Err(Issue::Busy));
    assert_eq!(state.ticket(), check);
    assert_eq!(state.begin(Action::Cancel), Ok(true));
    assert_eq!(state.ticket(), check);
    assert!(state.finished(check, Phase::Cancelled, Some(Issue::Cancelled)));
    assert_eq!(state.ticket(), check);
    assert_eq!(state.begin(Action::Download), Ok(true));
    let download = state.ticket();
    assert_eq!(download, check + 1);
    assert_eq!(state.begin(Action::Cancel), Ok(true));
    assert!(state.checked(download, false, Java::Available, ""));
    assert_eq!(state.ticket(), download);
    assert_eq!(state.begin(Action::Download), Ok(true));
    let again = state.ticket();
    assert_eq!(again, download + 1);
    assert!(state.progress(again, 1, 2, false));
    assert_eq!(state.begin(Action::Cancel), Ok(true));
    assert!(state.finished(again, Phase::Cancelled, Some(Issue::Cancelled)));
    assert_eq!(state.begin(Action::Check), Ok(true));
    let verified = state.ticket();
    assert!(state.checked(verified, true, Java::TooOld, "1.2.3"));
    assert_eq!(state.begin(Action::Start), Err(Issue::JavaTooOld));
    assert_eq!(state.ticket(), verified);
    assert_eq!(state.begin(Action::Download), Err(Issue::NotInstalled));
    assert_eq!(state.ticket(), verified);
}

#[test]
fn stale_checked_and_progress_cannot_complete_a_newer_operation() {
    let mut state = State::new(true);
    assert_eq!(state.begin(Action::Check), Ok(true));
    let old = state.ticket();
    assert!(state.checked(old, false, Java::Available, ""));
    assert_eq!(state.begin(Action::Download), Ok(true));
    let download = state.ticket();
    let before = state.snapshot();
    assert!(!state.checked(old, true, Java::Available, "evil"), "a finished check cannot install the newer download");
    assert_eq!(state.snapshot(), before);
    assert!(!state.progress(old, 5, 10, false), "a finished operation cannot report download progress");
    assert_eq!(state.snapshot(), before);
    assert!(state.progress(download, 5, 10, false));
    assert_eq!(state.snapshot().downloaded_bytes, 5);
}

#[test]
fn stale_publish_and_finished_cannot_override_a_newer_operation() {
    let mut state = verified_install(Java::Available);
    let check = state.ticket();
    assert_eq!(state.begin(Action::Start), Ok(true));
    let first = state.ticket();
    assert!(!state.publish(check, 32100), "an old check generation cannot publish");
    assert_eq!(state.phase(), Phase::Starting);
    assert!(state.publish(first, 32101));
    assert_eq!(state.begin(Action::Stop), Ok(true));
    assert!(state.finished(first, Phase::Stopped, None));
    assert_eq!(state.begin(Action::Start), Ok(true));
    let second = state.ticket();
    let before = state.snapshot();
    assert!(!state.publish(first, 32102), "a previous start cannot publish the new one");
    assert!(!state.finished(first, Phase::Failed, Some(Issue::StartFailed)), "a stale worker cannot fail the new start");
    assert_eq!(state.snapshot(), before);
    assert_eq!(state.phase(), Phase::Starting);
    assert!(state.publish(second, 32103));
    assert_eq!(state.phase(), Phase::Running);
}

#[test]
fn invalid_terminal_finish_phases_are_refused() {
    let mut state = State::new(true);
    assert_eq!(state.begin(Action::Check), Ok(true));
    let check = state.ticket();
    let before = state.snapshot();
    for phase in [
        Phase::Unavailable,
        Phase::Unchecked,
        Phase::Checking,
        Phase::NotInstalled,
        Phase::Downloading,
        Phase::Extracting,
        Phase::Cancelling,
        Phase::Installed,
        Phase::Starting,
        Phase::Running,
        Phase::Stopping,
    ] {
        assert!(!state.finished(check, phase, None), "{phase:?} is not a safe worker terminal phase");
        assert_eq!(state.snapshot(), before);
    }
    assert!(state.finished(check, Phase::Failed, Some(Issue::DownloadFailed)));
    assert_eq!(state.phase(), Phase::Failed);
    let failed = state.snapshot();
    assert!(!state.finished(check, Phase::Stopped, None), "a duplicate finish cannot rewrite a terminal operation");
    assert_eq!(state.snapshot(), failed);
}

#[test]
fn duplicate_finish_cannot_override_terminal_state_or_a_newer_operation() {
    let mut state = verified_install(Java::Available);
    assert_eq!(state.begin(Action::Start), Ok(true));
    let first = state.ticket();
    assert!(state.finished(first, Phase::Cancelled, Some(Issue::Cancelled)));
    let stopped = state.snapshot();
    assert_eq!(stopped.phase, Phase::Stopped);
    assert!(!state.finished(first, Phase::Failed, Some(Issue::StartFailed)));
    assert_eq!(state.snapshot(), stopped);
    assert_eq!(state.begin(Action::Start), Ok(true));
    let second = state.ticket();
    assert!(!state.finished(first, Phase::Stopped, None));
    assert_eq!(state.ticket(), second);
    assert_eq!(state.phase(), Phase::Starting);
}

#[test]
fn publish_port_zero_keeps_starting_state() {
    let mut state = verified_install(Java::Available);
    assert_eq!(state.begin(Action::Start), Ok(true));
    let start = state.ticket();
    assert!(!state.publish(start, 0), "port zero is never a publishable endpoint");
    let after = state.snapshot();
    assert_eq!(after.phase, Phase::Starting);
    assert!(after.endpoint.is_none());
    assert_eq!(after.epoch, 0);
    assert!(state.publish(start, 32100));
    assert_eq!(state.snapshot().epoch, 1);
}

#[test]
fn version_is_bounded_and_only_recorded_when_installed() {
    let mut state = State::new(true);
    assert_eq!(state.begin(Action::Check), Ok(true));
    let check = state.ticket();
    let long = "é".repeat(200);
    assert!(state.checked(check, true, Java::Available, &long));
    let installed = state.snapshot();
    assert!(installed.version.len() <= MAX_VERSION_BYTES);
    assert!(!installed.version.is_empty());
    assert!(long.starts_with(&installed.version), "a bounded version is a truthful prefix");
    assert!(installed.version.chars().all(|character| character == 'é'), "bounded at a character boundary");
    assert_eq!(state.begin(Action::Check), Ok(true));
    let second = state.ticket();
    assert!(state.checked(second, false, Java::Missing, "2.0.0"));
    let absent = state.snapshot();
    assert_eq!(absent.phase, Phase::NotInstalled);
    assert_eq!(absent.version, "", "an unverified result records no version");
    assert_eq!(absent.java, Java::Missing);
}
