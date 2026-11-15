// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! The LanguageTool server a grammar check runs on (#954): installing the official 6.6 archive on
//! this computer, and finding it again.
//!
//! LanguageTool is a separate program, licensed on its own (LGPL 2.1, kept in the archive's
//! `COPYING.txt`); nothing of it is copied into MailCraft, and nothing of it is run here. This
//! module fetches the release the project publishes, checks it against the digest this build
//! carries, unpacks it under the owner's data folder, and later answers where it is — and whether
//! every file of it is still the one that was checked.
//!
//! **One address, one digest.** [`URL`] is `https`, to `languagetool.org` on port 443, with no user
//! name, no query and no fragment; a redirect is followed only to that same host over `https` on
//! 443, and nothing else. No proxy is consulted (this crate's transport has none). The body is
//! read as it comes into a file, never more than [`DOWNLOAD_BYTES`], and the SHA-256 of exactly
//! those bytes must be [`SHA256`] **before a single byte of it is read as a ZIP** — so the archive
//! is one this build already knows, and nothing in it can steer the download.
//!
//! **What an archive is read for is decided before it is opened.** An end-of-central-directory
//! prescan (`Scan`) bounds what the ZIP reader may allocate — at most `MAX_ENTRIES` entries,
//! `MAX_CENTRAL` bytes of central directory, `MAX_FILE` a file, `MAX_UNPACKED` unpacked in
//! all, `MAX_PATH` bytes a name — and refuses a ZIP64 archive, a multi-disk one, an encrypted
//! one, a member compressed by a method other than stored or deflated, a duplicated name, and any
//! name that is absolute, has a `..` component, a backslash, a drive letter, a UNC start, a
//! trailing dot or space, a Windows device name, or a control character. A member that is not a
//! regular file or a directory — a symbolic link above all — is refused, so nothing is ever
//! unpacked through a link. Each member is then compared with the prescan (name, size, CRC-32),
//! extracted while its bytes are counted and hashed, and its written length and CRC-32 must be the
//! ones its own header declared.
//!
//! **Only after all of it is there.** Everything is unpacked into a staging directory of this
//! module's own (`base/languagetool/.staging-…`, made with `create_dir` so it cannot already
//! exist), every file is created with `create_new` so nothing is ever overwritten, and the only
//! step that makes an install visible is one `rename` of that directory onto its name — so a
//! half-unpacked tree is never an install, and an install that is there is complete. An install
//! already in place is never replaced, deleted or written over: if it is the one this build
//! pinned, it is left as it is and installed nothing; if it isn't, this refuses
//! ([`InstallError::Integrity`]) and still leaves it alone. A staging directory, a partial
//! download or the archive itself is deleted on every failure and on cancellation, and a staging
//! directory a killed process left behind is cleared by the next install while it holds the lock.
//!
//! **A second installer waits at the door.** `base/languagetool/.lock` is taken with the
//! operating system's own exclusive lock, so a second install in this process or another is told
//! [`InstallError::Busy`] rather than racing, and the lock is let go by the system when the
//! holder ends, however it ends.
//!
//! **Installed means every file, not the first one.** The archive is unpacked, and the manifest of
//! what came out of it — one line for every regular file, with its SHA-256 and its length, sorted
//! by path — is written beside the tree. That manifest's own SHA-256 is [`MANIFEST_SHA256`], a
//! constant of this build, worked out from the official archive and pinned here: so [`locate`]
//! needs no archive, no network and no copy of the digests to say that what is on disk is what was
//! verified. The canonical format is exactly:
//!
//! ```text
//! mailcraft-languagetool-manifest/1\n
//! <64 lowercase hex digits of SHA-256>  <length in decimal>  <path>\n   (once per file, …)
//! ```
//!
//! with the paths relative to the install directory (`6.6/`), each starting `LanguageTool-6.6/`,
//! separated by `/`, sorted by their bytes, and nothing else in the file — no BOM, no comment, no
//! blank line, always a final `\n`. [`locate`] reads it, hashes it, refuses anything but
//! [`MANIFEST_SHA256`], then hashes every file it names, and refuses a missing file, a file whose
//! bytes are not its line's, a symbolic link anywhere, and a file the manifest doesn't name. It
//! creates nothing, opens nothing for writing, downloads nothing and starts nothing: the app calls
//! it on a background thread and decides for itself.
//!
//! **What this does not do.** It starts no server (the host does, with Java, once the owner turns
//! proofing on); it downloads nothing on its own; it keeps no archive (the 240 MB ZIP is deleted
//! once the tree is verified); it writes nothing outside `base/languagetool/`; and it preserves no
//! executable bit — nothing here is ever executed. Because the manifest is the whole of what is
//! verified, an extra file placed inside the installed tree by hand (a custom dictionary, say)
//! makes [`locate`] refuse that install rather than run beside it.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, BufReader, ErrorKind, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ring::digest::{Context as Digest, SHA256 as SHA256_ALGORITHM};
use ring::rand::SecureRandom as _;
use zip::ZipArchive;
use zip::read::ZipFile;

use crate::http::{self, MAX_LONG_URL, StreamPatience, Streamed, Url};

