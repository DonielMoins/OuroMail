// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Installing the LanguageTool server, against archives made here and served from this machine:
//! what a good one comes to, and every way a bad one is refused.
//!
//! Two things are stood in for, and nothing else: the server the archive comes from (a socket on
//! loopback, in [`Server`]) and the archive itself (built by [`archive`], entry by entry, so that
//! one thing at a time can be wrong with it). The archive's own digest is taken from the bytes
//! that are served, so a test about what is *in* an archive never trips the digest check first —
//! and the check that does come first is the subject of its own test. Nothing here reaches
//! languagetool.org, and nothing here needs Java.
//!
//! The one test that is about the real thing, [`the_official_archive_installs_verifies_and_locates`],
//! runs only when `MAILCRAFT_LT_ARCHIVE` names the official ZIP on this machine; it is the test the
//! pinned manifest digest was worked out with, and it says so when it is skipped.

use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use proptest::prelude::*;

use crate::languagetool_install::*;

/// A counter that makes every scratch directory a test's own.
static NEXT: AtomicU64 = AtomicU64::new(0);

/// A directory of this test's own, emptied first.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mailcraft-lt-{name}-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

// ---------------------------------------------------------------- archives, made here

/// One member of a test's archive: the bytes that go into the file, and what its two headers say
/// about them. Everything but `name` and `body` has a sane default, so a test says only what is
/// wrong with it.
#[derive(Clone)]
struct Spec {
    name: String,
    /// The bytes written into the archive (the compressed ones, for a deflated member).
    body: Vec<u8>,
    method: u16,
    flags: u16,
    crc: u32,
    /// What the headers say the member unpacks to.
    size: u32,
    /// What the headers say the member takes in the archive.
    compressed: u32,
    /// The `st_mode` half of the external attributes (a Unix mode).
    mode: u32,
}

fn crc(data: &[u8]) -> u32 {
    crc32(&crc_table(), 0, data)
}

impl Spec {
    fn file(name: &str, data: &[u8]) -> Spec {
        Spec {
            name: name.to_string(),
            body: data.to_vec(),
            method: 0,
            flags: 0,
            crc: crc(data),
            size: u32::try_from(data.len()).unwrap_or(u32::MAX),
            compressed: u32::try_from(data.len()).unwrap_or(u32::MAX),
            mode: 0o100_644,
        }
    }

    fn deflated(name: &str, data: &[u8]) -> Spec {
        let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(data).expect("deflate");
        let packed = encoder.finish().expect("deflate");
        Spec {
            name: name.to_string(),
            compressed: u32::try_from(packed.len()).unwrap_or(u32::MAX),
            body: packed,
            method: 8,
            flags: 0,
            crc: crc(data),
            size: u32::try_from(data.len()).unwrap_or(u32::MAX),
            mode: 0o100_644,
        }
    }

    fn dir(name: &str) -> Spec {
        Spec { name: name.to_string(), body: Vec::new(), method: 0, flags: 0, crc: 0, size: 0, compressed: 0, mode: 0o040_755 }
    }

    /// A member that is a symbolic link, as a Unix-made archive can hold one.
    fn symlink(name: &str, target: &str) -> Spec {
        let mut spec = Spec::file(name, target.as_bytes());
        spec.mode = 0o120_777;
        spec
    }

    /// The same member, with something else said about it.
    fn saying(mut self, size: Option<u32>, crc: Option<u32>) -> Spec {
        if let Some(size) = size {
            self.size = size;
        }
        if let Some(crc) = crc {
            self.crc = crc;
        }
        self
    }
}

