use std::cell::RefCell;
use std::fmt;
use std::fmt::Display;
use std::rc::Rc;

use gtk::gdk::{EventMask, ModifierType};
use gtk::{cairo, gdk, glib};
use gtk::{DrawingArea, EventBox};

use gtk::prelude::*;

use crate::error::Error;
use crate::nvim_bridge::{
    GridLineSegment, GridScrollArea, GridScrollRegion, ModeInfo,
};
use crate::ui::color::HlDefs;
use crate::ui::font::Font;
use crate::ui::grid::context::Context;
use crate::ui::grid::render;

use super::row::Segment;

pub struct GridMetrics {
    // Row count in the grid.
    pub rows: f64,
    // Col count in the grid.
    pub cols: f64,
    // Height of a cell.
    pub cell_height: f64,
    // Width of a cell.
    pub cell_width: f64,

    // Width of the whole grid as required by the cell width and cols.
    pub height: f64,
    // Height of the whole grid as required by the cell height and rows.
    pub width: f64,
}

pub enum ScrollDirection {
    Up,
    Down,
    Right,
    Left,
}

impl Display for ScrollDirection {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ScrollDirection::Up => write!(fmt, "up"),
            ScrollDirection::Down => write!(fmt, "down"),
            ScrollDirection::Right => write!(fmt, "right"),
            ScrollDirection::Left => write!(fmt, "left"),
        }
    }
}

pub enum MouseButton {
    Left,
    Middle,
    Right,
}

impl Display for MouseButton {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        match self {
            MouseButton::Left => write!(fmt, "left"),
            MouseButton::Middle => write!(fmt, "middle"),
            MouseButton::Right => write!(fmt, "right"),
        }
    }
}

/// Single grid in the neovim UI. This matches the `ui-linegrid` stuff in
/// the ui.txt documentation for neovim.
pub struct Grid {
    pub id: i64,
    /// Our internal "widget". This is what is drawn to the screen.
    da: DrawingArea,
    /// EventBox to get mouse events for this grid.
    eb: EventBox,
    /// Internal context that is manipulated and used when handling events.
    context: Rc<RefCell<Context>>,
    /// Pointer position for dragging if we should call callback from
    /// `connect_motion_events_for_drag`.
    drag_position: Rc<RefCell<(u64, u64)>>,

    /// Smooth scrolling indicator.
    scroll_delta: Rc<RefCell<(f64, f64)>>,

    /// Input context that need to be updated for the cursor position
    im_context: Option<gtk::IMMulticontext>,
}