/// The LanguageTool release this build installs.
pub const VERSION: &str = "6.6";
/// Where it is downloaded from: the project's own site, over `https`, on 443, with no user name,
/// no query and no fragment.
pub const URL: &str = "https://languagetool.org/download/LanguageTool-6.6.zip";
/// The SHA-256 of that file, worked out from a complete download of it over `https` from that
/// address. The release carries no signature of its own, so this constant is the whole of what
/// says the archive is the right one; it is also the digest the file must have before it is read
/// as a ZIP, and before anything is unpacked from it.
pub const SHA256: [u8; 32] = [
    0x53, 0x60, 0x05, 0x06, 0xb3, 0x99, 0xbb, 0x5f, 0xfe, 0x1e, 0x4c, 0x8d, 0xec, 0x79, 0x4f, 0xd3, 0x78, 0x21, 0x2f, 0x14, 0xaa, 0xf3, 0x8c, 0xce,
    0xf9, 0xb6, 0xf8, 0x93, 0x14, 0xd1, 0x16, 0x31,
];
/// Exactly how long that file is. A body that says it is longer, or that turns out to be longer,
/// is refused before anything more of it is written; one that ends early is refused too.
pub const DOWNLOAD_BYTES: u64 = 251_998_221;
/// The SHA-256 of the canonical manifest of the official archive's 1,789 files — the constant
/// [`locate`] checks an installed manifest against. It was worked out by this module from the
/// verified archive itself (never written by hand), and is what lets an install be checked later
/// without the archive, without the network, and without trusting the installed manifest's own
/// words.
pub const MANIFEST_SHA256: [u8; 32] = [
    0x59, 0xcb, 0x50, 0x35, 0xbe, 0x3a, 0xa7, 0x19, 0x50, 0xfc, 0x0d, 0x49, 0x5b, 0xdd, 0xcf, 0xc8, 0x7b, 0xd3, 0xf4, 0x7f, 0x3d, 0xbf, 0x61, 0x6e,
    0xb8, 0x97, 0x3e, 0xd0, 0xa7, 0x70, 0x2d, 0x76,
];
/// How many regular files the official archive holds, and how many bytes they take when unpacked
/// (both counted from its central directory and checked again as it is unpacked).
pub(crate) const FILES: u64 = 1_789;
pub(crate) const UNPACKED_BYTES: u64 = 405_072_177;

/// The one directory the whole archive is under.
const TOP: &str = "LanguageTool-6.6";
/// What is made under the owner's data folder: `languagetool/`, the lock, the staging directories
/// and one directory a version.
const ROOT: &str = "languagetool";
const LOCK: &str = ".lock";
const STAGING: &str = ".staging-";
const DOWNLOAD: &str = ".download-";
/// The manifest's name inside a version's directory, and the first line of it.
const MANIFEST: &str = "manifest.txt";
const MANIFEST_HEADER: &str = "mailcraft-languagetool-manifest/1";

/// Most entries an archive may have, bytes of central directory it may take, bytes one member may
/// unpack to, bytes all of them may, and bytes a member's path may take.
const MAX_ENTRIES: usize = 16_000;
const MAX_CENTRAL: u64 = 8 * 1024 * 1024;
const MAX_FILE: u64 = 64 * 1024 * 1024;
const MAX_UNPACKED: u64 = 600 * 1024 * 1024;
const MAX_PATH: usize = 1_024;
/// Most components a path may have.
const MAX_DEPTH: usize = 32;
/// Most bytes the manifest of an install may take (the official one is about 140 KB).
const MAX_MANIFEST: u64 = 4 * 1024 * 1024;
/// The end-of-central-directory record is 22 bytes, and its comment at most 65,535.
const EOCD_MIN: usize = 22;
const EOCD_COMMENT_MAX: usize = 65_535;
/// How often a progress report is made while the body is coming, and while files are unpacked.
const REPORT_EVERY: u64 = 1 << 20;
const REPORT_FILES: u64 = 16;
/// Most redirects followed, and bytes of a chunk read while unpacking.
const MAX_REDIRECTS: usize = 3;
const CHUNK: usize = 64 * 1024;

/// ZIP's own signatures.
const EOCD: [u8; 4] = [b'P', b'K', 0x05, 0x06];
const CENTRAL: [u8; 4] = [b'P', b'K', 0x01, 0x02];
const ZIP64_LOCATOR: [u8; 4] = [b'P', b'K', 0x06, 0x07];

/// How long connecting may take, how long the body may be silent, and how long all of it may
/// take: a quarter of a gigabyte from a project's own site, with room to be slow.
const PATIENCE: StreamPatience =
    StreamPatience { connect: Duration::from_secs(20), idle: Duration::from_secs(40), total: Duration::from_secs(30 * 60) };

/// How far an install has come, for a window that shows it. Both counts only ever go up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
    /// The archive coming, as bytes written so far and bytes there will be ([`DOWNLOAD_BYTES`]).
    Download { written: u64, total: u64 },
    /// The tree being unpacked, as files finished and files there will be.
    Extract { files: u64, total: u64 },
}

/// A LanguageTool on this computer that [`locate`] checked whole.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Installed {
    /// The LanguageTool home: the directory holding `languagetool-server.jar` and `lib/`.
    pub directory: PathBuf,
    /// The release it is ([`VERSION`]).
    pub version: String,
}

/// Why there is no install. Each is one of a few things that can go wrong, and none of them holds
/// a path, an address, or a word of what a server said.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InstallError {
    /// It couldn't be had: no connection, an answer that isn't the file, one that was cut off, or
    /// one that is longer than the file is. Trying again later may work.
    #[error("the LanguageTool download could not be completed")]
    Download,
    /// It isn't the file this build knows: the bytes are not the pinned ones, an installed copy is
    /// not the one that was verified, or one of its files has changed since.
    #[error("the LanguageTool files are not the ones this version of MailCraft verified")]
    Integrity,
    /// The archive is not one that may be unpacked: it says it is a ZIP64 or a multi-disk one, a
    /// member is encrypted or compressed another way, or a name or a size is not one that is safe
    /// to unpack.
    #[error("the LanguageTool archive is not one that is safe to unpack")]
    Archive,
    /// It couldn't be written: the disk, the folder, or the permissions.
    #[error("the LanguageTool files could not be written on this computer")]
    Io,
    /// Another install of it is running: nothing was downloaded and nothing was changed.
    #[error("another LanguageTool install is running")]
    Busy,
    /// The owner (or the app for them) called it off. What had come is deleted.
    #[error("the LanguageTool install was cancelled")]
    Cancelled,
}

/// Where an install is had from, and how it is checked. Production has exactly one of these,
/// [`Source::official`]; a test may point one at a stand-in server on this machine, which is the
/// only way the address is ever anything but [`URL`] — there is no public way to change it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Source {
    /// The address of the archive, `https` in production.
    pub(crate) url: String,
    /// The one host it, and any redirect on the way, may be at.
    pub(crate) host: String,
    /// Whether it must be `https` on 443 (production), or plain on the URL's own port (a stand-in
    /// on this machine, which the crate's HTTP client only speaks to for loopback).
    pub(crate) secure: bool,
    pub(crate) version: String,
    pub(crate) bytes: u64,
    pub(crate) sha256: [u8; 32],
    /// What the archive unpacks to and what its manifest hashes to, when this build pins them.
    /// [`Source::official`] always does; a stand-in a test runs has nothing pinned, and is only
    /// reachable from this crate's own tests.
    pub(crate) pins: Option<Pins>,
}

