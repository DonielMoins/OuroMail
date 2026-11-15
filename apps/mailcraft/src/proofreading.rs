// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! The host's half of the Options connection test (#954): the window hands a text and a
//! LanguageTool server's address here (`mailcraft_ui_egui::proof::Request`), and this asks that
//! server on a thread of its own and sends back the places it found — or why there are none —
//! through a channel the window looks at each frame. The window never waits on it, and never sees
//! a socket.
//!
//! The check is the real multilingual adapter, `mailcraft_net::languagetool::check_multilingual`:
//! every language the owner enabled (`Request::languages`) is asked for as a variant and believed
//! in an answer, and every rule they silenced (`Request::ignored`) is left out. `Request::language`
//! is the single-language fallback of a host that can ask for one language only, and is **not**
//! used here: an empty `languages` list is refused rather than turned into consent to check the
//! one language beside it.
//!
//! The adapter reads the server's typed answer, keeps only the parts that fit the text, drops
//! spelling matches (those belong to the dictionaries), and says when it looked at part of the text
//! only. That last part matters here because this interface has no field for it: a partial look
//! comes back with the places it did find and a static warning in `Reply::error`, never as a clean
//! answer, and never with the draft's words in it.
//!
//! Nothing here tries again, logs or keeps anything: the text is asked for, its places are read,
//! and both are forgotten. A thread that cannot be started answers at once saying so, and a server
//! that is not on this computer is refused before a connection is made
//! (`mailcraft_net::languagetool`).

use std::sync::mpsc::{Receiver, channel};

use mailcraft_net::languagetool;
use mailcraft_ui_egui::proof::{Reply, Request};

/// What is said when only part of the text was looked at. It claims no answer the server did not
/// give: a text too long to send and a batch that stopped early are both covered.
const INCOMPLETE: &str = "Only part of the text was checked; the result is incomplete.";

/// Everything the window needs to ask a grammar server: the service it installs in
/// [`mailcraft_ui_egui::Services::proof_check`].
pub fn service() -> Box<dyn Fn(Request) -> Receiver<Reply>> {
    Box::new(|request: Request| {
        let (tx, rx) = channel();
        // A thread of its own for each request. This is the Options connection test, asked for by
        // a button and over in the time the server takes; the window asks at most one at a time and
        // never waits on it.
        let fallback = tx.clone();
        let made = std::thread::Builder::new().name("mailcraft-proof".into()).spawn(move || {
            let _ = tx.send(answer(&request));
        });
        if let Err(e) = made {
            let _ = fallback.send(Reply {
                text: String::new(),
                diagnostics: Vec::new(),
                error: Some(format!("grammar checking could not be started: {e}")),
            });
        }
        rx
    })
}

/// Asks the server `request` names, and reads its answer: what the window would have got, without
/// the thread.
pub fn answer(request: &Request) -> Reply {
    let answer = languagetool::check_multilingual(&request.url, &request.text, &request.languages, &request.ignored, languagetool::PATIENCE);
    match answer {
        Ok(outcome) => Reply {
            text: request.text.clone(),
            diagnostics: outcome.diagnostics,
            // A partial look keeps the places it did find, and is still said not to be a whole
            // one: `Reply` has no field for that, and a silent `error: None` would report it clean.
            error: outcome.partial.then(|| INCOMPLETE.to_string()),
        },
        // The adapter's own words are static and hold nothing of the text, the address or a
        // server's body, so they are safe to show as they are.
        Err(e) => Reply { text: request.text.clone(), diagnostics: Vec::new(), error: Some(e.to_string()) },
    }
}