impl Grid {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: i64,
        win: &gdk::Window,
        font: Font,
        line_space: i64,
        cols: usize,
        rows: usize,
        hl_defs: &HlDefs,
        enable_cursor_animations: bool,
        scroll_speed: i64,
    ) -> Result<Self, Error> {
        let da = DrawingArea::new();
        let ctx = Rc::new(RefCell::new(Context::new(
            &da,
            win,
            font,
            line_space,
            cols,
            rows,
            hl_defs,
            enable_cursor_animations,
            scroll_speed,
        )?));

        da.connect_draw(clone!(ctx => move |_, cr| {
            let mut ctx = ctx.borrow_mut();
            drawingarea_draw(cr, &mut ctx).expect("failed to draw");
            Inhibit(false)
        }));

        let eb = EventBox::new();
        eb.add_events(EventMask::SCROLL_MASK | EventMask::SMOOTH_SCROLL_MASK);
        eb.add(&da);

        da.add_tick_callback(clone!(ctx => move |da, clock| {
            let mut ctx = ctx.borrow_mut();
            ctx.tick(da, clock).expect("context tick failed");
            glib::Continue(true)
        }));

        Ok(Grid {
            id,
            da,
            eb,
            context: ctx,
            drag_position: Rc::new(RefCell::new((0, 0))),
            im_context: None,
            scroll_delta: Rc::new(RefCell::new((0.0, 0.0))),
        })
    }

    pub fn widget(&self) -> gtk::Widget {
        self.eb.clone().upcast()
    }

    pub fn flush(&self, hl_defs: &HlDefs) -> Result<(), Error> {
        let mut ctx = self.context.borrow_mut();

        if let Some(cell) = ctx.cell_at_cursor() {
            // If cursor isn't blinking, drawn the inverted cell into
            // the cursor's cairo context.
            if ctx.cursor.blink_on == 0 {
                render::cursor_cell(
                    &ctx.cursor_context,
                    &self.da.pango_context(),
                    cell,
                    &ctx.cell_metrics,
                    hl_defs,
                )?;
            }

            // Update cursor color.
            let hl = hl_defs.get(&cell.hl_id).unwrap();
            ctx.cursor.color = hl.foreground.unwrap_or(hl_defs.default_fg);
        }

        while let Some(area) = ctx.queue_draw_area.pop() {
            self.da.queue_draw_area(
                area.0.floor() as i32,
                area.1.floor() as i32,
                area.2.ceil() as i32,
                area.3.ceil() as i32,
            );
        }

        Ok(())
    }

    pub fn set_im_context(&mut self, im_context: &gtk::IMMulticontext) {
        im_context.set_client_window(self.da.window().as_ref());
        self.im_context = Some(im_context.clone());
    }

    /// Returns position (+ width and height) for cell (row, col) relative
    /// to the top level window of this grid.
    pub fn get_rect_for_cell(&self, row: u64, col: u64) -> gdk::Rectangle {
        let ctx = self.context.borrow();

        let (x, y) = render::get_coords(
            ctx.cell_metrics.height,
            ctx.cell_metrics.width,
            row as f64,
            col as f64,
        );

        let (x, y) = self
            .eb
            .translate_coordinates(
                &self.eb.parent().unwrap(),
                x as i32,
                y as i32,
            )
            .unwrap();

        gtk::Rectangle {
            x,
            y,
            width: ctx.cell_metrics.width as i32,
            height: ctx.cell_metrics.height as i32,
        }
    }

    /// Connects `f` to internal widget's scroll events. `f` params are scroll
    /// direction, row, col.
    pub fn connect_scroll_events<F: 'static>(&self, f: F)
    where
        F: Fn(ScrollDirection, u64, u64) -> Inhibit,
    {
        let ctx = self.context.clone();
        let scroll_delta = self.scroll_delta.clone();

        // NOTE(ville): Once we bump gtk from 3.20 to 3.24, use GtkEventControllerScroll
        // to improve smooth scrolling (ref #175).
        self.eb
            .connect_scroll_event(clone!(ctx, scroll_delta => move |_, e| {
                let ctx = ctx.borrow_mut();
                let dir = match e.direction() {
                    gdk::ScrollDirection::Right => ScrollDirection::Right,
                    gdk::ScrollDirection::Left => ScrollDirection::Left,
                    gdk::ScrollDirection::Up => ScrollDirection::Up,
                    gdk::ScrollDirection::Down => ScrollDirection::Down,
                    gdk::ScrollDirection::Smooth => {
                        // Smooth scrolling. During scroll, many little deltas
                        // are accumulated in scroll_deltas. Once a delta
                        // reaches -1.0 or +1.0, given delta is reset and
                        // scroll operation is made effective.
                        let (smooth_dx, smooth_dy) = e.scroll_deltas().unwrap();
                        let (prev_dx, prev_dy) = *scroll_delta.borrow();
                        let dy = prev_dy + smooth_dy;
                        let dx = prev_dx + smooth_dx;

                        let (new_delta, dir) = if dy <= -1.0 {
                            ((dx, 0.0), Some(ScrollDirection::Up))
                        } else if dy >= 1.0 {
                            ((dx, 0.0), Some(ScrollDirection::Down))
                        } else if dx <= -1.0 {
                            ((0.0, dy), Some(ScrollDirection::Left))
                        } else if dx >= 1.0 {
                            ((0.0, dy), Some(ScrollDirection::Right))
                        }else {
                            ((dx, dy), None)
                        };

                        *scroll_delta.borrow_mut() = new_delta;
                        if let Some(dir) = dir {
                            dir
                        } else {
                            return Inhibit(false);
                        }
                    },
                    _ => { return Inhibit(false); },
                };

                let pos = e.position();
                let col = (pos.0 / ctx.cell_metrics.width).floor() as u64;
                let row = (pos.1 / ctx.cell_metrics.height).floor() as u64;

                f(dir, row, col)
            }));
    }

    /// Connects `f` to internal widget's motion events. `f` params are button,
    /// row, col. `f` is only called when the cell under the pointer changes.
    pub fn connect_motion_events_for_drag<F: 'static>(&self, f: F)
    where
        F: Fn(MouseButton, u64, u64) -> Inhibit,
    {
        let ctx = self.context.clone();
        let drag_position = self.drag_position.clone();

        self.eb.connect_motion_notify_event(move |_, e| {
            let ctx = ctx.borrow();
            let mut drag_position = drag_position.borrow_mut();

            let button = match e.state() {
                ModifierType::BUTTON3_MASK => MouseButton::Right,
                ModifierType::BUTTON2_MASK => MouseButton::Middle,
                _ => MouseButton::Left,
            };

            let pos = e.position();
            let col = (pos.0 / ctx.cell_metrics.width).floor() as u64;
            let row = (pos.1 / ctx.cell_metrics.height).floor() as u64;

            if drag_position.0 != col || drag_position.1 != row {
                *drag_position = (col, row);
                f(button, row, col)
            } else {
                Inhibit(false)
            }
        });
    }

    /// Connects `f` to internal widget's mouse button press event. `f` params
    /// are button, row, col.
    pub fn connect_mouse_button_press_events<F: 'static>(&self, f: F)
    where
        F: Fn(MouseButton, u64, u64) -> Inhibit,
    {
        let ctx = self.context.clone();

        self.eb.connect_button_press_event(move |_, e| {
            let ctx = ctx.borrow();

            let button = match e.button() {
                3 => MouseButton::Right,
                2 => MouseButton::Middle,
                _ => MouseButton::Left,
            };

            let pos = e.position();
            let col = (pos.0 / ctx.cell_metrics.width).floor() as u64;
            let row = (pos.1 / ctx.cell_metrics.height).floor() as u64;

            f(button, row, col)
        });
    }

    /// Connects `f` to internal widget's mouse button release event. `f` params
    /// are button, row, col.
    pub fn connect_mouse_button_release_events<F: 'static>(&self, f: F)
    where
        F: Fn(MouseButton, u64, u64) -> Inhibit,
    {
        let ctx = self.context.clone();

        self.eb.connect_button_release_event(move |_, e| {
            let ctx = ctx.borrow();

            let button = match e.button() {
                3 => MouseButton::Right,
                2 => MouseButton::Middle,
                _ => MouseButton::Left,
            };

            let pos = e.position();
            let col = (pos.0 / ctx.cell_metrics.width).floor() as u64;
            let row = (pos.1 / ctx.cell_metrics.height).floor() as u64;

            f(button, row, col)
        });
    }

    /// Connects `f` to internal widget's resize events. `f` params are rows, cols.
    pub fn connect_da_resize<F: 'static>(&self, f: F)
    where
        F: Fn(u64, u64) -> bool,
    {
        let ctx = self.context.clone();

        self.da.connect_configure_event(move |da, _| {
            let ctx = ctx.borrow();

            let w = f64::from(da.allocated_width());
            let h = f64::from(da.allocated_height());
            let cols = (w / ctx.cell_metrics.width).floor() as u64;
            let rows = (h / ctx.cell_metrics.height).floor() as u64;

            f(rows, cols)
        });
    }

    pub fn put_line(
        &self,
        line: GridLineSegment,
        hl_defs: &HlDefs,
    ) -> Result<(), Error> {
        let mut ctx = self.context.borrow_mut();

        let row = line.row as usize;
        let mut affected_segments = ctx
            .rows
            .get_mut(row)
            .ok_or(Error::PutLineRowNotFound(row))?
            .update(line);

        // NOTE(ville): I haven't noticed any cases where a character is overflowing
        //              to the left. Probably doesn't apply to languages that goes
        //              from right to left, instead of left to right.
        // Rendering the segments in reversed order fixes issues when some character
        // is overflowing to the right.
        affected_segments.reverse();
        render::put_segments(
            &mut ctx,
            &self.da.pango_context(),
            hl_defs,
            affected_segments,
            row,
        )
    }

    pub fn redraw(&self, hl_defs: &HlDefs) -> Result<(), Error> {
        let mut ctx = self.context.borrow_mut();
        let pango_context = self.da.pango_context();
        ctx.rows
            .iter_mut()
            .enumerate()
            .map(|(i, row)| (i, row.as_segments(0, row.len)))
            .collect::<Vec<(usize, Vec<Segment>)>>()
            .into_iter()
            .try_for_each(|(i, segments)| {
                render::put_segments(
                    &mut ctx,
                    &pango_context,
                    hl_defs,
                    segments,
                    i,
                )
            })
    }

    pub fn cursor_goto(&self, row: u64, col: u64) {
        let clock = self.da.frame_clock().unwrap();
        let mut ctx = self.context.borrow_mut();
        ctx.cursor_goto(row, col, &clock);

        let (x, y, width, height) = ctx.get_cursor_rect();
        if let Some(ref im_context) = self.im_context {
            let rect = gdk::Rectangle {
                x,
                y,
                width,
                height,
            };
            im_context.set_cursor_location(&rect);
        }
    }

    pub fn get_grid_metrics(&self) -> GridMetrics {
        let ctx = self.context.borrow();

        let row = ctx.rows.get(0).unwrap();

        let rows = ctx.rows.len() as f64;
        let cols = row.len() as f64;
        let cell_width = ctx.cell_metrics.width;
        let cell_height = ctx.cell_metrics.height;

        GridMetrics {
            rows,
            cols,
            cell_width,
            cell_height,
            width: cols * cell_width,
            height: rows * cell_height,
        }
    }

    /// Calculates the size of a grid that can fit in the current drawingarea
    /// with current cell metrics.
    pub fn calc_size(&self) -> (i64, i64) {
        let ctx = self.context.borrow();

        let w = self.da.allocated_width();
        let h = self.da.allocated_height();
        let cols = (w / ctx.cell_metrics.width as i32) as i64;
        let rows = (h / ctx.cell_metrics.height as i32) as i64;

        (cols, rows)
    }

    pub fn resize(
        &self,
        win: &gdk::Window,
        cols: u64,
        rows: u64,
        hl_defs: &HlDefs,
    ) -> Result<(), Error> {
        let mut ctx = self.context.borrow_mut();
        ctx.resize(&self.da, win, cols as usize, rows as usize, hl_defs)
    }

    pub fn clear(&self, hl_defs: &HlDefs) -> Result<(), Error> {
        let mut ctx = self.context.borrow_mut();

        // Clear internal grid (rows).
        for row in ctx.rows.iter_mut() {
            row.clear();
        }

        render::clear(&self.da, &mut ctx, hl_defs)
    }

    pub fn scroll(
        &self,
        reg: GridScrollRegion,
        rows: i64,
        _cols: i64,
        hl_defs: &HlDefs,
    ) -> Result<(), Error> {
        let mut ctx = self.context.borrow_mut();

        let left = reg.0[2];
        let right = reg.0[3];
        let area = reg.calc_area(rows);

        let GridScrollArea {
            src_top,
            src_bot,
            dst_top,
            dst_bot,
            clr_top,
            clr_bot,
        } = area;

        // Modify the rows stored data of the rows.
        let mut src = vec![];
        for i in src_top as usize..src_bot as usize {
            let row = ctx.rows.get(i).unwrap().clone();
            let part = row.copy_range(left as usize, right as usize).clone();
            src.push(part);
        }
        src.reverse();

        for i in dst_top as usize..dst_bot as usize {
            ctx.rows
                .get_mut(i)
                .unwrap()
                .insert_at(left as usize, src.pop().unwrap());
        }

        for i in clr_top as usize..clr_bot as usize {
            ctx.rows
                .get_mut(i)
                .unwrap()
                .clear_range(left as usize, right as usize);
        }

        let clock = self.da.frame_clock().unwrap();
        render::scroll(
            &mut ctx,
            hl_defs,
            clock.frame_time(),
            area,
            left as f64,
            right as f64,
        )
    }

    pub fn set_active(&self, active: bool) {
        let mut ctx = self.context.borrow_mut();

        ctx.active = active;
    }

    /// Set a new font and line space. This will likely change the cell metrics.
    /// Use `calc_size` to receive the updated size (cols and rows) of the grid.
    pub fn update_cell_metrics(
        &self,
        font: Font,
        line_space: i64,
        win: &gdk::Window,
    ) -> Result<(), Error> {
        let mut ctx = self.context.borrow_mut();
        ctx.update_metrics(font, line_space, &self.da, win)
    }

    /// Get the current line space value.
    pub fn get_line_space(&self) -> i64 {
        let ctx = self.context.borrow();
        ctx.cell_metrics.line_space
    }

    /// Get a copy of the current font.
    pub fn get_font(&self) -> Font {
        let ctx = self.context.borrow();
        ctx.cell_metrics.font.clone()
    }

    pub fn set_mode(&self, mode: &ModeInfo) {
        let mut ctx = self.context.borrow_mut();

        ctx.cursor.blink_on = mode.blink_on;
        ctx.cursor.cell_percentage = mode.cell_percentage;
    }

    pub fn set_busy(&self, busy: bool) {
        let mut ctx = self.context.borrow_mut();

        ctx.busy = busy;
    }

    pub fn enable_cursor_animations(&self, enable: bool) {
        let mut ctx = self.context.borrow_mut();
        ctx.cursor.disable_animation = !enable;
    }
}