/// What the archive must come to, and what the manifest of what it unpacks to must hash to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Pins {
    pub(crate) files: u64,
    pub(crate) unpacked: u64,
    pub(crate) manifest: [u8; 32],
}

impl Source {
    /// The release this build pins.
    pub(crate) fn official() -> Source {
        Source {
            url: URL.to_string(),
            host: "languagetool.org".to_string(),
            secure: true,
            version: VERSION.to_string(),
            bytes: DOWNLOAD_BYTES,
            sha256: SHA256,
            pins: Some(Pins { files: FILES, unpacked: UNPACKED_BYTES, manifest: MANIFEST_SHA256 }),
        }
    }
}

/// Install the LanguageTool this build pins under `base` (the owner's data folder), or find it
/// already there and verified. Blocks for as long as the download and the unpacking take — a
/// quarter of a gigabyte down and 400 MB unpacked — so a host calls it on a thread of its own and
/// never on the window's. `cancel` is looked at while the body comes and between files; `progress`
/// is called as it goes.
///
/// Nothing is written outside `base/languagetool/`, nothing already there is overwritten, and an
/// install is only ever visible once it is whole. The archive is deleted once its tree is in
/// place. What is returned is checked as far as it can be while it is being written;
/// [`locate`] is what checks it again later.
pub fn install(base: &Path, cancel: &AtomicBool, progress: &dyn Fn(Progress)) -> Result<Installed, InstallError> {
    install_from(base, &Source::official(), cancel, progress)
}

/// [`install`] from `source`: the same work, for the stand-in server a test runs. Not the public
/// API: there is no way for the app, a plugin or an agent to name another address through here.
pub(crate) fn install_from(base: &Path, source: &Source, cancel: &AtomicBool, progress: &dyn Fn(Progress)) -> Result<Installed, InstallError> {
    if cancel.load(Ordering::Relaxed) {
        return Err(InstallError::Cancelled);
    }
    let root = base.join(ROOT);
    fs::create_dir_all(&root).map_err(|_| InstallError::Io)?;
    // One at a time, and only for as long as this runs: the lock is the system's, so it is let go
    // even if the process is killed.
    let _held = Held::take(&root)?;
    // Already there, and the one this build verified: nothing is downloaded, nothing is touched.
    let pin = source.pins.as_ref().map(|pins| pins.manifest);
    if let Some(found) = locate_with(base, &source.version, pin)? {
        return Ok(found);
    }
    tidy(&root);
    let (staging, nonce) = staging_of(&root)?;
    let mut unfinished = Unfinished { staging: staging.clone(), archive: root.join(format!("{DOWNLOAD}{nonce}")), done: false };
    download(source, &unfinished.archive, cancel, progress)?;
    let scan = prescan(&unfinished.archive, source)?;
    if let Some(pins) = &source.pins
        && (scan.files != pins.files || scan.unpacked != pins.unpacked)
    {
        return Err(InstallError::Integrity);
    }
    let manifest = extract(&unfinished.archive, &scan, &staging, cancel, progress)?;
    if source.pins.as_ref().is_some_and(|pins| sha256_of(&manifest) != pins.manifest) {
        return Err(InstallError::Integrity);
    }
    write_new(&staging.join(MANIFEST), &manifest)?;
    fs::remove_file(&unfinished.archive).map_err(|_| InstallError::Io)?;
    let wanted = root.join(&source.version);
    match fs::symlink_metadata(&wanted) {
        // Nothing but this process may put it there, and this one holds the lock: something else
        // did. It is left alone, and it is only taken if it is the one that was verified.
        Ok(_) => {
            if locate_with(base, &source.version, pin)?.is_none() {
                return Err(InstallError::Integrity);
            }
        }
        Err(e) if e.kind() == ErrorKind::NotFound => fs::rename(&staging, &wanted).map_err(|_| InstallError::Io)?,
        Err(_) => return Err(InstallError::Io),
    }
    unfinished.done = true;
    Ok(Installed { directory: wanted.join(TOP), version: source.version.clone() })
}

/// The LanguageTool this build pinned under `base`, checked whole, or `None` when there is none.
///
/// Read-only: it creates no directory, opens no file for writing, downloads nothing, and starts
/// nothing. Every file the installed manifest names is hashed and compared, the manifest itself is
/// hashed against [`MANIFEST_SHA256`], and a missing file, a changed one, a symbolic link, or one
/// the manifest doesn't name is [`InstallError::Integrity`]. Because it reads 400 MB, a host calls
/// it on a thread of its own.
pub fn locate(base: &Path) -> Result<Option<Installed>, InstallError> {
    locate_with(base, VERSION, Some(MANIFEST_SHA256))
}