/// A ZIP of `specs`, in order, with stored or deflated members: local headers, the central
/// directory, and the end record — written the way the format says, so that a reader that is not
/// this module's agrees about it.
fn archive(specs: &[Spec]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut offsets: Vec<u32> = Vec::with_capacity(specs.len());
    for spec in specs {
        offsets.push(u32::try_from(out.len()).unwrap_or(u32::MAX));
        let name_len = u16::try_from(spec.name.len()).unwrap_or(u16::MAX);
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&spec.flags.to_le_bytes());
        out.extend_from_slice(&spec.method.to_le_bytes());
        out.extend_from_slice(&0x6000u16.to_le_bytes());
        out.extend_from_slice(&0x5821u16.to_le_bytes());
        out.extend_from_slice(&spec.crc.to_le_bytes());
        out.extend_from_slice(&spec.compressed.to_le_bytes());
        out.extend_from_slice(&spec.size.to_le_bytes());
        out.extend_from_slice(&name_len.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(spec.name.as_bytes().get(..usize::from(name_len)).unwrap_or(&[]));
        out.extend_from_slice(&spec.body);
    }
    let central_at = u32::try_from(out.len()).unwrap_or(u32::MAX);
    for (spec, offset) in specs.iter().zip(&offsets) {
        let name_len = u16::try_from(spec.name.len()).unwrap_or(u16::MAX);
        out.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        out.extend_from_slice(&0x031eu16.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&spec.flags.to_le_bytes());
        out.extend_from_slice(&spec.method.to_le_bytes());
        out.extend_from_slice(&0x6000u16.to_le_bytes());
        out.extend_from_slice(&0x5821u16.to_le_bytes());
        out.extend_from_slice(&spec.crc.to_le_bytes());
        out.extend_from_slice(&spec.compressed.to_le_bytes());
        out.extend_from_slice(&spec.size.to_le_bytes());
        out.extend_from_slice(&name_len.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(spec.mode << 16).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        out.extend_from_slice(spec.name.as_bytes().get(..usize::from(name_len)).unwrap_or(&[]));
    }
    let central_size = u32::try_from(out.len()).unwrap_or(0).saturating_sub(central_at);
    out.extend_from_slice(&[b'P', b'K', 0x05, 0x06]);
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&u16::try_from(specs.len()).unwrap_or(u16::MAX).to_le_bytes());
    out.extend_from_slice(&u16::try_from(specs.len()).unwrap_or(u16::MAX).to_le_bytes());
    out.extend_from_slice(&central_size.to_le_bytes());
    out.extend_from_slice(&central_at.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

/// A field of the end-of-central-directory record, which is always the last 22 bytes.
fn eocd_field(archive: &mut [u8], at: usize, value: &[u8]) {
    let start = archive.len() - 22 + at;
    archive.get_mut(start..start + value.len()).expect("inside the record").copy_from_slice(value);
}

/// The archive every good test installs: the two licenses the release keeps, its server, one
/// library, one rule file, and the directories above them.
fn sample() -> Vec<u8> {
    archive(&[
        Spec::dir("LanguageTool-6.6/"),
        Spec::dir("LanguageTool-6.6/lib/"),
        Spec::file("LanguageTool-6.6/COPYING.txt", b"LanguageTool is licensed under the LGPL 2.1.\n"),
        Spec::file("LanguageTool-6.6/third-party-licenses/README.txt", b"and so are these\n"),
        Spec::file("LanguageTool-6.6/languagetool-server.jar", b"the server, as bytes"),
        Spec::deflated("LanguageTool-6.6/lib/languagetool-core.jar", &b"a library, deflated".repeat(200)),
        Spec::file("LanguageTool-6.6/org/languagetool/rules/en/rules.xml", b"<rules/>"),
    ])
}

// ---------------------------------------------------------------- a server on this machine

/// What the stand-in server answers with.
#[derive(Clone)]
enum Reply {
    /// The body whole, with its length.
    Whole,
    /// The body whole, and the answer cut off after this many bytes.
    Cut(usize),
    /// The first `n` bytes of the body, with a length that says so.
    Short(usize),
    /// The body, saying it is one byte longer than it is.
    LongBy(u32),
    /// No length at all, then the body and more: an answer that runs past the file.
    Unbounded,
    /// A redirect to this address (`{port}` filled in).
    Redirect(String),
    /// Not there.
    Missing,
    /// Nothing until the gate is opened, then the body whole.
    Gated(Arc<AtomicBool>),
}

/// A stand-in server for the archive: one request at a time on loopback, answering as `reply`
/// says, keeping what it was asked.
struct Server {
    port: u16,
    asked: Arc<Mutex<Vec<String>>>,
    halt: Arc<AtomicBool>,
}

impl Server {
    fn start(body: Vec<u8>, reply: Reply) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        let port = listener.local_addr().expect("an address").port();
        let asked = Arc::new(Mutex::new(Vec::new()));
        let halt = Arc::new(AtomicBool::new(false));
        let (kept, stop) = (asked.clone(), halt.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                let Ok(mut stream) = stream else { continue };
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") && head.len() < 8 * 1024 {
                    match stream.read(&mut byte) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => head.push(byte[0]),
                    }
                }
                if let Ok(mut asked) = kept.lock() {
                    asked.push(String::from_utf8_lossy(&head).into_owned());
                }
                let answer = answer(&body, &reply, port);
                let _ = stream.write_all(&answer);
                let _ = stream.flush();
            }
        });
        Server { port, asked, halt }
    }

    /// How many requests came.
    fn asked(&self) -> usize {
        self.asked.lock().map(|asked| asked.len()).unwrap_or(0)
    }

    fn request(&self, n: usize) -> String {
        self.asked.lock().map(|asked| asked.get(n).cloned().unwrap_or_default()).unwrap_or_default()
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.halt.store(true, Ordering::SeqCst);
        // Wake the accept, which then sees the flag.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

fn head(status: &str, length: u64) -> Vec<u8> {
    format!("HTTP/1.1 {status}\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n").into_bytes()
}

fn answer(body: &[u8], reply: &Reply, port: u16) -> Vec<u8> {
    match reply {
        Reply::Whole => {
            let mut out = head("200 OK", body.len() as u64);
            out.extend_from_slice(body);
            out
        }
        Reply::Cut(cut) => {
            let mut out = head("200 OK", body.len() as u64);
            out.extend_from_slice(body.get(..*cut).unwrap_or(body));
            out
        }
        Reply::Short(count) => {
            let mut out = head("200 OK", body.len() as u64);
            out.extend_from_slice(body.get(..*count).unwrap_or(body));
            out
        }
        Reply::LongBy(more) => {
            let mut out = head("200 OK", body.len() as u64 + u64::from(*more));
            out.extend_from_slice(body);
            out
        }
        Reply::Unbounded => {
            let mut out = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec();
            out.extend_from_slice(body);
            out.extend_from_slice(&[0u8; 4096]);
            out
        }
        Reply::Redirect(to) => {
            format!("HTTP/1.1 302 Found\r\nLocation: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", to.replace("{port}", &port.to_string()))
                .into_bytes()
        }
        Reply::Missing => head("404 Not Found", 0),
        Reply::Gated(gate) => {
            while !gate.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
            answer(body, &Reply::Whole, port)
        }
    }
}

/// A server that sends a file from the disk as it asks, 256 KB at a time, without reading it into
/// memory: for the official archive, which is a quarter of a gigabyte.
struct FileServer {
    port: u16,
    asked: Arc<Mutex<usize>>,
    halt: Arc<AtomicBool>,
}

impl FileServer {
    fn start(at: PathBuf) -> FileServer {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        let port = listener.local_addr().expect("an address").port();
        let asked = Arc::new(Mutex::new(0usize));
        let halt = Arc::new(AtomicBool::new(false));
        let (count, stop) = (asked.clone(), halt.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                let Ok(mut stream) = stream else { continue };
                let mut request = Vec::new();
                let mut byte = [0u8; 1];
                while !request.ends_with(b"\r\n\r\n") && request.len() < 8 * 1024 {
                    match stream.read(&mut byte) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => request.push(byte[0]),
                    }
                }
                if let Ok(mut count) = count.lock() {
                    *count += 1;
                }
                let Ok(file) = fs::File::open(&at) else { continue };
                let len = file.metadata().map(|m| m.len()).unwrap_or(0);
                if stream.write_all(&head("200 OK", len)).is_err() {
                    continue;
                }
                let mut reader = std::io::BufReader::with_capacity(256 * 1024, file);
                let mut buf = vec![0u8; 256 * 1024];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if stream.write_all(buf.get(..n).unwrap_or(&[])).is_err() {
                                break;
                            }
                        }
                    }
                }
                let _ = stream.flush();
            }
        });
        FileServer { port, asked, halt }
    }

    fn asked(&self) -> usize {
        self.asked.lock().map(|count| *count).unwrap_or(0)
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}/LanguageTool-6.6.zip", self.port)
    }
}

