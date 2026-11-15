// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! A settings file that can't be read is set aside without losing an earlier one, and the window
//! is told; one that can't be set aside is left where it is.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use super::*;

/// A folder of its own for one test, removed when it is done.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("mailcraft-settings-unread-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
    fn file(&self, name: &str, text: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, text).unwrap();
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    v.sort();
    v
}

/// One lock for the whole test binary, as `PREFS_LEFT_IN_PLACE` is one flag for the whole run.
///
/// `read_prefs` sets the flag when a settings file can't be read or can't be set aside, and
/// `save_prefs_at` then refuses to write for the rest of the process: a test that leaves it set
/// makes `launch_tests::a_window_closed_while_opening_writes_no_settings` fail at its second save,
/// and its own saves do nothing. Tests run in parallel threads of one binary, so every test that
/// can set the flag, and the launch test that needs it clear, holds this for its whole body.
/// Each starts with the flag clear and clears it before unlocking, including on a panic.
///
/// Reached from `launch_tests` as `crate::settings_unread_tests::PrefsFlag`: `pub(super)` here
/// names this module's parent, which is the crate root, and a module declared at the crate root is
/// visible throughout the crate — so any test of the binary can name it.
pub(super) struct PrefsFlag {
    /// Let go of after the flag is cleared: a `Drop` runs before its fields are dropped, so the
    /// next test to take the lock never sees this one's flag.
    _lock: std::sync::MutexGuard<'static, ()>,
}

static PREFS_FLAG: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl PrefsFlag {
    /// Take the lock and start from the flag clear, whatever another test did to it.
    pub(super) fn hold() -> PrefsFlag {
        let lock = PREFS_FLAG.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let was = PREFS_LEFT_IN_PLACE.swap(false, Ordering::Relaxed);
        // Every test that can set the flag holds this, so nothing else may have left it set: if
        // something ever does, this names the broken rule instead of failing in another test.
        assert!(!was, "a test set PREFS_LEFT_IN_PLACE without holding PrefsFlag");
        PrefsFlag { _lock: lock }
    }
}

impl Drop for PrefsFlag {
    fn drop(&mut self) {
        PREFS_LEFT_IN_PLACE.store(false, Ordering::Relaxed);
    }
}

/// What is written comes back whole, with nothing left beside it.
#[test]
fn settings_written_come_back_whole() {
    let s = Scratch::new("written");
    let path = s.0.join("ui.json");
    let ui = UiState::default();
    write_prefs(&path, &ui).unwrap();
    let back = read_prefs(&path).0.unwrap();
    assert_eq!(serde_json::to_value(&back).unwrap(), serde_json::to_value(&ui).unwrap());
    assert_eq!(names(&s.0), ["ui.json"]);
}

#[test]
fn proofing_preferences_and_personal_words_survive_the_real_settings_file() {
    use mailcraft_model::proof::Language;
    let scratch = Scratch::new("proofing-written");
    let path = scratch.0.join("ui.json");
    let mut ui = UiState::default();
    ui.options.proof.check_spelling = true;
    ui.options.proof.check_grammar = true;
    ui.options.proof.as_you_type = false;
    ui.options.proof.languages = vec![Language::EnCa, Language::FrCa];
    ui.options.proof.grammar_url = "http://127.0.0.1:18081".into();
    ui.options.proof.dictionary_dir = scratch.0.to_string_lossy().into_owned();
    ui.options.proof.ignored_rules = vec!["TEST_RULE".into()];
    assert!(ui.options.proof.add_word("Hochelaga"));
    assert!(ui.options.proof.add_word("Montréal"));
    write_prefs(&path, &ui).unwrap();
    let (back, unread) = read_prefs(&path);
    assert_eq!(unread, None);
    assert_eq!(back.unwrap().options.proof, ui.options.proof);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o077, 0);
    }
    assert_eq!(names(&scratch.0), ["ui.json"]);
}

