// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! The dictionary engine's own tests: the affixes it reads, the words they make, the ones it
//! refuses, and what it suggests in their place.
//!
//! Every dictionary here is invented (`example` words and made-up stems), so that what a test says
//! a word is never changes with a dictionary's version or with what this computer has installed.

use std::cell::Cell;
use std::time::{Duration, Instant};

use super::spell::MAX_ENTRIES;
use super::{Dictionary, Language, MAX_DIAGNOSTICS, MAX_WORD, Speller};

/// A dictionary read from a `.dic` alone, or a panic with the error in it.
fn dic(lang: Language, text: &str) -> Dictionary {
    Dictionary::from_dic(lang, text).expect("a dictionary")
}

/// A dictionary read with its `.aff`.
fn hunspell(aff: &str, text: &str) -> Dictionary {
    Dictionary::from_hunspell(Language::EnCa, aff, text).expect("a dictionary with affixes")
}

/// A speller with just this dictionary on it: what a suggestion is asked of.
fn spell(d: &Dictionary) -> Speller {
    let mut s = Speller::new();
    s.put(d.clone());
    s
}

#[test]
fn a_dictionary_is_read() {
    let d = dic(Language::EnCa, "1\ncolour\n");
    assert!(d.holds("colour"));
    assert!(d.holds("COLOUR"), "case is not what a word is");
    assert!(!d.holds("colourful"));
    assert_eq!(d.len(), 1);
    assert_eq!(d.language(), Language::EnCa);
}

/// A prefix and a suffix that both cross the product make the word both of them make: this is the
/// gap a reader that applies one rule per stem leaves open.
#[test]
fn cross_product_affixes_make_the_paired_word() {
    let aff = "SET UTF-8\nPFX A Y 1\nPFX A 0 re .\nSFX B Y 1\nSFX B 0 s .\n";
    let d = hunspell(aff, "1\nwrite/AB\n");
    assert!(d.holds("write"), "the stem is a word");
    assert!(d.holds("rewrite"), "the prefix's word");
    assert!(d.holds("writes"), "the suffix's word");
    assert!(d.holds("rewrites"), "the word both of them make");
    assert!(!d.holds("rewritess"));
    assert!(!d.holds("rerewrite"));
}

/// A rule that does not cross (`N`) is never joined to the other end, and neither is one whose
/// partner does not, so the cross product cannot be made up by the reader.
#[test]
fn a_rule_that_does_not_cross_is_never_joined() {
    let aff = "PFX A N 1\nPFX A 0 re .\nSFX B Y 1\nSFX B 0 s .\n";
    let d = hunspell(aff, "1\nwrite/AB\n");
    assert!(d.holds("rewrite"));
    assert!(d.holds("writes"));
    assert!(!d.holds("rewrites"), "the prefix says it does not cross");
    let aff = "PFX A Y 1\nPFX A 0 re .\nSFX B N 1\nSFX B 0 s .\n";
    let d = hunspell(aff, "1\nwrite/AB\n");
    assert!(!d.holds("rewrites"), "the suffix says it does not cross");
}

/// `NEEDAFFIX`: a stem listed so that its affixed forms exist is not a word by itself.
#[test]
fn a_needaffix_stem_is_not_a_word_by_itself() {
    let aff = "NEEDAFFIX X\nSFX S Y 1\nSFX S 0 s .\n";
    let d = hunspell(aff, "2\nrun/XS\ntalk/S\n");
    assert!(!d.holds("run"), "the stem is not a word on its own");
    assert!(d.holds("runs"), "the form its suffix makes is");
    assert!(d.holds("talk"), "a stem that is not NEEDAFFIX is a word");
    assert!(d.holds("talks"));
    // And the same through a whole speller: nothing accepts it, so it is flagged.
    let mut s = Speller::new();
    s.put(d);
    assert!(!s.is_word("run"));
    assert!(s.is_word("runs"));
    let text = "run runs";
    let flagged: Vec<&str> = s.check(text).iter().filter_map(|x| text.get(x.span.start..x.span.end)).collect();
    assert_eq!(flagged, ["run"]);
}

/// A word the dictionary forbids is never a word, whichever stem and whichever affix makes it, and
/// it is never suggested either.
#[test]
fn a_forbidden_word_is_never_a_word_nor_a_suggestion() {
    // `foos` is listed as a wrong spelling of `foo`, and the suffix of `foo` would make it too.
    let aff = "FORBIDDENWORD X\nSFX S Y 1\nSFX S 0 s .\n";
    let d = hunspell(aff, "2\nfoo/S\nfoos/X\n");
    assert!(d.holds("foo"));
    assert!(!d.holds("foos"), "the forbidden word is not a word");
    assert!(d.forbids("foos"));
    assert!(!d.has("foos"), "it is not in the word list at all, so nothing can let it through");
    assert!(!d.all().iter().any(|w| w == "foos"));
    // What could stand in its place never includes it.
    let got = spell(&d).suggest("fooz", 8);
    assert!(!got.iter().any(|w| w.eq_ignore_ascii_case("foos")), "{got:?}");
    assert!(got.iter().any(|w| w == "foo"), "{got:?}");
    // The forbidden stem's own affixed forms are wrong too: the flag is the entry's.
    let d = hunspell(aff, "2\nfoo/S\nfoos/X\n");
    assert!(!d.holds("foos"));
    let mut s = Speller::new();
    s.put(d);
    assert!(!s.is_word("foos"));
    assert!(s.is_word("foo"));
}

/// Every way a `.dic` can write its flags is read: one character, two (`long`), a number (`num`)
/// and one character of any script (`UTF-8`).
#[test]
fn every_kind_of_flag_is_read() {
    // One character, the default.
    let aff = "SFX S Y 1\nSFX S 0 s .\n";
    let d = hunspell(aff, "1\ncolour/S\n");
    assert!(d.holds("colours"), "a one-character flag");
    // Two characters.
    let aff = "FLAG long\nSFX AB Y 1\nSFX AB 0 s .\n";
    let d = hunspell(aff, "2\ncolour/AB\nfleur/CD\n");
    assert!(d.holds("colours"), "a two-character flag");
    assert!(!d.holds("fleurs"), "the flags are read two characters at a time, not as one");
    // A flag that is written as another pair of characters is another flag.
    let d = hunspell(aff, "1\nfleur/AC\n");
    assert!(!d.holds("fleurs"), "`AC` is not `AB`");
    // Numbers, with several on one line.
    let aff = "FLAG num\nSFX 101 Y 1\nSFX 101 0 s .\nPFX 102 Y 1\nPFX 102 0 re .\n";
    let d = hunspell(aff, "1\nwrite/101,102\n");
    assert!(d.holds("writes"), "a numbered suffix");
    assert!(d.holds("rewrite"), "a numbered prefix");
    assert!(d.holds("rewrites"), "and both together");
    assert!(!d.holds("101"));
    // UTF-8: a flag written as a letter of the language's own alphabet.
    let aff = "FLAG UTF-8\nSFX é Y 1\nSFX é 0 s .\nPFX ő Y 1\nPFX ő 0 re .\n";
    let d = hunspell(aff, "1\ncafé/éő\n");
    assert!(d.holds("cafés"), "a suffix named by a multi-byte flag");
    assert!(d.holds("recafé"), "a prefix named by another");
    assert!(d.holds("recafés"), "and both together");
    assert!(!d.holds("cafées"));
}

/// An affix may say which flags the word it makes carries, after a `/` (`SFX N aire aît/n'q' plaire`
/// in a French dictionary). Those flags are not part of the word, and a reader that keeps them
/// makes a word nobody could ever write.
#[test]
fn an_affix_that_carries_flags_makes_the_word_without_them() {
    let aff = "FLAG long\nSFX wN Y 2\nSFX wN aire aît/n'q'l' plaire\nSFX wN aire ait/n'q'l' taire\n";
    let d = hunspell(aff, "2\nplaire/wNq.\ntaire/wN\n");
    assert!(d.holds("plaît"), "the affix's word, without the flags it carries");
    assert!(d.holds("tait"), "and the same rule for the stem its condition names");
    assert!(!d.holds("plaît/n'q'l'"), "the flags are not part of it");
    assert!(d.all().iter().all(|w| !w.contains('/')), "no word of a dictionary holds a slash: {:?}", d.all());
    assert!(d.holds("plaire"), "the stem is a word too");
}

