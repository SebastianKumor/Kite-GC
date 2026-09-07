// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Marc Hoffmann (b14ckyy)

//! Move and resize the detached video window from the GTK button-press handler (Linux).
//!
//! Tauri's `startDragging()` / `startResizeDragging()` reach GTK through the IPC: by the time
//! `begin_move_drag` runs, the press is long over and no event is current. Under GNOME/Wayland the
//! compositor then honours the request only every SECOND press — measured on Debian 13
//! (2026-09-08): the page logged a `pointerdown` for every attempt and the command returned `Ok`
//! every time, and the window still moved on every other one. The resize the same window gets from
//! tauri-runtime-wry's undecorated-border handler never failed once, and that one runs INSIDE the
//! button-press event. So this does too.
//!
//! The handler cannot ask the page what sits under the pointer, so the page says so in advance:
//! [`set_zones`] takes the rects of its own corner chrome (buttons stay with the page — those
//! presses are passed through) and of the resize grip, in CSS px, refreshed whenever they move.
//! Everything else is the drag surface.
//!
//! Only the detached window gets this. The main window is decorated and drags by its title bar.

use std::sync::Mutex;

use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;

/// What the page handles itself, and where the resize corner is (CSS px, `[x, y, w, h]`).
#[derive(Clone, Default)]
struct Zones {
    /// Dragging the picture moves the window — off while fullscreen.
    drag: bool,
    grip: Option<[f64; 4]>,
    chrome: Vec<[f64; 4]>,
}

static ZONES: Mutex<Zones> = Mutex::new(Zones { drag: false, grip: None, chrome: Vec::new() });

/// The page's current layout. Called on every change (fullscreen, chrome appearing, resize).
pub fn set_zones(drag: bool, grip: Option<[f64; 4]>, chrome: Vec<[f64; 4]>) {
    if let Ok(mut z) = ZONES.lock() {
        *z = Zones { drag, grip, chrome };
    }
}

fn hit(r: &[f64; 4], x: f64, y: f64) -> bool {
    x >= r[0] && x < r[0] + r[2] && y >= r[1] && y < r[1] + r[3]
}

/// Take over left-button presses on `webview`. GTK runs handlers in connection order, so
/// tauri-runtime-wry's border-resize handler — connected when the window was created — still gets
/// the outer few pixels first; this sees everything inside them.
pub fn install(window: &gtk::Window, webview: &gtk::Widget) {
    let win = window.clone();
    webview.connect_button_press_event(move |webview, ev| {
        if ev.button() != 1 || ev.event_type() != gdk::EventType::ButtonPress {
            return glib::Propagation::Proceed;
        }
        let Ok(zones) = ZONES.lock() else { return glib::Propagation::Proceed };
        if !zones.drag {
            return glib::Propagation::Proceed;
        }
        let (x, y) = ev.position();
        // The outermost few pixels are the window's own resize border: tauri-runtime-wry hit-tests
        // them in a handler connected before this one (its inset, `BORDERLESS_RESIZE_INSET`,
        // scaled). It starts the edge resize and lets the press through, so anything done here
        // would take that gesture over — leave the rim alone.
        let border = f64::from(5 * webview.scale_factor());
        let (w, h) = (f64::from(webview.allocated_width()), f64::from(webview.allocated_height()));
        if x < border || y < border || x > w - border || y > h - border {
            return glib::Propagation::Proceed;
        }
        // The page's own controls: it gets the press, we do nothing.
        if zones.chrome.iter().any(|r| hit(r, x, y)) {
            return glib::Propagation::Proceed;
        }
        let grip = zones.grip.filter(|r| hit(r, x, y));
        drop(zones);
        let (rx, ry) = ev.root();
        let (rx, ry) = (rx as i32, ry as i32);
        match grip {
            // One corner, both axes — the page snaps the aspect back when the drag settles.
            Some(_) => win.begin_resize_drag(gdk::WindowEdge::NorthEast, 1, rx, ry, ev.time()),
            None => win.begin_move_drag(1, rx, ry, ev.time()),
        }
        // The press belongs to the window gesture now; letting WebKit have it as well would leave
        // the page thinking a button is held down for as long as the compositor owns the pointer.
        glib::Propagation::Stop
    });
}
