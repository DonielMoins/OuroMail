// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Writing a message: recipients, subject, attachments and the body, in place of the reading pane.

use egui::{Align2, Rect, Sense, Stroke, Ui, pos2, vec2};
use mailcraft_engine::ComposeMode;
use mailcraft_model::proof::Language;
use serde_json::json;

use crate::proof::language_label;
use crate::theme::{Tokens, paint_clipped, regular};
use crate::{MailApp, icons};

pub(crate) mod editor;
#[cfg(test)]
mod font_tests;
#[cfg(test)]
mod picture_tests;
#[cfg(test)]
mod several_tests;
#[cfg(test)]
mod tests;

const LABEL_W: f32 = 62.0;

/// One labelled single-line field. Returns the new text when it was edited.
fn field(ui: &mut Ui, label: &str, text: &str, hint: &str, focus: bool) -> Option<String> {
    let at = line(ui, label, 0.0);
    let mut buf = text.to_string();
    let resp = crate::text_menu::field(ui.put(at, line_edit(&mut buf, ui.scope_id().with(("compose", label)), hint, at.width())));
    if focus {
        resp.request_focus();
    }
    resp.changed().then_some(buf)
}

/// A labelled line, with the rule under it; where its text goes. `right` is room kept free at
/// the line's right end for something drawn there (the Bcc link).
fn line(ui: &mut Ui, label: &str, right: f32) -> Rect {
    let t = Tokens::get(ui.ctx());
    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::hover());
    ui.painter().text(pos2(r.min.x + 16.0, r.center().y), Align2::LEFT_CENTER, label, regular(12.0), t.text_dim);
    ui.painter().line_segment([pos2(r.min.x + 16.0, r.max.y), pos2(r.max.x - 12.0, r.max.y)], Stroke::new(1.0, t.border));
    Rect::from_min_max(pos2(r.min.x + 16.0 + LABEL_W, r.min.y + 3.0), pos2(r.max.x - 12.0 - right, r.max.y - 3.0))
}

/// The text of a labelled line, `width` wide.
fn line_edit<'t>(text: &'t mut String, id: egui::Id, hint: &str, width: f32) -> egui::TextEdit<'t> {
    egui::TextEdit::singleline(text)
        .id(id)
        .frame(egui::Frame::NONE)
        .hint_text(hint)
        .font(regular(13.0))
        .align(egui::Align2::LEFT_CENTER)
        .desired_width(width)
}

/// What takes the place of the name being typed on a line of recipients when the suggestion
/// `who` is taken: the name and the address, then `; ` for the next one; after a space, unless it
/// is the line's `first`.
pub(crate) fn completed(first: bool, who: &mailcraft_engine::cmd::compose::Suggestion) -> String {
    let name: String = who.name.chars().filter(|c| !c.is_control() && !"\"<>".contains(*c)).collect();
    let name = name.trim();
    let written = if name.is_empty() || name.eq_ignore_ascii_case(&who.email) {
        who.email.clone()
    } else if name.contains([',', ';']) {
        format!("\"{name}\" <{}>", who.email)
    } else {
        format!("{name} <{}>", who.email)
    };
    let gap = if first { "" } else { " " };
    format!("{gap}{written}; ")
}

/// A line of recipients (a message's To, Cc and Bcc, a meeting's attendees), `edit`, given the
/// id `id`: as a name is typed in it, egui's list under it suggests who it could be (contacts,
/// then whoever mail was exchanged with). The arrow keys choose; Enter, Tab or a click takes one,
/// and the keyboard stays in the line, after it; Escape closes the list until something else is
/// typed. `offer`: whether anything is suggested now.
pub(crate) fn recipient_line(
    ui: &mut Ui,
    s: &mailcraft_engine::Session,
    id: egui::Id,
    edit: egui::TextEdit<'_>,
    offer: bool,
) -> egui::CompletionOutput {
    let ctx = ui.ctx().clone();
    let shown = id.with("suggestions_shown");
    // The line keeps Tab while the list shows, for the list (below); Shift+Tab still goes back.
    if ctx.data(|d| d.get_temp::<bool>(shown)).unwrap_or(false)
        && ctx.memory(|m| m.has_focus(id))
        && ctx.input_mut(|i| i.consume_key(egui::Modifiers::SHIFT, egui::Key::Tab))
    {
        ctx.memory_mut(|m| m.move_focus(egui::FocusDirection::Previous));
    }
    let filter = edit.get_event_filter();
    let mut out = egui::CompletionPopup::new(id).word_boundary(|c| c == ';' || c == ',').align(egui::RectAlign::BOTTOM_START).show(ui, edit, |q| {
        let typed = q.word.trim_start();
        if !offer || typed.trim().is_empty() {
            return Vec::new();
        }
        suggestions(&ctx, s, id, typed)
            .into_iter()
            .map(|who| {
                let row = egui::Suggestion::new(completed(q.is_at_start(), &who));
                if who.name.is_empty() { row.content(who.email) } else { row.content(who.name).description(who.email) }
            })
            .collect()
    });
    // The line keeps the keyboard on Escape and Tab while the list is open, for the list, and only
    // then. egui's list asks for it as the list was in the pass before, a pass late both ways: an
    // Escape right after the list came up would go to the window as well, and a Tab right after it
    // closed would be nobody's. (egui changes the filter only of a line that had the keyboard in
    // the pass before.)
    out.text_edit.response.response = crate::text_menu::field(out.text_edit.response.response);
    let open = out.is_open;
    ctx.memory_mut(|m| m.set_focus_lock_filter(id, egui::EventFilter { tab: filter.tab || open, escape: filter.escape || open, ..filter }));
    ctx.data_mut(|d| d.insert_temp(shown, out.is_open));
    out
}

