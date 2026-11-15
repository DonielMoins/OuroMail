// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Private helper protocol regressions; no message text or arbitrary executable is accepted.

use super::proof_protocol::{self as protocol, Event, Mode};
use mailcraft_ui_egui::proof_server::Java;
use std::ffi::OsString;

#[test]
fn canonical_arguments_and_events_roundtrip_without_literal_drift() {
    let base = std::env::current_dir().unwrap();
    for mode in [Mode::Probe, Mode::Serve { base }] {
        assert_eq!(protocol::parse_mode(mode.arguments()), Some(mode));
    }
    for event in [
        Event::Bound(32100),
        Event::Stopped,
        Event::Failed,
        Event::Probe(Java::Available),
        Event::Probe(Java::Missing),
        Event::Probe(Java::TooOld),
        Event::Probe(Java::Unknown),
    ] {
        let mut bytes = Vec::new();
        protocol::write_event(event, &mut bytes).unwrap();
        assert_eq!(protocol::read_event(&mut bytes.as_slice()), Ok(Some(event)));
    }
    assert_eq!(
        protocol::parse_mode([OsString::from("mailcraft"), OsString::from(protocol::ENTRY), OsString::from("probe")]),
        None,
        "caller passes args_os().skip(1), not argv[0]"
    );
}

#[test]
fn bounded_reader_rejects_overflow_truncation_and_read_errors() {
    use protocol::ReadError;
    assert_eq!(protocol::read_event(&mut b"".as_slice()), Ok(None));
    assert_eq!(protocol::read_event(&mut b"BOUND 32100".as_slice()), Err(ReadError::Truncated));
    assert_eq!(protocol::read_event(&mut vec![b'x'; 4096].as_slice()), Err(ReadError::Invalid));
    let mut input = b"BOUND 32100\nSTOPPED\n".as_slice();
    assert_eq!(protocol::read_event(&mut input), Ok(Some(Event::Bound(32100))));
    assert_eq!(protocol::read_event(&mut input), Ok(Some(Event::Stopped)));
    assert_eq!(protocol::read_event(&mut input), Ok(None));
    struct Broken;
    impl std::io::Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::Other.into())
        }
    }
    assert_eq!(protocol::read_event(&mut Broken), Err(ReadError::Io));
}

#[test]
fn protocol_debug_redacts_the_private_installation_base() {
    let mode = Mode::Serve { base: std::path::PathBuf::from("/private-proofing-path-canary") };
    assert!(!format!("{mode:?}").contains("private-proofing-path-canary"));
}

#[test]
fn helper_entry_is_exact_bounded_and_accepts_no_executable_or_extra_argument() {
    let parse = |args: &[&str]| protocol::parse_mode(args.iter().map(OsString::from));
    assert_eq!(parse(&[protocol::ENTRY, "probe"]), Some(Mode::Probe));
    let base = std::env::current_dir().unwrap();
    assert_eq!(
        protocol::parse_mode([OsString::from(protocol::ENTRY), OsString::from("serve"), base.clone().into_os_string()]),
        Some(Mode::Serve { base })
    );
    for args in [
        vec![],
        vec!["other"],
        vec![protocol::ENTRY],
        vec![protocol::ENTRY, "probe", "extra"],
        vec![protocol::ENTRY, "serve", "relative"],
        vec![protocol::ENTRY, "java", "/arbitrary.jar"],
    ] {
        assert_eq!(parse(&args), None);
    }
}

#[test]
fn protocol_accepts_only_fixed_complete_events_and_literal_nonzero_ports() {
    assert_eq!(protocol::parse_event(b"BOUND 31234\n"), Some(Event::Bound(31234)));
    assert_eq!(protocol::parse_event(b"STOPPED\n"), Some(Event::Stopped));
    assert_eq!(protocol::parse_event(b"FAILED\n"), Some(Event::Failed));
    for (name, java) in [("AVAILABLE", Java::Available), ("MISSING", Java::Missing), ("TOOOLD", Java::TooOld), ("UNKNOWN", Java::Unknown)] {
        assert_eq!(protocol::parse_event(format!("PROBE {name}\n").as_bytes()), Some(Event::Probe(java)));
    }
    for line in [
        b"BOUND 0\n".as_slice(),
        b"BOUND 65536\n",
        b"BOUND +34\n",
        b"BOUND 34 junk\n",
        b"BOUND 34",
        b"PROBE /private/path\n",
        b"STOPPED\njunk",
        b"FAILED\r\n",
    ] {
        assert_eq!(protocol::parse_event(line), None);
    }
    assert_eq!(protocol::parse_event(&[b'a'; protocol::MAX_EVENT_BYTES + 1]), None);
}

#[test]
fn private_codes_map_to_fixed_issues_without_exposing_child_output() {
    use mailcraft_ui_egui::proof_server::Issue;
    assert_eq!(protocol::issue_for_exit(Some(protocol::EXIT_INTEGRITY)), Issue::IntegrityFailed);
    assert_eq!(protocol::issue_for_exit(Some(protocol::EXIT_NOT_INSTALLED)), Issue::NotInstalled);
    assert_eq!(protocol::issue_for_exit(Some(protocol::EXIT_JAVA_MISSING)), Issue::JavaMissing);
    assert_eq!(protocol::issue_for_exit(Some(protocol::EXIT_JAVA_TOO_OLD)), Issue::JavaTooOld);
    assert_eq!(protocol::issue_for_exit(Some(protocol::EXIT_TIMEOUT)), Issue::StartTimedOut);
    assert_eq!(protocol::issue_for_exit(None), Issue::StartFailed);
}
