use gtk::pango::Attribute;
use gtk::prelude::*;
use gtk::DrawingArea;
use gtk::{cairo, pango};

use crate::error::Error;
use crate::nvim_bridge::GridScrollArea;
use crate::ui::color::Highlight;
use crate::ui::color::HlDefs;
use crate::ui::grid::context::{CellMetrics, Context};
use crate::ui::grid::row::{Cell, Segment};

/// Renders text to `cr`.
///
/// * `cr` - The cairo context to render to.
/// * `pango_context` - The pango context to use for text rendering.
/// * `cm` - Cell metrics to use for text placement.
/// * `hl` - The highlighting to use.
/// * `hl_defs` - Global hl defs. Used to get default values.
/// * `text` - The text to render.
/// * `pos` - Target position for `cr`.
fn render_text(
    cr: &cairo::Context,
    pango_context: &pango::Context,
    cm: &CellMetrics,
    hl: &Highlight,
    hl_defs: &HlDefs,
    text: &str,
    pos: cairo::Rectangle,
) -> Result<(), Error> {
    let cairo::Rectangle {
        x,
        y,
        width: w,
        height: h,
    } = pos;

    let (fg, bg) = if hl.reverse {
        (
            hl.background.unwrap_or(hl_defs.default_bg),
            hl.foreground.unwrap_or(hl_defs.default_fg),
        )
    } else {
        (
            hl.foreground.unwrap_or(hl_defs.default_fg),
            hl.background.unwrap_or(hl_defs.default_bg),
        )
    };

    cr.save()?;
    cr.set_source_rgb(bg.r, bg.g, bg.b);
    cr.rectangle(x, y, w, h);
    cr.fill()?;
    cr.restore()?;

    let attrs = pango::AttrList::new();

    if hl.bold {
        let attr = Attribute::new_weight(pango::Weight::Bold);
        attrs.insert(attr);
    }
    if hl.italic {
        let attr = Attribute::new_style(pango::Style::Italic);
        attrs.insert(attr);
    }

    cr.save()?;
    cr.set_source_rgb(fg.r, fg.g, fg.b);

    let items =
        pango::itemize(pango_context, text, 0, text.len() as i32, &attrs, None);

    let mut x_offset = 0.0;
    let scale = f64::from(pango::SCALE);
    for item in items {
        let a = item.analysis();
        let item_offset = item.offset() as usize;
        let mut glyphs = pango::GlyphString::new();

        pango::shape(
            &text[item_offset..item_offset + item.length() as usize],
            a,
            &mut glyphs,
        );

        cr.move_to(x + x_offset, y + cm.ascent);
        pangocairo::functions::show_glyph_string(cr, &a.font(), &mut glyphs);

        x_offset += f64::from(glyphs.width()) / scale;
    }

    // Since we can't (for some reason) use pango attributes to draw
    // underline and undercurl, we'll have to do that manually.
    let sp = hl.special.unwrap_or(hl_defs.default_sp);
    cr.set_source_rgb(sp.r, sp.g, sp.b);
    if hl.undercurl {
        pangocairo::functions::show_error_underline(
            cr,
            x,
            y + h + cm.underline_position - cm.underline_thickness,
            w,
            cm.underline_thickness * 2.0,
        );
    }
    if hl.underline {
        let y = y + h + cm.underline_position;
        cr.rectangle(x, y, w, cm.underline_thickness);
        cr.fill()?;
    }

    cr.restore()?;

    Ok(())
}

/// Draws (inverted) cell to `cr`.
pub fn cursor_cell(
    cr: &cairo::Context,
    pango_context: &pango::Context,
    cell: &Cell,
    cm: &CellMetrics,
    hl_defs: &HlDefs,
) -> Result<(), Error> {
    let mut hl = *hl_defs.get(&cell.hl_id).unwrap();

    hl.reverse = !hl.reverse;

    let width = if cell.double_width {
        cm.width * 2.0
    } else {
        cm.width
    };

    render_text(
        cr,
        pango_context,
        cm,
        &hl,
        hl_defs,
        &cell.text,
        cairo::Rectangle {
            x: 0.0,
            y: 0.0,
            width,
            height: cm.height,
        },
    )
}