/// The recipients suggested for `typed` on the line `id`. They are asked for again only when what is
/// typed or the mailbox changes ([`Session::rev`], [`Session::openings`]): a line with the
/// keyboard is drawn at least twice a second (its cursor blinks), and asking reads the whole
/// mailbox.
///
/// [`Session::rev`]: mailcraft_engine::Session::rev
/// [`Session::openings`]: mailcraft_engine::Session::openings
fn suggestions(ctx: &egui::Context, s: &mailcraft_engine::Session, id: egui::Id, typed: &str) -> Vec<mailcraft_engine::cmd::compose::Suggestion> {
    type Asked = ((String, u64, u64), Vec<mailcraft_engine::cmd::compose::Suggestion>);
    let kept = id.with("suggestions");
    let asking = (typed.to_string(), s.rev(), s.openings());
    if let Some((at, found)) = ctx.data(|d| d.get_temp::<Asked>(kept))
        && at == asking
    {
        return found;
    }
    let found = mailcraft_engine::cmd::compose::suggest(s, typed, 6);
    ctx.data_mut(|d| d.insert_temp::<Asked>(kept, (asking, found.clone())));
    found
}

/// Whether the quiet Bcc link is shown (no Bcc line yet).
fn bcc_link(app: &MailApp) -> bool {
    app.session.compose.as_ref().is_some_and(|c| c.bcc.is_empty()) && !app.bcc_shown()
}

/// A recipient line, with the recipient suggestions under it ([`recipient_line`]).
fn recipient_field(app: &mut MailApp, ui: &mut Ui, key: &str, label: &str, text: &str, hint: &str, focus: bool) {
    let id = ui.scope_id().with(("compose", label));
    // The Bcc link sits at the right end of the To line: a click there is for it, so the line's
    // text stops short of it (a long To line was otherwise impossible to click at its end).
    let right = if key == "to" && bcc_link(app) { 44.0 } else { 0.0 };
    let at = line(ui, label, right);
    let mut buf = text.to_string();
    let edit = line_edit(&mut buf, id, hint, at.width());
    let layout = egui::Layout::centered_and_justified(egui::Direction::TopDown);
    let out = ui.scope_builder(egui::UiBuilder::new().max_rect(at).layout(layout), |ui| recipient_line(ui, &app.session, id, edit, true)).inner;
    if focus {
        out.text_edit.response.request_focus();
    }
    // (A suggestion taken changes the text after the line has said whether it changed.)
    if out.accepted.is_some() || out.text_edit.response.changed() {
        let _ = app.execute("compose.set", json!({ key: buf }));
    }
}

/// What the window shows of the message being written besides its body (copied out of the
/// session once a frame, so the commands it runs can change the message meanwhile).
struct Shown {
    /// The draft this is, by its id: its own languages are the ones its footer shows.
    id: u64,
    mode: ComposeMode,
    dirty: bool,
    draft: bool,
    account: u64,
    to: String,
    cc: String,
    bcc: String,
    reply_to: String,
    subject: String,
    /// Each attachment's name, as it is written to a file (a forward carries the sender's), and size.
    attachments: Vec<(String, String)>,
    quote: bool,
    /// The languages proofing checks this draft against: its own choice over the defaults.
    languages: Vec<Language>,
}

impl Shown {
    fn of(app: &MailApp) -> Option<Shown> {
        let c = app.session.compose.as_ref()?;
        Some(Shown {
            id: c.id,
            mode: c.mode,
            dirty: c.dirty,
            draft: c.draft.is_some(),
            account: c.account,
            to: c.to.clone(),
            cc: c.cc.clone(),
            bcc: c.bcc.clone(),
            reply_to: c.reply_to.clone(),
            subject: c.subject.clone(),
            attachments: c
                .attachments
                .iter()
                .map(|a| (mailcraft_engine::opening::file_name(&a.name), mailcraft_model::size_label_in(crate::i18n::current(), a.size() as u64)))
                .collect(),
            quote: c.quote.is_some(),
            languages: app.session.proof_settings_for(c.id).languages,
        })
    }
}