/// [`locate`] for a release's directory and, when this build pins one, the manifest digest the
/// install must have been made with. Production always pins one ([`MANIFEST_SHA256`]); the stand-in
/// archives this crate's tests install pin their own, or none.
pub(crate) fn locate_with(base: &Path, version: &str, pin: Option<[u8; 32]>) -> Result<Option<Installed>, InstallError> {
    let directory = base.join(ROOT).join(version);
    match fs::symlink_metadata(&directory) {
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(InstallError::Io),
        // A link where the install belongs is not the install, whether or not it leads somewhere.
        Ok(found) if !found.is_dir() || found.file_type().is_symlink() => return Err(InstallError::Integrity),
        Ok(_) => {}
    }
    let manifest_at = directory.join(MANIFEST);
    let meta = fs::symlink_metadata(&manifest_at).map_err(|_| InstallError::Integrity)?;
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > MAX_MANIFEST {
        return Err(InstallError::Integrity);
    }
    let manifest = fs::read(&manifest_at).map_err(|_| InstallError::Integrity)?;
    if manifest.len() as u64 != meta.len() || pin.is_some_and(|pin| sha256_of(&manifest) != pin) {
        return Err(InstallError::Integrity);
    }
    let files = read_manifest(&manifest)?;
    let mut wanted: HashMap<&str, (u64, [u8; 32])> = HashMap::with_capacity(files.len());
    for (path, size, digest) in &files {
        if wanted.insert(path.as_str(), (*size, *digest)).is_some() {
            return Err(InstallError::Integrity);
        }
    }
    for (path, size, digest) in &files {
        let at = directory.join(path);
        let meta = fs::symlink_metadata(&at).map_err(|_| InstallError::Integrity)?;
        if !meta.is_file() || meta.file_type().is_symlink() || meta.len() != *size {
            return Err(InstallError::Integrity);
        }
        let mut file = File::open(&at).map_err(|_| InstallError::Integrity)?;
        let mut hasher = Digest::new(&SHA256_ALGORITHM);
        let mut buf = vec![0u8; CHUNK];
        loop {
            match file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => hasher.update(buf.get(..n).ok_or(InstallError::Integrity)?),
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => return Err(InstallError::Integrity),
            }
        }
        if <[u8; 32]>::try_from(hasher.finish().as_ref()).map_err(|_| InstallError::Integrity)? != *digest {
            return Err(InstallError::Integrity);
        }
    }
    // Nothing the manifest doesn't name, and nothing that isn't a file or a directory: an added
    // JAR is as much a change to the program as a changed one.
    let mut pending = vec![(directory.clone(), String::new())];
    let mut seen = 0usize;
    while let Some((at, prefix)) = pending.pop() {
        let entries = fs::read_dir(&at).map_err(|_| InstallError::Integrity)?;
        for entry in entries {
            seen = seen.saturating_add(1);
            if seen > MAX_ENTRIES.saturating_add(1) {
                return Err(InstallError::Integrity);
            }
            let entry = entry.map_err(|_| InstallError::Integrity)?;
            let name = entry.file_name().into_string().map_err(|_| InstallError::Integrity)?;
            let rel = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
            let kind = entry.file_type().map_err(|_| InstallError::Integrity)?;
            if kind.is_symlink() {
                return Err(InstallError::Integrity);
            }
            if kind.is_dir() {
                pending.push((entry.path(), rel));
            } else if kind.is_file() {
                if rel == MANIFEST && prefix.is_empty() {
                    continue;
                }
                if wanted.remove(rel.as_str()).is_none() {
                    return Err(InstallError::Integrity);
                }
            } else {
                return Err(InstallError::Integrity);
            }
        }
    }
    if !wanted.is_empty() {
        return Err(InstallError::Integrity);
    }
    Ok(Some(Installed { directory: directory.join(TOP), version: version.to_string() }))
}

/// The SHA-256 of `bytes`, written as 64 lowercase hex digits, for a log line or a window.
pub fn fingerprint(bytes: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// The lock, held for as long as this is alive.
struct Held {
    _file: File,
}

impl Held {
    fn take(root: &Path) -> Result<Held, InstallError> {
        let file = OpenOptions::new().create(true).truncate(false).read(true).write(true).open(root.join(LOCK)).map_err(|_| InstallError::Io)?;
        match file.try_lock() {
            Ok(()) => Ok(Held { _file: file }),
            Err(TryLockError::WouldBlock) => Err(InstallError::Busy),
            Err(TryLockError::Error(_)) => Err(InstallError::Io),
        }
    }
}

/// What a failed or cancelled install leaves behind, and how it is taken away again: the staging
/// directory and the partial download are this run's own, and are deleted however the run ends.
struct Unfinished {
    staging: PathBuf,
    archive: PathBuf,
    done: bool,
}

impl Drop for Unfinished {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        if fs::symlink_metadata(&self.staging).is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink()) {
            let _ = fs::remove_dir_all(&self.staging);
        }
        if fs::symlink_metadata(&self.archive).is_ok_and(|m| m.is_file() && !m.file_type().is_symlink()) {
            let _ = fs::remove_file(&self.archive);
        }
    }
}