/// A hyphenated word the dictionary knows as one word is not taken apart: a French dictionary lists
/// `rendez-vous`, and its halves are not words of it.
#[test]
fn a_hyphenated_word_the_dictionary_knows_is_not_flagged() {
    let mut s = Speller::new();
    s.put(dic(Language::FrCa, "3\nrendez-vous\naujourd'hui\nun\n"));
    assert!(s.is_word("Rendez-vous"));
    let text = "un rendez-vous aujourd'hui.";
    let found = s.check(text);
    assert!(found.is_empty(), "nothing here is a mistake: {found:?}");
    // A misspelt one: this dictionary lists the whole word and not its halves, so the half that is
    // not a word is what is flagged.
    let text = "un rendez-vuos";
    let found = s.check(text);
    let flagged: Vec<&str> = found.iter().filter_map(|d| text.get(d.span.start..d.span.end)).collect();
    assert_eq!(flagged, ["rendez"]);
    // In a dictionary that lists the halves, only the half that is wrong is flagged.
    let mut parts = Speller::new();
    parts.put(dic(Language::FrCa, "3\nrendez\nvous\nun\n"));
    let found = parts.check(text);
    let flagged: Vec<&str> = found.iter().filter_map(|d| text.get(d.span.start..d.span.end)).collect();
    assert_eq!(flagged, ["vuos"]);
}

/// A French elision is a word when the dictionary itself has the prefix: a French dictionary lists
/// `j'`, `l'`, `c'`, `d'` (and `n'`, `s'`, `t'`, `m'`, `qu'`) as entries, so `j'ai` is the prefix
/// `j'` and the word `ai`, with the apostrophe kept. A prefix the dictionary does not have is not
/// an elision: what is not a word is not made one by an apostrophe.
#[test]
fn a_french_elision_is_a_word_when_the_dictionary_has_the_prefix() {
    let mut s = Speller::new();
    s.put(dic(Language::FrCa, "18\nl'\nj'\nc'\nd'\nn'\ns'\nt'\nm'\nqu'\nhomme\nai\nest\naccord\nil\naujourd'hui\nbonjour\nvu\net\n"));
    // The straight apostrophe and the typographic one are one word.
    for word in ["j'ai", "j’ai", "l'homme", "l’homme", "c'est", "c’est", "d'accord", "d’accord", "s'il", "s’il", "n'est", "qu'il"] {
        assert!(s.is_word(word), "{word:?} is a word: its prefix is the dictionary's own");
    }
    let text = "Bonjour, j’ai vu l’homme et c’est d’accord.";
    let flagged: Vec<&str> = s.check(text).iter().filter_map(|d| text.get(d.span.start..d.span.end)).collect();
    assert!(flagged.is_empty(), "none of these is a mistake: {flagged:?}");
    // A whole contraction is one word and is not looked into: `aujourd'hui` is not `aujourd'` + `hui`.
    assert!(s.is_word("aujourd'hui"));
    assert!(s.is_word("aujourd’hui"));
    assert!(s.check("aujourd’hui").is_empty());
    // A prefix the dictionary does not have is not one, even when the rest is; and a bare letter
    // before the apostrophe is not a prefix at all. Discarding the apostrophe would accept both.
    let mut s = Speller::new();
    s.put(dic(Language::FrCa, "6\nz\nai\nhomme\nl'\nest\nc'\n"));
    for word in ["z'ai", "z’homme", "x'est", "lz’homme"] {
        assert!(!s.is_word(word), "{word:?} is not a word: its prefix is not one of the dictionary's");
    }
    // A prefix the dictionary has with a rest it does not: the whole token is the mistake.
    let text = "j'zzz";
    let found = s.check(text);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(text.get(found[0].span.start..found[0].span.end), Some("j'zzz"));
}

/// What is wrong after an elision is only what follows the apostrophe: the correction replaces the
/// word's own end, not its prefix. The range is in the draft's bytes, whatever its Unicode form.
#[test]
fn a_typo_after_a_french_elision_flags_only_the_rest() {
    let mut s = Speller::new();
    s.put(dic(Language::FrCa, "8\nl'\nc'\nj'\nhomme\nest\nécole\naime\naimée\n"));
    let text = "j'aimee";
    let found = s.check(text);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(text.get(found[0].span.start..found[0].span.end), Some("aimee"), "only the rest is flagged");
    assert_eq!(found[0].span.start, "j'".len(), "the span begins after the apostrophe");
    assert!(found[0].suggestions.iter().any(|w| w == "aime" || w == "aimée"), "{:?}", found[0].suggestions);
    assert!(found[0].suggestions.iter().all(|w| !w.starts_with("j'")), "the prefix is not part of it: {:?}", found[0].suggestions);

    // The typographic apostrophe, and a draft whose `é` is written as `e` and a combining accent:
    // the diagnostic is over the draft's own bytes, and the word is still understood.
    let nfd = "l’e\u{301}colle";
    let found = s.check(nfd);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(nfd.get(found[0].span.start..found[0].span.end), Some("e\u{301}colle"));
    assert_eq!(found[0].span.start, "l’".len(), "the range is counted in the draft's bytes");
    assert_eq!(found[0].span.end, nfd.len());
    assert!(found[0].suggestions.iter().any(|w| w == "école"), "{:?}", found[0].suggestions);
    assert!(s.is_word("l'e\u{301}cole"), "a canonically equivalent spelling is the same word");
}

/// An elision is read with every dictionary that is on, and a whole contraction of one language is
/// never taken apart because another language is on too.
#[test]
fn an_elision_is_read_with_every_dictionary_that_is_on() {
    let mut s = Speller::new();
    s.put(dic(Language::FrCa, "4\nl'\nj'\nhomme\nai\n"));
    s.put(dic(Language::EnCa, "7\ndocument\ndon't\nplease\nai\nthe\nlose\nit\n"));
    assert!(s.is_word("l'document"), "the prefix is one language's and the rest another's");
    assert!(s.is_word("j'ai"));
    assert!(s.is_word("don't"), "a whole English contraction is one word, apostrophe and all");
    assert!(!s.is_word("z'document"), "no dictionary has the prefix `z'`");
    let text = "The document, don't lose it: l'document.";
    let flagged: Vec<&str> = s.check(text).iter().filter_map(|d| text.get(d.span.start..d.span.end)).collect();
    assert!(flagged.is_empty(), "{flagged:?}");
}

/// An elision does not widen what a hostile run does: a run too long to be a word is not checked,
/// and many apostrophes do not make a word out of its pieces.
#[test]
fn an_elision_does_not_widen_a_hostile_token() {
    let mut s = Speller::new();
    s.put(dic(Language::FrCa, "4\nl'\nj'\nhomme\nai\n"));
    let long = format!("l'{}", "a".repeat(MAX_WORD * 200));
    assert!(s.check(&long).is_empty(), "a run longer than a word is not checked at all");
    assert!(!s.is_word(&long), "and it is not a word when asked directly");
    for word in ["l'a'i", "l''homme", "'''''", "'ai"] {
        assert!(!s.is_word(word), "{word:?} is not a word: only one prefix is one");
    }
}

/// Conditions are read from the right end of the stem for a suffix, and a set or a negated set is
/// one character of it — which is what keeps `-s` off the words `y → ies` is for.
#[test]
fn conditions_are_read_from_the_end_of_the_stem() {
    let aff = "SFX S Y 3\nSFX S y ies [^aeiou]y\nSFX S 0 s [aeiou]y\nSFX S 0 s [^y]\n";
    let d = hunspell(aff, "3\nhappy/S\nboy/S\ncolour/S\n");
    assert!(d.holds("happies"), "a consonant before the y takes `-ies`");
    assert!(!d.holds("happys"), "and not `-s`");
    assert!(d.holds("boys"), "a vowel before the y takes `-s`");
    assert!(!d.holds("boies"), "and not `-ies`");
    assert!(d.holds("colours"), "a word not ending in y takes `-s`");
    assert!(!d.holds("coloury"));
    // A dot is one character of any kind, and a prefix's condition is read from the start.
    let aff = "PFX P Y 2\nPFX P 0 un .\nPFX P 0 in [^aeiou]\n";
    let d = hunspell(aff, "3\nkind/P\ntrue/P\naudible/P\n");
    assert!(d.holds("unkind"));
    assert!(d.holds("untrue"));
    assert!(d.holds("unaudible"), "a dot holds whatever the stem starts with");
    assert!(d.holds("inkind"), "a negated set at the start of the stem");
    assert!(d.holds("intrue"));
    assert!(!d.holds("inaudible"), "but not where the stem starts with a vowel");
}

/// Accents, ligatures and the typographic apostrophe: a word is the word it is written as, and a
/// missing accent is a different spelling, which is what a correction is for.
#[test]
fn accents_ligatures_and_typographic_apostrophes_are_words() {
    let d = dic(Language::FrCa, "4\ncœur\nœuvre\naujourd’hui\nfrançais\n");
    assert!(d.holds("cœur"));
    assert!(d.holds("CŒUR"));
    assert!(d.holds("œuvre"));
    assert!(d.holds("aujourd’hui"), "a dictionary's typographic apostrophe is the plain one too");
    assert!(d.holds("aujourd'hui"));
    assert!(d.holds("AUJOURD’HUI"));
    assert!(d.holds("français"));
    assert!(!d.holds("coeur"), "a ligature is not two letters: that is another spelling");
    assert!(!d.holds("francais"), "nor is a letter without its accent");
    // Both of those are what a suggestion is for.
    let got = spell(&d).suggest("coeur", 5);
    assert!(got.iter().any(|w| w == "cœur"), "{got:?}");
    let got = spell(&d).suggest("francais", 5);
    assert!(got.iter().any(|w| w == "français"), "{got:?}");
    let got = spell(&d).suggest("aujourd'hui", 5);
    assert!(got.is_empty(), "the word as it is written is not its own suggestion: {got:?}");
}