/// The message being written in the reading pane.
pub fn show(app: &mut MailApp, ui: &mut Ui) {
    show_in(app, ui, None, true);
}

/// The current message being written: in the reading pane, or in the window of its own numbered
/// `window` (#286), which has its own way to close (and nothing to pop out of). `keyboard`: it may
/// take the keyboard when it has just opened and nothing has it (not one that opened behind the
/// owner's window).
pub(crate) fn show_in(app: &mut MailApp, ui: &mut Ui, window: Option<u64>, keyboard: bool) {
    let t = Tokens::get(ui.ctx());
    let Some(c) = Shown::of(app) else { return };
    ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
    // The pane as the composer found it, before its own lines grow the ui past it: the footer is
    // drawn under them, and it is kept inside this width rather than in that overhang.
    let pane = ui.available_width().max(0.0);

    // In the reading pane, what this is and the actions; a window of its own has them in its
    // ribbon, and Send beside the lines, as a message's window has it (#286).
    if window.is_none() {
        action_bar(app, ui, &c, window);
    }
    // A message that just opened has the keyboard: To for a new one, the body for a reply. Not
    // while a dialog is over it: what is typed would go under the dialog, and the keyboard goes
    // back to what had it when the dialog closes (#406).
    let fresh = keyboard && !c.dirty && ui.memory(|m| m.focused().is_none() && m.allows_interaction(ui.layer_id()));

    let focus_to = fresh && c.to.is_empty();

    if window.is_some() {
        ui.horizontal_top(|ui| {
            send_beside(app, ui);
            ui.vertical(|ui| header(app, ui, &c, focus_to));
        });
    } else {
        header(app, ui, &c, focus_to);
    }

    // Attachments, each with a way to take it off again.
    if !c.attachments.is_empty() {
        egui::Frame::NONE.inner_margin(egui::Margin { left: 16, right: 12, top: 6, bottom: 6 }).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
                for (i, (name, size)) in c.attachments.iter().enumerate() {
                    let label = format!("{name}  ({size})");
                    let w = ui.ctx().fonts_mut(|f| f.layout_no_wrap(label.clone(), regular(12.0), t.text).size().x).min(260.0);
                    let (r, _) = ui.allocate_exact_size(vec2(w + 52.0, 26.0), Sense::hover());
                    ui.painter().rect(r, 5.0, t.panel, Stroke::new(1.0, t.border), egui::StrokeKind::Inside);
                    icons::paint(
                        ui.painter(),
                        Rect::from_center_size(pos2(r.min.x + 13.0, r.center().y), vec2(16.0, 16.0)),
                        "attach",
                        t.icon,
                        t.accent,
                    );
                    paint_clipped(ui.painter(), pos2(r.min.x + 26.0, r.center().y), w, &label, regular(12.0), t.text);
                    let x = Rect::from_center_size(pos2(r.max.x - 12.0, r.center().y), vec2(18.0, 18.0));
                    let xr = ui.interact(x, ui.scope_id().with(("unattach", i)), Sense::click());
                    if xr.hovered() {
                        ui.painter().rect_filled(x, 3.0, t.hover);
                    }
                    icons::paint(ui.painter(), x.shrink(4.0), "close", t.text_dim, t.accent);
                    if xr.on_hover_text(tl!("Remove")).clicked() {
                        let _ = app.execute("compose.removeAttachment", json!({"index": i}));
                    }
                }
            });
        });
        let r = ui.available_rect_before_wrap();
        ui.painter().line_segment([pos2(r.min.x + 16.0, r.min.y), pos2(r.max.x - 12.0, r.min.y)], Stroke::new(1.0, t.border));
    }

    // The languages this draft alone is proofread in: a fixed strip under the body, before the
    // ScrollArea takes the rest, so that a long message scrolls under it rather than past it.
    proof_languages(app, ui, pane, c.id, &c.languages);

    // The body takes what is left: the owner's text, and under it the original of a reply or
    // forward in HTML, as the reading pane shows it.
    egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("compose_body").show(ui, |ui| {
        egui::Frame::NONE.inner_margin(egui::Margin { left: 16, right: 16, top: 10, bottom: 16 }).show(ui, |ui| {
            let below = c.quote || editor::unedited_html_signature(app).is_some();
            let room = if below { 8.0 * 19.0 } else { (ui.available_height() - 26.0).max(8.0 * 19.0) };
            editor::show(app, ui, room, fresh && !focus_to);
            signature(app, ui);
            if c.quote {
                ui.add_space(8.0);
                // The original senses the pointer over all of the view it is drawn in: kept to
                // below where it begins, so that it doesn't lie over the owner's text and take a
                // click meant for it (the body then never got the keyboard back, #158).
                let below = Rect::everything_below(ui.cursor().min.y);
                ui.scope(|ui| {
                    ui.set_clip_rect(ui.clip_rect().intersect(below));
                    crate::reading::show_quote(app, ui);
                });
            }
        });
    });
}