/// What a killed process left: a staging directory or a partial download, under a name this
/// module alone writes. Only a directory that is one, and a file that is one, is taken away; a
/// link is left where it is, never followed.
fn tidy(root: &Path) {
    let Ok(entries) = fs::read_dir(root) else { return };
    for entry in entries.flatten().take(64) {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !(name.starts_with(STAGING) || name.starts_with(DOWNLOAD)) {
            continue;
        }
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            let _ = fs::remove_dir_all(entry.path());
        } else if kind.is_file() {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// A fresh staging directory, made so that it cannot have been there (`create_dir`), with the
/// nonce that names it and the download beside it.
fn staging_of(root: &Path) -> Result<(PathBuf, String), InstallError> {
    for _ in 0..8 {
        let nonce = nonce();
        let at = root.join(format!("{STAGING}{nonce}"));
        match fs::create_dir(&at) {
            Ok(()) => return Ok((at, nonce)),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(_) => return Err(InstallError::Io),
        }
    }
    Err(InstallError::Io)
}

/// Random bytes for a name nothing else has, with the process's own number and the clock mixed in
/// so that a failing source of randomness still can't collide with itself.
fn nonce() -> String {
    let mut bytes = [0u8; 8];
    let _ = ring::rand::SystemRandom::new().fill(&mut bytes);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    let random: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{}-{random}-{now:08x}", std::process::id())
}

/// A file that must not exist yet, written whole, and synced: the manifest, whose bytes are the
/// ones that were verified, and the only thing an install is found by.
fn write_new(at: &Path, bytes: &[u8]) -> Result<(), InstallError> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(at).map_err(|_| InstallError::Io)?;
    file.write_all(bytes).map_err(|_| InstallError::Io)?;
    file.sync_all().map_err(|_| InstallError::Io)
}

/// The body of the archive, into `at`, as it comes.
fn download(source: &Source, at: &Path, cancel: &AtomicBool, progress: &dyn Fn(Progress)) -> Result<(), InstallError> {
    let mut url = Url::parse_within(&source.url, MAX_LONG_URL).map_err(|_| InstallError::Download)?;
    if !allowed(&url, source) || url.path.contains('?') || url.path.contains('#') {
        return Err(InstallError::Download);
    }
    let mut file = OpenOptions::new().write(true).create_new(true).open(at).map_err(|_| InstallError::Io)?;
    (progress)(Progress::Download { written: 0, total: source.bytes });
    let mut hasher = Digest::new(&SHA256_ALGORITHM);
    let mut redirects = 0usize;
    let written = loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(InstallError::Cancelled);
        }
        let accept = |said: &Streamed| said.status == 200 && said.length.is_none_or(|length| length == source.bytes);
        let mut sink = Sink { file: &mut file, hasher: &mut hasher, written: 0, reported: 0, total: source.bytes, cancel, progress, trouble: None };
        let said = http::stream(&url, &[("Accept", "application/octet-stream")], PATIENCE, source.bytes, &accept, &mut sink);
        let trouble = sink.trouble;
        match said {
            Ok(said) => match said.status {
                301 | 302 | 303 | 307 | 308 => {
                    let to = said.location.as_deref().ok_or(InstallError::Download)?;
                    let to = Url::parse_within(to, MAX_LONG_URL).map_err(|_| InstallError::Download)?;
                    if !allowed(&to, source) {
                        return Err(InstallError::Download);
                    }
                    redirects = redirects.saturating_add(1);
                    if redirects > MAX_REDIRECTS {
                        return Err(InstallError::Download);
                    }
                    url = to;
                    continue;
                }
                200 => {
                    if let Some(trouble) = trouble {
                        return Err(trouble);
                    }
                    break sink.written;
                }
                _ => return Err(InstallError::Download),
            },
            Err(e) => return Err(trouble.unwrap_or_else(|| if cancel.load(Ordering::Relaxed) { InstallError::Cancelled } else { net_problem(&e) })),
        }
    };
    if written != source.bytes {
        return Err(InstallError::Download);
    }
    if <[u8; 32]>::try_from(hasher.finish().as_ref()).map_err(|_| InstallError::Integrity)? != source.sha256 {
        return Err(InstallError::Integrity);
    }
    file.sync_all().map_err(|_| InstallError::Io)?;
    (progress)(Progress::Download { written: source.bytes, total: source.bytes });
    Ok(())
}

/// Whether an address is one the archive may be had from: `source`'s one host, and `https` on 443
/// in production. A user name is already refused by [`Url::parse_within`], and no other host, no
/// other port and no other way of speaking is taken — a redirect to one included.
pub(crate) fn allowed(url: &Url, source: &Source) -> bool {
    if !url.host.eq_ignore_ascii_case(&source.host) {
        return false;
    }
    if source.secure { url.secure && matches!(url.port, 0 | 443) } else { !url.secure && url.port != 0 }
}

/// What a network error means for the download: an answer longer than the file, or anything else.
/// In neither case does the error's own words (which may name the host) leave this function.
fn net_problem(_error: &crate::NetError) -> InstallError {
    InstallError::Download
}

/// The body's way into the file: counted, hashed, bounded by the caller, and stopped when the
/// owner cancels. A disk error is remembered as [`InstallError::Io`] rather than a download one,
/// and a cancellation as [`InstallError::Cancelled`], because the HTTP client's error says only
/// that the sink wouldn't take it.
struct Sink<'a> {
    file: &'a mut File,
    hasher: &'a mut Digest,
    written: u64,
    reported: u64,
    total: u64,
    cancel: &'a AtomicBool,
    progress: &'a dyn Fn(Progress),
    trouble: Option<InstallError>,
}

