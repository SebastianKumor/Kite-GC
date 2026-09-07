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
    static HOST: RefCell<Option<Host>> = const { RefCell::new(None) };
}

/// The clip paints the letterbox: everything in the hole that the video doesn't cover
/// must be opaque, or the desktop shows through the transparent window.
const CSS: &str = ".kite-video-clip, .kite-video-base { background-color: #000; }";

/// Re-host the main window's WebView inside a GtkOverlay above the video layer. Call once
/// from Tauri's setup (the WebView exists by then). Failure leaves the window as it was and
/// only costs the native sink route.
pub fn install(app: &AppHandle) {
    let handle = app.clone();
    glib::MainContext::default().invoke(move || {
        if let Err(e) = install_from_app(&handle) {
            log::warn!("[video] linux host: {e} — the native decode sink is unavailable");
        }
    });
}

fn install_from_app(app: &AppHandle) -> Result<(), String> {
    let window = app.get_webview_window("main").ok_or("no main window")?;
    let gtk_window = window.gtk_window().map_err(|e| format!("gtk window: {e}"))?;
    let vbox = window.default_vbox().map_err(|e| format!("default vbox: {e}"))?;
    let children = vbox.children();
    let Some(webview) = children.iter().find(|c| c.type_().name() == "WebKitWebView").cloned() else {
        let names: Vec<String> = children.iter().map(|c| c.type_().name().to_string()).collect();
        return Err(format!("no WebKitWebView in the default vbox (children: {names:?})"));
    };
    install_tree(gtk_window.upcast_ref(), &vbox, Some(&webview))
}

/// Build the layer tree under `window` (main thread). `webview` is re-hosted as the
/// overlay child; `None` (tests) leaves the overlay with just the video layer.
pub(crate) fn install_tree(
    window: &gtk::Window,
    vbox: &gtk::Box,
    webview: Option<&gtk::Widget>,
) -> Result<(), String> {
    if HOST.with(|h| h.borrow().is_some()) {
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
        "[video] linux host: video layer installed, {SLOTS} slots (scale factor {})",
        window.scale_factor()
    );
    HOST.with(|h| {
        *h.borrow_mut() = Some(Host {
            window: window.clone(),
            base,
            outs,
        })
    });
    Ok(())
}

/// Run `f` against the host on the GTK main loop. Silently nothing without a host
/// (install failed / not called — the MJPEG route needs none of this).
fn on_main(f: impl FnOnce(&mut Host) + Send + 'static) {
    glib::MainContext::default().invoke(move || {
        HOST.with(|h| {
            if let Some(host) = h.borrow_mut().as_mut() {
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
pub fn set_rect(slot: usize, rect: [i32; 8]) {
    on_main(move |host| {
        if let Some(out) = host.outs.get_mut(slot) {
            out.rect = rect;
        }
        layout(host, slot);
    });
}

/// Give a slot to a surface, or park it because no surface wants it right now. Parking is not
/// hiding — see [`PARK_POS`] for why that distinction is what keeps the pipeline alive.
pub fn set_visible(slot: usize, visible: bool) {
    on_main(move |host| {
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
pub fn restack(order: Vec<usize>) {
    on_main(move |host| {
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
    slot: usize,
    make: impl FnOnce() -> Option<gtk::Widget> + Send + 'static,
) -> std::sync::mpsc::Receiver<bool> {
    let (tx, rx) = std::sync::mpsc::channel();
    on_main(move |host| {
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
pub fn detach() -> std::sync::mpsc::Receiver<()> {
    let (tx, rx) = std::sync::mpsc::channel();
    on_main(move |host| {
        for slot in 0..host.outs.len() {
            detach_widget(host, slot);
            if let Some(out) = host.outs.get_mut(slot) {
                out.parked = true;
                // Really hidden this time: the pipeline is going away, there is nothing left that
                // could need a realized widget.
                out.clip.hide();
            }
        }
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
        let _ = detach();
        return;
    }
    let _ = attach(0, || {
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