/// The width of `text` in `font`, as the painter lays it out.
fn text_width(ui: &Ui, text: &str, font: egui::FontId) -> f32 {
    ui.ctx().fonts_mut(|f| f.layout_no_wrap(text.to_string(), font, egui::Color32::WHITE).size().x)
}

/// How tall the bar over the lines of a message is, and the room kept at its ends.
const BAR_H: f32 = 44.0;
const BAR_SIDE: f32 = 12.0;
/// The room between two of the bar's controls.
const BAR_GAP: f32 = 6.0;
/// What a bare icon button of the bar (`close`, `popOut`, `…`) takes: `icon_button`'s size at 14.
const ICON_W: f32 = 22.0;
const ICON_H: f32 = 20.0;
/// The least the title and its state are elided into before they give way to the actions: a
/// title too short to say anything is dropped rather than drawn as an ellipsis.
const TITLE_MIN: f32 = 48.0;
/// The room a bar wide enough for the whole row keeps for the title and its state.
const TITLE_ROOM: f32 = 96.0;
/// The footer's `▾`: a part of the selector of its own, never a part of its caption, so that a
/// caption elided to fit cannot take the arrow with it.
const ARROW_W: f32 = 8.0;
const ARROW_ROOM: f32 = 18.0;

/// Put `add`'s widget at `at`: the bar's pieces keep the size the shared widgets give them, and
/// the bar decides where each of them stands.
fn at(ui: &mut Ui, at: Rect, add: impl FnOnce(&mut Ui) -> egui::Response) -> egui::Response {
    ui.scope_builder(egui::UiBuilder::new().max_rect(at).layout(egui::Layout::left_to_right(egui::Align::Center)), add).inner
}

/// The languages this draft alone is proofread in, at the bottom of its composer.
///
/// The selector opens all four languages; each one is chosen or unchosen, so English and French
/// are chosen together. One is always left on: unchoosing the last is refused rather than sent as
/// an empty list. A choice writes the draft and only the draft — `compose.set` with its id, never
/// the global `proof.set` — and starts no dictionary, no download and no server.
///
/// It is a panel inside the composer's own layout, drawn before the body's ScrollArea, so it stays
/// under the scrolling body in the reading pane and in a window of its own alike. It is painted
/// and interacted with rather than focused: drawing it, or choosing at it, asks no keyboard.
///
/// Its `▾` is drawn apart from the languages' names, in the room kept for it at the selector's
/// right end: the names give way on a narrow pane, the arrow never does.
fn proof_languages(app: &mut MailApp, ui: &mut Ui, pane: f32, id: u64, languages: &[Language]) {
    let t = Tokens::get(ui.ctx());
    egui::Panel::bottom(("compose_proof_languages", id)).exact_size(32.0).frame(egui::Frame::NONE.fill(t.panel)).show(ui, |ui| {
        // What is left of the pane before the body's lines, never more than the pane shows and
        // never more than the composer found: a ui can grow past its pane, and the selector is
        // not to be drawn in that overhang.
        let r = ui.available_rect_before_wrap().intersect(ui.clip_rect());
        let r = Rect::from_min_max(r.min, pos2((r.min.x + pane).min(r.max.x), r.max.y));
        ui.painter().line_segment([pos2(r.min.x, r.min.y), pos2(r.max.x, r.min.y)], Stroke::new(1.0, t.border));
        // The room kept at the selector's ends gives way on a pane too narrow to keep it: the
        // arrow's room comes first, so that `▾` is painted in whatever is left rather than past
        // the pane, as the captions were.
        let keep = (ARROW_ROOM + 8.0).min(r.width().max(0.0));
        let spare = (r.width() - keep).max(0.0);
        let left = 16.0_f32.min(spare);
        let right = 12.0_f32.min((spare - left).max(0.0));
        let at = Rect::from_min_max(pos2(r.min.x + left, r.min.y + 4.0), pos2(r.max.x - right, r.max.y - 4.0));
        ui.scope_builder(egui::UiBuilder::new().max_rect(at).layout(egui::Layout::left_to_right(egui::Align::Center)), |ui| {
            ui.spacing_mut().item_spacing = vec2(8.0, 0.0);
            // On a narrow reading pane the selector is the control, and the caption can give
            // way; neither the hit rectangle nor the list of all four choices may leave it.
            if ui.available_width() >= 240.0 {
                ui.label(egui::RichText::new(tl!("Proofing languages")).font(regular(11.5)).color(t.text_dim));
            }
            let names: Vec<&str> =
                Language::ALL.iter().filter(|language| languages.contains(language)).map(|language| language_label(*language)).collect();
            let label = names.join(", ");
            let w = text_width(ui, &label, regular(12.0)).min(420.0);
            // (The arrow's room is kept whether or not the names have any left to elide.)
            let width = (w + 8.0 + ARROW_ROOM + 8.0).min(ui.available_width().max(0.0));
            let (slot, _) = ui.allocate_exact_size(vec2(width, 24.0), Sense::hover());
            let resp = ui.interact(slot, ui.scope_id().with(("compose_proof_languages_menu", id)), Sense::click());
            app.hit.add_key(format!("proofComposeLanguages:{id}"), slot);
            if resp.hovered() {
                ui.painter().rect_filled(slot, 4.0, t.hover);
            }
            // The names, elided to what their room can hold...
            let caption = (width - ARROW_ROOM - 16.0).max(0.0);
            paint_clipped(ui.painter(), pos2(slot.min.x + 8.0, slot.center().y), caption, &label, regular(12.0), t.text);
            // ...and the arrow, drawn on its own at the slot's right end: a `▾` cut into a caption
            // elided to fit was the clipped affordance the narrow pane showed.
            let arrow = Rect::from_center_size(pos2(slot.max.x - 12.0, slot.center().y), vec2(ARROW_W, ARROW_W)).intersect(slot);
            app.hit.add_key(format!("proofComposeArrow:{id}"), arrow);
            icons::paint(ui.painter(), arrow, "dropdown", t.text, t.text);
            resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, label.clone()));
            let mut chosen = None;
            // Choosing one language leaves the list open, so another can be chosen beside it.
            egui::Popup::menu(&resp).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
                ui.set_min_width(220.0);
                for language in Language::ALL {
                    let on = languages.contains(&language);
                    let row = ui.selectable_label(on, language_label(language));
                    app.hit.add_key(format!("proofComposeLanguage:{id}:{}", language.code()), row.rect);
                    if row.clicked() {
                        chosen = Some(language);
                    }
                }
            });
            let Some(choice) = chosen else { return };
            let mut next: Vec<Language> = languages.to_vec();
            if next.contains(&choice) {
                next.retain(|language| *language != choice);
            } else {
                next.push(choice);
            }
            // Proofing always has one language: the last one stays, and nothing is sent.
            if !next.is_empty() {
                let codes: Vec<&str> = next.iter().map(|language| language.code()).collect();
                let _ = app.execute("compose.set", json!({ "compose": id, "proofLanguages": codes }));
            }
        });
    });
}