impl Write for Sink<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let stopped = || io::Error::other("the download stopped");
        if self.trouble.is_some() {
            return Err(stopped());
        }
        if self.cancel.load(Ordering::Relaxed) {
            self.trouble = Some(InstallError::Cancelled);
            return Err(stopped());
        }
        if self.file.write_all(bytes).is_err() {
            self.trouble = Some(InstallError::Io);
            return Err(stopped());
        }
        self.hasher.update(bytes);
        self.written = self.written.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        if self.written.saturating_sub(self.reported) >= REPORT_EVERY {
            self.reported = self.written;
            (self.progress)(Progress::Download { written: self.written.min(self.total), total: self.total });
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// One member of the archive, as its central directory describes it, after it was checked.
pub(crate) struct Member {
    pub(crate) name: String,
    /// The path under the install directory, `LanguageTool-6.6/…`, with `/` separators.
    pub(crate) path: String,
    pub(crate) crc: u32,
    pub(crate) compressed: u64,
    pub(crate) size: u64,
    pub(crate) dir: bool,
}

/// What the end-of-central-directory prescan found: every member, and the counts an install is
/// checked against. Nothing was allocated for it that the archive's own bytes did not already
/// bound.
pub(crate) struct Scan {
    pub(crate) members: Vec<Member>,
    /// Regular files (what the manifest lists), and what they unpack to.
    pub(crate) files: u64,
    pub(crate) unpacked: u64,
}

/// Read the archive's central directory, within every bound, before the ZIP reader sees the file.
/// Every refusal here is [`InstallError::Archive`]: the archive is not one that may be unpacked.
pub(crate) fn prescan(archive: &Path, source: &Source) -> Result<Scan, InstallError> {
    let mut file = File::open(archive).map_err(|_| InstallError::Archive)?;
    let len = file.metadata().map_err(|_| InstallError::Archive)?.len();
    if len != source.bytes || len < EOCD_MIN as u64 {
        return Err(InstallError::Archive);
    }
    // The record is at the end, before a comment of at most 65,535 bytes.
    let tail_len = u64::try_from(EOCD_MIN.saturating_add(EOCD_COMMENT_MAX)).unwrap_or(u64::MAX).min(len);
    let tail_at = len.saturating_sub(tail_len);
    let mut tail = vec![0u8; usize::try_from(tail_len).map_err(|_| InstallError::Archive)?];
    read_at(&mut file, tail_at, &mut tail)?;
    let at = tail.windows(4).rposition(|four| four == EOCD).ok_or(InstallError::Archive)?;
    let eocd_at = tail_at.saturating_add(u64::try_from(at).map_err(|_| InstallError::Archive)?);
    let record = tail.get(at..at.saturating_add(EOCD_MIN)).ok_or(InstallError::Archive)?;
    // A ZIP64 archive names its own record here; this doesn't read one.
    if at >= 20 && tail.get(at - 20..at).is_some_and(|twenty| twenty.get(..4) == Some(ZIP64_LOCATOR.as_slice())) {
        return Err(InstallError::Archive);
    }
    let disk = u16_of(record, 4)?;
    let central_disk = u16_of(record, 6)?;
    let on_disk = u16_of(record, 8)?;
    let entries = usize::from(u16_of(record, 10)?);
    let central_size = u64::from(u32_of(record, 12)?);
    let central_at = u64::from(u32_of(record, 16)?);
    let comment = usize::from(u16_of(record, 20)?);
    // One disk, one directory, and a record that says exactly where it ends: the sentinels that
    // mean "see the ZIP64 record" (0xFFFF, 0xFFFFFFFF) are none of them.
    if disk != 0 || central_disk != 0 || on_disk as usize != entries {
        return Err(InstallError::Archive);
    }
    if entries == 0 || entries > MAX_ENTRIES || u16_of(record, 10)? == u16::MAX {
        return Err(InstallError::Archive);
    }
    if central_size == 0 || central_size > MAX_CENTRAL || u32_of(record, 12)? == u32::MAX || u32_of(record, 16)? == u32::MAX {
        return Err(InstallError::Archive);
    }
    let comment_end = eocd_at.saturating_add(EOCD_MIN as u64).saturating_add(comment as u64);
    if comment_end != len {
        return Err(InstallError::Archive);
    }
    // The directory ends where the record begins: no room for anything between them.
    if central_at.saturating_add(central_size) != eocd_at {
        return Err(InstallError::Archive);
    }
    let mut central = vec![0u8; usize::try_from(central_size).map_err(|_| InstallError::Archive)?];
    read_at(&mut file, central_at, &mut central)?;

    let mut members: Vec<Member> = Vec::with_capacity(entries);
    let mut names: HashSet<String> = HashSet::with_capacity(entries);
    let mut at = 0usize;
    let mut files = 0u64;
    let mut unpacked = 0u64;
    for _ in 0..entries {
        let fixed = central.get(at..at.saturating_add(46)).ok_or(InstallError::Archive)?;
        if fixed.get(..4) != Some(CENTRAL.as_slice()) {
            return Err(InstallError::Archive);
        }
        let flags = u16_of(fixed, 8)?;
        let method = u16_of(fixed, 10)?;
        let crc = u32_of(fixed, 16)?;
        let compressed = u64::from(u32_of(fixed, 20)?);
        let size = u64::from(u32_of(fixed, 24)?);
        let name_len = usize::from(u16_of(fixed, 28)?);
        let extra_len = usize::from(u16_of(fixed, 30)?);
        let comment_len = usize::from(u16_of(fixed, 32)?);
        let made_by = u16_of(fixed, 4)?;
        let external = u32_of(fixed, 38)?;
        let offset = u64::from(u32_of(fixed, 42)?);
        let name_at = at.saturating_add(46);
        let name = central.get(name_at..name_at.saturating_add(name_len)).ok_or(InstallError::Archive)?;
        // Encrypted (bit 0), a masked header (bit 13), or a flag no reader here writes: refused.
        // The only ones taken are a data descriptor (bit 3) and UTF-8 names (bit 11).
        if flags & !(0x0008 | 0x0800) != 0 || flags & 0x0001 != 0 {
            return Err(InstallError::Archive);
        }
        // Stored or deflated, and no other way: no bzip2, LZMA, zstd, PPMd, or anything unknown.
        if method != 0 && method != 8 {
            return Err(InstallError::Archive);
        }
        // The sentinels of a ZIP64 member, and of a member on another disk.
        if compressed == u64::from(u32::MAX) || size == u64::from(u32::MAX) || u16_of(fixed, 34)? != 0 {
            return Err(InstallError::Archive);
        }
        if name_len == 0 || name_len > MAX_PATH {
            return Err(InstallError::Archive);
        }
        let name = std::str::from_utf8(name).map_err(|_| InstallError::Archive)?;
        let path = safe_path(name)?;
        // A link, a device, a FIFO, a socket: not a file to write. Only a regular file or a
        // directory is taken, and the top of the archive must be the one directory it is.
        let mode = external >> 16;
        let unix = made_by >> 8;
        let kind = mode & 0o170000;
        if unix == 3 && kind != 0 && kind != 0o100000 && kind != 0o040000 {
            return Err(InstallError::Archive);
        }
        let dir = path.ends_with('/');
        if dir != (kind == 0o040000) && unix == 3 && kind != 0 {
            return Err(InstallError::Archive);
        }
        if dir {
            if size != 0 || compressed != 0 {
                return Err(InstallError::Archive);
            }
        } else {
            if size > MAX_FILE {
                return Err(InstallError::Archive);
            }
            unpacked = unpacked.saturating_add(size);
            if unpacked > MAX_UNPACKED {
                return Err(InstallError::Archive);
            }
            files = files.saturating_add(1);
        }
        if offset >= central_at {
            return Err(InstallError::Archive);
        }
        if !names.insert(path.to_lowercase()) {
            return Err(InstallError::Archive);
        }
        members.push(Member { name: name.to_string(), path, crc, compressed, size, dir });
        at = name_at.saturating_add(name_len).saturating_add(extra_len).saturating_add(comment_len);
        if at > central.len() {
            return Err(InstallError::Archive);
        }
    }
    // Every byte of the directory accounted for, by exactly the number of records it announced.
    if at != central.len() {
        return Err(InstallError::Archive);
    }
    Ok(Scan { members, files, unpacked })
}

/// The path of a member, as it may be written under the install directory: under
/// `LanguageTool-6.6/`, and nothing that means anything but a name to the filesystem it lands on.
/// The returned string has no trailing `/` unless the member is a directory.
pub(crate) fn safe_path(name: &str) -> Result<String, InstallError> {
    if name.len() > MAX_PATH || name.is_empty() {
        return Err(InstallError::Archive);
    }
    if name.bytes().any(|b| b < 0x20 || b == 0x7f) || name.contains('\\') || name.starts_with('/') {
        return Err(InstallError::Archive);
    }
    let (body, dir) = match name.strip_suffix('/') {
        Some(body) => (body, true),
        None => (name, false),
    };
    if body.is_empty() || body.ends_with('/') {
        return Err(InstallError::Archive);
    }
    let mut parts = body.split('/');
    if parts.next() != Some(TOP) {
        return Err(InstallError::Archive);
    }
    let rest: Vec<&str> = parts.collect();
    // The archive's own top directory is an entry of its own, and may be.
    if rest.is_empty() {
        return if dir { Ok(format!("{body}/")) } else { Err(InstallError::Archive) };
    }
    if rest.len() > MAX_DEPTH {
        return Err(InstallError::Archive);
    }
    for part in &rest {
        check_part(part)?;
    }
    Ok(if dir { format!("{body}/") } else { body.to_string() })
}

/// One component of a member's path, as it must be to be written: a plain name, no `..`, no `.`,
/// no drive letter or stream, no character Windows refuses, no trailing dot or space, and not one
/// of the device names Windows keeps for itself.
pub(crate) fn check_part(part: &str) -> Result<(), InstallError> {
    if part.is_empty() || part == "." || part == ".." || part.len() > 255 {
        return Err(InstallError::Archive);
    }
    if part.bytes().any(|b| matches!(b, b'<' | b'>' | b':' | b'"' | b'|' | b'?' | b'*')) {
        return Err(InstallError::Archive);
    }
    if part.ends_with('.') || part.ends_with(' ') || part.starts_with(' ') {
        return Err(InstallError::Archive);
    }
    let stem = part.split('.').next().unwrap_or(part);
    let upper = stem.to_ascii_uppercase();
    const DEVICES: [&str; 23] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5",
        "LPT6", "LPT7", "LPT8", "LPT9", "CONIN$",
    ];
    if DEVICES.contains(&upper.as_str()) || upper == "CONOUT$" {
        return Err(InstallError::Archive);
    }
    Ok(())
}