/// Renders `segments` to ctx.cairo_context.
pub fn put_segments(
    ctx: &mut Context,
    pango_context: &pango::Context,
    hl_defs: &HlDefs,
    segments: Vec<Segment>,
    row: usize,
) -> Result<(), Error> {
    let cw = ctx.cell_metrics.width;
    let ch = ctx.cell_metrics.height;

    for seg in segments {
        let hl = hl_defs.get(&seg.hl_id).unwrap();

        let pos = cairo::Rectangle {
            x: (seg.start as f64 * cw).floor(),
            y: (row as f64 * ch).floor(),
            width: (seg.len as f64 * cw).ceil(),
            height: ch.ceil(),
        };

        render_text(
            &ctx.surfaces.front,
            pango_context,
            &ctx.cell_metrics,
            hl,
            hl_defs,
            &seg.text,
            pos,
        )?;

        ctx.queue_draw_area
            .push((pos.x, pos.y, pos.width, pos.height));
    }

    Ok(())
}

/// Clears whole `da` with `hl_defs.default_bg`.
pub fn clear(
    da: &DrawingArea,
    ctx: &mut Context,
    hl_defs: &HlDefs,
) -> Result<(), Error> {
    let cr = &ctx.surfaces.front;
    let w = da.allocated_width();
    let h = da.allocated_height();
    let bg = &hl_defs.default_bg;

    cr.save()?;
    cr.set_source_rgb(bg.r, bg.g, bg.b);
    cr.rectangle(0.0, 0.0, f64::from(w), f64::from(h));
    cr.fill()?;
    cr.restore()?;

    ctx.queue_draw_area
        .push((0.0, 0.0, f64::from(w), f64::from(h)));

    Ok(())
}

/// Scrolls contents in `ctx.cairo_context` and `ctx.rows`, based on `reg`.
pub fn scroll(
    ctx: &mut Context,
    hl_defs: &HlDefs,
    frame_time: i64,
    area: GridScrollArea,
    left: f64,
    right: f64,
) -> Result<(), Error> {
    let cm = &ctx.cell_metrics;
    let bg = &hl_defs.default_bg;

    let GridScrollArea {
        src_top,
        dst_top,
        dst_bot,
        ..
    } = area;

    let front = &ctx.surfaces.front;
    let back = &ctx.surfaces.back;
    let prev = &ctx.surfaces.prev;

    // Draw move the scrolled part on the cairo surface.
    front.save()?;
    // Create pattern which we can then "safely" draw to the surface. On X11, the pattern part was
    // not needed but on wayland it is - I suppose it has something to do with the underlying
    // backbuffer.
    front.push_group();
    let (_, y) = get_coords(cm.height, cm.width, dst_top - src_top, 0.0);
    front.set_source_surface(&front.target(), 0.0, y)?;
    front.set_operator(cairo::Operator::Source);
    let (x1, y1, x2, y2) = get_rect(
        cm.height,
        cm.width,
        dst_top,
        dst_bot,
        left as f64,
        right as f64,
    );
    let w = x2 - x1;
    let h = y2 - y1;
    front.rectangle(x1, y1, w, h);
    front.fill()?;
    // Draw the parttern.
    front.pop_group_to_source()?;
    front.set_operator(cairo::Operator::Source);
    front.rectangle(x1, y1, w, h);
    front.fill()?;
    front.restore()?;

    // Store the prev buffer in our back buffer.
    back.save()?;
    back.set_source_surface(&prev.target(), 0.0, 0.0)?;
    back.paint()?;
    back.restore()?;

    // Reset our prev buffer.
    prev.save()?;
    prev.set_source_rgb(bg.r, bg.g, bg.b);
    prev.paint()?;
    prev.restore()?;

    ctx.queue_draw_area.push((x1, y1, w, h));
    ctx.surfaces.set_animation(y, ctx.scroll_speed, frame_time);

    Ok(())
}

pub fn get_rect(
    col_h: f64,
    col_w: f64,
    top: f64,
    bot: f64,
    left: f64,
    right: f64,
) -> (f64, f64, f64, f64) {
    let (x1, y1) = get_coords(col_h, col_w, top, left);
    let (x2, y2) = get_coords(col_h, col_w, bot, right);
    (x1, y1, x2, y2)
}

pub fn get_coords(h: f64, w: f64, row: f64, col: f64) -> (f64, f64) {
    let x = col * w;
    let y = row * h;
    (x, y)
}