/// What the bar's row holds, from the widest arrangement to the least: each draws only the
/// controls it has measured room for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Arrangement {
    /// Send, the three actions, Pop Out and Close, with room kept for the title.
    Wide,
    /// Send, `…` and Close, with Pop Out beside Close while it fits.
    Narrow,
    /// `…` and Close: Send and Pop Out are behind `…`.
    Tight,
    /// One `…` holding every action, Close included: the pane has less room than `…` and Close.
    Menu,
}

/// What the message is and its actions, over its lines in the reading pane: Send, Attach, Save
/// draft, Discard, and at the right Close and Pop Out.
///
/// The way out is reserved first, at the row's right end: Close always, Pop Out beside it while
/// both fit. A pane narrow enough that the whole row cannot stand in it keeps Send, `…` and the
/// way out, and gives the other three up to `…` beside Send; a pane too narrow for that keeps `…`
/// and Close and gives Send and Pop Out to `…` as well; and a pane too narrow even for those two
/// icons keeps one `…` holding every action, Close included. Nothing is drawn outside the bar or
/// over another control, however little the pane leaves: every width here is the room there is.
/// The title and its state give way first, elided to the room left, and dropped when that room
/// says nothing.
fn action_bar(app: &mut MailApp, ui: &mut Ui, c: &Shown, window: Option<u64>) {
    let t = Tokens::get(ui.ctx());
    let (bar, _) = ui.allocate_exact_size(vec2(ui.available_width().max(0.0), BAR_H), Sense::hover());
    app.hit.add_key(format!("composeActionBar:{}", c.id), bar);
    let key = |what: &str| format!("composeAction:{}:{what}", c.id);
    let menu_key = |what: &str| format!("composeActionMenu:{}:{what}", c.id);
    // The row is the bar less the room kept at its ends; that room gives way too when keeping it
    // would leave no room for even one icon, so the row's width is never negative.
    let pad = BAR_SIDE.min(((bar.width() - ICON_W) / 2.0).max(0.0)).max(0.0);
    let row = Rect::from_min_max(pos2(bar.min.x + pad, bar.min.y + (BAR_H - 28.0) / 2.0), pos2(bar.max.x - pad, bar.max.y - (BAR_H - 28.0) / 2.0));
    let title = match c.mode {
        ComposeMode::New => tl!("New message"),
        ComposeMode::Reply => tl!("Reply"),
        ComposeMode::ReplyAll => tl!("Reply all"),
        ComposeMode::Forward => tl!("Forward"),
        ComposeMode::Draft => tl!("Draft"),
    };
    let state = if c.dirty {
        tl!("Not saved")
    } else if c.draft {
        tl!("Saved to Drafts")
    } else {
        ""
    };
    let title = if state.is_empty() { title.to_string() } else { format!("{title}  {state}") };

    // What every piece of the row takes, measured as the shared widgets will draw them.
    let send_label = tl!("Send");
    let send_w = text_width(ui, send_label, regular(12.5)).max(44.0) + 26.0;
    let send_sc = crate::widgets::shortcut_text(app, "compose.send");
    let secondary: Vec<(&str, &str, &str, bool)> = vec![
        ("attach", tl!("Attach"), "ui.attach", true),
        ("saveDraft", tl!("Save draft"), "compose.saveDraft", c.dirty || !c.draft),
        ("discard", tl!("Discard"), "compose.discard", true),
    ];
    let widths: Vec<f32> = secondary.iter().map(|(_, label, ..)| text_width(ui, label, regular(12.0)) + 34.0).collect();
    let full: f32 = send_w + widths.iter().sum::<f32>() + BAR_GAP * widths.len() as f32;
    let title_w = text_width(ui, &title, regular(11.5));
    // The way out: Close, and Pop Out beside it, at the bar's right end. A window of its own has
    // its own way to close, and nothing to pop out of.
    let (close_w, pop_w) = if window.is_none() { (ICON_W, ICON_W) } else { (0.0, 0.0) };
    let out = if pop_w > 0.0 { close_w + BAR_GAP + pop_w } else { close_w };
    let w = row.width().max(0.0);
    let wide = full + out + BAR_GAP + TITLE_ROOM <= w;
    // Send, `…` and Close, and Pop Out beside Close; then `…` and Close alone; then one `…`.
    let send_more_close = send_w + BAR_GAP + ICON_W + BAR_GAP + close_w;
    let keeps_pop_out = pop_w > 0.0 && w >= send_more_close + BAR_GAP + pop_w;
    let tight = ICON_W + BAR_GAP + close_w;
    let arrangement = if wide {
        Arrangement::Wide
    } else if w >= send_more_close {
        Arrangement::Narrow
    } else if w >= tight {
        Arrangement::Tight
    } else {
        Arrangement::Menu
    };

    let mut x = row.min.x;
    if matches!(arrangement, Arrangement::Wide | Arrangement::Narrow) {
        let send = at(ui, Rect::from_min_size(pos2(x, row.min.y), vec2(send_w, row.height())), |ui| {
            crate::widgets::primary_button(ui, send_label, true).on_hover_text(format!("{send_label} ({send_sc})"))
        });
        app.hit.add_key(key("send"), send.rect);
        if send.clicked() {
            let _ = app.run("compose.send", json!({}));
        }
        x = send.rect.max.x + BAR_GAP;
    }

    if arrangement == Arrangement::Wide {
        for (i, (what, label, command, enabled)) in secondary.iter().enumerate() {
            let w = widths.get(i).copied().unwrap_or(0.0);
            let resp = at(ui, Rect::from_min_size(pos2(x, row.min.y), vec2(w, row.height())), |ui| {
                crate::widgets::icon_text_button(ui, what, label, *enabled)
            });
            app.hit.add_key(key(what), resp.rect);
            if resp.clicked() {
                let _ = app.run(command, json!({}));
            }
            x = resp.rect.max.x + BAR_GAP;
        }
    } else {
        // `…` right of Send, or at the row's left end when Send is behind it; as wide as the row
        // has, when that is less than an icon.
        let left = if arrangement == Arrangement::Narrow { x } else { row.min.x };
        let menu = Rect::from_min_size(pos2(left, row.min.y), vec2(ICON_W.min(row.width()).max(0.0), row.height()));
        let more = if menu.width() >= ICON_W {
            at(ui, menu, |ui| crate::widgets::icon_button(ui, "more", tl!("More commands"), 14.0, true))
        } else {
            // A bar narrower than one icon: the same mark, pressed in the room there is.
            let resp = ui.interact(menu, ui.scope_id().with(("compose_more", c.id)), Sense::click()).on_hover_text(tl!("More commands"));
            if resp.hovered() {
                ui.painter().rect_filled(menu, 4.0, t.hover);
            }
            let ink = if resp.hovered() { t.text } else { t.icon };
            icons::paint(ui.painter(), Rect::from_center_size(menu.center(), vec2(14.0, 14.0)).intersect(menu), "more", ink, t.accent);
            resp
        };
        app.hit.add_key(key("more"), more.rect);
        // Named for screen readers, as the actions it holds are.
        more.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, tl!("More commands")));
        x = more.rect.max.x + BAR_GAP;
        // What the row cannot hold, in the order the row draws it; Close joins them only when the
        // row has no room to draw it.
        let mut folded: Vec<(&str, &str, &str, bool)> = Vec::new();
        if arrangement != Arrangement::Narrow {
            folded.push(("send", send_label, "compose.send", true));
        }
        folded.extend(secondary.iter().copied());
        if pop_w > 0.0 && !keeps_pop_out {
            folded.push(("popOut", tl!("Pop Out"), "compose.popOut", true));
        }
        if arrangement == Arrangement::Menu && close_w > 0.0 {
            folded.push(("close", tl!("Close"), "compose.close", true));
        }
        egui::Popup::menu(&more).show(|ui| {
            ui.set_min_width(200.0);
            for (what, label, command, enabled) in &folded {
                let row = ui.add_enabled(*enabled, egui::Button::new(*label).min_size(vec2(190.0, 0.0)));
                app.hit.add_key(menu_key(what), row.rect);
                if row.clicked() {
                    let _ = app.run(command, json!({}));
                    ui.close();
                }
            }
        });
    }

    // The way out, at the right end: Close, with Pop Out beside it. A bar with room for neither
    // keeps them in `…` instead.
    let mut right = row.max.x;
    if close_w > 0.0 && arrangement != Arrangement::Menu {
        let close = at(ui, Rect::from_min_size(pos2(right - ICON_W, row.center().y - ICON_H / 2.0), vec2(ICON_W, ICON_H)), |ui| {
            crate::widgets::icon_button(ui, "close", tl!("Close"), 14.0, true)
        });
        app.hit.add_key(key("close"), close.rect);
        if close.clicked() {
            let _ = app.run("compose.close", json!({}));
        }
        right = close.rect.min.x - BAR_GAP;
        if keeps_pop_out {
            let pop = at(ui, Rect::from_min_size(pos2(right - ICON_W, row.center().y - ICON_H / 2.0), vec2(ICON_W, ICON_H)), |ui| {
                crate::widgets::icon_button(ui, "popOut", tl!("Pop Out"), 14.0, true)
            });
            app.hit.add("composePopOut", 0, pop.rect);
            app.hit.add_key(key("popOut"), pop.rect);
            if pop.clicked() {
                let _ = app.run("compose.popOut", json!({}));
            }
            right = pop.rect.min.x - BAR_GAP;
        }
    }

    // The title and its state, right-aligned in what is left between the actions and the way
    // out: elided to that room, and never drawn as little more than an ellipsis. It was a label
    // before the bar was painted; it is one to a screen reader still, and it only senses the
    // pointer over the words it drew.
    let room = (right - x).max(0.0);
    if room >= TITLE_MIN {
        let width = room.min(title_w);
        let drawn = paint_clipped(ui.painter(), pos2(right - width, row.center().y), width, &title, regular(11.5), t.text_dim);
        if drawn > 0.0 {
            let at = Rect::from_min_size(pos2(right - drawn, row.center().y - 8.0), vec2(drawn, 16.0));
            let resp = ui.interact(at, ui.scope_id().with(("compose_title", c.id)), Sense::hover());
            resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Label, true, &title));
            app.hit.add_key(key("title"), at);
        }
    }
    ui.painter().line_segment([pos2(bar.min.x, bar.max.y), pos2(bar.max.x, bar.max.y)], Stroke::new(1.0, t.border));
}