impl Drop for FileServer {
    fn drop(&mut self) {
        self.halt.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

// ---------------------------------------------------------------- the sources a test stands in

/// A `Source` for the archive `bytes`, served by `server`: the digest is the one of exactly those
/// bytes, so the digest check passes and what the test is about is what comes next.
fn stand_in(bytes: &[u8], server: &Server) -> Source {
    source_of(server.url("/LanguageTool-6.6.zip"), bytes.len() as u64, sha256_of(bytes))
}

fn source_of(url: String, bytes: u64, sha256: [u8; 32]) -> Source {
    Source { url, host: "127.0.0.1".to_string(), secure: false, version: VERSION.to_string(), bytes, sha256, pins: None }
}

/// What the install reports as it goes.
#[derive(Default)]
struct Diary {
    seen: Mutex<Vec<Progress>>,
}

impl Diary {
    fn report(&self, progress: Progress) {
        if let Ok(mut seen) = self.seen.lock() {
            seen.push(progress);
        }
    }

    fn all(&self) -> Vec<Progress> {
        self.seen.lock().map(|seen| seen.clone()).unwrap_or_default()
    }
}

fn not_cancelled() -> AtomicBool {
    AtomicBool::new(false)
}

/// Everything in `base/languagetool/` but the lock, which is a file and stays.
fn left_behind(base: &Path) -> Vec<String> {
    let root = base.join("languagetool");
    let Ok(entries) = fs::read_dir(&root) else { return Vec::new() };
    let mut names: Vec<String> = entries.flatten().filter_map(|entry| entry.file_name().into_string().ok()).filter(|name| name != ".lock").collect();
    names.sort();
    names
}

/// A link, where a test needs one in the way. Only Unix makes one without a privilege.
#[cfg(unix)]
fn link(at: &Path, to: &Path) {
    std::os::unix::fs::symlink(to, at).expect("a symbolic link");
}

/// Whether anything anywhere under `at` is called `name`.
fn holds(at: &Path, name: &str) -> bool {
    let Ok(entries) = fs::read_dir(at) else { return false };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy() == name {
            return true;
        }
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) && holds(&entry.path(), name) {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------- the pins themselves

#[test]
fn the_pins_are_the_ones_this_build_carries() {
    assert_eq!(VERSION, "6.6");
    assert_eq!(URL, "https://languagetool.org/download/LanguageTool-6.6.zip");
    assert_eq!(DOWNLOAD_BYTES, 251_998_221);
    assert_eq!(fingerprint(&SHA256), "53600506b399bb5ffe1e4c8dec794fd378212f14aaf38ccef9b6f89314d11631");
    // A manifest digest nobody wrote by hand, and not a place-holder.
    assert_ne!(MANIFEST_SHA256, [0u8; 32]);
    assert_ne!(MANIFEST_SHA256, [0xffu8; 32]);
    // The production source is the pinned one, and nothing else: `https`, that host, that port,
    // that length, that digest, and both counts and the manifest digest pinned with it.
    let official = Source::official();
    assert_eq!(official.url, URL);
    assert_eq!(official.host, "languagetool.org");
    assert!(official.secure);
    assert_eq!(official.bytes, DOWNLOAD_BYTES);
    assert_eq!(official.sha256, SHA256);
    assert_eq!(official.pins, Some(Pins { files: FILES, unpacked: UNPACKED_BYTES, manifest: MANIFEST_SHA256 }));
    // The address is `https` on the usual port, with no user name, no query, no fragment.
    let parsed = crate::http::Url::parse_within(URL, crate::http::MAX_LONG_URL).unwrap();
    assert!(parsed.secure && parsed.port == 0 && parsed.host == "languagetool.org");
    assert_eq!(parsed.path, "/download/LanguageTool-6.6.zip");
    assert!(!parsed.path.contains(['?', '#', '@']));
    // And the address rule takes that one and nothing else.
    assert!(allowed(&parsed, &official));
    for no in [
        "http://languagetool.org/download/LanguageTool-6.6.zip",
        "https://languagetool.org:8443/download/LanguageTool-6.6.zip",
        "https://www.languagetool.org/download/LanguageTool-6.6.zip",
        "https://languagetool.org.evil.example/LanguageTool-6.6.zip",
        "https://127.0.0.1/LanguageTool-6.6.zip",
    ] {
        let url = crate::http::Url::parse_within(no, crate::http::MAX_LONG_URL);
        assert!(url.map(|url| !allowed(&url, &official)).unwrap_or(true), "{no} was allowed");
    }
}

#[test]
fn crc32_is_the_one_the_format_says() {
    // The check value every CRC-32 (IEEE) implementation answers with.
    assert_eq!(crc(b"123456789"), 0xCBF4_3926);
    assert_eq!(crc(b""), 0);
}

// ---------------------------------------------------------------- the good path

#[test]
fn a_verified_archive_is_installed_and_located() {
    let bytes = sample();
    let server = Server::start(bytes.clone(), Reply::Whole);
    let base = scratch("good");
    let diary = Diary::default();
    let installed = install_from(&base, &stand_in(&bytes, &server), &not_cancelled(), &|p| diary.report(p)).expect("an install");

    assert_eq!(installed.version, "6.6");
    assert_eq!(installed.directory, base.join("languagetool").join("6.6").join("LanguageTool-6.6"));
    // The tree is the archive's, file for file, with the licenses the release keeps.
    assert_eq!(fs::read(installed.directory.join("languagetool-server.jar")).unwrap(), b"the server, as bytes");
    assert_eq!(fs::read(installed.directory.join("lib/languagetool-core.jar")).unwrap(), b"a library, deflated".repeat(200));
    assert!(installed.directory.join("COPYING.txt").is_file());
    assert!(installed.directory.join("third-party-licenses/README.txt").is_file());
    assert!(installed.directory.join("org/languagetool/rules/en/rules.xml").is_file());
    // The manifest is beside the tree, in the canonical format, and names every file and no
    // directory.
    let manifest = fs::read(base.join("languagetool/6.6/manifest.txt")).unwrap();
    let files = read_manifest(&manifest).unwrap();
    assert_eq!(files.len(), 5);
    let names: Vec<&str> = files.iter().map(|(path, _, _)| path.as_str()).collect();
    assert_eq!(
        names,
        [
            "LanguageTool-6.6/COPYING.txt",
            "LanguageTool-6.6/languagetool-server.jar",
            "LanguageTool-6.6/lib/languagetool-core.jar",
            "LanguageTool-6.6/org/languagetool/rules/en/rules.xml",
            "LanguageTool-6.6/third-party-licenses/README.txt",
        ]
    );
    assert!(manifest.starts_with(b"mailcraft-languagetool-manifest/1\n"));
    // Nothing else is left: the archive is gone, and so is the staging directory.
    assert_eq!(left_behind(&base), ["6.6"]);
    // The report went up, from the first byte to the last, and then through the files.
    let seen = diary.all();
    assert!(seen.len() >= 3, "{seen:?}");
    assert_eq!(seen.first(), Some(&Progress::Download { written: 0, total: bytes.len() as u64 }));
    assert_eq!(seen.last(), Some(&Progress::Extract { files: 5, total: 5 }), "{seen:?}");
    assert!(seen.contains(&Progress::Download { written: bytes.len() as u64, total: bytes.len() as u64 }), "{seen:?}");
    // The request said nothing of the owner, and asked for the file and nothing else.
    let request = server.request(0);
    let head: Vec<&str> = request.split("\r\n").take_while(|line| !line.is_empty()).collect();
    assert_eq!(head[0], "GET /LanguageTool-6.6.zip HTTP/1.1");
    for line in &head[1..] {
        assert!(
            line.starts_with("Host: ")
                || line == &"User-Agent: MailCraft"
                || line == &"Accept: application/octet-stream"
                || line == &"Connection: close",
            "{line}"
        );
    }
    // `locate` finds it, whole, by the manifest digest it was made with — and refuses it under
    // any other, which is what a build with another pin would say.
    assert_eq!(locate_with(&base, VERSION, Some(pin_of(&base))).unwrap(), Some(installed.clone()));
    assert_eq!(locate_with(&base, VERSION, None).unwrap(), Some(installed.clone()));
    assert_eq!(locate_with(&base, VERSION, Some([0x42; 32])), Err(InstallError::Integrity));
    // Asking for an install again when it is there downloads nothing.
    let before = server.asked();
    let again = install_from(&base, &stand_in(&bytes, &server), &not_cancelled(), &|_| {}).expect("the install that is there");
    assert_eq!(again.version, "6.6");
    assert_eq!(server.asked(), before);
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn an_install_needs_every_file_the_archive_declared() {
    // A member the headers say is longer than what is in the archive: an archive can't be
    // unpacked further than it says, so it is refused rather than written short.
    let bytes = archive(&[Spec::dir("LanguageTool-6.6/"), Spec::file("LanguageTool-6.6/a", b"short").saying(Some(100), None)]);
    let server = Server::start(bytes.clone(), Reply::Whole);
    let base = scratch("short-member");
    assert_eq!(install_from(&base, &stand_in(&bytes, &server), &not_cancelled(), &|_| {}), Err(InstallError::Archive));
    assert!(left_behind(&base).is_empty());
    assert!(locate(&base).unwrap().is_none());
    let _ = fs::remove_dir_all(&base);
}

// ---------------------------------------------------------------- the digest, before anything else

#[test]
fn a_body_that_is_not_the_pinned_one_is_refused_before_it_is_read_as_a_zip() {
    // The bytes that come are a perfectly good archive; the digest this build pins is another's.
    let good = sample();
    let other = archive(&[Spec::dir("LanguageTool-6.6/"), Spec::file("LanguageTool-6.6/a", b"b")]);
    let server = Server::start(good.clone(), Reply::Whole);
    let base = scratch("wrong-digest");
    let source = source_of(server.url("/x.zip"), good.len() as u64, sha256_of(&other));
    assert_eq!(install_from(&base, &source, &not_cancelled(), &|_| {}), Err(InstallError::Integrity));
    // Nothing of it is left, and nothing of it became an install.
    assert!(left_behind(&base).is_empty());
    assert!(locate(&base).unwrap().is_none());
    // Not one file was unpacked: the staging directory never had a tree in it, and is gone.
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn the_digest_is_checked_before_the_archive_is() {
    // The bytes are not an archive at all, and the digest says they are the right ones: what is
    // refused is the archive, never a byte written from it. (Were the order the other way, this
    // would be `Integrity`.)
    let bytes = b"this is not a zip, it only says it is".to_vec();
    let server = Server::start(bytes.clone(), Reply::Whole);
    let base = scratch("not-a-zip");
    assert_eq!(install_from(&base, &stand_in(&bytes, &server), &not_cancelled(), &|_| {}), Err(InstallError::Archive));
    assert!(left_behind(&base).is_empty());
    let _ = fs::remove_dir_all(&base);
}

// ---------------------------------------------------------------- the download's own bounds

#[test]
fn a_body_longer_than_the_file_is_refused() {
    let bytes = sample();
    let server = Server::start(bytes.clone(), Reply::Unbounded);
    let base = scratch("longer");
    assert_eq!(install_from(&base, &stand_in(&bytes, &server), &not_cancelled(), &|_| {}), Err(InstallError::Download));
    assert!(left_behind(&base).is_empty());
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn a_body_that_says_it_is_another_length_is_refused() {
    let bytes = sample();
    for reply in [Reply::LongBy(1), Reply::Short(bytes.len() / 2), Reply::Cut(bytes.len() / 2)] {
        let server = Server::start(bytes.clone(), reply);
        let base = scratch("length");
        assert_eq!(install_from(&base, &stand_in(&bytes, &server), &not_cancelled(), &|_| {}), Err(InstallError::Download));
        assert!(left_behind(&base).is_empty());
        let _ = fs::remove_dir_all(&base);
    }
    // Nothing there at all.
    let server = Server::start(bytes.clone(), Reply::Missing);
    let base = scratch("missing");
    assert_eq!(install_from(&base, &stand_in(&bytes, &server), &not_cancelled(), &|_| {}), Err(InstallError::Download));
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn a_redirect_is_followed_nowhere_but_the_archive_host() {
    let bytes = sample();
    for elsewhere in ["https://evil.example/x.zip", "http://127.0.0.2/x.zip", "https://127.0.0.1/x.zip", ""] {
        let server = Server::start(bytes.clone(), Reply::Redirect(elsewhere.to_string()));
        let base = scratch("redirect");
        assert_eq!(install_from(&base, &stand_in(&bytes, &server), &not_cancelled(), &|_| {}), Err(InstallError::Download), "{elsewhere}");
        assert!(left_behind(&base).is_empty());
        let _ = fs::remove_dir_all(&base);
    }
    // Round and round.
    let server = Server::start(bytes.clone(), Reply::Redirect("http://127.0.0.1:{port}/again".to_string()));
    let base = scratch("loop");
    let source = stand_in(&bytes, &server);
    assert_eq!(install_from(&base, &source, &not_cancelled(), &|_| {}), Err(InstallError::Download));
    assert!(server.asked() > 1 && server.asked() <= 8, "{}", server.asked());
    let _ = fs::remove_dir_all(&base);
}

// ---------------------------------------------------------------- cancelling

#[test]
fn a_cancelled_install_writes_nothing_anywhere() {
    let bytes = sample();
    // Called off before it began: not even the folder is made.
    let base = scratch("cancel-first");
    let cancel = AtomicBool::new(true);
    assert_eq!(install_from(&base, &Source::official(), &cancel, &|_| {}), Err(InstallError::Cancelled));
    assert!(!base.join("languagetool").exists());
    assert!(locate(&base).unwrap().is_none());
    // Called off while the body is coming: the first report of it turns the flag.
    let server = Server::start(bytes.clone(), Reply::Whole);
    let base = scratch("cancel-coming");
    let cancel = AtomicBool::new(false);
    let got = install_from(&base, &stand_in(&bytes, &server), &cancel, &|progress| {
        // The report that comes with the first bytes on disk turns the flag, so the body is
        // stopped where it is rather than before it began.
        if matches!(progress, Progress::Download { written, .. } if written > 0) {
            cancel.store(true, Ordering::SeqCst);
        }
    });
    assert_eq!(got, Err(InstallError::Cancelled));
    assert!(left_behind(&base).is_empty());
    assert!(locate(&base).unwrap().is_none());
    // Called off while the answer is still on its way: the first byte written turns it away.
    let gate = Arc::new(AtomicBool::new(false));
    let server = Server::start(bytes.clone(), Reply::Gated(gate.clone()));
    let base = scratch("cancel-waiting");
    let cancel = Arc::new(AtomicBool::new(false));
    let waiting = {
        let (base, cancel) = (base.clone(), cancel.clone());
        let source = stand_in(&bytes, &server);
        std::thread::spawn(move || install_from(&base, &source, &cancel, &|_| {}))
    };
    while server.asked() == 0 {
        std::thread::sleep(Duration::from_millis(5));
    }
    cancel.store(true, Ordering::SeqCst);
    gate.store(true, Ordering::SeqCst);
    assert_eq!(waiting.join().expect("the thread"), Err(InstallError::Cancelled));
    assert!(left_behind(&base).is_empty());
    let _ = fs::remove_dir_all(&base);
    // Nothing was written beside the folder, either.
    for name in ["evil", "6.6.zip"] {
        assert!(!std::env::temp_dir().join(name).exists(), "{name}");
    }
}

// ---------------------------------------------------------------- what is there already

#[cfg(unix)]
#[test]
fn an_install_that_is_there_is_never_overwritten() {
    let bytes = sample();
    let server = Server::start(bytes.clone(), Reply::Whole);
    let base = scratch("occupied");
    let at = base.join("languagetool").join("6.6");
    fs::create_dir_all(at.join("LanguageTool-6.6")).unwrap();
    fs::write(at.join("LanguageTool-6.6/notes of the owner.txt"), b"do not lose me").unwrap();
    assert_eq!(install_from(&base, &stand_in(&bytes, &server), &not_cancelled(), &|_| {}), Err(InstallError::Integrity));
    assert_eq!(fs::read(at.join("LanguageTool-6.6/notes of the owner.txt")).unwrap(), b"do not lose me");
    assert_eq!(server.asked(), 0, "it downloaded before it looked");
    // A link where the install belongs is not the install, and is not followed.
    let base = scratch("linked");
    let elsewhere = scratch("linked-target");
    fs::write(elsewhere.join("keep"), b"keep").unwrap();
    fs::create_dir_all(base.join("languagetool")).unwrap();
    link(&base.join("languagetool").join("6.6"), &elsewhere);
    assert_eq!(install_from(&base, &stand_in(&bytes, &server), &not_cancelled(), &|_| {}), Err(InstallError::Integrity));
    assert_eq!(fs::read(elsewhere.join("keep")).unwrap(), b"keep");
    assert!(!elsewhere.join("LanguageTool-6.6").exists());
    assert_eq!(locate(&base), Err(InstallError::Integrity));
    let _ = fs::remove_dir_all(&base);
    let _ = fs::remove_dir_all(&elsewhere);
}

#[cfg(unix)]
#[test]
fn a_staging_directory_a_killed_run_left_is_cleared() {
    let bytes = sample();
    let server = Server::start(bytes.clone(), Reply::Whole);
    let base = scratch("stale");
    let root = base.join("languagetool");
    fs::create_dir_all(root.join(".staging-4242")).unwrap();
    fs::write(root.join(".staging-4242/half a tree"), b"left behind").unwrap();
    fs::write(root.join(".download-4242"), b"half an archive").unwrap();
    // A link under one of those names is not ours to delete, and is left where it is.
    let elsewhere = scratch("stale-target");
    link(&root.join(".staging-9999"), &elsewhere);
    assert!(install_from(&base, &stand_in(&bytes, &server), &not_cancelled(), &|_| {}).is_ok());
    assert!(!root.join(".staging-4242").exists());
    assert!(!root.join(".download-4242").exists());
    assert!(fs::symlink_metadata(root.join(".staging-9999")).unwrap().file_type().is_symlink());
    assert_eq!(left_behind(&base), [".staging-9999", "6.6"]);
    let _ = fs::remove_dir_all(&base);
    let _ = fs::remove_dir_all(&elsewhere);
}

#[test]
fn a_second_install_is_told_busy() {
    let bytes = sample();
    let gate = Arc::new(AtomicBool::new(false));
    let server = Server::start(bytes.clone(), Reply::Gated(gate.clone()));
    let base = scratch("busy");
    let first = {
        let (base, source) = (base.clone(), stand_in(&bytes, &server));
        std::thread::spawn(move || install_from(&base, &source, &not_cancelled(), &|_| {}))
    };
    while server.asked() == 0 {
        std::thread::sleep(Duration::from_millis(5));
    }
    // While the first is still taking the body, the second is turned away at the door.
    assert_eq!(install_from(&base, &stand_in(&bytes, &server), &not_cancelled(), &|_| {}), Err(InstallError::Busy));
    gate.store(true, Ordering::SeqCst);
    assert!(first.join().expect("the thread").is_ok());
    // And once it is done, the lock is let go, and the install that is there is simply found.
    assert_eq!(install_from(&base, &stand_in(&bytes, &server), &not_cancelled(), &|_| {}).map(|found| found.version), Ok("6.6".to_string()));
    let _ = fs::remove_dir_all(&base);
}

// ---------------------------------------------------------------- a changed install

/// The manifest digest of what is installed under `base`: the pin that install was verified with.
fn pin_of(base: &Path) -> [u8; 32] {
    sha256_of(&fs::read(base.join("languagetool").join(VERSION).join("manifest.txt")).expect("the manifest"))
}

#[cfg(unix)]
#[test]
fn a_changed_file_or_a_changed_manifest_is_refused() {
    let bytes = sample();
    let server = Server::start(bytes.clone(), Reply::Whole);
    let install = |name: &str| {
        let base = scratch(name);
        let installed = install_from(&base, &stand_in(&bytes, &server), &not_cancelled(), &|_| {}).expect("an install");
        let pin = pin_of(&base);
        // What was just installed is whole, by its own manifest's digest.
        assert_eq!(locate_with(&base, VERSION, Some(pin)), Ok(Some(installed.clone())), "{name}");
        (base, installed, pin)
    };

    // One byte of one library, changed.
    let (base, installed, pin) = install("changed-lib");
    let library = installed.directory.join("lib/languagetool-core.jar");
    let mut changed = fs::read(&library).unwrap();
    if let Some(last) = changed.last_mut() {
        *last ^= 0x01;
    }
    fs::write(&library, &changed).unwrap();
    assert_eq!(locate_with(&base, VERSION, Some(pin)), Err(InstallError::Integrity));
    let _ = fs::remove_dir_all(&base);

    // A file deleted, and a file added.
    let (base, installed, pin) = install("deleted");
    fs::remove_file(installed.directory.join("org/languagetool/rules/en/rules.xml")).unwrap();
    assert_eq!(locate_with(&base, VERSION, Some(pin)), Err(InstallError::Integrity));
    let _ = fs::remove_dir_all(&base);
    let (base, installed, pin) = install("added");
    fs::write(installed.directory.join("lib/something-else.jar"), b"not in the archive").unwrap();
    assert_eq!(locate_with(&base, VERSION, Some(pin)), Err(InstallError::Integrity));
    let _ = fs::remove_dir_all(&base);

    // A file replaced by a link to the same bytes: still not the file.
    let (base, installed, pin) = install("linked-file");
    let jar = installed.directory.join("languagetool-server.jar");
    let elsewhere = scratch("linked-file-target");
    fs::write(elsewhere.join("jar"), fs::read(&jar).unwrap()).unwrap();
    fs::remove_file(&jar).unwrap();
    link(&jar, &elsewhere.join("jar"));
    assert_eq!(locate_with(&base, VERSION, Some(pin)), Err(InstallError::Integrity));
    let _ = fs::remove_dir_all(&base);
    let _ = fs::remove_dir_all(&elsewhere);

    // A changed library *and* a manifest rewritten to match it: the manifest's own digest is
    // pinned, so metadata cannot authorise a changed binary.
    let (base, installed, pin) = install("rewritten-manifest");
    let library = installed.directory.join("lib/languagetool-core.jar");
    fs::write(&library, b"a different library entirely").unwrap();
    let manifest_at = base.join("languagetool").join(VERSION).join("manifest.txt");
    let old = fs::read_to_string(&manifest_at).unwrap();
    let rewritten: String = old
        .lines()
        .map(|line| match line.split_once("  LanguageTool-6.6/lib/languagetool-core.jar") {
            Some(_) => format!("{}  28  LanguageTool-6.6/lib/languagetool-core.jar", fingerprint(&sha256_of(b"a different library entirely"))),
            None => line.to_string(),
        })
        .collect::<Vec<String>>()
        .join("\n")
        + "\n";
    fs::write(&manifest_at, rewritten).unwrap();
    // The rewritten manifest is self-consistent, and is still not the one this install was made
    // with.
    assert_ne!(pin_of(&base), pin);
    assert_eq!(locate_with(&base, VERSION, Some(pin)), Err(InstallError::Integrity));
    let _ = fs::remove_dir_all(&base);

    // A manifest that is a few bytes short of the one that was written.
    let (base, installed, pin) = install("truncated-manifest");
    assert!(installed.directory.is_dir());
    let manifest_at = base.join("languagetool").join(VERSION).join("manifest.txt");
    let mut short = fs::read(&manifest_at).unwrap();
    short.truncate(short.len() - 3);
    fs::write(&manifest_at, &short).unwrap();
    assert_eq!(locate_with(&base, VERSION, Some(pin)), Err(InstallError::Integrity));
    let _ = fs::remove_dir_all(&base);
}

// ---------------------------------------------------------------- what an archive may not be

/// Every archive a test offers is refused as one that may not be unpacked, and nothing of it is
/// written anywhere.
fn refused(name: &str, specs: &[Spec]) {
    let bytes = archive(specs);
    let server = Server::start(bytes.clone(), Reply::Whole);
    let base = scratch(name);
    assert_eq!(install_from(&base, &stand_in(&bytes, &server), &not_cancelled(), &|_| {}), Err(InstallError::Archive), "{name}");
    assert!(left_behind(&base).is_empty(), "{name}: {:?}", left_behind(&base));
    assert!(locate(&base).unwrap().is_none(), "{name}");
    // A name that tried to leave the folder left nothing behind anywhere.
    for outside in ["evil", "evil.txt", "passwd"] {
        assert!(!std::env::temp_dir().join(outside).exists(), "{name} wrote {outside}");
        assert!(!holds(&base, outside), "{name} wrote {outside}");
    }
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn a_name_that_is_not_a_plain_name_is_refused() {
    for name in [
        "LanguageTool-6.6/../evil",
        "LanguageTool-6.6/../../evil.txt",
        "/LanguageTool-6.6/evil",
        "LanguageTool-6.6\\evil",
        "LanguageTool-6.6/C:/evil",
        "LanguageTool-6.6/C:evil",
        "//server/share/evil",
        "LanguageTool-6.6/evil.",
        "LanguageTool-6.6/evil ",
        "LanguageTool-6.6/ evil",
        "LanguageTool-6.6/CON",
        "LanguageTool-6.6/CON.txt",
        "LanguageTool-6.6/aux",
        "LanguageTool-6.6/LPT9.log",
        "LanguageTool-6.6/a:b",
        "LanguageTool-6.6/a\u{1}b",
        "LanguageTool-6.6/a/b\u{0}",
        "LanguageTool-6.5/evil",
        "evil.jar",
        "LanguageTool-6.6",
        "",
    ] {
        refused("bad-name", &[Spec::dir("LanguageTool-6.6/"), Spec::file(name, b"x")]);
    }
    // A path longer than a path may be, and one deeper than a tree may be.
    let long = format!("LanguageTool-6.6/{}", "a".repeat(1_100));
    refused("long-name", &[Spec::dir("LanguageTool-6.6/"), Spec::file(&long, b"x")]);
    let mut deep = String::from("LanguageTool-6.6");
    for _ in 0..40 {
        deep.push_str("/a");
    }
    refused("deep-name", &[Spec::dir("LanguageTool-6.6/"), Spec::file(&deep, b"x")]);
}

#[test]
fn a_member_that_is_not_a_file_or_a_directory_is_refused() {
    // A symbolic link, and an extraction that would go through one.
    refused(
        "symlink",
        &[
            Spec::dir("LanguageTool-6.6/"),
            Spec::symlink("LanguageTool-6.6/a", "/etc/passwd"),
            Spec::file("LanguageTool-6.6/a/b", b"through the link"),
        ],
    );
    // A directory entry that is a link all the same.
    refused("symlink-dir", &[Spec::dir("LanguageTool-6.6/"), Spec::symlink("LanguageTool-6.6/lib", "/tmp")]);
    // A FIFO, and a device.
    for mode in [0o010_644u32, 0o020_644, 0o060_644] {
        let mut spec = Spec::file("LanguageTool-6.6/a", b"x");
        spec.mode = mode;
        refused("not-regular", &[Spec::dir("LanguageTool-6.6/"), spec]);
    }
    // The same name twice, and the same name in another case.
    refused("duplicate", &[Spec::dir("LanguageTool-6.6/"), Spec::file("LanguageTool-6.6/a", b"x"), Spec::file("LanguageTool-6.6/a", b"y")]);
    refused("duplicate-case", &[Spec::dir("LanguageTool-6.6/"), Spec::file("LanguageTool-6.6/a", b"x"), Spec::file("LanguageTool-6.6/A", b"y")]);
}

#[test]
fn a_member_compressed_another_way_or_encrypted_or_wrong_is_refused() {
    // bzip2, LZMA, zstd, and a method nobody has numbered.
    for method in [12u16, 14, 93, 99, 7] {
        let mut spec = Spec::file("LanguageTool-6.6/a", b"x");
        spec.method = method;
        refused("method", &[Spec::dir("LanguageTool-6.6/"), spec]);
    }
    // Encrypted (bit 0), strong encryption (bit 6), a masked header (bit 13).
    for flags in [0x0001u16, 0x0041, 0x2001] {
        let mut spec = Spec::file("LanguageTool-6.6/a", b"x");
        spec.flags = flags;
        refused("encrypted", &[Spec::dir("LanguageTool-6.6/"), spec]);
    }
    // A CRC that is not the bytes'.
    refused("crc", &[Spec::dir("LanguageTool-6.6/"), Spec::file("LanguageTool-6.6/a", b"x").saying(None, Some(0xDEAD_BEEF))]);
    // A member that says it unpacks to more than a member may.
    let mut huge = Spec::file("LanguageTool-6.6/a", b"x");
    huge.size = 64 * 1024 * 1024 + 1;
    huge.compressed = 1;
    refused("huge", &[Spec::dir("LanguageTool-6.6/"), huge]);
    // An archive that unpacks to more than an archive may, between members that each may.
    let many: Vec<Spec> = (0..20)
        .map(|n| {
            let mut spec = Spec::file(&format!("LanguageTool-6.6/lib/{n}.jar"), b"");
            spec.size = 33 * 1024 * 1024;
            spec.compressed = 0;
            spec
        })
        .collect();
    let mut all = vec![Spec::dir("LanguageTool-6.6/")];
    all.extend(many);
    refused("too-much", &all);
}

#[test]
fn an_archive_that_is_not_one_is_refused() {
    refused("empty", &[]);
    // Nothing under the one directory it must be.
    refused("elsewhere", &[Spec::file("other/thing.jar", b"x")]);
    // A directory of the top's own that says it has bytes in it.
    let mut odd = Spec::dir("LanguageTool-6.6/");
    odd.size = 10;
    refused("dir-with-bytes", &[odd]);
    // A file where the one directory is.
    refused("file-as-top", &[Spec::file("LanguageTool-6.6", b"x")]);
}

#[test]
fn a_zip64_or_multi_disk_or_ragged_archive_is_refused() {
    let one = [Spec::dir("LanguageTool-6.6/"), Spec::file("LanguageTool-6.6/a", b"x")];
    let patch = |name: &str, at: usize, value: &[u8]| {
        let mut bytes = archive(&one);
        eocd_field(&mut bytes, at, value);
        refused_with_bytes(name, &bytes);
    };
    // The sentinels that say "see the ZIP64 record".
    patch("zip64-entries", 10, &0xFFFFu16.to_le_bytes());
    patch("zip64-central", 12, &0xFFFFFFFFu32.to_le_bytes());
    patch("zip64-offset", 16, &0xFFFFFFFFu32.to_le_bytes());
    // Two disks, and a count that doesn't match.
    patch("disk", 4, &1u16.to_le_bytes());
    patch("central-disk", 6, &1u16.to_le_bytes());
    patch("count", 8, &1u16.to_le_bytes());
    // A central directory that ends before the record does.
    patch("central-size", 12, &(100u32).to_le_bytes());
    // A comment that isn't there.
    patch("comment", 20, &(4u16).to_le_bytes());
    // Bytes after the record.
    let mut bytes = archive(&one);
    bytes.extend_from_slice(b"and something after it");
    refused_with_bytes("trailing", &bytes);
    // The ZIP64 locator, right where it would be.
    let mut bytes = archive(&one);
    let at = bytes.len() - 22;
    let mut locator = vec![b'P', b'K', 0x06, 0x07];
    locator.extend_from_slice(&[0u8; 16]);
    bytes.splice(at..at, locator);
    refused_with_bytes("zip64-locator", &bytes);
    // Not a ZIP at all, and one cut off in the middle of its directory.
    refused_with_bytes("garbage", b"PK\x05\x06 and then nothing that is one".as_slice());
    let mut bytes = archive(&one);
    bytes.truncate(bytes.len() - 10);
    refused_with_bytes("cut", &bytes);
}

/// The same refusals, for an archive that was built and then changed by hand.
fn refused_with_bytes(name: &str, bytes: &[u8]) {
    let server = Server::start(bytes.to_vec(), Reply::Whole);
    let base = scratch(name);
    assert_eq!(install_from(&base, &stand_in(bytes, &server), &not_cancelled(), &|_| {}), Err(InstallError::Archive), "{name}");
    assert!(left_behind(&base).is_empty(), "{name}");
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn too_many_entries_is_refused_before_the_directory_is_read() {
    let mut specs = vec![Spec::dir("LanguageTool-6.6/")];
    for n in 0..16_050 {
        specs.push(Spec::file(&format!("LanguageTool-6.6/lib/{n}"), b""));
    }
    // The count is written as 16,050 — more than an archive may have.
    let bytes = archive(&specs);
    refused_with_bytes("many-entries", &bytes);
}

// ---------------------------------------------------------------- the manifest, read strictly

#[test]
fn the_manifest_is_the_canonical_lines_that_were_said() {
    let files =
        vec![("LanguageTool-6.6/a.jar".to_string(), 2u64, sha256_of(b"ab")), ("LanguageTool-6.6/lib/b.jar".to_string(), 0u64, sha256_of(b""))];
    let manifest = manifest_of(&files);
    let text = String::from_utf8(manifest.clone()).unwrap();
    assert_eq!(
        text,
        format!(
            "mailcraft-languagetool-manifest/1\n{}  2  LanguageTool-6.6/a.jar\n{}  0  LanguageTool-6.6/lib/b.jar\n",
            fingerprint(&sha256_of(b"ab")),
            fingerprint(&sha256_of(b""))
        )
    );
    // What is read back is what was written, in the order it is in.
    assert_eq!(read_manifest(&manifest).unwrap(), files);
    // Not a manifest: no header; another header; an upper-case digest; a digest too short; a
    // length that isn't one; a line of two fields; a path out of the tree; an unsorted pair; a
    // trailing slash; a duplicate.
    let hex = fingerprint(&sha256_of(b"ab"));
    for bad in [
        String::new(),
        format!("mailcraft-languagetool-manifest/2\n{hex}  2  LanguageTool-6.6/a.jar\n"),
        format!("mailcraft-languagetool-manifest/1\n{}  2  LanguageTool-6.6/a.jar\n", hex.to_uppercase()),
        format!("mailcraft-languagetool-manifest/1\n{}  2  LanguageTool-6.6/a.jar\n", &hex[..62]),
        format!("mailcraft-languagetool-manifest/1\n{hex}  two  LanguageTool-6.6/a.jar\n"),
        format!("mailcraft-languagetool-manifest/1\n{hex}  LanguageTool-6.6/a.jar\n"),
        format!("mailcraft-languagetool-manifest/1\n{hex}  2  ../a.jar\n"),
        format!("mailcraft-languagetool-manifest/1\n{hex}  2  /etc/a.jar\n"),
        format!("mailcraft-languagetool-manifest/1\n{hex}  2  LanguageTool-6.6/b.jar\n{hex}  2  LanguageTool-6.6/a.jar\n"),
        format!("mailcraft-languagetool-manifest/1\n{hex}  2  LanguageTool-6.6/\n"),
        format!("mailcraft-languagetool-manifest/1\n{hex}  2  LanguageTool-6.6/a.jar\n{hex}  2  LanguageTool-6.6/a.jar\n"),
        "mailcraft-languagetool-manifest/1\n".to_string(),
    ] {
        assert_eq!(read_manifest(bad.as_bytes()), Err(InstallError::Integrity), "{bad}");
    }
    // And bytes that aren't text at all.
    assert_eq!(read_manifest(&[0xff, 0xfe, 0x00]), Err(InstallError::Integrity));
}

#[test]
fn a_manifest_that_is_not_the_pinned_one_is_refused_even_when_it_is_self_consistent() {
    // A stand-in whose manifest is pinned to a digest that isn't the one its archive makes: the
    // install is refused after unpacking, so the pin is the thing that decides.
    let bytes = sample();
    let server = Server::start(bytes.clone(), Reply::Whole);
    let base = scratch("pin-manifest");
    let mut source = stand_in(&bytes, &server);
    source.pins = Some(Pins { files: 5, unpacked: 0, manifest: [0x11; 32] });
    assert_eq!(install_from(&base, &source, &not_cancelled(), &|_| {}), Err(InstallError::Integrity));
    assert!(left_behind(&base).is_empty());
    // The counts are pinned too.
    let mut source = stand_in(&bytes, &server);
    source.pins = Some(Pins { files: 4, unpacked: 0, manifest: MANIFEST_SHA256 });
    assert_eq!(install_from(&base, &source, &not_cancelled(), &|_| {}), Err(InstallError::Integrity));
    assert!(left_behind(&base).is_empty());
    let _ = fs::remove_dir_all(&base);
}

// ---------------------------------------------------------------- reading is not writing

#[test]
fn locate_creates_nothing_and_starts_nothing() {
    // Nothing at all: no folder is made, not even `languagetool/`.
    let base = std::env::temp_dir().join(format!("mailcraft-lt-absent-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    let _ = fs::remove_dir_all(&base);
    assert_eq!(locate(&base), Ok(None));
    assert!(!base.exists());
    // A folder with something else in it is left exactly as it is.
    let base = scratch("locate-elsewhere");
    fs::create_dir_all(base.join("languagetool")).unwrap();
    fs::write(base.join("languagetool/notes.txt"), b"mine").unwrap();
    let before: BTreeSet<String> =
        fs::read_dir(base.join("languagetool")).unwrap().flatten().map(|entry| entry.file_name().into_string().unwrap()).collect();
    assert_eq!(locate(&base), Ok(None));
    let after: BTreeSet<String> =
        fs::read_dir(base.join("languagetool")).unwrap().flatten().map(|entry| entry.file_name().into_string().unwrap()).collect();
    assert_eq!(before, after);
    assert_eq!(fs::read(base.join("languagetool/notes.txt")).unwrap(), b"mine");
    let _ = fs::remove_dir_all(&base);
}

// ---------------------------------------------------------------- what an archive may not be, on paper

proptest! {
    /// Whatever bytes a file holds, they are not an archive this reads, and nothing panics.
    #[test]
    fn arbitrary_bytes_are_never_an_archive(bytes in proptest::collection::vec(any::<u8>(), 0..2_048)) {
        let base = std::env::temp_dir().join(format!("mailcraft-lt-any-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        let _ = fs::create_dir_all(&base);
        let at = base.join("any.zip");
        prop_assume!(fs::write(&at, &bytes).is_ok());
        let source = source_of("http://127.0.0.1:1/x".to_string(), bytes.len() as u64, [0u8; 32]);
        let scanned = prescan(&at, &source);
        prop_assert!(scanned.is_err(), "arbitrary bytes were read as an archive");
        let _ = fs::remove_dir_all(&base);
    }

    /// However a name is written, saying whether it may be a path is a decision, never a panic.
    #[test]
    fn arbitrary_names_are_decided_not_panicked(name in "\\PC{0,300}") {
        let _ = safe_path(&name);
        let _ = check_part(&name);
        // And a manifest line made of one is never taken to be a file outside the tree.
        if let Ok(path) = safe_path(&name) {
            prop_assert!(!path.starts_with('/') && !path.contains("..") && !path.contains('\\'));
        }
    }
}

// ---------------------------------------------------------------- the real archive

/// The official 6.6 archive, whole: installed through the same code the app runs, located, and
/// checked file by file — and then one byte of one library changed, and refused.
///
/// It runs only where `MAILCRAFT_LT_ARCHIVE` names the ZIP (the one this build's pins were worked
/// out from); nothing is downloaded, and no Java is started. `MAILCRAFT_LT_KEEP=1` leaves the
/// install where it is, for a look.
#[test]
fn the_official_archive_installs_verifies_and_locates() {
    let Some(at) = std::env::var_os("MAILCRAFT_LT_ARCHIVE") else {
        eprintln!("skipped: set MAILCRAFT_LT_ARCHIVE to the official LanguageTool-6.6.zip to run this test");
        return;
    };
    let at = PathBuf::from(at);
    let len = fs::metadata(&at).expect("the archive").len();
    assert_eq!(len, DOWNLOAD_BYTES, "the archive is not the pinned one");
    let digest = {
        let mut file = fs::File::open(&at).expect("the archive");
        let mut hasher = ring::digest::Context::new(&ring::digest::SHA256);
        let mut buf = vec![0u8; 1 << 20];
        loop {
            match file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => hasher.update(buf.get(..n).unwrap()),
                Err(e) => panic!("reading the archive: {e}"),
            }
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(hasher.finish().as_ref());
        out
    };
    assert_eq!(digest, SHA256, "the archive is not the pinned one");
    eprintln!("fixture: {} bytes, {}", len, fingerprint(&digest));

    // What its own central directory says, before anything is unpacked from it.
    let source = source_of(format!("http://127.0.0.1:1/{}", "x"), len, digest);
    let scan = prescan(&at, &source).expect("the official archive");
    eprintln!("central directory: {} members, {} files, {} bytes unpacked", scan.members.len(), scan.files, scan.unpacked);
    assert_eq!(scan.files, FILES);
    assert_eq!(scan.unpacked, UNPACKED_BYTES);
    assert_eq!(scan.members.len(), 2_032);

    // The manifest of the archive, derived here and only here: it is what the pinned digest is
    // the digest of, and the install below has to come to exactly the same bytes.
    let staging = scratch("official-manifest");
    let derived = extract(&at, &scan, &staging, &not_cancelled(), &|_| {}).expect("unpacking the official archive");
    eprintln!(
        "manifest derived from the archive: {} bytes, {} files, sha256 {}",
        derived.len(),
        read_manifest(&derived).expect("a manifest").len(),
        fingerprint(&sha256_of(&derived))
    );
    let _ = fs::remove_dir_all(&staging);

    let server = FileServer::start(at.clone());
    let base = scratch("official");
    let mut official = source_of(server.url(), len, digest);
    official.pins = Some(Pins { files: FILES, unpacked: UNPACKED_BYTES, manifest: MANIFEST_SHA256 });

    let started = Instant::now();
    let installed = install_from(&base, &official, &not_cancelled(), &|_| {}).expect("an install of the official archive");
    let installing = started.elapsed();
    assert_eq!(installed.version, VERSION);
    assert_eq!(installed.directory, base.join("languagetool").join(VERSION).join("LanguageTool-6.6"));
    // The server JAR is the one the independent fixture has.
    let jar = installed.directory.join("languagetool-server.jar");
    let jar_digest = {
        let mut out = [0u8; 32];
        out.copy_from_slice(ring::digest::digest(&ring::digest::SHA256, &fs::read(&jar).expect("the jar")).as_ref());
        out
    };
    assert_eq!(fingerprint(&jar_digest), "f5279d946d901c90c0bb09cddaa6fdea8b26db9c548145d09041e8d1ac2d2b45");
    // And the licenses the release keeps are all there.
    for license in [
        "COPYING.txt",
        "third-party-licenses",
        "org/languagetool/resource/de/hunspell/COPYING_GPLv2.txt",
        "org/languagetool/resource/ro/hunspell/COPYING.LGPL",
        "org/languagetool/resource/ar/hunspell/COPYING",
    ] {
        assert!(installed.directory.join(license).exists(), "{license}");
    }
    // The manifest is the pinned one, and it names every file of the archive.
    let manifest_at = base.join("languagetool").join(VERSION).join("manifest.txt");
    let manifest = fs::read(&manifest_at).expect("the manifest");
    eprintln!("manifest: {} bytes, sha256 {}, derived {}", manifest.len(), fingerprint(&sha256_of(&manifest)), fingerprint(&MANIFEST_SHA256));
    assert_eq!(sha256_of(&manifest), MANIFEST_SHA256, "the manifest of the official archive hashes to the pinned constant");
    assert_eq!(read_manifest(&manifest).expect("a manifest").len() as u64, FILES);

    // Nothing of the quarter of a gigabyte is left: the archive is gone, and the staging too.
    assert_eq!(left_behind(&base), [VERSION]);

    let started = Instant::now();
    let found = locate(&base).expect("a located install");
    let locating = started.elapsed();
    assert_eq!(found, Some(installed.clone()));
    eprintln!("installed in {installing:?}, located (every file hashed) in {locating:?}, {} requests", server.asked());

    // One byte of one library, changed: the whole install is refused.
    let library = installed.directory.join("libs/languagetool-core.jar");
    let mut changed = fs::read(&library).expect("a library");
    changed[0] ^= 0x01;
    fs::write(&library, &changed).expect("writing the library back");
    assert_eq!(locate(&base), Err(InstallError::Integrity));
    eprintln!("a changed library refused, as it should be");

    if std::env::var_os("MAILCRAFT_LT_KEEP").is_none() {
        let _ = fs::remove_dir_all(&base);
    } else {
        eprintln!("kept: {}", base.display());
    }
}