/// Unpack every member of `scan` under `staging`, checking each against its own header as it is
/// written, and give the canonical manifest of what was written.
pub(crate) fn extract(
    archive: &Path,
    scan: &Scan,
    staging: &Path,
    cancel: &AtomicBool,
    progress: &dyn Fn(Progress),
) -> Result<Vec<u8>, InstallError> {
    let file = File::open(archive).map_err(|_| InstallError::Archive)?;
    let mut zip = ZipArchive::new(BufReader::with_capacity(CHUNK, file)).map_err(|_| InstallError::Archive)?;
    if zip.len() != scan.members.len() {
        return Err(InstallError::Archive);
    }
    // The local headers must be the directory's, and no local header may be one it doesn't name:
    // what is unpacked is what was checked above.
    zip.validate_header_consistency().map_err(|_| InstallError::Archive)?;
    let mut made: HashSet<PathBuf> = HashSet::new();
    let mut manifest: Vec<(String, u64, [u8; 32])> = Vec::with_capacity(usize::try_from(scan.files).unwrap_or(0));
    let mut done = 0u64;
    (progress)(Progress::Extract { files: 0, total: scan.files });
    for (index, member) in scan.members.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err(InstallError::Cancelled);
        }
        let mut entry = zip.by_index(index).map_err(|_| InstallError::Archive)?;
        if entry.name().map_err(|_| InstallError::Archive)?.as_ref() != member.name
            || entry.size() != member.size
            || entry.compressed_size() != member.compressed
            || entry.crc32() != member.crc
            || entry.encrypted()
            || !matches!(entry.compression(), zip::CompressionMethod::Stored | zip::CompressionMethod::Deflated)
        {
            return Err(InstallError::Archive);
        }
        let rel = member.path.trim_end_matches('/');
        if member.dir {
            ensure_dir(staging, Path::new(rel), &mut made)?;
            continue;
        }
        let parent = Path::new(rel).parent().unwrap_or(Path::new(""));
        ensure_dir(staging, parent, &mut made)?;
        let at = staging.join(rel);
        let written = write_member(&mut entry, &at, member, cancel)?;
        manifest.push((member.path.clone(), written.0, written.1));
        done = done.saturating_add(1);
        if done.is_multiple_of(REPORT_FILES) {
            (progress)(Progress::Extract { files: done, total: scan.files });
        }
    }
    if done != scan.files {
        return Err(InstallError::Archive);
    }
    (progress)(Progress::Extract { files: done, total: scan.files });
    manifest.sort_unstable_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
    Ok(manifest_of(&manifest))
}

/// One member into a file that must not exist yet: counted, hashed and CRC-checked as it comes,
/// and refused the moment it writes a byte more than it said it would. `create_new` means the
/// final path is opened only if nothing at all is there — a link included.
fn write_member(entry: &mut ZipFile<'_, BufReader<File>>, at: &Path, member: &Member, cancel: &AtomicBool) -> Result<(u64, [u8; 32]), InstallError> {
    let mut out = OpenOptions::new().write(true).create_new(true).open(at).map_err(|e| match e.kind() {
        ErrorKind::AlreadyExists => InstallError::Archive,
        _ => InstallError::Io,
    })?;
    let mut hasher = Digest::new(&SHA256_ALGORITHM);
    let mut table = crc_table();
    let mut crc = 0u32;
    let mut written = 0u64;
    let mut buf = vec![0u8; CHUNK];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(InstallError::Cancelled);
        }
        let got = match entry.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return Err(InstallError::Archive),
        };
        let piece = buf.get(..got).ok_or(InstallError::Archive)?;
        written = written.saturating_add(u64::try_from(piece.len()).unwrap_or(u64::MAX));
        if written > member.size {
            return Err(InstallError::Archive);
        }
        crc = crc32(&table, crc, piece);
        hasher.update(piece);
        out.write_all(piece).map_err(|_| InstallError::Io)?;
    }
    let _ = &mut table;
    if written != member.size || crc != member.crc {
        return Err(InstallError::Archive);
    }
    out.sync_all().map_err(|_| InstallError::Io)?;
    Ok((written, <[u8; 32]>::try_from(hasher.finish().as_ref()).map_err(|_| InstallError::Integrity)?))
}