/// Send, beside the lines of a message in a window of its own, as tall as To and Cc.
fn send_beside(app: &mut MailApp, ui: &mut Ui) {
    let t = Tokens::get(ui.ctx());
    let label = tl!("Send");
    let (slot, _) = ui.allocate_exact_size(vec2(84.0, 84.0), Sense::hover());
    let r = Rect::from_min_size(slot.min + vec2(12.0, 8.0), vec2(64.0, 68.0));
    let resp = ui.interact(r, ui.scope_id().with("compose_send_beside"), Sense::click());
    app.hit.add("composeSend", 0, r);
    let fill = if resp.hovered() { t.accent_text } else { t.accent };
    ui.painter().rect_filled(r, 6.0, fill);
    icons::paint(ui.painter(), Rect::from_center_size(pos2(r.center().x, r.min.y + 24.0), vec2(20.0, 20.0)), "send", t.on_accent, t.on_accent);
    ui.painter().text(pos2(r.center().x, r.max.y - 16.0), Align2::CENTER_CENTER, label, crate::theme::medium(12.5), t.on_accent);
    let sc = crate::widgets::shortcut_text(app, "compose.send");
    let resp = resp.on_hover_text(format!("{label} ({sc})"));
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, label));
    if resp.clicked() {
        let _ = app.run("compose.send", json!({}));
    }
}