/// The owner's own words are theirs: they are accepted, in any case, in the language that is on.
#[test]
fn personal_words_are_accepted_and_bounded() {
    let mut s = Speller::new();
    s.put(dic(Language::FrCa, "1\nbonjour\n"));
    s.set_personal(&["Hochelaga".into(), "  ".into(), "hochelaga".into(), "x".repeat(MAX_WORD + 40)]);
    assert_eq!(s.accept("hochelaga"), Some(Language::FrCa));
    assert_eq!(s.accept("HOCHELAGA"), Some(Language::FrCa), "case is folded");
    assert_eq!(s.accept("Hochelaga"), Some(Language::FrCa));
    assert!(s.is_word("hochelaga"));
    assert_eq!(s.accept("ailleurs"), None);
    assert_eq!(s.personal.len(), 1, "an empty word and one that is too long are not words");
    // A word of the owner's is not a word of the dictionary, and it is never suggested back.
    let got = s.suggest("hochelago", 5);
    assert!(!got.iter().any(|w| w.eq_ignore_ascii_case("hochelaga")), "{got:?}");
    // The speller is not a checker with no dictionary at all, even once a word is added: the
    // owner's word is accepted, and nothing else can be called wrong.
    let mut only = Speller::new();
    assert!(only.is_empty());
    only.set_personal(&["Hochelaga".into()]);
    assert!(only.is_empty(), "a personal word is not a language dictionary");
    assert!(only.is_word("Hochelaga"));
}

/// A personal dictionary on its own is not a spelling checker: the owner's words are accepted, but
/// nothing there can say another word is wrong, so a speller with no language dictionary reports
/// that it is not ready and finds no places at all.
#[test]
fn a_personal_dictionary_alone_is_not_a_checker() {
    let mut s = Speller::new();
    s.set_personal(&["Hochelaga".into()]);
    assert!(!s.is_word("hello"), "no dictionary knows it");
    assert!(s.is_empty(), "no language dictionary was loaded");
    assert!(s.is_word("Hochelaga"), "the owner's word is still a word");
    let text = "hello world Hochelaga";
    assert!(s.check(text).is_empty(), "nothing can be called a mistake without a dictionary: {:?}", s.check(text));
    assert!(s.languages().is_empty());
    // With a dictionary on, the same speller checks, and its own words are still accepted.
    s.put(dic(Language::EnCa, "2\nhello\nworld\n"));
    assert!(!s.is_empty(), "a dictionary that holds words is a checker");
    assert!(s.check(text).is_empty(), "every word of it is right: {:?}", s.check(text));
    assert!(s.check("hello wurld Hochelaga").len() == 1, "the wrong word is the one flagged");
}

/// A personal entry is one word: what holds a space or a control character inside it is refused
/// rather than kept where it could never match a token.
#[test]
fn personal_entries_are_single_words() {
    let mut s = Speller::new();
    s.set_personal(&["Hoc helaga".into(), "Hoc\u{1}helaga".into(), "Hochelaga".into(), "rendez-vous".into()]);
    assert_eq!(s.personal.len(), 2, "only the single words are kept: {:?}", s.personal);
    assert_eq!(s.accept("Hoc helaga"), None);
    assert_eq!(s.accept("Hochelaga"), Some(Language::EnCa), "a personal word counts for the first language");
    assert!(s.is_word("rendez-vous"));
}

/// A hostile `FLAG long` line names millions of flags: only the flags a stem may name are taken
/// from it, and the rest of the line is never turned into strings at all.
#[test]
fn a_long_flag_line_is_cut_before_it_is_read() {
    let pulled = std::cell::Cell::new(0usize);
    let chars = (0..4_000_000u32).map(|_| 'A').inspect(|_| pulled.set(pulled.get() + 1));
    let flags = super::spell::long_flags(chars);
    assert!(flags.len() <= super::spell::MAX_FLAGS, "at most the cap is kept: {}", flags.len());
    assert!(pulled.get() <= super::spell::MAX_FLAGS * 2, "only the capped characters are read: {}", pulled.get());
    // And a `.dic` whose flag line is two million characters long still loads, with its flags.
    let d = hunspell("FLAG long\nSFX AB Y 1\nSFX AB 0 s .\n", &format!("1\ndocument/{}\n", "AB".repeat(1_000_000)));
    assert!(d.holds("document"));
    assert!(d.holds("documents"), "the first flags are still read");
}

/// Suggestions are the nearest words first, in the case they were asked in, never the word itself
/// and never a word the dictionary keeps out of them.
#[test]
fn suggestions_are_ranked_capitalised_and_never_the_word_itself() {
    let s = {
        let mut s = Speller::new();
        s.put(dic(Language::EnCa, "6\ndocument\ndocuments\ndoctor\ndoctrine\ncolour\ncolours\n"));
        s
    };
    let got = s.suggest("documant", 5);
    assert_eq!(got.first().map(String::as_str), Some("document"), "the nearest word comes first: {got:?}");
    assert!(got.iter().all(|w| !w.is_empty()));
    // A word written with a capital comes back with one, and in the same place in the order.
    let got = s.suggest("Documant", 5);
    assert_eq!(got.first().map(String::as_str), Some("Document"), "{got:?}");
    // The word itself is never the answer, even when the dictionary has it.
    let got = s.suggest("document", 8);
    assert!(!got.iter().any(|w| w.eq_ignore_ascii_case("document")), "{got:?}");
    assert!(got.iter().any(|w| w == "documents"), "{got:?}");
    // Nothing in common, nothing offered; and no more than asked for.
    assert!(s.suggest("zzzz", 5).is_empty());
    assert!(s.suggest("documant", 2).len() <= 2);
    assert!(s.suggest("", 5).is_empty());
    assert!(s.suggest("documant", 0).is_empty());
    assert!(s.suggest("documant", 10_000).len() <= super::MAX_SUGGESTIONS);
}

/// `NOSUGGEST`: a word that is a word, and is not offered — its affixed forms neither.
#[test]
fn a_quiet_word_is_a_word_but_is_never_suggested() {
    let aff = "NOSUGGEST !\nSFX S Y 1\nSFX S 0 s .\n";
    let d = hunspell(aff, "2\ncolour/!S\ndocument/S\n");
    assert!(d.holds("colour"));
    assert!(d.holds("colours"));
    assert!(d.is_quiet("colour"));
    assert!(d.is_quiet("colours"), "the forms of a quiet stem are quiet too");
    for word in ["coluor", "coloures"] {
        let got = spell(&d).suggest(word, 8);
        assert!(!got.iter().any(|w| w.starts_with("colou")), "{word} gave {got:?}");
    }
    // A word that is not quiet is still offered, so the test would catch an over-wide filter.
    let got = spell(&d).suggest("documant", 5);
    assert!(got.iter().any(|w| w == "document"), "{got:?}");
}

/// Every place a diagnostic points at is a range of the text it was found in, on its character
/// boundaries, and never empty: a correction goes where the wrong word is or nowhere.
#[test]
fn every_diagnostic_span_is_a_char_boundary_range_of_its_text() {
    let mut s = Speller::new();
    s.put(dic(Language::EnCa, "5\ndocument\ncolour\ncentre\nneighbour\nthe\n"));
    s.put(dic(Language::FrCa, "3\nfrançais\naujourd'hui\nMontréal\n"));
    let texts = [
        "Bonjour, I reviewed les documents hier soir.",
        "a\u{300}b\u{fffd}\u{1F600} 🙂 ok, the documant is hear",
        "«Ça va?» dit-il, en français — aujourd’hui, à Montréal.",
        "e\u{301}\u{302}\u{303} documant\u{200d}colour",
        "rendez-vuos et aujourd'hui",
        "  ",
        "",
    ];
    for text in texts {
        for d in s.check(text) {
            assert!(d.span.fits(text), "{d:?} does not fit {text:?}");
            assert!(d.span.start < d.span.end, "{d:?} is empty");
            assert!(text.is_char_boundary(d.span.start) && text.is_char_boundary(d.span.end));
            assert!(text.get(d.span.start..d.span.end).is_some(), "{d:?}");
            assert!(text.get(d.span.start..d.span.end).is_some_and(|w| !w.is_empty()));
        }
    }
}

