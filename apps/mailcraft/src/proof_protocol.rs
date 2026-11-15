// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Fixed, bounded messages on the private native proofing helper's pipes.

use mailcraft_ui_egui::proof_server::{Issue, Java};
use std::ffi::OsString;
use std::path::PathBuf;

pub const ENTRY: &str = "--mailcraft-proof-helper";
pub const MAX_EVENT_BYTES: usize = 64;
pub const EXIT_INVOCATION: i32 = 2;
pub const EXIT_INTEGRITY: i32 = 3;
pub const EXIT_NOT_INSTALLED: i32 = 4;
pub const EXIT_JAVA_MISSING: i32 = 5;
pub const EXIT_JAVA_TOO_OLD: i32 = 6;
pub const EXIT_TIMEOUT: i32 = 9;

#[derive(Clone, PartialEq, Eq)]
pub enum Mode {
    Probe,
    Serve { base: PathBuf },
}

impl std::fmt::Debug for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Probe => "Probe",
            Self::Serve { .. } => "Serve { base: <redacted> }",
        })
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Bound(u16),
    Probe(Java),
    Stopped,
    Failed,
}

impl Mode {
    /// Arguments to `Command::new(current_exe())`, with no argv[0].
    pub fn arguments(&self) -> Vec<OsString> {
        match self {
            Self::Probe => vec![ENTRY.into(), "probe".into()],
            Self::Serve { base } => vec![ENTRY.into(), "serve".into(), base.clone().into_os_string()],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadError {
    Invalid,
    Truncated,
    Io,
}

/// Read one bounded frame, retaining at most 64 bytes. A partial EOF or a read error never
/// validates a prefix. This is blocking: the host runs one reader off-thread, enforces its own
/// operation deadline, closes the helper lifetime pipe on cancellation, and reaps before reuse.
pub fn read_event(reader: &mut impl std::io::Read) -> Result<Option<Event>, ReadError> {
    let mut line = Vec::with_capacity(MAX_EVENT_BYTES);
    let mut byte = [0u8; 1];
    loop {
        match reader.read(&mut byte) {
            Ok(0) if line.is_empty() => return Ok(None),
            Ok(0) => return Err(ReadError::Truncated),
            Ok(1) => {
                if line.len() >= MAX_EVENT_BYTES {
                    return Err(ReadError::Invalid);
                }
                line.extend_from_slice(&byte);
                if byte == *b"\n" {
                    return parse_event(&line).map(Some).ok_or(ReadError::Invalid);
                }
            }
            Ok(_) => return Err(ReadError::Io),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(ReadError::Io),
        }
    }
}

pub fn write_event(event: Event, writer: &mut impl std::io::Write) -> std::io::Result<()> {
    let line = match event {
        Event::Bound(0) => return Err(std::io::ErrorKind::InvalidInput.into()),
        Event::Bound(port) => format!("BOUND {port}\n"),
        Event::Probe(java) => format!(
            "PROBE {}\n",
            match java {
                Java::Available => "AVAILABLE",
                Java::Missing => "MISSING",
                Java::TooOld => "TOOOLD",
                Java::Unknown => "UNKNOWN",
            }
        ),
        Event::Stopped => "STOPPED\n".to_string(),
        Event::Failed => "FAILED\n".to_string(),
    };
    writer.write_all(line.as_bytes())?;
    writer.flush()
}

/// Inspect at most four arguments; no Java executable or JAR is accepted by this protocol.
pub fn parse_mode(args: impl IntoIterator<Item = OsString>) -> Option<Mode> {
    let mut args = args.into_iter();
    if args.next()? != ENTRY {
        return None;
    }
    let mode = match args.next()?.to_str()? {
        "probe" => Mode::Probe,
        "serve" => {
            let base = PathBuf::from(args.next()?);
            if !base.is_absolute() {
                return None;
            }
            Mode::Serve { base }
        }
        _ => return None,
    };
    if args.next().is_some() { None } else { Some(mode) }
}

/// A whole newline-terminated event, never a truncated prefix or an arbitrary child's words.
pub fn parse_event(line: &[u8]) -> Option<Event> {
    if line.len() > MAX_EVENT_BYTES {
        return None;
    }
    let line = std::str::from_utf8(line.strip_suffix(b"\n")?).ok()?;
    match line {
        "STOPPED" => Some(Event::Stopped),
        "FAILED" => Some(Event::Failed),
        "PROBE AVAILABLE" => Some(Event::Probe(Java::Available)),
        "PROBE MISSING" => Some(Event::Probe(Java::Missing)),
        "PROBE TOOOLD" => Some(Event::Probe(Java::TooOld)),
        "PROBE UNKNOWN" => Some(Event::Probe(Java::Unknown)),
        _ => {
            let port = line.strip_prefix("BOUND ")?;
            if port.is_empty() || port.len() > 5 || !port.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let port = port.parse::<u16>().ok()?;
            (port != 0).then_some(Event::Bound(port))
        }
    }
}

pub fn issue_for_exit(code: Option<i32>) -> Issue {
    match code {
        Some(EXIT_INTEGRITY) => Issue::IntegrityFailed,
        Some(EXIT_NOT_INSTALLED) => Issue::NotInstalled,
        Some(EXIT_JAVA_MISSING) => Issue::JavaMissing,
        Some(EXIT_JAVA_TOO_OLD) => Issue::JavaTooOld,
        Some(EXIT_TIMEOUT) => Issue::StartTimedOut,
        _ => Issue::StartFailed,
    }
}