/// The lines of the message: From (when there is a choice), To, Cc, Bcc, Reply to and Subject.
fn header(app: &mut MailApp, ui: &mut Ui, c: &Shown, focus_to: bool) {
    let t = Tokens::get(ui.ctx());
    let top = ui.cursor().min.y;
    // From, when there is a choice.
    if app.session.mailbox.accounts.len() > 1 {
        let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::hover());
        ui.painter().text(pos2(r.min.x + 16.0, r.center().y), Align2::LEFT_CENTER, tl!("From"), regular(12.0), t.text_dim);
        let current = app.session.mailbox.account(c.account).map(|a| a.address().full()).unwrap_or_default();
        let pick = Rect::from_min_max(pos2(r.min.x + 16.0 + LABEL_W, r.min.y + 4.0), pos2(r.max.x - 12.0, r.max.y - 4.0));
        let resp = ui.interact(pick, ui.scope_id().with("compose_from"), Sense::click());
        if resp.hovered() {
            ui.painter().rect_filled(pick.expand2(vec2(4.0, 0.0)), 4.0, t.hover);
        }
        paint_clipped(ui.painter(), pos2(pick.min.x, pick.center().y), pick.width(), &format!("{current} ▾"), regular(13.0), t.text);
        // (In the order of the folder pane, #702.)
        let accounts: Vec<(u64, String)> = app.session.mailbox.accounts_in_order().into_iter().map(|a| (a.id, a.address().full())).collect();
        egui::Popup::menu(&resp).show(|ui| {
            for (id, label) in accounts {
                if ui.selectable_label(id == c.account, label).clicked() {
                    let _ = app.execute("compose.set", json!({"account": id}));
                    ui.close();
                }
            }
        });
        ui.painter().line_segment([pos2(r.min.x + 16.0, r.max.y), pos2(r.max.x - 12.0, r.max.y)], Stroke::new(1.0, t.border));
    }
    let hint = tl!("Names or addresses, separated by ;");
    for (key, label, text, show) in
        [("to", "To", &c.to, true), ("cc", "Cc", &c.cc, true), ("bcc", "Bcc", &c.bcc, !c.bcc.is_empty() || app.bcc_shown())]
    {
        if !show {
            continue;
        }
        recipient_field(app, ui, key, tl!(label), text, hint, key == "to" && focus_to);
    }
    // (Shown once Direct Replies To has set it.)
    if !c.reply_to.is_empty() {
        recipient_field(app, ui, "replyTo", tl!("Reply to"), &c.reply_to, hint, false);
    }
    if let Some(new) = field(ui, tl!("Subject"), &c.subject, "", false) {
        let _ = app.execute("compose.set", json!({"subject": new}));
    }
    if bcc_link(app) {
        // A quiet way to get the Bcc line.
        let last = ui.min_rect();
        let br =
            Rect::from_min_size(pos2(last.max.x - 52.0, top + if app.session.mailbox.accounts.len() > 1 { 34.0 } else { 4.0 }), vec2(40.0, 22.0));
        let resp = ui.interact(br, ui.scope_id().with("show_bcc"), Sense::click());
        ui.painter().text(br.center(), Align2::CENTER_CENTER, tl!("Bcc"), regular(11.5), if resp.hovered() { t.accent_text } else { t.text_dim });
        if resp.clicked()
            && let Some(id) = app.session.compose_id()
        {
            app.compose_bcc.insert(id);
        }
    }
}

