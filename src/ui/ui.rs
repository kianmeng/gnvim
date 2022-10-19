use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time;

use gtk::prelude::*;
use gtk::{gdk, glib};

use log::{debug, error};
use nvim_rs::Value;

use crate::error::Error;
use crate::nvim_bridge::{Message, Request};
use crate::nvim_gio::GioNeovim;
use crate::ui::cmdline::Cmdline;
use crate::ui::color::{Highlight, HlDefs};
use crate::ui::common::spawn_local;
use crate::ui::font::Font;
use crate::ui::grid::Grid;
use crate::ui::popupmenu::Popupmenu;
use crate::ui::state::{attach_grid_events, UIState, Windows};
use crate::ui::tabline::Tabline;
use crate::ui::window::MsgWindow;

/// Main UI structure.
pub struct UI {
    /// Main window.
    win: gtk::ApplicationWindow,
    /// Neovim instance.
    nvim: GioNeovim,
    /// Channel to receive event from nvim.
    rx: glib::Receiver<Message>,
    /// Our internal state, containing basically everything we manipulate
    /// when we receive an event from nvim.
    state: Rc<RefCell<UIState>>,
}

impl UI {
    /// Creates new UI.
    ///
    /// * `app` - GTK application for the UI.
    /// * `rx` - Channel to receive nvim UI events.
    /// * `nvim` - Neovim instance to use. Should be the same that is the source
    ///            of `rx` events.
    pub fn init(
        app: &gtk::Application,
        rx: glib::Receiver<Message>,
        window_size: (i32, i32),
        nvim: GioNeovim,
        grid_scroll_speed: i64,
    ) -> Result<Self, Error> {
        // Create the main window.
        let window = gtk::ApplicationWindow::new(app);
        window.set_title("Neovim");
        window.set_default_size(window_size.0, window_size.1);

        // Realize window resources.
        window.realize();

        // Top level widget.
        let b = gtk::Box::new(gtk::Orientation::Vertical, 0);
        window.add(&b);

        let tabline = Tabline::new(nvim.clone());
        b.pack_start(&tabline.get_widget(), false, false, 0);

        // Our root widget for all grids/windows.
        let overlay = gtk::Overlay::new();
        b.pack_start(&overlay, true, true, 0);

        // Create hl defs and initialize 0th element because we'll need to have
        // something that is accessible for the default grid that we're gonna
        // make next.
        let mut hl_defs = HlDefs::default();
        hl_defs.insert(0, Highlight::default());

        let font = Font::from_guifont("Monospace:h12").unwrap();
        let line_space = 0;

        // Create default grid.
        let mut grid = Grid::new(
            1,
            &window.window().unwrap(),
            font.clone(),
            line_space,
            80,
            30,
            &hl_defs,
            true,
            grid_scroll_speed,
        )?;
        // Mark the default grid as active at the beginning.
        grid.set_active(true);
        overlay.add(&grid.widget());

        let windows_container = gtk::Fixed::new();
        windows_container.set_widget_name("windows-container");
        let windows_float_container = gtk::Fixed::new();
        windows_float_container.set_widget_name("windows-container-float");
        let msg_window_container = gtk::Fixed::new();
        msg_window_container.set_widget_name("message-grid-container");
        overlay.add_overlay(&windows_container);
        overlay.add_overlay(&msg_window_container);
        overlay.add_overlay(&windows_float_container);

        let css_provider = gtk::CssProvider::new();
        let msg_window =
            MsgWindow::new(msg_window_container.clone(), css_provider.clone());

        overlay.set_overlay_pass_through(&windows_container, true);
        overlay.set_overlay_pass_through(&windows_float_container, true);
        overlay.set_overlay_pass_through(&msg_window_container, true);

        // When resizing our window (main grid), we'll have to tell neovim to
        // resize it self also. The notify to nvim is send with a small delay,
        // so we don't spam it multiple times a second. source_id is used to
        // track the function timeout. This timeout might be canceled in
        // redraw even handler if we receive a message that changes the size
        // of the main grid.
        let source_id = Rc::new(RefCell::new(None));
        grid.connect_da_resize(clone!(nvim, source_id => move |rows, cols| {

            // Set timeout to notify nvim about the new size.
            let new = glib::timeout_add_local(time::Duration::from_millis(30), clone!(nvim, source_id => move || {
                let nvim = nvim.clone();
                spawn_local(async move {
                    if let Err(err) = nvim.ui_try_resize(cols as i64, rows as i64).await {
                        error!("Error: failed to resize nvim when grid size changed ({:?})", err);
                    }
                });

                // Set the source_id to none, so we don't accidentally remove
                // it since it used at this point.
                source_id.borrow_mut().take();

                Continue(false)
            }));

            let mut source_id = source_id.borrow_mut();
            // If we have earlier timeout, remove it.
            if let Some(old) = source_id.take() {
                glib::source::source_remove(old);
            }

            *source_id = Some(new);

            false
        }));

        attach_grid_events(&grid, nvim.clone());

        // IMMulticontext is used to handle most of the inputs.
        let im_context = gtk::IMMulticontext::new();
        im_context.set_use_preedit(false);
        im_context.connect_commit(clone!(nvim => move |_, input| {
            // "<" needs to be escaped for nvim.input()
            let nvim_input = input.replace("<", "<lt>");

            let nvim = nvim.clone();
            spawn_local(async move {
                nvim.input(&nvim_input).await.expect("Couldn't send input");
            });
        }));

        window.connect_key_press_event(clone!(nvim, im_context => move |_, e| {
            if im_context.filter_keypress(e) {
                Inhibit(true)
            } else {
                if let Some(input) = event_to_nvim_input(e) {
                    let nvim = nvim.clone();
                    spawn_local(async move {
                        nvim.input(input.as_str()).await.expect("Couldn't send input");
                    });
                    return Inhibit(true);
                } else {
                    debug!(
                        "Failed to turn input event into nvim key (keyval: {})",
                        e.keyval()
                    )
                }

                Inhibit(false)
            }
        }));

        window.connect_key_release_event(clone!(im_context => move |_, e| {
            im_context.filter_keypress(e);

            Inhibit(false)
        }));

        window.connect_focus_in_event(clone!(im_context, nvim => move |_, _| {
            im_context.focus_in();

            let nvim = nvim.clone();
            spawn_local(async move {
                let res = nvim.command("if exists('#FocusGained') | doautocmd FocusGained | endif").await;
                if let Err(err) = res {
                    error!("Failed to issue FocusGained autocmd: {:?}", err)
                }
            });

            Inhibit(false)
        }));

        window.connect_focus_out_event(clone!(im_context, nvim => move |_, _| {
            im_context.focus_out();

            let nvim = nvim.clone();
            spawn_local(async move {
                let res = nvim.command("if exists('#FocusLost') | doautocmd FocusLost | endif").await;
                if let Err(err) = res {
                    error!("Failed to issue FocusLost autocmd: {:?}", err)
                }
            });

            Inhibit(false)
        }));

        let cmdline = Cmdline::new(&overlay, nvim.clone());

        window.show_all();

        grid.set_im_context(&im_context);

        cmdline.hide();

        let mut grids = HashMap::new();
        grids.insert(1, grid);

        add_css_provider!(&css_provider, window);

        Ok(UI {
            win: window,
            rx,
            state: Rc::new(RefCell::new(UIState {
                css_provider,
                windows: Windows::new(),
                windows_container,
                _msg_window_container: msg_window_container,
                msg_window,
                windows_float_container,
                grids,
                mode_infos: vec![],
                current_grid: 1,
                wildmenu_shown: false,
                popupmenu: Popupmenu::new(&overlay, nvim.clone()),
                cmdline,
                overlay,
                tabline,
                resize_source_id: source_id,
                hl_defs,
                resize_on_flush: None,
                hl_changed: false,
                font,
                line_space,
                current_mode: None,
                enable_cursor_animations: true,
                grid_scroll_speed,
            })),
            nvim,
        })
    }

