// SPDX-FileCopyrightText: MailCraft contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! The composer keeps a paragraph's layout, not its glyphs' place in the font atlas. A pass that
//! rebuilds the atlas must not leave the body painting coordinates that point into the old one;
//! a pass that changes the faces must not keep rows measured from the old ones.

use std::sync::Arc;

use egui::text::LayoutJob;
use egui::{Galley, RawInput};
use mailcraft_model::rich::{Paragraph, Run, Style};

use super::*;

/// A context with its fonts ready, as after a frame.
fn ctx() -> egui::Context {
    let ctx = egui::Context::default();
    ctx.run_ui(RawInput::default(), |_| {}).textures_delta.clear();
    ctx
}

fn paragraph(text: &str) -> Paragraph {
    Paragraph { runs: vec![Run::plain(text)], ..Default::default() }
}

fn key(ctx: &egui::Context, width: f32) -> LayoutKey {
    LayoutKey {
        width: width as u32,
        dark: false,
        scale: ctx.pixels_per_point().to_bits(),
        plain: false,
        pictures: 0,
        families: ctx.fonts(|f| f.families().len()),
    }
}

fn frame(ctx: &egui::Context, mut run: impl FnMut(&egui::Context)) {
    ctx.run_ui(RawInput::default(), |ui| run(ui.ctx())).textures_delta.clear();
}

/// Every glyph's place in the atlas, in paint order.
fn uv_rects(galley: &Galley) -> Vec<[u16; 4]> {
    galley.rows.iter().flat_map(|row| row.glyphs.iter().map(|g| [g.uv_rect.min[0], g.uv_rect.min[1], g.uv_rect.max[0], g.uv_rect.max[1]])).collect()
}

/// The geometry a paragraph's galley gives the body — its rows, its height, and where the caret
/// stands at each character — as bits, so two layouts can be compared exactly.
fn geometry(galley: &Galley, chars: usize) -> (usize, u32, Vec<(u32, u32)>) {
    let at = |i: usize| galley.pos_from_cursor(CCursor { index: i.into(), prefer_next_row: true }).min;
    (galley.rows.len(), galley.size().y.to_bits(), (0..=chars).map(|i| (at(i).x.to_bits(), at(i).y.to_bits())).collect())
}

#[test]
fn a_rebuilt_font_atlas_cannot_leave_the_body_painting_stale_glyphs() {
    let ctx = ctx();
    let paragraphs =
        vec![paragraph("The quick brown fox jumps over the lazy dog."), paragraph("A second paragraph keeps two line boxes in the body.")];
    let key = key(&ctx, 600.0);
    let mut layout = Layout::default();
    frame(&ctx, |ctx| {
        layout.sync(ctx, &paragraphs, &[], key, &Tokens::light(), 600.0);
    });
    let jobs: Vec<Arc<LayoutJob>> = layout.paras.iter().map(|p| p.galley.job.clone()).collect();
    let before: Vec<Vec<[u16; 4]>> = layout.paras.iter().map(|p| uv_rects(&p.galley)).collect();
    let kept: Vec<_> = layout.paras.iter().map(|p| geometry(&p.galley, p.chars)).collect();
    let laid = layout.laid;

    // Rebuild the atlas without changing a face or a metric, as egui does when the text options
    // change: every galley laid out before then points into the atlas that is gone, while the
    // paragraphs' rows and carets are still right.
    frame(&ctx, |ctx| {
        ctx.all_styles_mut(|s| {
            s.visuals.text_options.color_transfer_function = egui::epaint::FontColorTransferFunction::Off;
        });
    });
    let (mut painted, mut reference) = (Vec::new(), Vec::new());
    let mut painted_geometry = Vec::new();
    frame(&ctx, |ctx| {
        // Other text first, so the new atlas does not put the body's glyphs where the old one
        // happened to leave them.
        for size in [9.0_f32, 11.5, 17.0, 23.0] {
            let _ = ctx.fonts_mut(|f| f.layout_no_wrap("Wg@#%&0123456789".to_owned(), crate::theme::regular(size), egui::Color32::WHITE));
        }
        layout.sync(ctx, &paragraphs, &[], key, &Tokens::light(), 600.0);
        painted = layout.paras.iter().map(|p| uv_rects(&p.paint_galley(ctx))).collect();
        reference = jobs.iter().map(|job| uv_rects(&ctx.fonts_mut(|f| f.layout_job((**job).clone())))).collect();
        painted_geometry = layout.paras.iter().map(|p| geometry(&p.paint_galley(ctx), p.chars)).collect();
    });

    assert_eq!(layout.laid, laid, "a new atlas alone re-measures nothing");
    assert_ne!(reference, before, "the fixture must really move the body's glyphs into a new atlas");
    assert_eq!(painted, reference, "the body must paint glyphs laid out in the current font atlas, not ones retained from the old one");
    assert_eq!(painted_geometry, kept, "a new atlas moves no row and no caret: the body paints the geometry it kept");
    assert_eq!(
        painted_geometry.iter().map(|(_, height, _)| f32::from_bits(*height)).sum::<f32>(),
        layout.paras.iter().map(|p| p.height()).sum::<f32>(),
        "the painted paragraphs are as tall as the body says"
    );
}