/// The words the diagnostics of `found` point at, in order: what a test reads instead of spans.
fn flagged<'a>(text: &'a str, found: &[super::Diagnostic]) -> Vec<&'a str> {
    found.iter().filter_map(|d| text.get(d.span.start..d.span.end)).collect()
}

/// A scan for a window that checks as the owner types must find exactly the places the eager check
/// finds — the same spans, kind, source and language — and nothing else. What it does not do is
/// keep a suggestion for any of them: a suggestion is asked for about one word, on the owner's
/// click, not computed for every word of a draft before the answer is given.
#[test]
fn a_fast_scan_finds_the_same_places_as_the_eager_check() {
    let mut s = Speller::new();
    s.put(dic(Language::EnCa, "7\ndocument\ndocuments\ncolour\nthe\nand\nplease\nreviewed\n"));
    s.put(dic(Language::FrCa, "7\nl'\nj'\nhomme\nai\naime\naujourd'hui\nrendez\n"));
    s.set_personal(&["Hochelaga".to_string()]);
    let texts = [
        "The documant is hear, please.",
        "Bonjour, j'aimee l'homme et le documant.",
        "un rendez-vuos et aujourd'hui",
        "l'e\u{301}colle documant",
        "Montre\u{301}al documant",
        "colour colour colour",
        "Hochelaga documant",
        "",
        "   ",
    ];
    for text in texts {
        let eager = s.check(text);
        let fast = s.scan(text);
        assert_eq!(fast.len(), eager.len(), "{text:?}: scan gave {fast:?}, check gave {eager:?}");
        for (f, e) in fast.iter().zip(&eager) {
            assert_eq!(f.span, e.span, "{text:?}: the same place");
            assert_eq!(f.kind, e.kind, "{text:?}: the same kind");
            assert_eq!(f.source, e.source, "{text:?}: the same source");
            assert_eq!(f.language, e.language, "{text:?}: the same language");
            assert_eq!(f.message, e.message, "{text:?}: the same words about it");
            assert_eq!(f.rule, e.rule, "{text:?}: the same rule");
            assert!(f.suggestions.is_empty(), "a scan keeps no suggestion: {f:?}");
        }
    }
}

/// A scan follows the same reading of a token as the check does: only what follows an apostrophe is
/// flagged in an elision, and only the part nothing knows is flagged in a joined word.
#[test]
fn a_fast_scan_flags_the_rest_of_an_elision_and_the_part_of_a_joined_word() {
    let mut s = Speller::new();
    s.put(dic(Language::FrCa, "8\nl'\nj'\nhomme\naime\naimée\nrendez\nvous\nun\n"));
    let text = "j'aimee";
    let fast = s.scan(text);
    assert_eq!(flagged(text, &fast), ["aimee"], "only the rest is flagged: {fast:?}");
    assert_eq!(fast.first().map(|d| d.span.start), Some("j'".len()), "the span begins after the apostrophe");
    // A joined word the dictionary lists only whole has its halves looked at where they are.
    let text = "un rendez-vuos";
    let fast = s.scan(text);
    assert_eq!(flagged(text, &fast), ["vuos"], "{fast:?}");
    assert!(fast.iter().all(|d| d.suggestions.is_empty()), "{fast:?}");
    // The same places the eager check finds, with nothing kept in them.
    assert_eq!(fast.iter().map(|d| d.span).collect::<Vec<_>>(), s.check(text).iter().map(|d| d.span).collect::<Vec<_>>());
}

/// The scan is not a second, weaker answer: what it found can be asked about one word at a time,
/// and the eager `suggest` that a right click uses still answers with the dictionary's words.
#[test]
fn a_fast_scan_stores_no_suggestion_but_asking_about_one_still_answers() {
    let mut s = Speller::new();
    s.put(dic(Language::EnCa, "7\ndocument\ndocuments\ndoctor\ndoctrine\ncolour\nthe\nand\n"));
    let text = "the documant and the coluor";
    let fast = s.scan(text);
    assert_eq!(flagged(text, &fast), ["documant", "coluor"], "{fast:?}");
    assert!(fast.iter().all(|d| d.suggestions.is_empty()), "a scan stores no suggestion: {fast:?}");
    // What the owner's click asks of the very diagnostics the scan gave: the word under each span.
    for d in &fast {
        let word = text.get(d.span.start..d.span.end).unwrap_or_default();
        let offered = s.suggest(word, 5);
        assert!(!offered.is_empty(), "{word:?} is offered a correction on demand: {fast:?}");
    }
    let one = fast.first().and_then(|d| text.get(d.span.start..d.span.end));
    assert_eq!(one, Some("documant"));
    let offered = s.suggest("documant", 5);
    assert_eq!(offered.first().map(String::as_str), Some("document"), "{offered:?}");
    // The eager check is unchanged: it still carries the suggestions it always did.
    let eager = s.check(text);
    assert!(eager.iter().any(|d| !d.suggestions.is_empty()), "check still suggests: {eager:?}");
}

/// A scan that is called off is not a scan that found nothing: `None` says it was called off, and
/// `Some` with an empty list says the text is clean. A called-off scan never hands back the part of
/// the list it had worked out as though it were the whole of it.
#[test]
fn a_cancelled_scan_is_not_a_clean_scan() {
    let mut s = Speller::new();
    s.put(dic(Language::EnCa, "4\ndocument\ncolour\nthe\nand\n"));
    let text = "the documant and the coluor";
    // Called off before any work: no answer at all, and it is asked once.
    let asked = Cell::new(0usize);
    let before = s.scan_cancellable(text, &|| {
        asked.set(asked.get() + 1);
        true
    });
    assert!(before.is_none(), "a scan called off before it started is no answer at all");
    assert_eq!(asked.get(), 1, "it is asked once, before any work");
    // Nobody calls it off: the whole answer, the same places `scan` gives.
    let complete = s.scan_cancellable(text, &|| false);
    assert_eq!(complete.as_deref(), Some(s.scan(text).as_slice()), "the whole list, word for word");
    // Called off between two words: no partial list, however much of it was worked out.
    let asked = Cell::new(0usize);
    let during = s.scan_cancellable(text, &|| {
        asked.set(asked.get() + 1);
        asked.get() >= 2
    });
    assert!(during.is_none(), "half a draft is not a scan of it (asked {} times)", asked.get());
    // Called off after the last word, before the answer is given: still no answer.
    let words = super::words(text).len();
    let publish = words + 2;
    let asked = Cell::new(0usize);
    let late = s.scan_cancellable(text, &|| {
        asked.set(asked.get() + 1);
        asked.get() == publish
    });
    assert!(late.is_none(), "an answer worked out but no longer wanted is not given");
    assert_eq!(asked.get(), publish, "it was asked once more before publishing: {publish} questions");
    // A text with nothing wrong, cancelled: not a clean scan either.
    let clean = "the document and the colour";
    assert!(s.scan(clean).is_empty(), "nothing is wrong here");
    assert!(s.scan_cancellable(clean, &|| true).is_none(), "cancelled is not clean");
    let cancelled = s.scan_cancellable(clean, &|| true);
    assert_eq!(cancelled, None, "and it is not Some(Vec::new())");
}

/// The questions are asked where the contract says: once before any work, once before each word,
/// and once more before the answer — never in the middle of a word, and never after the answer is
/// given.
#[test]
fn cancellation_is_asked_before_work_between_words_and_before_the_answer() {
    let mut s = Speller::new();
    s.put(dic(Language::EnCa, "2\ndocument\nthe\n"));
    let asked = Cell::new(0usize);
    let complete = s.scan_cancellable("the documant", &|| {
        asked.set(asked.get() + 1);
        false
    });
    assert!(complete.is_some(), "nobody called it off");
    assert_eq!(complete.map(|v| v.len()), Some(1), "the misspelling is found");
    assert_eq!(asked.get(), 4, "1 before work + 2 words + 1 before the answer");
    // An empty text: before the work and before the answer, and nothing in between.
    let asked = Cell::new(0usize);
    let empty = s.scan_cancellable("", &|| {
        asked.set(asked.get() + 1);
        false
    });
    assert_eq!(empty, Some(Vec::new()));
    assert_eq!(asked.get(), 2, "nothing to look at is still asked before and after");
    // No dictionary: nothing is read, and the same two questions.
    let asked = Cell::new(0usize);
    let none = Speller::new().scan_cancellable("the documant", &|| {
        asked.set(asked.get() + 1);
        false
    });
    assert_eq!(none, Some(Vec::new()), "no dictionary is no places, not a cancelled scan");
    assert_eq!(asked.get(), 2, "a speller with no dictionary is still asked before and after");
}