/// A directory of the unpacked tree, made component by component so that no part of the way is a
/// link: everything but writing the files happens under directories this run made itself.
fn ensure_dir(staging: &Path, rel: &Path, made: &mut HashSet<PathBuf>) -> Result<(), InstallError> {
    if rel.as_os_str().is_empty() || made.contains(rel) {
        return Ok(());
    }
    let mut so_far = staging.to_path_buf();
    for part in rel.components() {
        let Component::Normal(name) = part else { return Err(InstallError::Archive) };
        so_far.push(name);
        match fs::create_dir(&so_far) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                let meta = fs::symlink_metadata(&so_far).map_err(|_| InstallError::Io)?;
                if !meta.is_dir() || meta.file_type().is_symlink() {
                    return Err(InstallError::Archive);
                }
            }
            Err(_) => return Err(InstallError::Io),
        }
    }
    made.insert(rel.to_path_buf());
    Ok(())
}

/// The canonical manifest of the files that were written: its first line, then one line a file,
/// sorted by path, every line its SHA-256 and its length. Nothing else is in it, and every byte of
/// it is what [`MANIFEST_SHA256`] is the SHA-256 of.
pub(crate) fn manifest_of(files: &[(String, u64, [u8; 32])]) -> Vec<u8> {
    let mut out = Vec::with_capacity(files.len().saturating_mul(140).saturating_add(64));
    out.extend_from_slice(MANIFEST_HEADER.as_bytes());
    out.push(b'\n');
    for (path, size, digest) in files {
        out.extend_from_slice(fingerprint(digest).as_bytes());
        out.extend_from_slice(b"  ");
        out.extend_from_slice(size.to_string().as_bytes());
        out.extend_from_slice(b"  ");
        out.extend_from_slice(path.as_bytes());
        out.push(b'\n');
    }
    out
}

/// A manifest read back strictly: the header, then one line a file, each a 64-digit SHA-256, a
/// length, and a path that is one this module would write. Anything else is no manifest.
pub(crate) fn read_manifest(bytes: &[u8]) -> Result<Vec<(String, u64, [u8; 32])>, InstallError> {
    let text = std::str::from_utf8(bytes).map_err(|_| InstallError::Integrity)?;
    let mut lines = text.split('\n');
    if lines.next() != Some(MANIFEST_HEADER) {
        return Err(InstallError::Integrity);
    }
    let mut out: Vec<(String, u64, [u8; 32])> = Vec::new();
    let mut previous: Option<String> = None;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(3, "  ");
        let (Some(hex), Some(size), Some(path)) = (parts.next(), parts.next(), parts.next()) else {
            return Err(InstallError::Integrity);
        };
        let digest = digest_of(hex).ok_or(InstallError::Integrity)?;
        let size = size.parse::<u64>().map_err(|_| InstallError::Integrity)?;
        if safe_path(path).map_err(|_| InstallError::Integrity)?.trim_end_matches('/') != path || path.ends_with('/') {
            return Err(InstallError::Integrity);
        }
        if previous.as_deref().is_some_and(|last| last.as_bytes() >= path.as_bytes()) {
            return Err(InstallError::Integrity);
        }
        previous = Some(path.to_string());
        out.push((path.to_string(), size, digest));
    }
    if out.is_empty() || out.len() > MAX_ENTRIES {
        return Err(InstallError::Integrity);
    }
    Ok(out)
}

/// 64 lowercase hex digits as 32 bytes; anything else is `None`.
fn digest_of(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return None;
    }
    let mut out = [0u8; 32];
    for (index, byte) in out.iter_mut().enumerate() {
        let at = index.checked_mul(2)?;
        *byte = u8::from_str_radix(hex.get(at..at.checked_add(2)?)?, 16).ok()?;
    }
    Some(out)
}

/// The SHA-256 of bytes in memory.
pub(crate) fn sha256_of(bytes: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    let digest = ring::digest::digest(&SHA256_ALGORITHM, bytes);
    out.copy_from_slice(digest.as_ref());
    out
}

/// Read exactly `into.len()` bytes at `at`.
fn read_at(file: &mut File, at: u64, into: &mut [u8]) -> Result<(), InstallError> {
    use std::io::Seek;
    file.seek(io::SeekFrom::Start(at)).map_err(|_| InstallError::Archive)?;
    file.read_exact(into).map_err(|_| InstallError::Archive)
}

fn u16_of(bytes: &[u8], at: usize) -> Result<u16, InstallError> {
    let end = at.checked_add(2).ok_or(InstallError::Archive)?;
    let two = bytes.get(at..end).ok_or(InstallError::Archive)?;
    Ok(u16::from_le_bytes(<[u8; 2]>::try_from(two).map_err(|_| InstallError::Archive)?))
}

fn u32_of(bytes: &[u8], at: usize) -> Result<u32, InstallError> {
    let end = at.checked_add(4).ok_or(InstallError::Archive)?;
    let four = bytes.get(at..end).ok_or(InstallError::Archive)?;
    Ok(u32::from_le_bytes(<[u8; 4]>::try_from(four).map_err(|_| InstallError::Archive)?))
}

/// CRC-32 (IEEE, as ZIP writes it): the table a byte is looked up in, made once a run.
pub(crate) fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    for (index, slot) in table.iter_mut().enumerate() {
        let mut value = u32::try_from(index).unwrap_or(0);
        for _ in 0..8 {
            value = if value & 1 == 1 { 0xEDB8_8320 ^ (value >> 1) } else { value >> 1 };
        }
        *slot = value;
    }
    table
}

pub(crate) fn crc32(table: &[u32; 256], mut crc: u32, bytes: &[u8]) -> u32 {
    crc = !crc;
    for byte in bytes {
        let index = usize::try_from((crc ^ u32::from(*byte)) & 0xff).unwrap_or(0);
        crc = table.get(index).copied().unwrap_or(0) ^ (crc >> 8);
    }
    !crc
}