    /// Starts to listen events from `rx` (e.g. from nvim) and processing those.
    /// Think this as the "main" function of the UI.
    pub fn start(self) {
        let UI {
            rx,
            state,
            win,
            nvim,
        } = self;

        rx.attach(None, move |message| {
            match message {
                // Handle a notify.
                Message::Notify(notify) => {
                    let mut state = state.borrow_mut();

                    state
                        .handle_notify(&win, notify, &nvim)
                        .expect("failed to handle a notify");
                }
                // Handle a request.
                Message::Request(tx, request) => {
                    let mut state = state.borrow_mut();
                    let res = handle_request(&request, &mut state);
                    tx.send(res).expect("Failed to respond to a request");
                }
                // Handle close.
                Message::Close => {
                    win.close();
                    return Continue(false);
                }
            }

            Continue(true)
        });
    }
}

fn handle_request(
    _request: &Request,
    _state: &mut UIState,
) -> Result<Value, Value> {
    // NOTE(ville): Leftovers from old code.
    Err("Unknown request".into())
}

fn keyname_to_nvim_key(s: &str) -> Option<&str> {
    // Originally sourced from python-gui.
    match s {
        "asciicircum" => Some("^"), // fix #137
        "slash" => Some("/"),
        "backslash" => Some("\\"),
        "dead_circumflex" => Some("^"),
        "at" => Some("@"),
        "numbersign" => Some("#"),
        "dollar" => Some("$"),
        "percent" => Some("%"),
        "ampersand" => Some("&"),
        "asterisk" => Some("*"),
        "parenleft" => Some("("),
        "parenright" => Some(")"),
        "underscore" => Some("_"),
        "plus" => Some("+"),
        "minus" => Some("-"),
        "bracketleft" => Some("["),
        "bracketright" => Some("]"),
        "braceleft" => Some("{"),
        "braceright" => Some("}"),
        "dead_diaeresis" => Some("\""),
        "dead_acute" => Some("\'"),
        "less" => Some("<"),
        "greater" => Some(">"),
        "comma" => Some(","),
        "period" => Some("."),
        "space" => Some("Space"),
        "BackSpace" => Some("BS"),
        "Insert" => Some("Insert"),
        "Return" => Some("CR"),
        "Escape" => Some("Esc"),
        "Delete" => Some("Del"),
        "Page_Up" => Some("PageUp"),
        "Page_Down" => Some("PageDown"),
        "Enter" => Some("CR"),
        "ISO_Left_Tab" => Some("Tab"),
        "Tab" => Some("Tab"),
        "Up" => Some("Up"),
        "Down" => Some("Down"),
        "Left" => Some("Left"),
        "Right" => Some("Right"),
        "Home" => Some("Home"),
        "End" => Some("End"),
        "F1" => Some("F1"),
        "F2" => Some("F2"),
        "F3" => Some("F3"),
        "F4" => Some("F4"),
        "F5" => Some("F5"),
        "F6" => Some("F6"),
        "F7" => Some("F7"),
        "F8" => Some("F8"),
        "F9" => Some("F9"),
        "F10" => Some("F10"),
        "F11" => Some("F11"),
        "F12" => Some("F12"),
        _ => None,
    }
}

fn event_to_nvim_input(e: &gdk::EventKey) -> Option<String> {
    let mut input = String::from("");

    let keyval = e.keyval();
    let keyname = keyval.name()?;

    let state = e.state();

    if state.contains(gdk::ModifierType::SHIFT_MASK) {
        input.push_str("S-");
    }
    if state.contains(gdk::ModifierType::CONTROL_MASK) {
        input.push_str("C-");
    }
    if state.contains(gdk::ModifierType::MOD1_MASK) {
        input.push_str("A-");
    }

    if keyname.chars().count() > 1 {
        let n = keyname_to_nvim_key(keyname.as_str())?;
        input.push_str(n);
    } else {
        input.push(keyval.to_unicode()?);
    }

    Some(format!("<{}>", input))
}