/// A scan is held to the same bounds as the check — the diagnostic cap, and only the first
/// [`super::MAX_CHECKED`] characters — and every place it gives is a range of the text it came
/// from, on that text's character boundaries and never empty.
#[test]
fn a_scan_is_bounded_and_every_place_is_a_range_of_its_text() {
    let mut s = Speller::new();
    s.put(dic(Language::EnCa, "3\ndocument\nthe\nand\n"));
    // More wrong words than the cap allows: the cap is what stops it, and it is reached.
    let many: String = (0..(MAX_DIAGNOSTICS * 2)).map(|n| format!("zzzz{n} ")).collect();
    let found = s.scan(&many);
    assert_eq!(found.len(), MAX_DIAGNOSTICS, "the diagnostic cap holds");
    assert!(found.len() <= s.check(&many).len(), "a scan never finds more than the check does");
    // Only the first MAX_CHECKED characters are looked at, exactly as the check looks at them.
    let long = "the ".repeat(super::MAX_CHECKED / 4);
    let past = format!("{long}documant");
    assert_eq!(super::words(&past).len(), super::words(&long).len(), "the tail is past what is read");
    assert!(s.scan(&past).is_empty(), "a word past the checked text is not looked at");
    assert_eq!(s.scan(&past).len(), s.check(&past).len(), "and the eager check agrees");
    let inside = format!("documant {long}");
    assert_eq!(flagged(&inside, &s.scan(&inside)), ["documant"], "a word inside it is looked at");
    // Every place of every text is a range of it.
    for text in ["", "  ", "documant", "a\u{300}documant\u{1F600}", "the\u{200d}documant", "documant-documant"] {
        let complete = s.scan_cancellable(text, &|| false).unwrap_or_default();
        for d in s.scan(text).iter().chain(complete.iter()) {
            assert!(d.span.fits(text), "{d:?} does not fit {text:?}");
            assert!(d.span.start < d.span.end, "{d:?} is empty");
            assert!(text.is_char_boundary(d.span.start) && text.is_char_boundary(d.span.end), "{d:?}");
            assert!(text.get(d.span.start..d.span.end).is_some_and(|w| !w.is_empty()), "{d:?}");
        }
    }
    // Nothing at all without a dictionary, however much is wrong.
    assert!(Speller::new().scan("documant documant").is_empty(), "no dictionary is no places");
}

/// A dictionary far larger than the one on a desktop still loads, and is held to
/// [`MAX_ENTRIES`] however many words its affixes would make.
#[test]
fn a_hundred_thousand_words_load_within_the_caps() {
    let mut text = String::with_capacity(1_100_000);
    text.push_str("100000\n");
    for n in 0..100_000usize {
        text.push_str(&format!("word{n}\n"));
    }
    let started = Instant::now();
    let d = dic(Language::EnCa, &text);
    assert_eq!(d.len(), 100_000, "every word of it is a word");
    assert!(d.holds("word0"));
    assert!(d.holds("word99999"));
    assert!(!d.holds("word100000"));
    assert!(started.elapsed() < Duration::from_secs(30), "loading stays quick: {:?}", started.elapsed());
}

/// A big `.dic` with affixes, and lines that cannot be read: it finishes, keeps to the caps, and
/// nothing panics.
#[test]
fn a_huge_dictionary_with_affixes_finishes() {
    let aff = "SET UTF-8\nFLAG long\nFORBIDDENWORD XX\nNEEDAFFIX YY\nSFX SS Y 2\nSFX SS 0 s .\nSFX SS 0 es [sxz]\nPFX PP Y 1\nPFX PP 0 re .\n";
    let mut text = String::with_capacity(4_000_000);
    text.push_str("200000\n");
    for n in 0..200_000usize {
        match n % 5 {
            0 => text.push_str(&format!("stem{n}/SSPP\n")),
            1 => text.push_str(&format!("stem{n}/SS\n")),
            2 => text.push_str(&format!("stem{n}/YYSS\n")),
            3 => text.push_str(&format!("stem{n}/XX\n")),
            _ => text.push_str(&format!("stem{n}\n")),
        }
    }
    let started = Instant::now();
    let d = hunspell(aff, &text);
    assert!(d.len() <= MAX_ENTRIES, "the cap is kept: {}", d.len());
    assert!(d.len() > 200_000, "the affixes made words of the stems: {}", d.len());
    assert!(d.holds("stem0"));
    assert!(d.holds("restem0"));
    assert!(d.holds("stem0s"));
    assert!(d.holds("restem0s"), "both ends of the cross product");
    assert!(!d.holds("stem2"), "a NEEDAFFIX stem is not a word");
    assert!(d.holds("stem2s"), "but the form its suffix makes is");
    assert!(!d.holds("stem3"), "a forbidden stem is not a word");
    assert!(!d.holds("restem3"), "nor any form of it");
    assert!(started.elapsed() < Duration::from_secs(60), "loading stays bounded: {:?}", started.elapsed());
}

/// A `.aff` that is truncated, random, or full of lines that cannot be read is read for what it
/// says and skipped for the rest: no panic, no loop, and the words it can make are still made.
#[test]
fn a_corrupt_or_truncated_affix_file_is_read_without_a_panic() {
    let dictionary = "2\ndocument/S\ncolour/S\n";
    let cases = [
        "",
        "\n\n\n",
        "SFX",
        "SFX S",
        "SFX S Y",
        "FLAG",
        "FLAG long extra words here",
        "FLAG UTF-8",
        "SFX S Y 999999999999999999999",
        "SFX S Y 3\nSFX S 0 s .\n",
        "SFX S Y 1\nSFX S 0\n",
        "SFX S Y 1\nSFX S 0 s [abc",
        "SFX S Y 1\nSFX S 0 s [",
        "SFX S Y 1\nPFX S Y 1\nPFX S 0 re .",
        "NEEDAFFIX\nFORBIDDENWORD\nNOSUGGEST\n",
        "PFX A Y 1\nPFX A 0 re .\ntrailing garbage\n\x01\x02\x03\n",
        "# a comment\nSFX S Y 1\nSFX S 0 s .\n# another\n",
        "SFX S Y 1\r\nSFX S 0 s .\r\n",
        "SFX S Y 1\nSFX S 0 s .\nSFX S Y 1\nSFX S 0 s .\n",
    ];
    for aff in cases {
        let d = hunspell(aff, dictionary);
        assert!(d.len() >= 2, "{aff:?} made {} words", d.len());
        assert!(d.holds("document"), "{aff:?}");
    }
    // A condition nobody can read is taken to hold, so the rule still makes its word.
    let d = hunspell("SFX S Y 1\nSFX S 0 s [abc", dictionary);
    assert!(d.holds("colours"), "an unreadable condition does not lose the word");
    // Random text, a very long line, and a line with thousands of flags: read, and bounded.
    let noise = noise(0x5eed_1234_9876_abcd, 4_000);
    let d = hunspell(&noise, dictionary);
    assert!(d.holds("document"));
    let long = "x".repeat(100_000);
    let d = hunspell(&long, dictionary);
    assert!(d.holds("document"));
    let flags = "S".repeat(10_000);
    let d = hunspell("SFX S Y 1\nSFX S 0 s .\n", &format!("1\ndocument/{flags}\n"));
    assert!(d.len() <= 4, "a line is cut to a bounded number of flags: {}", d.len());
}

/// The edits between two words, worked out in full with no cut-off and no shortcuts: one letter
/// changed, put in or taken out, or two neighbours swapped — the format's distance, and what a
/// suggestion is measured with. The engine may stop early; this may not, because it is what the
/// engine is checked against.
fn exact(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut d: Vec<Vec<usize>> = vec![vec![0; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    if let Some(row) = d.first_mut() {
        for (j, cell) in row.iter_mut().enumerate() {
            *cell = j;
        }
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let swap = if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] { d[i - 2][j - 2] + 1 } else { usize::MAX };
            d[i][j] = (d[i - 1][j] + 1).min(d[i][j - 1] + 1).min(d[i - 1][j - 1] + usize::from(a[i - 1] != b[j - 1])).min(swap);
        }
    }
    d[a.len()][b.len()]
}

/// An edit-distance table worked out by hand, so that no change to the search can make a word look
/// nearer than it is. Each pair is also measured in full, so a wrong number in the table is a
/// failure of the table.
#[test]
fn an_edit_distance_table_is_exact() {
    // (the word in the dictionary, what was typed, the edits between them)
    let table = [
        ("colour", "colours", 1),
        ("colour", "colur", 1),
        ("colour", "coluor", 1),
        ("colour", "calour", 1),
        ("colour", "coluors", 2),
        ("colour", "coluosr", 2),
        ("colour", "cluoor", 3),
        ("colour", "colourful", 3),
        ("please", "pleese", 1),
        ("please", "pealse", 2),
        ("please", "pealses", 3),
        // A swap and two letters changed: three, and only a table that charges one edit for the
        // swap says so.
        ("abcdef", "acbxyf", 3),
        ("abcdef", "acbdef", 1),
        ("pierre", "pirre", 1),
        ("pierre", "pierr", 1),
        ("pierre", "please", 3),
        ("pierre", "pour", 4),
    ];
    for (word, typed, edits) in table {
        assert_eq!(exact(typed, word), edits, "{typed:?} → {word:?} by hand");
        let s = spell(&dic(Language::EnCa, &format!("1\n{word}\n")));
        let got = s.suggest(typed, 5);
        let offered = got.iter().any(|w| w.eq_ignore_ascii_case(word));
        assert_eq!(offered, edits <= 2, "{typed:?} → {word:?} is {edits} edits apart: {got:?}");
    }
}

