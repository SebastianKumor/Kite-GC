// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Marc Hoffmann (b14ckyy)

//! GTK host for the Linux hole-punch video layer (MOBILE_RTSP.md P2.3) — the Linux
//! counterpart of the Android `NativeVideo` view host: a video widget BELOW the transparent
//! WebKitWebView, in the same GtkWindow, visible wherever the DOM cut its hole.
//!
//! Since VIDEO_MULTISINK_WINDOW.md §4.2 there are [`SLOTS`] of them — the video widget and the
//! floating window at the same time. A slot is a fixed seat: it owns one clip layer and holds one
//! pipeline branch's widget for the sink's whole life. The sink decides which SURFACE a slot shows;
//! the widget itself never moves. That is deliberate — a `gtkglsink` widget carries a realized
//! GdkWindow and a GL context, and re-parenting it between clips (let alone between windows) is
//! exactly the kind of surgery that costs the display, as `GL_DISPLAYS` in `linux_sink` documents.
//!
//! And there is one such tree PER WINDOW, keyed by its Tauri label: the main window, and the
//! detached video window when it is open. That is also why the detached window gets a pipeline of
//! its own in `linux_sink` rather than a branch of the main one — a widget cannot move between
//! windows any more than between clips, and two `gtkglsink`s inside ONE pipeline cannot share the
//! single GL context a pipeline distributes (they can across two pipelines).
//!
//! Widget tree, built once at startup by re-hosting the WebView that tao/wry packed into
//! the window's default vbox:
//!
//!   window → GtkOverlay { main child: the default vbox → base GtkLayout (window-sized,
//!                         transparent)
//!                         overlay child: WebKitWebView (fills; alpha-0 background from the
//!                         `transparent` window flag in tauri.linux.conf.json) }
//!   base → clip GtkLayout per slot (own GdkWindow: clips children, paints black) at its VISIBLE box
//!   clip → that slot's video widget at the FULL box, offset so it lands in window coordinates
//!
//! The WebView sits exactly two levels below the window, as tao/wry built it —
//! tauri-runtime-wry's undecorated-resize handler relies on that (see `install_tree`).
//!
//! That is the two-rect sink contract: the video is laid out (aspect-fit) in the full box
//! and CUT at the visible edge — a scrolled panel crops the picture, never shrinks it.
//! GtkLayout on both levels because its size request ignores its children (a GtkFixed
//! grows to contain them and would push the window's minimum size along).
//!
//! GTK is single-threaded: every mutation hops onto the GTK main loop through
//! `glib::MainContext::default().invoke` (the loop tao runs — no Tauri handle involved, so
//! the tree also works under a plain GtkWindow in tests); the callers are the RTSP/sink
//! worker threads. Geometry arrives in PHYSICAL px (the frontend's devicePixelRatio-scaled
//! rects) and is mapped to GTK's logical px through the window's integer scale factor.

use std::cell::RefCell;
use std::collections::HashMap;

use gtk::glib;
use gtk::prelude::*;
use tauri::{AppHandle, Manager};

/// On-screen surfaces this host can show at once (VIDEO_MULTISINK_WINDOW.md D1: the widget tile
/// plus one large surface). One clip layer and one pipeline branch each, both built up front —
/// the maximum is known, so nothing is added to a running pipeline.
pub const SLOTS: usize = 2;

/// One seat: its clip layer, the widget the sink parked in it, and the last rect it was given.
struct Out {
    clip: gtk::Layout,
    video: Option<gtk::Widget>,
    /// Last rect pushed (physical px: x, y, w, h, cx, cy, cw, ch) — re-applied when a
    /// widget is attached after the rect arrived.
    rect: [i32; 8],
    /// No surface wants this slot: it is parked (see [`PARK_POS`]) rather than hidden.
    parked: bool,
}

/// Where an idle slot's clip goes: one logical pixel, off the base layout's top-left corner, so it
/// is clipped away instead of drawn.
///
/// It must NOT be hidden. A hidden GtkLayout is not mapped, its child is never realized, and an
/// unrealized `gtkglsink` widget has no GtkGLArea and therefore no GL context — `glupload` then has
/// nothing to negotiate against and the whole pipeline dies with `not-negotiated (-4)` at the
/// appsrc, before the first frame. Cost of parking instead: one 1×1 GL area that renders nothing,
/// because that slot's valve drops every buffer upstream of it.
const PARK_POS: i32 = -8;

