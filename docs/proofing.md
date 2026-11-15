# Spelling & Grammar

> **Status:** current · **Written:** 2026-11-12 · **Last updated:** 2026-11-12 · **Change:** minor

MailCraft checks the message you are writing as you write it, on this computer:
spelling against **every dictionary you turned on at once**, and, when you turn it on, grammar
through a [LanguageTool](https://languagetool.org/) server that runs on this computer.

English and French in one paragraph need no switching. A word is right when *any* dictionary that
is on knows it:

> Bonjour Pierre, I reviewed les documents hier soir and everything looks correct.

`Bonjour`, `les`, `hier` and `soir` are French; `reviewed`, `documents` and `correct` are English
(and `documents` is both). Nothing is flagged for the mixing itself, and nothing is flagged for
being in the other language: the dictionaries are all consulted for every word, always.

## What it looks like

- A word no dictionary knows is underlined in red; a grammar place the server found is underlined
  in blue.
- **Right click** a flagged word for the words that could stand in its place, then *Add to
  Dictionary*, *Ignore Once*, or *Ignore This Rule*.
- **Review › Proofing › Spelling & Grammar** walks through everything found in the message, one
  place at a time, with a field to type a correction of your own.
- A correction is **one step of the message's own undo** (Ctrl+Z / ⌘Z): the text, the formatting
  and the list it was in are as they were a keystroke ago.
- File › Options › Spelling & Grammar turns each part on or off, picks the dictionaries, keeps
  your own words, and names the grammar server it asks on this computer.

The language selector below a message chooses the languages, and so the dictionaries, for that
message alone. Its choice is kept with the local draft; File › Options supplies the defaults for new
messages, and is what the signature editor is checked with. These choices are not Internet mail
headers: nothing of them goes into a message that is sent or a draft saved to a server. Grammar
requests do include the enabled language variants.

Both themes are drawn from the window's own colours (`theme::Tokens`): the spelling line is the
theme's red and the grammar line its blue, never a colour the sender of a message chose.

## Dictionaries

A dictionary is a Hunspell-format `.dic`/`.aff` pair already on your computer. MailCraft reads
a bounded subset of that format with its own Rust reader; it does not embed the full Hunspell
engine or download dictionaries. Dictionary compatibility is therefore not identical to Hunspell.

| System | Install | Where MailCraft looks |
|---|---|---|
| Arch | `sudo pacman -S hunspell-en_ca hunspell-fr` | `/usr/share/hunspell` |
| Debian/Ubuntu | `sudo apt install hunspell-en-ca hunspell-fr` | `/usr/share/hunspell`, `/usr/share/myspell/dicts` |
| Fedora | `sudo dnf install hunspell-en hunspell-fr` | `/usr/share/hunspell` |
| FreeBSD | `pkg install en-hunspell fr-hunspell` | `/usr/local/share/hunspell` |
| macOS | UTF-8 Hunspell-format dictionaries supplied by the owner | `~/Library/Spelling` |
| Windows | LibreOffice's dictionaries | `%APPDATA%\MailCraft\dictionaries` |

Your own folder can be typed or chosen with Browse in File › Options › Spelling & Grammar, then
kept with Save, or named in a script. Merely editing or browsing the field does not save it. The
path is kept exactly, including spaces at either end; an empty path restores the usual places:

```
mailcraft-cli run --sample --cmd proof.set --params '{"dictionaryDir": "/home/me/dicts"}'
```

What is looked for, in order, for each language: `en_CA.dic`, `en-CA.dic`, `en_ca.dic`, `en.dic`,
then the same for the other English variant (a Canadian English dictionary stands in with American
English when there is no Canadian one, and French of France when there is no Canadian French).

**Read of the format:** stems and their flags, `PFX`/`SFX` affixes with their conditions (including
cross-product ones), `NEEDAFFIX`, `FORBIDDENWORD`, `NOSUGGEST`, and the supported flag encodings
(one character, `FLAG UTF-8`, `FLAG long`, `FLAG num`). **Not read:** continuation-affix chains,
compound rules, `ICONV`/`OCONV`, `SET`
(other than UTF-8 files being read as UTF-8), and the format's own suggestion engine — suggestions
are MailCraft's. A line that cannot be read is skipped; a dictionary that cannot be read at all is
reported instead of crashing the window. A French elision is accepted when an enabled dictionary
lists its prefix, apostrophe included (`j'`, `l'`), and the word after it is known. Whole contractions
such as `aujourd'hui` and `don't` are checked first, without splitting them. This does not implement
continuation-affix chains: derived forms needing those rules can still be false positives even
when a full Hunspell engine accepts them.

When no dictionary is found, **nothing is flagged** (rather than every word), and File › Options
says that none was found: a window that underlined a whole message would be lying about it.

### Canadian spellings

Canadian forms such as `colour`, `centre` and `neighbour` depend on the installed dictionary.
Accented words, straight and typographic apostrophes, and canonically equivalent Unicode accents
are compared without changing the draft's original bytes. The built-in test dictionaries are
synthetic; a French-of-France dictionary used as fallback is not a Canadian French dictionary.

### Your own words

*Add to Dictionary* (right click, or the dialog) keeps a word in your settings — never in the
system's dictionary files. Words are compared without regard to case and with either apostrophe
(`aujourd'hui` and `aujourd’hui` are one word), duplicates are refused, and File › Options lists
them with a × to take one out. The list is capped at 10,000 words of 64 characters.

## Grammar

Grammar is LanguageTool's: MailCraft sends the text to `POST /v2/check` on a server on this
computer and reads the answer. The default address is `http://127.0.0.1:8081`.

Running one:

```
# Arch
sudo pacman -S languagetool
languagetool-server --port 8081 --allow-origin

# or, from the standalone LanguageTool directory, with Java installed
java -cp languagetool-server.jar org.languagetool.server.HTTPServer --port 8081
```

Then File › Options › Spelling & Grammar › **Check grammar with a LanguageTool server**, and
*Test the connection* in manual-server mode. MailCraft uses the request and response types from the
third-party `languagetool-rust` (LTRS) 3.0.1 crate, with its own bounded Rust HTTP transport, not
that crate's HTTP client or an LSP server, and a separate LanguageTool Java server. For each bounded
sentence it requests automatic language detection with the enabled regional variants, such as
`en-CA,fr-CA`. Low-confidence or unsupported-language answers are suppressed. Detection confidence
is a heuristic, not proof that a sentence is monolingual;
grammar for code-switching remains deliberately conservative rather than selecting one spelling
language for the whole sentence. Local dictionary-union spelling remains authoritative.

**Only this computer.** An address that is not `http://`/`https://`, or whose host is not this
computer (loopback, by name or by address), is refused before a single byte is sent. The network
layer makes that refusal on its own account, whatever a caller has checked, so no setting, no
command and no typo in the address makes MailCraft contact a remote grammar server. Remote servers
are not supported. The owner must also trust the local service: MailCraft cannot prevent a locally
configured server or proxy from forwarding the text it receives.

Secure (`https://`) grammar addresses must use a literal loopback IP, such as `127.0.0.1` or
`[::1]`. Secure hostnames, including `localhost`, are refused because this transport cannot apply
its resolved-address filter to them. Plain HTTP accepts loopback names only when their resolved
addresses are loopback. Requests use no environment proxy, cookies or credentials, and follow no
redirects.

Nothing is asked while you type: the text rests 250 ms, at most one request is on its way, and
automatic grammar batches for one document are spaced at least 800 ms apart (a manual recheck can
ask sooner). Spelling and grammar use separate bounded background workers, so neither blocks the
window. An answer for a text the message no longer has is thrown away, and so is a place that does
not fit the text it is applied to.

### MailCraft-managed local server

The desktop's Built-in Proofing Server option keeps a managed-mode preference of its own; the manual
server address is preserved when switching modes. Opening Options, changing modes or reading status
does not download anything or start Java.

1. Install Java 17 or newer separately. MailCraft does not install Java.
2. Check installation explicitly to verify an existing managed copy and look for Java.
3. If no copy is installed, Download explicitly obtains the version pinned by this build from
   LanguageTool's official HTTPS archive, checks its SHA-256 pin, extracts it with bounded readers
   and verifies its files before promotion. This build uses LanguageTool 6.6. The pin is
   MailCraft's own digest of the official archive, not an upstream signature or a publisher's
   checksum. The installed tree keeps its licences and notices; the downloaded ZIP is not kept.
4. Start explicitly verifies the installation again, starts MailCraft's own helper and Java child,
   and waits for a successful bind plus bounded synthetic English and French checks before
   reporting Running. Startup checks contain no draft text. A first start can take longer than a
   warm check while the model loads; Running is not a promise of a fixed startup time.
5. Stop revokes the grammar endpoint at once. Stopping stays pending until the owned helper has
   exited; local spelling does not depend on it. Closing the app closes the helper's lifetime
   pipe, so its Java child can be reaped. MailCraft does not adopt or stop a server started
   outside it.

The managed server listens on a port of this computer alone. Its installation and process actions
are the owner's, not consent an agent can give: reading the cached status is one thing, and an agent
may never run Check, Download, Start, Stop or Cancel. Cancel asks for cleanup; it does not report
that cleanup has finished. An installed copy that fails verification is refused and left alone, not
removed or overwritten; this version has no in-app remove or reset for it.

## Privacy

- Spelling is entirely local: no dictionary is fetched, no word is sent anywhere.
- MailCraft sends grammar requests only to the server on this computer, without adding headers,
  recipients or attachments. The server is a separate program; its logging and forwarding
  configuration are outside MailCraft's control. MailCraft's proofing request/reply Debug output
  and transport errors do not contain the draft.
- What is sent is the body's text as plain text (paragraphs joined by line breaks), for that one
  request, with the language's code. Nothing of the headers, the recipients or the attachments.
- A server's answer is treated as hostile: capped at 1 MB, no redirect followed, offsets checked
  against the text sent, a place that does not fit is dropped rather than clamped. LanguageTool's
  offsets are UTF-16 code units and are converted to MailCraft's byte positions once, with tests
  (`crates/model/src/proof/grammar.rs`).

## Commands

Everything here is a command, so the window, a script, the CLI and MCP do the same thing:

| Command | What it does |
|---|---|
| `proof.check` | the spelling places of the body (or a range, or a text) |
| `proof.suggest` | what could stand in for a word |
| `proof.change` | puts a word in place of a place, as one step of the message's undo |
| `proof.addWord` / `proof.removeWord` | the personal dictionary |
| `proof.ignore` / `proof.ignoreRule` / `proof.forgetRules` | one place, or a grammar rule, left quiet |
| `proof.set` / `proof.status` | the settings, and what was read |
| `review.spelling` | starts, or goes on with, the walk through everything found |

```
mailcraft-cli run --sample --cmd proof.status
mailcraft-cli run --sample --cmd proof.addWord --params '{"word": "Hochelaga"}'
mailcraft-cli run --sample --cmd proof.set --params '{"checkGrammar": true}'
```

A command that fails changes nothing: a correction whose place no longer fits the message (because
it was edited meanwhile) is refused, and the message is left as it was.

`proof.check`, `proof.change` and `review.spelling` require an open composer or signature editor.
Check ranges count Unicode characters; returned diagnostic spans are bytes relative to the checked
text. A named `proof.change` must include the complete `expectedText` and the checked `composeId`
(or `signature:true` for the signature editor). Review corrections use the walk's checked snapshot.
Settings mutations do not load dictionaries; explicit `proof.status` and headless spelling checks
may do so synchronously.

## Tests

- `crates/model/src/proof/tests.rs` — a word being right when either language says so, mixed
  sentences, hyphens, addresses left alone, spans that always fit, the personal dictionary.
- `crates/model/src/proof/spell_tests.rs` — the format: affixes and their conditions, the three
  flag kinds, words that may not be suggested, and a property test over arbitrary Unicode.
- `crates/model/src/proof/grammar_tests.rs` — the answer: UTF-16 offsets with accents and emoji,
  places that do not fit, hostile and oversized answers, and a rule id or a replacement too long
  to keep.
- `crates/net/src/languagetool_tests.rs` — `POST /v2/check` against servers of the test's own:
  success, 500, a hang-up, garbage, an oversized answer, a slow server, nothing listening, an
  address on another computer refused.
- `crates/engine/src/cmd/proof_tests.rs` — through the commands: mixed language checking, a
  correction as one step of undo, the look of the corrected word kept, a stale place refused, the
  walk, and the settings.
- `apps/mailcraft/src/proofreading_tests.rs` — the host's half: a place comes back with the right
  bytes, an unreachable server says so and says nothing else.

## What is not here

- **No Language Server Protocol.** An LSP server (LTeX+, for instance) could be a fourth provider,
  and the provider seam is a command (`proof.check`, plus the host's `Services::proof_check`), so
  adding one would not disturb the window. It is not in this change because the HTTP server gives
  the same grammar rules without a second protocol and a second dependency.
- **Not a full Hunspell implementation:** continuation-affix chains, compound-word rules,
  conversion tables, dictionary-specific suggestion algorithms and non-UTF-8 files are not read.
- **No grammar on the browser build**: it has no sockets of its own, so nothing is asked.
- Grammar checks the composing body, not a selection or the whole mailbox. Work is capped: spelling
  checks at most 100,000 characters, and grammar checks at most 32 qualifying sentence segments,
  each no larger than 16 KiB. Very large or unusually punctuated text can therefore be only partly
  checked. A provider failure is not a clean bill of health; editing again or a manual check can
  retry it without idle-frame retry loops.
- The managed runtime and shutdown workflow have been exercised on Linux with synthetic drafts.
  Native runtime acceptance on macOS, Windows and BSD is still outstanding; compiling another
  target is not the same as exercising its Java discovery, child lifetime or native dialogs.
- Conservative language detection is not proof that code-switching is error-free. It can skip an
  uncertain sentence without a diagnostic, or be confidently wrong. A review with no places is
  not, by itself, evidence that every sentence was checked.

## Revision history

| Date | Change | Summary |
|---|---|---|