#[test]
fn calendar_reminder_popup_preferences_survive_the_hosts_file_roundtrip() {
    let s = Scratch::new("calendar-reminder-popups");
    let path = s.file("ui.json", r#"{"theme":"dark"}"#);
    let old = read_prefs(&path).0.unwrap();
    assert_eq!(serde_json::to_value(&old).unwrap()["showCalendarReminderPopups"], true);
    let ui: UiState = serde_json::from_value(serde_json::json!({
        "showCalendarReminderPopups": false,
        "suppressedCalendarReminderPopups": ["42@2026-10-14T16:00"],
        "alertNewMail": true,
        "alertReminders": true
    }))
    .unwrap();
    write_prefs(&path, &ui).unwrap();
    drop(ui);
    let (restored, unread) = read_prefs(&path);
    assert_eq!(unread, None);
    let restored = serde_json::to_value(restored.unwrap()).unwrap();
    assert_eq!(restored["showCalendarReminderPopups"], false);
    assert_eq!(restored["suppressedCalendarReminderPopups"], serde_json::json!(["42@2026-10-14T16:00"]));
    assert_eq!(restored["alertNewMail"], true);
    assert_eq!(restored["alertReminders"], true);
    assert_eq!(names(&s.0), ["ui.json"]);
}

#[test]
fn a_good_file_is_read_and_a_missing_one_is_nothing_to_say() {
    let s = Scratch::new("good");
    let path = s.file("ui.json", r#"{"theme": "dark", "view": {"sort": "from"}}"#);
    let (ui, unread) = read_prefs(&path);
    assert_eq!(unread, None);
    assert_eq!(ui.unwrap().view.sort, mailcraft_engine::SortBy::From);
    assert_eq!(names(&s.0), ["ui.json"]);
    let (ui, unread) = read_prefs(&s.0.join("nothing.json"));
    assert!(ui.is_none() && unread.is_none());
}

#[test]
fn a_file_that_doesnt_parse_is_set_aside_and_said() {
    let s = Scratch::new("aside");
    let bad = r#"{"theme": "dark", "view": {"sort": "colour"}}"#;
    let path = s.file("ui.json", bad);
    let (ui, unread) = read_prefs(&path);
    assert!(ui.is_none());
    assert_eq!(unread, Some(SettingsUnread::SetAside("ui.json.damaged".into())));
    assert_eq!(names(&s.0), ["ui.json.damaged"]);
    assert_eq!(std::fs::read_to_string(s.0.join("ui.json.damaged")).unwrap(), bad);

    // Another one later keeps the first.
    s.file("ui.json", "not json at all");
    assert_eq!(read_prefs(&path).1, Some(SettingsUnread::SetAside("ui.json.damaged-2".into())));
    assert_eq!(names(&s.0), ["ui.json.damaged", "ui.json.damaged-2"]);
    assert_eq!(std::fs::read_to_string(s.0.join("ui.json.damaged")).unwrap(), bad);
}

/// Where the folder can't be written, the file stays, and nothing claims a copy was kept.
#[cfg(unix)]
#[test]
fn a_file_that_cant_be_set_aside_is_left_in_place() {
    use std::os::unix::fs::PermissionsExt;
    let _flag = PrefsFlag::hold(); // this file sets the flag: don't carry that to other tests
    let s = Scratch::new("stuck");
    let path = s.file("ui.json", "{broken");
    std::fs::set_permissions(&s.0, std::fs::Permissions::from_mode(0o555)).unwrap();
    // Root can write anyway; then there is nothing to show.
    let writable = std::fs::write(s.0.join("probe"), "").is_ok();
    let (ui, unread) = read_prefs(&path);
    std::fs::set_permissions(&s.0, std::fs::Permissions::from_mode(0o755)).unwrap();
    if writable {
        return;
    }
    assert!(ui.is_none());
    assert_eq!(unread, Some(SettingsUnread::LeftInPlace));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "{broken");
}

/// A dangling link on a name must not be replaced: the next free name is used.
#[cfg(unix)]
#[test]
fn a_dangling_link_on_a_name_is_not_replaced() {
    let s = Scratch::new("dangling");
    std::os::unix::fs::symlink(s.0.join("nowhere"), s.0.join("ui.json.damaged")).unwrap();
    let path = s.file("ui.json", "{broken");
    assert_eq!(read_prefs(&path).1, Some(SettingsUnread::SetAside("ui.json.damaged-2".into())));
    assert!(std::fs::symlink_metadata(s.0.join("ui.json.damaged")).unwrap().file_type().is_symlink());
    assert!(!s.0.join("nowhere").exists());
}

/// Past the cap the oldest copy is replaced, and settings go on being saved.
#[test]
fn past_the_cap_the_oldest_copy_is_replaced() {
    let s = Scratch::new("cap");
    for n in 1..=MAX_DAMAGED_COPIES {
        let name = if n == 1 { "ui.json.damaged".to_string() } else { format!("ui.json.damaged-{n}") };
        s.file(&name, &format!("copy {n}"));
        let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000 + (MAX_DAMAGED_COPIES - n) as u64);
        std::fs::File::options().write(true).open(s.0.join(&name)).unwrap().set_modified(when).unwrap();
    }
    let path = s.file("ui.json", "{newest");
    let last = format!("ui.json.damaged-{MAX_DAMAGED_COPIES}");
    // The highest number is the oldest here.
    let unread = read_prefs(&path).1;
    assert_eq!(unread, Some(SettingsUnread::SetAside(last.clone())));
    assert_eq!(std::fs::read_to_string(s.0.join(&last)).unwrap(), "{newest");
    assert!(!path.exists());
    assert_eq!(names(&s.0).len(), MAX_DAMAGED_COPIES);
}