#[test]
fn a_changed_face_with_the_same_row_height_lays_the_body_out_again() {
    let ctx = egui::Context::default();
    let mut defs = crate::theme::font_definitions();
    // Inter's weights share their vertical metrics and differ in their advances: the same rows,
    // wider glyphs. Nothing a row height tells apart, and everything a caret or a line's end
    // depends on.
    defs.families.insert(egui::FontFamily::Proportional, vec!["Inter".to_owned()]);
    ctx.set_fonts(defs.clone());
    frame(&ctx, |_| {});
    let paragraphs = vec![paragraph("The quick brown fox jumps over the lazy dog.")];
    let key = key(&ctx, 600.0);
    let mut layout = Layout::default();
    let (mut width, mut row) = (0.0_f32, 0.0_f32);
    frame(&ctx, |ctx| {
        layout.sync(ctx, &paragraphs, &[], key, &Tokens::light(), 600.0);
        // The line as the face draws it now, whatever the body kept.
        let job = layout.paras.first().map(|p| (*p.galley.job).clone());
        width = job.map_or(0.0, |job| ctx.fonts_mut(|f| f.layout_job(job)).rect.width());
        row = ctx.fonts_mut(|f| f.row_height(&regular(BASE_SIZE)));
    });
    let laid = layout.laid;

    defs.families.insert(egui::FontFamily::Proportional, vec!["InterMedium".to_owned()]);
    ctx.set_fonts(defs);
    let (mut again, mut width_after, mut row_after) = (false, 0.0_f32, 0.0_f32);
    let (mut kept, mut painted) = ((0usize, 0u32, Vec::new()), (0usize, 0u32, Vec::new()));
    let (mut height, mut caret) = (0.0_f32, egui::Rect::NOTHING);
    frame(&ctx, |ctx| {
        again = layout.sync(ctx, &paragraphs, &[], key, &Tokens::light(), 600.0);
        let job = layout.paras.first().map(|p| (*p.galley.job).clone());
        width_after = job.map_or(0.0, |job| ctx.fonts_mut(|f| f.layout_job(job)).rect.width());
        row_after = ctx.fonts_mut(|f| f.row_height(&regular(BASE_SIZE)));
        // The geometry the new face measured, and the geometry the frame paints from it.
        if let Some(p) = layout.paras.first() {
            kept = geometry(&p.galley, p.chars);
            painted = geometry(&p.paint_galley(ctx), p.chars);
        }
        height = layout.height;
        caret = layout.caret(layout.paras.first().map_or(0, |p| p.start + p.chars));
    });

    assert_eq!(row_after.to_bits(), row.to_bits(), "the fixture must keep the row height");
    assert!(width_after > width, "the fixture must widen the line: {width} -> {width_after}");
    assert_eq!(layout.laid, laid + 1, "a changed face lays the body out again, whatever its row height");
    assert!(again, "the sync that saw the new face must say that it laid something out");
    assert_eq!(painted, kept, "a changed face's rows and carets are the geometry the body paints");
    assert_eq!(height, layout.paras.iter().map(|p| p.height()).sum::<f32>(), "the body is as tall as its paragraphs");
    assert!(caret.max.y <= height + 0.5, "a caret at the end of the body stands in it: {caret:?} in {height}");
}