/// Main-thread state; `None` until the tree was installed.
struct Host {
    window: gtk::Window,
    base: gtk::Layout,
    outs: Vec<Out>,
}

thread_local! {
    /// One tree per window label (`main`, and `video` while the detached window is open).
    static HOSTS: RefCell<HashMap<String, Host>> = RefCell::new(HashMap::new());
}

/// The clip paints the letterbox: everything in the hole that the video doesn't cover
/// must be opaque, or the desktop shows through the transparent window.
const CSS: &str = ".kite-video-clip, .kite-video-base { background-color: #000; }";

/// Re-host the main window's WebView inside a GtkOverlay above the video layer. Call once
/// from Tauri's setup (the WebView exists by then). Failure leaves the window as it was and
/// only costs the native sink route.
pub fn install(app: &AppHandle) {
    install_for(app, "main");
}

/// Same for another window — the detached video window, once it exists. Idempotent: a label that
/// already has a tree is left alone.
pub fn install_for(app: &AppHandle, label: &str) {
    let handle = app.clone();
    let label = label.to_string();
    glib::MainContext::default().invoke(move || {
        if let Err(e) = install_from_app(&handle, &label) {
            log::warn!("[video] linux host: {label}: {e} — the native decode sink is unavailable there");
        }
    });
}

fn install_from_app(app: &AppHandle, label: &str) -> Result<(), String> {
    let window = app.get_webview_window(label).ok_or("no such window")?;
    let gtk_window = window.gtk_window().map_err(|e| format!("gtk window: {e}"))?;
    let vbox = window.default_vbox().map_err(|e| format!("default vbox: {e}"))?;
    let children = vbox.children();
    let Some(webview) = children.iter().find(|c| c.type_().name() == "WebKitWebView").cloned() else {
        let names: Vec<String> = children.iter().map(|c| c.type_().name().to_string()).collect();
        return Err(format!("no WebKitWebView in the default vbox (children: {names:?})"));
    };
    let gtk_window: &gtk::Window = gtk_window.upcast_ref();

    // A tree may still be on file for this label from a window that is GONE — the detached video
    // window's, whenever its pipeline was not torn down through the path that calls `uninstall`
    // (the stream stopping while it was open, say). Keeping it would park the NEW window's video
    // widget in the dead one's clip layer: `gtkglsink` finds no toplevel above it and opens a
    // window of its own ("Gtk+ GL renderer"), the app's own frame stays transparent, and closing
    // that stray window pulls the widget out from under the running pipeline — which kills the
    // stream (Marc, 2026-09-08). Same window → nothing to do; a different one → start over.
    let known = HOSTS.with(|h| {
        h.borrow().get(label).map(|host| host.window.as_ptr() == gtk_window.as_ptr())
    });
    match known {
        Some(true) => return Ok(()),
        Some(false) => {
            log::info!("[video] linux host: {label}: the window changed — rebuilding the video layer");
            HOSTS.with(|h| {
                if let Some(mut host) = h.borrow_mut().remove(label) {
                    for slot in 0..host.outs.len() {
                        detach_widget(&mut host, slot);
                    }
                }
            });
        }
        None => {}
    }

    install_tree_for(label, gtk_window, &vbox, Some(&webview))?;
    // The detached window is undecorated: its move and resize gestures have to come from the press
    // handler, not from the IPC (see `linux_drag`).
    if label != "main" {
        crate::video::linux_drag::install(gtk_window, &webview);
    }
    Ok(())
}