/// Handler for grid's drawingarea's draw event. Draws the internal cairo
/// context (`ctx`) surface to the `cr`.
fn drawingarea_draw(
    cr: &cairo::Context,
    ctx: &mut Context,
) -> Result<(), Error> {
    let prev = &ctx.surfaces.prev;

    if let Some(ref anim) = ctx.surfaces.offset_y_anim {
        let surface = ctx.surfaces.back.target();
        surface.flush();

        prev.save()?;
        let back_offset = ctx.surfaces.offset_y - anim.start;
        prev.set_source_surface(&surface, 0.0, back_offset)?;
        prev.paint()?;
        prev.restore()?;
    }

    let surface = ctx.surfaces.front.target();
    surface.flush();

    prev.save()?;
    prev.set_source_surface(&surface, 0.0, ctx.surfaces.offset_y)?;
    prev.paint()?;
    prev.restore()?;

    cr.save()?;
    cr.set_source_surface(&prev.target(), 0.0, 0.0)?;
    cr.paint()?;
    cr.restore()?;

    // If we're not "busy", draw the cursor.
    if !ctx.busy && ctx.active {
        let (x, y, w, h) = ctx.get_cursor_rect();

        cr.save()?;
        cr.rectangle(
            f64::from(x),
            f64::from(y),
            f64::from(w) * ctx.cursor.cell_percentage,
            f64::from(h),
        );
        let surface = ctx.cursor_context.target();
        surface.flush();
        cr.set_source_surface(&surface, x.into(), y.into())?;
        cr.fill()?;
        cr.restore()?;
    }

    Ok(())
}