/// The window's ink is baked into every section of the body's jobs: plain text, a link's ink and
/// its underline, and a sender's own colour made readable on the window's sheet. Changing the
/// accent or turning high contrast on inside one appearance changes that ink and nothing the
/// layout is keyed on, so a body kept from before went on painting the old one — until a width,
/// scale or appearance change happened to clear it. What a paragraph is laid out with now is what
/// the palette says now.
#[test]
fn a_changed_accent_or_contrast_recolours_the_body() {
    let ctx = ctx();
    let own = "#B0BEC5";
    let link = Style { link: Some("https://example.org".to_owned()), ..Default::default() };
    let sender = Style { color: Some(own.to_owned()), ..Default::default() };
    let paragraphs = vec![
        paragraph("plain"),
        Paragraph { runs: vec![Run::new("link", link.clone())], ..Default::default() },
        Paragraph { runs: vec![Run::new("theirs", sender.clone())], ..Default::default() },
    ];
    let key = key(&ctx, 600.0);
    let mut layout = Layout::default();
    // What the palette in force bakes into each paragraph's sections: the code's own answer, not
    // a second copy of its rules here.
    let expected = |t: &Tokens| -> Vec<egui::TextFormat> {
        let has = |family: &egui::FontFamily| ctx.fonts(|f| f.families().contains(family));
        vec![
            super::text_format(&Style::default(), false, t, &has),
            super::text_format(&link, false, t, &has),
            super::text_format(&sender, false, t, &has),
        ]
    };
    let sections = |layout: &Layout| -> Vec<egui::TextFormat> { layout.paras.iter().map(|p| p.galley.job.sections[0].format.clone()).collect() };
    let light = Tokens::light();
    frame(&ctx, |ctx| {
        layout.sync(ctx, &paragraphs, &[], key, &light, 600.0);
    });
    assert_eq!(sections(&layout), expected(&light), "a body laid out afresh carries the window's ink");

    // Nothing changed: nothing is laid out again.
    let laid = layout.laid;
    let mut again = true;
    frame(&ctx, |ctx| {
        again = layout.sync(ctx, &paragraphs, &[], key, &light, 600.0);
    });
    assert!(!again && layout.laid == laid, "a still body is not laid out again");

    // Another accent, in the same appearance: a link's ink is the accent's, and its underline's.
    let teal = Tokens::build(false, crate::theme::Accent::Teal, false);
    assert_ne!(teal.accent_text, light.accent_text, "the fixture must change the link's ink");
    let mut relaid = false;
    frame(&ctx, |ctx| {
        relaid = layout.sync(ctx, &paragraphs, &[], key, &teal, 600.0);
    });
    assert!(relaid, "another accent must lay the body out again");
    assert_eq!(sections(&layout), expected(&teal), "a link is painted in the accent in force");

    // High contrast, the same accent and the same appearance: plain text, links and a sender's
    // own colour are all the palette's ink now.
    let contrast = Tokens::build(false, crate::theme::Accent::Teal, true);
    assert_ne!(contrast.text, teal.text, "the fixture must change the plain ink");
    let mut relaid = false;
    frame(&ctx, |ctx| {
        relaid = layout.sync(ctx, &paragraphs, &[], key, &contrast, 600.0);
    });
    assert!(relaid, "high contrast must lay the body out again");
    assert_eq!(sections(&layout), expected(&contrast), "the body's ink follows the palette in force");

    // And the plain ink is what the window draws text in, not the appearance it opened with.
    assert_eq!(layout.paras[0].galley.job.sections[0].format.color, contrast.text);
    assert_eq!(layout.paras[1].galley.job.sections[0].format.underline.color, contrast.accent_text);
}

#[test]
fn a_still_body_lays_nothing_out_and_paints_the_galleys_it_kept() {
    let ctx = ctx();
    let paragraphs: Vec<Paragraph> =
        (0..240).map(|i| paragraph(&format!("Paragraph {i} of a message long enough to scroll past the window."))).collect();
    let key = key(&ctx, 600.0);
    let mut layout = Layout::default();
    frame(&ctx, |ctx| {
        layout.sync(ctx, &paragraphs, &[], key, &Tokens::light(), 600.0);
    });
    assert_eq!(layout.laid, paragraphs.len(), "a long body is measured once");
    let (laid, layouts, height) = (layout.laid, layout.layouts, layout.height);

    // The paragraphs the window shows, as `show_doc` paints them: from the first still in view to
    // the last, never the whole message.
    let clip = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(600.0, 400.0));
    let first = layout.paras.partition_point(|p| p.top + p.height() < clip.min.y);
    let shown: Vec<usize> = layout.paras.iter().enumerate().skip(first).take_while(|(_, p)| p.top <= clip.max.y).map(|(i, _)| i).collect();
    assert!(!shown.is_empty() && shown.len() < paragraphs.len(), "the fixture must show some of the body, not all of it");

    frame(&ctx, |ctx| {
        for &i in &shown {
            let _ = layout.paras[i].paint_galley(ctx);
        }
    });
    let cached = ctx.fonts_mut(|f| f.num_galleys_in_cache());
    let mut kept = Vec::new();
    frame(&ctx, |ctx| {
        for &i in &shown {
            kept.push(Arc::ptr_eq(&layout.paras[i].paint_galley(ctx), &layout.paras[i].galley));
        }
    });
    assert!(kept.iter().all(|same| *same), "a still body paints the galleys it kept, not ones laid out again");
    assert_eq!(layout.laid, laid, "a still body lays nothing out");
    assert_eq!(layout.layouts, layouts, "a still body's paragraphs are not laid out again");
    assert_eq!(layout.height, height, "a still body's rows do not move");
    let after = ctx.fonts_mut(|f| f.num_galleys_in_cache());
    assert!(after <= cached, "a still body adds no galleys to the cache: {cached} -> {after}");
    let mut still = true;
    frame(&ctx, |ctx| {
        still = !layout.sync(ctx, &paragraphs, &[], key, &Tokens::light(), 600.0);
    });
    assert!(still, "syncing a body that did not change says so");
}