/// Every suggestion the search makes is what a full edit distance says it is, in the order that
/// distance says: every word of one, two and three letters over three letters is asked about, and
/// the answer is compared with what the same distance worked out in full would offer.
#[test]
fn suggestions_agree_with_an_exact_edit_distance() {
    let mut all: Vec<String> = Vec::new();
    for n in 1..=3usize {
        let mut level = vec![String::new()];
        for _ in 0..n {
            let mut next: Vec<String> = Vec::new();
            for base in &level {
                for c in ['a', 'b', 'c'] {
                    let mut word = base.clone();
                    word.push(c);
                    next.push(word);
                }
            }
            level = next;
        }
        all.extend(level);
    }
    let mut text = format!("{}\n", all.len());
    for word in &all {
        text.push_str(word);
        text.push('\n');
    }
    let s = spell(&dic(Language::EnCa, &text));
    assert!(s.is_word("abc"));
    for query in &all {
        let got = s.suggest(query, 8);
        let first = query.chars().next();
        for word in &got {
            let edits = exact(query, word);
            assert!((1..=2).contains(&edits), "{query:?} was offered {word:?}, {edits} edits away");
            assert!(word.chars().next() == first, "only words that begin as it does are looked at");
        }
        let mut want: Vec<String> = all.iter().filter(|w| *w != query && w.chars().next() == first && exact(query, w) <= 2).cloned().collect();
        want.sort_by(|a, b| exact(query, a).cmp(&exact(query, b)).then(a.chars().count().cmp(&b.chars().count())).then(a.cmp(b)));
        want.truncate(8);
        assert_eq!(got, want, "the suggestions for {query:?}");
    }
}

/// The dictionaries the owner has, together: Canadian English and Canadian French. What a wrong
/// word is offered is a word near its letters, and what only shares its first letter is not
/// offered at all.
#[test]
fn a_bilingual_dictionary_offers_only_what_is_near() {
    let mut s = Speller::new();
    s.put(dic(Language::EnCa, "7\ndocument\ncolour\nplease\nproject\nplus\nsee\nthe\n"));
    s.put(dic(Language::FrCa, "7\nbonjour\npierre\npeut\npour\nprojet\nmerci\nrevoir\n"));
    // `pierre` is three edits from `please` and four from `pour`: a reader that offers them for it
    // has measured the letters wrongly, and its corrections cannot be trusted.
    let got = s.suggest("Pierre", 8);
    assert!(got.iter().all(|w| exact("pierre", &w.to_lowercase()) <= 2), "nothing far away: {got:?}");
    for far in ["please", "project", "projet", "pour", "peut", "plus"] {
        assert!(!got.iter().any(|w| w.eq_ignore_ascii_case(far)), "{far} is not near pierre: {got:?}");
    }
    // And the words that are near it are offered, nearest first, in the case they were typed in.
    let got = s.suggest("Pirre", 5);
    assert_eq!(got.first().map(String::as_str), Some("Pierre"), "{got:?}");
    let got = s.suggest("pierr", 5);
    assert_eq!(got.first().map(String::as_str), Some("pierre"), "{got:?}");
    // The one word that is not a word is the one flagged, with a word of the same dictionary.
    let text = "Bonjour Pierre, see the documant please.";
    let found = s.check(text);
    let flagged: Vec<&str> = found.iter().filter_map(|d| text.get(d.span.start..d.span.end)).collect();
    assert_eq!(flagged, ["documant"], "only the word that is not one: {flagged:?}");
    assert_eq!(found.first().and_then(|d| d.suggestions.first()).map(String::as_str), Some("document"));
}

/// Random text of the alphabet a `.aff` is written in, from a seed, so a failure can be run again.
fn noise(mut seed: u64, lines: usize) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 /-^[]#.\t\\\"'";
    let mut out = String::with_capacity(lines * 24);
    for _ in 0..lines {
        seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        let len = (seed >> 33) as usize % 40;
        for _ in 0..len {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            if let Some(c) = ALPHABET.get((seed >> 33) as usize % ALPHABET.len()) {
                out.push(*c as char);
            }
        }
        out.push('\n');
    }
    out
}

/// A dictionary of 20,000 stems, as a package installs one: four 2-letter syllables, a suffix that
/// crosses with a prefix, and enough words for a suggestion to be a search.
fn twenty_thousand_stems() -> (String, String) {
    const SYL: [&str; 16] = ["ka", "lo", "ri", "men", "tor", "sa", "vi", "nel", "dar", "po", "lin", "qua", "ber", "sto", "mi", "ra"];
    let aff = "SET UTF-8\nFLAG UTF-8\nPFX P Y 1\nPFX P 0 re .\nSFX S Y 2\nSFX S 0 s .\nSFX S 0 es [sxz]\n";
    let mut text = String::with_capacity(20_000 * 16);
    text.push_str("20000\n");
    for n in 0..20_000usize {
        let a = SYL.get(n % 16).copied().unwrap_or("ka");
        let b = SYL.get((n / 16) % 16).copied().unwrap_or("ka");
        let c = SYL.get((n / 256) % 16).copied().unwrap_or("ka");
        let d = SYL.get((n / 4_096) % 16).copied().unwrap_or("ka");
        text.push_str(&format!("{a}{b}{c}{d}/PS\n"));
    }
    (aff.into(), text)
}

/// How long loading one is, one word of it, and a suggestion for a misspelling of one: the numbers
/// a right click and a fresh start are felt in.
///
/// Run with `cargo test -p mailcraft-model --lib proof -- --ignored --nocapture`.
#[test]
#[ignore = "a measurement, not an assertion: run it with --ignored --nocapture"]
fn a_twenty_thousand_stem_dictionary_is_quick() {
    let (aff, text) = twenty_thousand_stems();
    let started = Instant::now();
    let d = hunspell(&aff, &text);
    let loading = started.elapsed();
    assert!(d.len() > 20_000, "the affixes made words of the stems: {}", d.len());
    assert_eq!(d.len(), 80_000, "stem, prefix, suffix and cross product for each: {}", d.len());

    // One word of it, asked the way the window asks: through the speller, which folds the case
    // and looks in every dictionary that is on.
    let mut s = Speller::new();
    s.put(d.clone());
    let rounds = 1_000u32;
    let started_word = Instant::now();
    let mut known = 0usize;
    for _ in 0..rounds {
        if s.is_word("kalorimen") && s.is_word("kalorimens") {
            known += 1;
        }
    }
    let word_total = started_word.elapsed();
    let per_word = word_total / rounds;
    assert_eq!(known as u32, rounds, "the words asked about are the dictionary's own");

    // A misspelling of an 8-letter stem, two letters the wrong way round, twenty times over.
    let started_suggest = Instant::now();
    let mut worst = Duration::ZERO;
    let mut got = Vec::new();
    for _ in 0..20 {
        let started = Instant::now();
        got = s.suggest("kalorimne", 5);
        worst = worst.max(started.elapsed());
    }
    let per_suggest = started_suggest.elapsed() / 20;
    let one = s.suggest("kalorimne", 5);
    println!(
        "20k stems ({} words, 3 affix rules): load {loading:?}, is_word {per_word:?} ({rounds} calls in {:?}), \
         suggest 8 letters {per_suggest:?} (worst {worst:?}), {one:?}",
        d.len(),
        word_total,
    );
    assert!(!got.is_empty(), "the stem itself is not its own suggestion, its forms are: {got:?}");
    assert!(loading < Duration::from_secs(10), "loading a real dictionary stays quick: {loading:?}");
    assert!(per_word < Duration::from_millis(1), "one word is a lookup: {per_word:?}");
    assert!(worst < Duration::from_millis(500), "a suggestion stays quick: {worst:?}");
}