/// A name that is a folder is never replaced, and if every name is such, the file stays.
#[test]
fn nothing_is_replaced_that_is_not_a_plain_file() {
    let s = Scratch::new("dirs");
    for n in 1..=MAX_DAMAGED_COPIES {
        let name = if n == 1 { "ui.json.damaged".to_string() } else { format!("ui.json.damaged-{n}") };
        std::fs::create_dir(s.0.join(name)).unwrap();
    }
    let path = s.file("ui.json", "{broken");
    assert!(set_aside(&path).is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "{broken");
}

/// A settings file that can't be read at all (here a folder) says so, not "couldn't be renamed".
#[test]
fn a_file_that_cant_be_read_is_said_so() {
    let _flag = PrefsFlag::hold(); // a folder can't be read: don't carry the flag to other tests
    let s = Scratch::new("unreadable");
    let path = s.0.join("ui.json");
    std::fs::create_dir(&path).unwrap();
    assert_eq!(read_prefs(&path).1, Some(SettingsUnread::CouldntRead));
    assert!(path.is_dir());
}

// ─── How large a settings file may be ───────────────────────────────────────────────────────────
//
// `ui.json` is read with a limit of its own: past `MAX_PREFS_BYTES` it is not read whole, not
// parsed and never written over — it is left where it is, as a file that can't be read at all is
// (`SettingsUnread::CouldntRead`), and the run starts on its defaults.
//
// The limit is 4 MiB (`4 << 20`). It is a chosen policy for a local preferences file, and these
// tests claim nothing about the largest file any version could write. Some lists in `UiState` are
// capped where this program fills them — `collapsedFolders` 10 000 (`apply_prefs`),
// `pictureSenders` and `pictureDomains` 500 each (`reading::remote`), `dismissedReminders` 400
// (`reminders`), `confirmedMailboxes` and `collapsedCalendarGroups` 200 each (`apply_prefs`) — but
// the strings inside most of them are not capped, newer versions add fields (the proofreading
// branch adds `Prefs::proof`, with personal words and ignored rules), and 4 MiB is headroom above
// a settings file with those lists full — a chosen number, not a derived one.
//
// The sizes below are fixed points, not estimates: 5 MiB well past the limit, one byte past it,
// the limit itself, and 128 KiB well inside it.

/// A valid settings file of exactly `total` bytes, written a piece at a time so that the test never
/// holds it whole. `padding` is a field `UiState` has no part of: the file would parse and be read
/// as settings, so its size is the only thing that may refuse it (one that couldn't be parsed
/// would be set aside, which is a different answer from the one these tests are about).
fn padded_settings(s: &Scratch, name: &str, total: u64) -> PathBuf {
    const HEAD: &str = "{\"theme\":\"dark\",\"padding\":\"";
    const TAIL: &str = "\",\"view\":{\"sort\":\"from\"}}";
    let path = s.0.join(name);
    let fill = total.saturating_sub((HEAD.len() + TAIL.len()) as u64);
    let mut out = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
    out.write_all(HEAD.as_bytes()).unwrap();
    let piece = [b'a'; 8192];
    let mut left = fill;
    while left > 0 {
        let n = usize::try_from(left.min(piece.len() as u64)).unwrap_or(piece.len());
        out.write_all(&piece[..n]).unwrap();
        left -= n as u64;
    }
    out.write_all(TAIL.as_bytes()).unwrap();
    out.flush().unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().len(), total);
    path
}