/// Build the layer tree under `window` (main thread). `webview` is re-hosted as the
/// overlay child; `None` (tests) leaves the overlay with just the video layer.
pub(crate) fn install_tree_for(
    label: &str,
    window: &gtk::Window,
    vbox: &gtk::Box,
    webview: Option<&gtk::Widget>,
) -> Result<(), String> {
    if HOSTS.with(|h| h.borrow().contains_key(label)) {
        return Ok(());
    }
    let provider = gtk::CssProvider::new();
    provider.load_from_data(CSS.as_bytes()).map_err(|e| format!("css: {e}"))?;
    if let Some(screen) = gtk::gdk::Screen::default() {
        gtk::StyleContext::add_provider_for_screen(
            &screen,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }

    let base = gtk::Layout::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
    // Black under the whole hole: letterbox bars outside the clip layer must not show the
    // window theme colour.
    base.style_context().add_class("kite-video-base");
    let mut outs = Vec::with_capacity(SLOTS);
    for _ in 0..SLOTS {
        let clip = gtk::Layout::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
        clip.style_context().add_class("kite-video-clip");
        clip.set_size_request(1, 1);
        // Stays hidden until a rect and a visible=true arrive from the surface router.
        clip.set_no_show_all(true);
        base.put(&clip, PARK_POS, PARK_POS);
        outs.push(Out { clip, video: None, rect: [0, 0, 1, 1, 0, 0, 1, 1], parked: true });
    }

    // New tree: window → overlay { main: vbox → base ; overlay: webview }. The vbox stays
    // (tao's `default_vbox` keeps pointing at a live, hosted box) but moves under the
    // overlay, because the WebView MUST stay exactly two levels below the window:
    // tauri-runtime-wry's undecorated-resize handler walks `webview.parent().parent()` and
    // unwraps a GtkWindow downcast — a mismatch aborts the process on the first click.
    // wry holds its own reference to the view, so the remove cannot destroy it.
    if let Some(wv) = webview {
        vbox.remove(wv);
    }
    window.remove(vbox);
    vbox.pack_start(&base, true, true, 0);
    let overlay = gtk::Overlay::new();
    overlay.add(vbox);
    if let Some(wv) = webview {
        overlay.add_overlay(wv);
    }
    window.add(&overlay);
    overlay.show_all();

    log::info!(
        "[video] linux host: video layer installed in window {label}, {SLOTS} slots (scale factor {})",
        window.scale_factor()
    );
    HOSTS.with(|h| {
        h.borrow_mut().insert(
            label.to_string(),
            Host {
                window: window.clone(),
                base,
                outs,
            },
        )
    });
    Ok(())
}

/// Forget a window's tree — its window is going away (the detached video window closed). The
/// widgets are removed first so the sink's pipeline can drop them safely.
pub fn uninstall(label: &str) {
    let label = label.to_string();
    glib::MainContext::default().invoke(move || {
        HOSTS.with(|h| {
            if let Some(mut host) = h.borrow_mut().remove(&label) {
                for slot in 0..host.outs.len() {
                    detach_widget(&mut host, slot);
                }
            }
        });
    });
}

/// Run `f` against the host on the GTK main loop. Silently nothing without a host
/// (install failed / not called — the MJPEG route needs none of this).
fn on_main(label: &str, f: impl FnOnce(&mut Host) + Send + 'static) {
    let label = label.to_string();
    glib::MainContext::default().invoke(move || {
        HOSTS.with(|h| {
            if let Some(host) = h.borrow_mut().get_mut(&label) {
                f(host);
            }
        });
    });
}

/// Apply one slot's rect to its clip and video widget (physical → logical px). A parked slot goes
/// to its corner at 1×1 instead — visible, so its widget stays realized (see [`PARK_POS`]).
fn layout(host: &Host, slot: usize) {
    let s = host.window.scale_factor().max(1) as f64;
    let l = |v: i32| (v as f64 / s).round() as i32;
    let Some(out) = host.outs.get(slot) else { return };
    if out.parked {
        host.base.move_(&out.clip, PARK_POS, PARK_POS);
        out.clip.set_size_request(1, 1);
        if let Some(v) = &out.video {
            out.clip.move_(v, 0, 0);
            v.set_size_request(1, 1);
        }
        return;
    }
    let [x, y, w, h, cx, cy, cw, ch] = out.rect;
    host.base.move_(&out.clip, l(cx), l(cy));
    out.clip.set_size_request(l(cw).max(1), l(ch).max(1));
    if let Some(v) = &out.video {
        out.clip.move_(v, l(x - cx), l(y - cy));
        v.set_size_request(l(w).max(1), l(h).max(1));
    }
}

/// On-screen rect of one slot (PHYSICAL px, window client coords): FULL box `x/y/w/h` for the
/// video's aspect-fit layout, VISIBLE box `cx/cy/cw/ch` for the clip.
pub fn set_rect(label: &str, slot: usize, rect: [i32; 8]) {
    on_main(label, move |host| {
        if let Some(out) = host.outs.get_mut(slot) {
            out.rect = rect;
        }
        layout(host, slot);
    });
}

/// Give a slot to a surface, or park it because no surface wants it right now. Parking is not
/// hiding — see [`PARK_POS`] for why that distinction is what keeps the pipeline alive.
pub fn set_visible(label: &str, slot: usize, visible: bool) {
    on_main(label, move |host| {
        if let Some(out) = host.outs.get_mut(slot) {
            out.parked = !visible;
            out.clip.set_visible(true);
        }
        layout(host, slot);
    });
}

/// Stack the clips the way the DOM stacks their surfaces. `order` arrives highest-priority FIRST
/// (`main` > `floating` > `widget`) and DOM stacking is the reverse of that — the widget dock
/// paints over the floating window — so raising them in that order leaves the LAST one, the widget,
/// on top. Only matters where two surfaces overlap; the clips have their own GdkWindows, so this is
/// a stacking change, not a re-parent.
pub fn restack(label: &str, order: Vec<usize>) {
    on_main(label, move |host| {
        for slot in order {
            if let Some(out) = host.outs.get(slot) {
                if let Some(win) = out.clip.window() {
                    win.raise();
                }
            }
        }
    });
}

/// Put a video widget into a slot, replacing any previous one. `make` runs on the GTK
/// main thread — GTK objects don't cross threads, so the caller builds the widget there
/// (e.g. reads a gtksink's `widget` property). `None` from `make` leaves the layer empty.
/// The returned receiver yields once, `true` when the widget is placed — a sink that
/// needs its widget realized before it starts (GL context) waits on it.
pub fn attach(
    label: &str,
    slot: usize,
    make: impl FnOnce() -> Option<gtk::Widget> + Send + 'static,
) -> std::sync::mpsc::Receiver<bool> {
    let (tx, rx) = std::sync::mpsc::channel();
    on_main(label, move |host| {
        detach_widget(host, slot);
        let Some(w) = make() else {
            let _ = tx.send(false);
            return;
        };
        let Some(out) = host.outs.get_mut(slot) else {
            let _ = tx.send(false);
            return;
        };
        out.clip.set_visible(true);
        out.clip.put(&w, 0, 0);
        w.show();
        out.video = Some(w);
        layout(host, slot);
        let _ = tx.send(true);
    });
    rx
}

/// Remove every video widget and hide every layer. The receiver yields once the main loop did
/// it — a sink tears its pipeline down only after that (see linux_sink's Drop).
pub fn detach(label: &str, slots: std::ops::Range<usize>) -> std::sync::mpsc::Receiver<()> {
    let (tx, rx) = std::sync::mpsc::channel();
    let label = label.to_string();
    // Not through `on_main`: the answer has to come even when the tree is already gone (a window
    // that closed hands its layer back before its pipeline is torn down), or the caller sits out
    // the full timeout for nothing.
    glib::MainContext::default().invoke(move || {
        HOSTS.with(|h| {
            if let Some(host) = h.borrow_mut().get_mut(&label) {
                // Only this pipeline's seats: a window may be served by SEVERAL pipelines
                // (`linux_sink`'s per-seat mode), and one of them going away must not pull the
                // others' widgets out of the tree.
                for slot in slots {
                    detach_widget(host, slot);
                    if let Some(out) = host.outs.get_mut(slot) {
                        out.parked = true;
                        // Really hidden this time: the pipeline is going away, there is nothing
                        // left that could need a realized widget.
                        out.clip.hide();
                    }
                }
            }
        });
        let _ = tx.send(());
    });
    rx
}

fn detach_widget(host: &mut Host, slot: usize) {
    let Some(out) = host.outs.get_mut(slot) else { return };
    if let Some(w) = out.video.take() {
        out.clip.remove(&w);
    }
}

/// Dev-only stand-in (P2.3 stage A): a coloured, framed, crossed drawing area in place of
/// the video widget, so transparency, hole geometry and clipping can be verified by eye
/// without a decoder. Static drawing — nothing loops.
#[cfg(debug_assertions)]
pub fn spike(on: bool) {
    if !on {
        let _ = detach("main", 0..SLOTS);
        return;
    }
    let _ = attach("main", 0, || {
        let area = gtk::DrawingArea::new();
        area.connect_draw(|w, cr| {
            let (aw, ah) = (w.allocated_width() as f64, w.allocated_height() as f64);
            cr.set_source_rgb(0.12, 0.66, 0.86);
            let _ = cr.paint();
            cr.set_source_rgb(1.0, 1.0, 1.0);
            cr.set_line_width(4.0);
            cr.rectangle(2.0, 2.0, aw - 4.0, ah - 4.0);
            let _ = cr.stroke();
            cr.move_to(0.0, 0.0);
            cr.line_to(aw, ah);
            cr.move_to(aw, 0.0);
            cr.line_to(0.0, ah);
            let _ = cr.stroke();
            glib::Propagation::Stop
        });
        Some(area.upcast())
    });
}