/// A measurement against the dictionaries installed on this computer: how long they take to read,
/// and how long a draft of 50 to 10,000 of their own words takes to check, cold (the first call)
/// and warm (the next). Then the same draft misspelt, which is where the eager suggestion computed
/// inside every diagnostic shows its cost, and the diagnostic cap cuts the work short.
///
/// This reads no dictionary of its own and ships none; with no folder named it says so and stops.
/// Run with
/// `MAILCRAFT_DICT_DIR=/usr/share/hunspell cargo test -p mailcraft-model --lib
/// proof::spell_tests::real_dictionary_compositions -- --ignored --nocapture`.
#[test]
#[ignore = "reads the dictionaries installed on this computer and is a measurement: name their folder in MAILCRAFT_DICT_DIR"]
fn real_dictionary_compositions_are_measured() {
    let Some(dir) = std::env::var("MAILCRAFT_DICT_DIR").ok().filter(|d| !d.is_empty()) else {
        println!("no MAILCRAFT_DICT_DIR: nothing to read, nothing to say");
        return;
    };
    let mut s = Speller::new();
    for (lang, stem) in [(Language::EnCa, "en_CA"), (Language::FrCa, "fr_CA")] {
        let (Ok(aff), Ok(dic)) = (std::fs::read_to_string(format!("{dir}/{stem}.aff")), std::fs::read_to_string(format!("{dir}/{stem}.dic"))) else {
            println!("{stem}.aff/.dic is not in {dir}");
            continue;
        };
        let started = Instant::now();
        match Dictionary::from_hunspell(lang, &aff, &dic) {
            Ok(d) => {
                println!("{stem}: {} words read in {:?}", d.len(), started.elapsed());
                s.put(d);
            }
            Err(e) => println!("{stem}: {e}"),
        }
    }
    if s.is_empty() {
        println!("no dictionary was read in {dir}");
        return;
    }
    let words: Vec<String> = s
        .dicts
        .first()
        .map(|d| {
            d.all()
                .iter()
                .filter(|w| (4..=12).contains(&w.chars().count()) && w.chars().all(|c| c.is_ascii_lowercase()))
                .take(2_000)
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    assert!(!words.is_empty(), "a dictionary of words to compose with");
    let compose = |words: &[String], n: usize| -> String {
        let mut text = String::with_capacity(n * 9);
        for w in words.iter().cycle().take(n) {
            text.push_str(w);
            text.push(' ');
        }
        text
    };
    for n in [50usize, 250, 1_000, 5_000, 10_000] {
        let text = compose(&words, n);
        let started = Instant::now();
        let found = s.check(&text);
        let cold = started.elapsed();
        assert!(found.is_empty(), "{} of the dictionary's own words were flagged", found.len());
        let started = Instant::now();
        let again = s.check(&text);
        let warm = started.elapsed();
        assert!(again.is_empty());
        println!("valid    {n:>5} words: cold {cold:?}, warm {warm:?}, {} diagnostics", found.len());
    }
    // A misspelling of every word: `check` computes each diagnostic's suggestions eagerly, so this
    // is the cost the cap bounds, not a cost of the dictionary lookup.
    let bad: Vec<String> = words.iter().map(|w| format!("{w}zz")).collect();
    for n in [50usize, 250, 1_000, 5_000, 10_000] {
        let text = compose(&bad, n);
        let started = Instant::now();
        let found = s.check(&text);
        let cold = started.elapsed();
        assert!(found.len() <= MAX_DIAGNOSTICS, "the cap holds: {}", found.len());
        let started = Instant::now();
        let again = s.check(&text);
        let warm = started.elapsed();
        assert!(again.len() <= MAX_DIAGNOSTICS);
        let each = cold.checked_div(u32::try_from(found.len().max(1)).unwrap_or(1)).unwrap_or_default();
        println!("misspelt {n:>5} words: cold {cold:?} ({each:?} a diagnostic), warm {warm:?}, {} diagnostics (cap {MAX_DIAGNOSTICS})", found.len());
    }
}

/// The measurement a window's checking is judged by: how long the fast scan takes over a draft of
/// 50 to 10,000 of a real dictionary's own words, valid and misspelt, cold (the first call) and
/// warm (the next), and what one suggestion for one of the places it found costs when the owner
/// asks for that one and not for four thousand.
///
/// The eager `check` is deliberately **not** run over the misspelt drafts: its cost is the recorded
/// baseline below, and to measure it again would spend half a minute of this machine's time on a
/// number that only moves when the suggestion search does.
///
/// Every number printed says what it measured: the words actually read (a draft longer than
/// [`super::MAX_CHECKED`] characters is cut, and the count printed says so), the places found, the
/// suggestions the scan kept (none), and this build's `test` profile and warm page cache.
///
/// This reads no dictionary of its own and ships none; it names the fixture this machine has.
/// Run with
/// `MAILCRAFT_DICT_DIR=<folder> cargo test -p mailcraft-model --lib
/// proof::spell_tests::a_fast_scan_and_one_suggestion -- --ignored --nocapture`.
#[test]
#[ignore = "reads the dictionaries installed on this computer and is a measurement: name their folder in MAILCRAFT_DICT_DIR"]
fn a_fast_scan_and_one_suggestion_are_measured() {
    let Some(dir) = std::env::var("MAILCRAFT_DICT_DIR").ok().filter(|d| !d.is_empty()) else {
        println!("no MAILCRAFT_DICT_DIR: nothing to read, nothing to say");
        return;
    };
    let mut s = Speller::new();
    // The stems this fixture has. `fr_CA` here is the file that is installed, an alias of `fr_FR`,
    // so nothing below is a claim about Canadian French.
    for (lang, stem) in [(Language::EnCa, "en_CA"), (Language::FrCa, "fr_CA")] {
        let (Ok(aff), Ok(dic)) = (std::fs::read_to_string(format!("{dir}/{stem}.aff")), std::fs::read_to_string(format!("{dir}/{stem}.dic"))) else {
            println!("{stem}.aff/.dic is not in {dir}");
            continue;
        };
        let started = Instant::now();
        match Dictionary::from_hunspell(lang, &aff, &dic) {
            Ok(d) => {
                println!("{stem}: {} words read in {:?} ({:.2} MB .dic)", d.len(), started.elapsed(), dic.len() as f64 / 1_048_576.0);
                s.put(d);
            }
            Err(e) => println!("{stem}: {e}"),
        }
    }
    if s.is_empty() {
        println!("no dictionary was read in {dir}");
        return;
    }
    println!(
        "build: cargo's `test` profile as this workspace builds it (the `Finished test profile` line of the run), \
         debug_assertions {}; dictionaries held in memory, so `cold` is cold in the speller and the .dic files are warm in the page cache",
        cfg!(debug_assertions)
    );
    println!(
        "recorded eager baseline, same machine and fixture, before the scan existed: valid 10,000 words 6 ms; \
         misspelt 5,000-10,000 words 25-29 s from about 4,000 eager suggestions. Not measured again here."
    );
    let words: Vec<String> = s
        .dicts
        .first()
        .map(|d| {
            d.all()
                .iter()
                .filter(|w| (4..=12).contains(&w.chars().count()) && w.chars().all(|c| c.is_ascii_lowercase()))
                .take(2_000)
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    assert!(!words.is_empty(), "a dictionary of words to compose with: {dir}");
    let compose = |words: &[String], n: usize| -> String {
        let mut text = String::with_capacity(n * 9);
        for w in words.iter().cycle().take(n) {
            text.push_str(w);
            text.push(' ');
        }
        text
    };
    for n in [50usize, 250, 1_000, 5_000, 10_000] {
        let text = compose(&words, n);
        let looked = super::words(&text).len();
        let started = Instant::now();
        let cold = s.scan(&text);
        let cold_at = started.elapsed();
        assert!(cold.is_empty(), "{} of the dictionary's own words were flagged", cold.len());
        let started = Instant::now();
        let warm = s.scan(&text);
        let warm_at = started.elapsed();
        assert!(warm.is_empty());
        let kept: usize = cold.iter().chain(&warm).map(|d| d.suggestions.len()).sum();
        assert_eq!(kept, 0, "a fast scan keeps no suggestion");
        println!(
            "valid    {n:>5} asked, {} read ({} chars): scan cold {cold_at:?}, warm {warm_at:?}, {} places, {kept} suggestions kept",
            looked,
            text.chars().count(),
            cold.len()
        );
    }
    // A misspelling of every word: what the eager check used to pay a suggestion for, and the cap
    // that bounds the places.
    let bad: Vec<String> = words.iter().map(|w| format!("{w}zz")).collect();
    for n in [50usize, 250, 1_000, 5_000, 10_000] {
        let text = compose(&bad, n);
        let looked = super::words(&text).len();
        let started = Instant::now();
        let found = s.scan(&text);
        let cold = started.elapsed();
        assert!(found.len() <= MAX_DIAGNOSTICS, "the cap holds: {}", found.len());
        assert!(found.iter().all(|d| d.suggestions.is_empty()), "a fast scan keeps no suggestion");
        let started = Instant::now();
        let again = s.scan(&text);
        let warm = started.elapsed();
        assert_eq!(again.len(), found.len(), "the same places, twice");
        let each = cold.checked_div(u32::try_from(found.len().max(1)).unwrap_or(1)).unwrap_or_default();
        println!(
            "misspelt {n:>5} asked, {looked} read ({} chars): scan cold {cold:?} ({each:?} a place), warm {warm:?}, \
             {} places (cap {MAX_DIAGNOSTICS}), 0 suggestions kept",
            text.chars().count(),
            found.len()
        );
        // One suggestion, asked the way a right click asks it, about one of those places.
        if let Some(first) = found.first() {
            let word = text.get(first.span.start..first.span.end).unwrap_or_default();
            let started = Instant::now();
            let offered = s.suggest(word, 5);
            let one = started.elapsed();
            println!("           one suggestion for {word:?}: {one:?} -> {offered:?}");
            assert!(!offered.is_empty(), "a suggestion is offered for {word:?}");
        }
    }
}

/// A smoke test against the dictionaries a distribution installs, when they are on this computer:
/// the words Canadian English and Canadian French are expected to know, and a few sentences, read
/// and measured rather than assumed.
///
/// It reads no dictionary of its own and ships none: with no folder named it says so and stops.
/// Run with
/// `MAILCRAFT_DICT_DIR=/usr/share/hunspell cargo test -p mailcraft-model --lib
/// proof::spell_tests::the_installed_dictionaries -- --ignored --nocapture`
/// (Arch: the packages `hunspell-en_ca` and `hunspell-fr-comprehensive`).
#[test]
#[ignore = "reads the dictionaries installed on this computer: name their folder in MAILCRAFT_DICT_DIR"]
fn the_installed_dictionaries_accept_their_own_words() {
    let Some(dir) = std::env::var("MAILCRAFT_DICT_DIR").ok().filter(|d| !d.is_empty()) else {
        println!("no MAILCRAFT_DICT_DIR: nothing to read, nothing to say");
        return;
    };
    let mut s = Speller::new();
    for (lang, stem) in [(Language::EnCa, "en_CA"), (Language::FrCa, "fr_CA")] {
        let aff = std::fs::read_to_string(format!("{dir}/{stem}.aff"));
        let dic = std::fs::read_to_string(format!("{dir}/{stem}.dic"));
        let (Ok(aff), Ok(dic)) = (aff, dic) else {
            println!("{stem}.aff/.dic is not in {dir}");
            continue;
        };
        let started = Instant::now();
        match Dictionary::from_hunspell(lang, &aff, &dic) {
            Ok(d) => {
                println!(
                    "{stem}: {} words out of {:.2} MB of .dic and {} bytes of .aff in {:?}",
                    d.len(),
                    dic.len() as f64 / 1_048_576.0,
                    aff.len(),
                    started.elapsed()
                );
                s.put(d);
            }
            Err(e) => println!("{stem}: {e}"),
        }
    }
    assert!(!s.is_empty(), "a dictionary was read: {dir}");
    // The words a Canadian pair is expected to know, in both spellings of the apostrophe, and the
    // elided forms a French dictionary's own prefixes (`j'`, `l'`, `c'`, `d'`, `s'`) make of them.
    let known = [
        "colour",
        "centre",
        "neighbour",
        "Montréal",
        "Québec",
        "français",
        "aujourd'hui",
        "aujourd’hui",
        "rendez-vous",
        "j'ai",
        "j’ai",
        "l’homme",
        "c'est",
        "d’accord",
        "s'il",
    ];
    let missing: Vec<&str> = known.iter().copied().filter(|w| !s.is_word(w)).collect();
    println!("words refused by the installed dictionaries: {missing:?}");
    assert!(missing.is_empty(), "{missing:?} should be known: {dir}");
    // What a person writes: every word is one the dictionaries know, elisions included.
    for text in [
        "The colour of the centre is fine, neighbour.",
        "Bonjour Pierre, j'ai vu le document sur la table à Montréal.",
        "Rendez-vous aujourd’hui à Québec, s'il vous plaît.",
        "Je suis d’accord : c’est l’homme que j’ai vu.",
        "I reviewed les documents hier soir and everything looks correct.",
    ] {
        let found = s.check(text);
        let flagged: Vec<&str> = found.iter().filter_map(|d| text.get(d.span.start..d.span.end)).collect();
        let said: Vec<&String> = found.iter().filter_map(|d| d.suggestions.first()).collect();
        println!("{text:?}\n  flagged {flagged:?}\n  offered {said:?}");
        assert!(flagged.is_empty(), "{flagged:?} in {text:?} should be known: {dir}");
    }
    // A misspelling of one, so that a suggestion is seen against a real dictionary too.
    let got = s.suggest("documant", 5);
    println!("documant → {got:?}");
    assert!(got.iter().any(|w| w == "document"), "{got:?}");
}

proptest::proptest! {
    /// Whatever it is given — combining marks, emoji, ligatures, unpaired brackets — checking,
    /// suggesting and asking about a word never panics, and every diagnostic is a range of the
    /// text it was found in.
    #[test]
    fn arbitrary_unicode_is_checked_without_a_panic(text in "\\PC{0,160}", word in "\\PC{0,40}") {
        let mut s = Speller::new();
        s.put(dic(Language::EnCa, "4\ndocument\ncolour\nthe\nand\n"));
        s.put(dic(Language::FrCa, "3\nfrançais\naujourd'hui\nMontréal\n"));
        s.set_personal(&["Hochelaga".into(), "œ".into()]);
        let found = s.check(&text);
        proptest::prop_assert!(found.len() <= MAX_DIAGNOSTICS);
        for d in &found {
            proptest::prop_assert!(d.span.fits(&text), "{:?} does not fit {:?}", d, text);
            proptest::prop_assert!(d.span.start < d.span.end);
            proptest::prop_assert!(text.is_char_boundary(d.span.start));
            proptest::prop_assert!(text.is_char_boundary(d.span.end));
            for said in &d.suggestions {
                proptest::prop_assert!(!said.is_empty());
                proptest::prop_assert!(!said.chars().any(char::is_control), "{said:?}");
            }
        }
        let offered = s.suggest(&word, 5);
        proptest::prop_assert!(offered.len() <= super::MAX_SUGGESTIONS);
        proptest::prop_assert!(!offered.iter().any(|w| w.is_empty()));
        let _ = s.is_word(&word);
        let _ = s.is_word(&text);
        let _ = s.accept(&text);
        let _ = super::words(&text);
    }

    /// The fast scan is the eager check without its suggestions, whatever the text is: one reader,
    /// two costs, and no place either of them finds that the other does not.
    #[test]
    fn a_scan_matches_the_check_on_any_text(text in "\\PC{0,160}") {
        let mut s = Speller::new();
        s.put(dic(Language::EnCa, "5\ndocument\ncolour\nthe\nand\nplease\n"));
        s.put(dic(Language::FrCa, "4\nl'\nj'\nhomme\nai\n"));
        s.set_personal(&["Hochelaga".into()]);
        let eager = s.check(&text);
        let fast = s.scan(&text);
        proptest::prop_assert_eq!(fast.len(), eager.len());
        for (f, e) in fast.iter().zip(eager.iter()) {
            proptest::prop_assert_eq!(f.span, e.span);
            proptest::prop_assert_eq!(f.kind, e.kind);
            proptest::prop_assert_eq!(f.source, e.source);
            proptest::prop_assert_eq!(&f.language, &e.language);
            proptest::prop_assert_eq!(&f.message, &e.message);
            proptest::prop_assert_eq!(&f.rule, &e.rule);
            proptest::prop_assert!(f.suggestions.is_empty(), "a scan keeps no suggestion");
            proptest::prop_assert!(f.span.fits(&text));
        }
        // A scan nobody calls off is the whole of the same answer, and one called off is no answer.
        let cancellable = s.scan_cancellable(&text, &|| false);
        proptest::prop_assert_eq!(cancellable.as_deref(), Some(fast.as_slice()));
        proptest::prop_assert!(s.scan_cancellable(&text, &|| true).is_none());
    }

    /// A `.dic` and its `.aff` written by somebody hostile: a line of any bytes loads without a
    /// panic, and what comes out is a word list within its caps.
    #[test]
    fn a_hostile_dictionary_loads_without_a_panic(dic_text in "\\PC{0,400}", aff_text in "\\PC{0,400}") {
        if let Ok(d) = Dictionary::from_hunspell(Language::EnCa, &aff_text, &dic_text) {
            proptest::prop_assert!(d.len() <= MAX_ENTRIES);
            for w in d.all() {
                proptest::prop_assert!(!w.is_empty());
                proptest::prop_assert!(w.chars().count() <= MAX_WORD.max(64));
                proptest::prop_assert!(!w.chars().any(char::is_control));
                // A flag an affix carries is not part of the word it makes, and no word is two.
                proptest::prop_assert!(!w.contains('/'));
                proptest::prop_assert!(!w.chars().any(char::is_whitespace));
            }
        }
    }
}