/// An HTML signature as it goes out, under the owner's text in place of its simpler copy
/// ([`editor::html_signature`]): read-only, its pictures on the web not fetched, with a way to
/// edit it as text instead, and back while that copy is unedited.
fn signature(app: &mut MailApp, ui: &mut Ui) {
    let ctx = ui.ctx().clone();
    let Some(html) = editor::unedited_html_signature(app).map(str::to_string) else { return };
    if editor::html_signature(app).is_none() {
        let back = ui
            .link(tl!("Show signature as it is sent"))
            .on_hover_text(tl!("Shows the signature as it was designed again, as long as its text is unchanged"));
        app.hit.add("composeSignatureAsHtml", 0, back.rect);
        if back.clicked() {
            let _ = app.run("compose.signatureAsText", json!({"value": false}));
        }
        return;
    }
    let key = egui::Id::unique("mailcraft-compose-signature").with(&html);
    let message = match ctx.data(|d| d.get_temp::<std::sync::Arc<mailcraft_model::Message>>(key)) {
        Some(m) => m,
        None => {
            // As it is sent: cleaned.
            let m = std::sync::Arc::new(crate::signatures::message_of(mailcraft_engine::html::quotable(&html), &[], |_| String::new()));
            ctx.data_mut(|d| d.insert_temp(key, m.clone()));
            m
        }
    };
    let top = ui.cursor().min.y;
    // Kept to below where it begins, as the quote is, so that it takes no click meant for the text.
    let below = Rect::everything_below(top);
    ui.scope(|ui| {
        ui.set_clip_rect(ui.clip_rect().intersect(below));
        crate::reading::show_plain_view(app, ui, &message, crate::reading::Place::Preview);
    });
    let shown = Rect::from_min_max(pos2(ui.min_rect().min.x, top), pos2(ui.min_rect().max.x, ui.cursor().min.y));
    app.hit.add("composeSignature", 0, shown);
    let edit = ui
        .link(tl!("Edit signature as text"))
        .on_hover_text(tl!("The signature is then sent as the simpler text shown here, not as it was designed"));
    app.hit.add("composeSignatureAsText", 0, edit.rect);
    if edit.clicked() {
        let _ = app.run("compose.signatureAsText", json!({"value": true}));
    }
}

#[cfg(test)]
#[path = "recipient_suggest_tests.rs"]
mod recipient_suggest_tests;