/// A settings file past the limit is refused with none of it read as settings, and neither this
/// run nor a save a moment later touches it: the owner's file stays exactly as they left it.
#[test]
fn a_settings_file_past_the_limit_is_refused_and_left_alone() {
    // A megabyte past the limit, and valid settings JSON: only its size may refuse it.
    const OVERSIZED: u64 = 5 << 20;
    let _flag = PrefsFlag::hold(); // the refusal sets the flag
    let s = Scratch::new("oversized");
    let path = padded_settings(&s, "ui.json", OVERSIZED);
    let kept = std::fs::read(&path).unwrap(); // capture before the reader, not after it
    let app = MailApp::new(Session::new(mailcraft_model::Mailbox::default()), Default::default());

    // A run without preferences writes nothing anywhere (`MAILCRAFT_NO_PREFS`), and the check at
    // the end would then hold for the wrong reason: that saving works at all is shown first.
    let control = s.0.join("control.json");
    save_prefs_at(&app, &control);
    assert!(control.exists(), "this test needs MAILCRAFT_NO_PREFS unset");
    std::fs::remove_file(&control).unwrap();

    let (ui, unread) = read_prefs(&path);
    assert!(ui.is_none(), "a file past the limit isn't read, however well it would parse");
    assert_eq!(unread, Some(SettingsUnread::CouldntRead), "it is left where it is, not set aside");
    assert!(PREFS_LEFT_IN_PLACE.load(Ordering::Relaxed), "nothing of this run is saved over it");

    assert!(std::fs::read(&path).unwrap() == kept, "reading must leave every original byte unchanged");
    assert_eq!(kept.len() as u64, OVERSIZED);
    assert_eq!(names(&s.0), ["ui.json"], "nothing was renamed and nothing was written beside it");

    // Nor does the window's settings go over it a moment later.
    save_prefs_at(&app, &path);
    assert!(std::fs::read(&path).unwrap() == kept, "the file is exactly as its owner left it");
    assert_eq!(names(&s.0), ["ui.json"]);
}

/// The two answers meet at the limit itself and nowhere else: a file of exactly the limit is the
/// owner's settings, and one byte more is refused and left where it is.
#[test]
fn a_settings_file_at_the_limit_is_read_and_one_byte_past_is_not() {
    const LIMIT: u64 = 4 << 20;
    let _flag = PrefsFlag::hold(); // the refusal sets the flag
    let s = Scratch::new("boundary");

    // Exactly the limit, valid, with a field this version knows nothing about.
    let at = padded_settings(&s, "at.json", LIMIT);
    let (ui, unread) = read_prefs(&at);
    assert_eq!(unread, None, "a file of exactly the limit is read");
    let ui = ui.expect("a file of exactly the limit is read");
    assert_eq!(ui.theme, mailcraft_ui_egui::theme::Appearance::Dark);
    assert_eq!(ui.view.sort, mailcraft_engine::SortBy::From);

    // One byte more is not read at all, and stays as it was.
    let past = padded_settings(&s, "past.json", LIMIT + 1);
    let kept = std::fs::read(&past).unwrap();
    let (ui, unread) = read_prefs(&past);
    assert!(ui.is_none(), "one byte past the limit isn't read, however well it would parse");
    assert_eq!(unread, Some(SettingsUnread::CouldntRead), "one byte past the limit is left where it is");
    assert!(PREFS_LEFT_IN_PLACE.load(Ordering::Relaxed), "nothing of this run is saved over it");
    let after_read = std::fs::read(&past).unwrap();
    assert_eq!(after_read.len() as u64, LIMIT + 1);
    assert!(after_read == kept, "refusal must preserve every original byte");
    assert_eq!(names(&s.0), ["at.json", "past.json"], "neither was renamed or written beside");
}

