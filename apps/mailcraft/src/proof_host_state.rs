// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Cached managed-service admission. All calls are memory-only, under the host's short lock.
use mailcraft_ui_egui::proof_server::{Action, Issue, Java, Phase, Status};

/// A version string comes from the pinned installer, but every outside value is treated as hostile.
pub(crate) const MAX_VERSION_BYTES: usize = 64;

pub(crate) struct State {
    status: Status,
    verified: bool,
    /// Generation of the currently admitted operation. New Check/Download/Start work only.
    ticket: u64,
}
impl State {
    pub(crate) fn new(available: bool) -> Self {
        Self {
            status: Status { phase: if available { Phase::Unchecked } else { Phase::Unavailable }, ..Status::default() },
            verified: false,
            ticket: 0,
        }
    }
    /// The host reads a private snapshot; it never mutates status directly.
    pub(crate) fn snapshot(&self) -> Status {
        self.status.clone()
    }
    pub(crate) fn phase(&self) -> Phase {
        self.status.phase
    }
    /// The generation of the currently admitted operation; zero before the first one.
    pub(crate) fn ticket(&self) -> u64 {
        self.ticket
    }
    /// `true` admits exactly one operation/flag transition; `false` is an idempotent no-op.
    /// Rejections leave every field unchanged and must never queue work in the host.
    pub(crate) fn begin(&mut self, action: Action) -> Result<bool, Issue> {
        use Phase::*;
        let phase = self.status.phase;
        if phase == Unavailable {
            return Err(Issue::Unavailable);
        }
        match action {
            Action::Cancel => {
                if matches!(phase, Cancelling | Stopping) {
                    return Ok(false);
                }
                if matches!(phase, Checking | Downloading | Extracting | Starting) {
                    self.revoke();
                    self.status.phase = Cancelling;
                    return Ok(true);
                }
                return Ok(false);
            }
            Action::Stop => {
                if phase == Stopping {
                    return Ok(false);
                }
                if matches!(phase, Starting | Running) {
                    self.revoke();
                    self.status.phase = Stopping;
                    return Ok(true);
                }
                return if matches!(phase, Checking | Downloading | Extracting | Cancelling) { Err(Issue::Busy) } else { Ok(false) };
            }
            _ => {}
        }
        if matches!(phase, Checking | Downloading | Extracting | Cancelling | Starting | Running | Stopping) {
            return Err(Issue::Busy);
        }
        let next = match action {
            Action::Check => Checking,
            Action::Download
                if (matches!(phase, NotInstalled | Cancelled) && !self.verified)
                    || (phase == Failed && self.status.issue == Some(Issue::DownloadFailed)) =>
            {
                Downloading
            }
            Action::Download => return Err(Issue::NotInstalled),
            Action::Start if !self.verified || !matches!(phase, Installed | Stopped) => return Err(Issue::NotInstalled),
            Action::Start if self.status.java == Java::TooOld => return Err(Issue::JavaTooOld),
            Action::Start if self.status.java != Java::Available => return Err(Issue::JavaMissing),
            Action::Start => Starting,
            Action::Cancel | Action::Stop => return Ok(false),
        };
        // Generations are unique and monotonic. Exhaustion is unreachable in practice and must
        // still refuse admission without mutating any state rather than wrap or panic.
        let Some(ticket) = self.ticket.checked_add(1) else { return Err(Issue::Busy) };
        // No endpoint may survive into fresh work. Revoking an existing one is the only epoch move.
        self.revoke();
        self.ticket = ticket;
        self.status.phase = next;
        self.status.issue = None;
        self.status.downloaded_bytes = 0;
        self.status.total_bytes = 0;
        Ok(true)
    }
    /// Completes the operation `ticket` belongs to. A stale worker result is refused untouched.
    /// During cancellation the same operation may still truthfully record a completed promotion.
    pub(crate) fn checked(&mut self, ticket: u64, installed: bool, java: Java, version: &str) -> bool {
        if ticket != self.ticket {
            return false;
        }
        match self.status.phase {
            Phase::Checking | Phase::Downloading | Phase::Extracting => {
                self.verified = installed;
                self.status.java = java;
                if installed {
                    self.status.version = bounded_version(version);
                } else {
                    self.status.version.clear();
                }
                self.revoke();
                self.status.phase = if installed { Phase::Installed } else { Phase::NotInstalled };
                self.status.issue = None;
                true
            }
            Phase::Cancelling => {
                self.verified = installed;
                self.status.java = java;
                if installed {
                    self.status.version = bounded_version(version);
                } else {
                    self.status.version.clear();
                }
                self.revoke();
                self.status.phase = if installed { Phase::Installed } else { Phase::Cancelled };
                self.status.issue = Some(Issue::Cancelled);
                true
            }
            _ => false,
        }
    }
    /// Download progress, or extraction progress once extraction has begun. Extraction is
    /// monotonic: no later packet may report the operation back as downloading.
    pub(crate) fn progress(&mut self, ticket: u64, written: u64, total: u64, extracting: bool) -> bool {
        if ticket != self.ticket {
            return false;
        }
        match self.status.phase {
            Phase::Downloading => self.status.phase = if extracting { Phase::Extracting } else { Phase::Downloading },
            Phase::Extracting => {}
            _ => return false,
        }
        self.status.downloaded_bytes = written.min(total);
        self.status.total_bytes = total;
        true
    }
    /// Called only after an owned helper's BOUND marker, liveness and bounded health/warmup.
    pub(crate) fn publish(&mut self, ticket: u64, port: u16) -> bool {
        if ticket != self.ticket || !self.verified || self.status.phase != Phase::Starting || port == 0 {
            return false;
        }
        self.status.endpoint = Some(format!("http://127.0.0.1:{port}"));
        self.status.epoch = self.status.epoch.saturating_add(1);
        self.status.phase = Phase::Running;
        self.status.issue = None;
        true
    }
    /// Completes the operation `ticket` belongs to with a safe terminal phase. A stale ticket,
    /// an active/regressive phase, or a duplicate finish over an already terminal operation is
    /// refused without touching status. A cancelled, still-verified installation becomes
    /// `Stopped`, so its version and installation remain startable.
    pub(crate) fn finished(&mut self, ticket: u64, phase: Phase, issue: Option<Issue>) -> bool {
        if ticket != self.ticket || !matches!(phase, Phase::Failed | Phase::Stopped | Phase::Cancelled) {
            return false;
        }
        if !matches!(
            self.status.phase,
            Phase::Checking | Phase::Downloading | Phase::Extracting | Phase::Cancelling | Phase::Starting | Phase::Running | Phase::Stopping
        ) {
            return false;
        }
        if matches!(issue, Some(Issue::IntegrityFailed | Issue::NotInstalled)) {
            self.verified = false;
            self.status.version.clear();
        }
        match issue {
            Some(Issue::JavaMissing) => self.status.java = Java::Missing,
            Some(Issue::JavaTooOld) => self.status.java = Java::TooOld,
            _ => {}
        }
        self.revoke();
        self.status.phase = if phase == Phase::Cancelled && self.verified { Phase::Stopped } else { phase };
        self.status.issue = issue;
        true
    }
    fn revoke(&mut self) {
        if self.status.endpoint.take().is_some() {
            self.status.epoch = self.status.epoch.saturating_add(1);
        }
    }
}

fn bounded_version(version: &str) -> String {
    let mut end = version.len().min(MAX_VERSION_BYTES);
    while end > 0 && !version.is_char_boundary(end) {
        end -= 1;
    }
    match version.get(..end) {
        Some(prefix) => prefix.to_owned(),
        None => String::new(),
    }
}
