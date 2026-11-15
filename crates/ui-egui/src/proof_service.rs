// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Background proofreading service contract. Requests own their snapshots, and cancellation is
//! not a clean result. Implementations enqueue bounded work; they never perform I/O in a call
//! from the window. Replies are data only, never instructions or permission to edit a document.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Receiver;

use mailcraft_model::proof::{Diagnostic, Language, ProofSettings};

/// A request's cancellation flag, shared with the editor that owns it.
pub type Cancellation = Arc<AtomicBool>;

/// Fixed error kinds only. No server body, draft, address, or operating-system error is carried.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    Unavailable,
    Busy,
    Cancelled,
    Refused,
    TooSlow,
    TooLong,
    Failed,
    Garbled,
}

#[derive(Clone)]
pub struct SpellingRequest {
    pub text: String,
    pub settings: ProofSettings,
    pub cancelled: Cancellation,
}

#[derive(Clone, Default)]
pub struct SpellingReply {
    /// Local spelling only, without eagerly generated suggestions.
    pub diagnostics: Vec<Diagnostic>,
    /// Dictionary availability/fallback information for the owner; never rendered by Debug.
    pub note: Option<String>,
    pub error: Option<Failure>,
}

#[derive(Clone)]
pub struct GrammarRequest {
    pub url: String,
    pub text: String,
    pub languages: Vec<Language>,
    pub ignored_rules: Vec<String>,
    pub cancelled: Cancellation,
}

#[derive(Clone, Default)]
pub struct GrammarReply {
    /// Server grammar/style only. Local spelling remains authoritative.
    pub diagnostics: Vec<Diagnostic>,
    pub checked_segments: usize,
    pub skipped_segments: usize,
    pub partial: bool,
    pub error: Option<Failure>,
}

#[derive(Clone)]
pub struct SuggestionRequest {
    pub word: String,
    pub settings: ProofSettings,
    pub limit: usize,
    pub cancelled: Cancellation,
}

#[derive(Clone, Default)]
pub struct SuggestionReply {
    pub suggestions: Vec<String>,
    pub error: Option<Failure>,
}

impl fmt::Debug for SpellingRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpellingRequest").field("bytes", &self.text.len()).finish_non_exhaustive()
    }
}

impl fmt::Debug for GrammarRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GrammarRequest")
            .field("bytes", &self.text.len())
            .field("languages", &self.languages.len())
            .field("ignored_rules", &self.ignored_rules.len())
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for SuggestionRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SuggestionRequest").field("bytes", &self.word.len()).field("limit", &self.limit).finish_non_exhaustive()
    }
}

impl fmt::Debug for SpellingReply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpellingReply")
            .field("diagnostics", &self.diagnostics.len())
            .field("has_note", &self.note.is_some())
            .field("error", &self.error)
            .finish()
    }
}

impl fmt::Debug for GrammarReply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GrammarReply")
            .field("diagnostics", &self.diagnostics.len())
            .field("checked_segments", &self.checked_segments)
            .field("skipped_segments", &self.skipped_segments)
            .field("partial", &self.partial)
            .field("error", &self.error)
            .finish()
    }
}

impl fmt::Debug for SuggestionReply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SuggestionReply").field("suggestions", &self.suggestions.len()).field("error", &self.error).finish()
    }
}

/// The host owns worker threads and dictionary caches. There is at most one active operation and
/// one newest pending operation per provider; spelling suggestions share the spelling worker.
/// Dropping an obsolete request sets its cancellation flag in the editor. Implementations check
/// it before starting and before sending, and fast spelling also checks it between tokens.
pub trait ProofService: Send + Sync {
    fn spelling(&self, request: SpellingRequest) -> Receiver<SpellingReply>;
    fn grammar(&self, request: GrammarRequest) -> Receiver<GrammarReply>;
    fn suggest(&self, request: SuggestionRequest) -> Receiver<SuggestionReply>;
}