/// Six representative lists at their supported counts are read back whole. Other lists and
/// strings are not maximized here; this asserts this fixture's round trip, not a universal
/// maximum for settings MailCraft can write.
#[test]
fn a_settings_file_with_representative_full_lists_comes_back_whole() {
    let s = Scratch::new("full-lists");
    let path = s.0.join("ui.json");
    let full = UiState {
        collapsed_folders: (1..=10_000u64).collect(),
        picture_senders: (0..500).map(|i| format!("sender{i:03}@shop.example")).collect(),
        picture_domains: (0..500).map(|i| format!("shop{i:03}.example")).collect(),
        dismissed_reminders: (0..400).map(|i| format!("{i}@2026-11-15T09:00:00")).collect(),
        confirmed_mailboxes: (0..200)
            .map(|i| mailcraft_ui_egui::ConfirmedMailbox {
                path: format!("/home/someone/.local/share/mailcraft/mailbox-{i:03}.mailcraft"),
                fingerprint: "0123456789abcdef".repeat(4),
            })
            .collect(),
        collapsed_calendar_groups: (0..200).map(|i| format!("Calendar group {i:03}")).collect(),
        ..Default::default()
    };
    write_prefs(&path, &full).unwrap();
    let written = std::fs::metadata(&path).unwrap().len();

    let (back, unread) = read_prefs(&path);
    assert_eq!(unread, None, "{written} bytes of representative settings are inside the limit");
    let back = back.expect("the settings MailCraft writes are read");
    assert_eq!(back.collapsed_folders, full.collapsed_folders, "every collapsed folder, in order");
    assert_eq!(back.picture_senders, full.picture_senders);
    assert_eq!(back.picture_domains, full.picture_domains);
    assert_eq!(back.dismissed_reminders, full.dismissed_reminders);
    assert_eq!(back.confirmed_mailboxes, full.confirmed_mailboxes);
    assert_eq!(back.collapsed_calendar_groups, full.collapsed_calendar_groups);
    assert_eq!(names(&s.0), ["ui.json"], "nothing was set aside");
}

/// A file with room to spare, holding a field this version knows nothing about, is read and its
/// own fields are kept: only a size past the limit refuses a file, never padding inside it.
#[test]
fn a_settings_file_inside_the_limit_is_read_with_its_fields() {
    let s = Scratch::new("inside");
    let path = padded_settings(&s, "ui.json", 128 << 10);
    let (ui, unread) = read_prefs(&path);
    assert_eq!(unread, None);
    let ui = ui.expect("a file this size is read");
    assert_eq!(ui.theme, mailcraft_ui_egui::theme::Appearance::Dark);
    assert_eq!(ui.view.sort, mailcraft_engine::SortBy::From);
    assert!(!ui.download_pictures);
    assert_eq!(names(&s.0), ["ui.json"]);
}

/// A state that would serialize past the reader's limit must not replace a readable settings
/// file with one this version cannot reopen. Check the fixture expansion before the write.
#[test]
fn settings_that_would_exceed_the_read_limit_do_not_replace_the_old_file() {
    const LIMIT: usize = 4 << 20;
    const HEAD: &[u8] = b"{\"collapsedCalendarGroups\":[\"";
    const TAIL: &[u8] = b"\"]}";
    let _flag = PrefsFlag::hold();
    let s = Scratch::new("write-limit");
    let path = s.0.join("ui.json");
    let mut compact = HEAD.to_vec();
    compact.resize(LIMIT - TAIL.len(), b'a');
    compact.extend_from_slice(TAIL);
    assert_eq!(compact.len(), LIMIT);
    std::fs::write(&path, &compact).unwrap();
    let (ui, unread) = read_prefs(&path);
    assert_eq!(unread, None, "the original compact file is inside the inclusive limit");
    let ui = ui.expect("settings exactly at the limit are readable");
    assert!(serde_json::to_vec_pretty(&ui).unwrap().len() > LIMIT, "fixture must actually expand past the limit");

    let result = write_prefs(&path, &ui);
    assert!(result.is_err(), "a writer must not create a file this reader refuses");
    assert!(std::fs::read(&path).unwrap() == compact, "a refused write leaves the readable original intact");
    assert_eq!(names(&s.0), ["ui.json"], "no temporary file or damaged copy is left");
}
