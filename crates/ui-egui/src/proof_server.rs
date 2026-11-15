// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! The native host's optional, owner-operated local LanguageTool installation and process.
//!
//! This is a service contract, not an installer or a process runner. Status is a cached snapshot;
//! neither method may wait for network, disk, Java, or a child process on the window's thread.
//! A download/run request is an explicit owner action, never a side effect of reading status.

/// Work an owner explicitly requests from the native host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Read-only installation verification and Java discovery, explicitly requested by the owner.
    Check,
    Download,
    Cancel,
    Start,
    Stop,
}

/// The host's current operation. Progress and failures are kept separately from draft checking.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    #[default]
    Unavailable,
    /// This host is available, but the owner has not asked it to inspect the installation yet.
    Unchecked,
    Checking,
    NotInstalled,
    Downloading,
    Extracting,
    /// Cancellation was requested; the worker has not yet finished cleanup.
    Cancelling,
    Installed,
    Starting,
    Running,
    /// The endpoint is no longer available; the owned helper is still being reaped.
    Stopping,
    Stopped,
    Cancelled,
    Failed,
}

/// Java is only discovered, never downloaded or installed without a separate owner decision.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Java {
    #[default]
    Unknown,
    Available,
    Missing,
    TooOld,
}

/// Static error kinds only: no provider error body, command line, draft, or server output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Issue {
    Unavailable,
    Busy,
    JavaMissing,
    JavaTooOld,
    DownloadFailed,
    IntegrityFailed,
    InvalidArchive,
    InstallFailed,
    NotInstalled,
    StartFailed,
    StartTimedOut,
    Cancelled,
}

/// A cheap, bounded snapshot. The endpoint is meaningful only while `phase == Running` and must
/// still pass the grammar adapter's loopback validation before any text is sent to it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub phase: Phase,
    pub java: Java,
    pub version: String,
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
    pub endpoint: Option<String>,
    /// Changes when the running instance changes, not for every download-progress update.
    pub epoch: u64,
    pub issue: Option<Issue>,
}

/// Implemented by a native host. The web build has no such service.
///
/// A host owns only the child it started. Stop must not discover or kill unrelated servers.
/// Parent shutdown closes the supervisor's private pipe so an abrupt app exit also stops Java.
///
/// Reading status never initiates discovery, download, or execution. `Check` is read-only;
/// `Download` and `Start` require their own explicit window action and are never consequences of
/// selecting managed mode or asking for status. `request` only queues bounded work (or sets its
/// cancellation/stop flag): it does no disk, network, Java probe, pipe write, or process wait on
/// the caller's thread. `Busy` means nothing was admitted because work is already in flight;
/// `NotInstalled` means there is no verified installation to start, and `Unavailable` means the
/// host cannot perform the operation. Repeated cancellation is idempotent. Pending cancellation
/// and stopping remain distinct from completed cleanup; only `Running` publishes an endpoint.
pub trait ManagedServer: Send + Sync {
    fn status(&self) -> Status;
    fn request(&self, action: Action) -> Result<(), Issue>;
}
