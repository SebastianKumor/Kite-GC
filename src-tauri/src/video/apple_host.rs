// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Marc Hoffmann (b14ckyy)

//! AppKit host for the macOS hole-punch video layer (MOBILE_RTSP.md P3) — the macOS counterpart
//! of `linux_host` / the Android `NativeVideo` view: one layer-backed NSView BELOW the transparent
//! WKWebView in the main window's content view, visible wherever the DOM cut its hole.
//!
//! View tree (built once at startup, nothing is re-parented — wry keeps its own view tree):
//!
//!   NSWindow.contentView
//!     ├─ container NSView (ours, added `positioned: below` every existing sibling)
//!     │    layer: opaque black (the letterbox must be opaque — a transparent gap shows the desktop
//!     │           through the transparent window), `masksToBounds` = the VISIBLE box
//!     │      └─ the sink's AVSampleBufferDisplayLayer at the FULL box, offset into the container
//!     └─ WKWebView (wry's) — see-through thanks to `"transparent": true` in tauri.macos.conf.json
//!
//! That is the two-rect sink contract: the video is laid out (aspect-fit) in the full box and CUT at
//! the visible edge — a scrolled panel crops the picture, never shrinks it.
//!
//! AppKit is main-thread only: every mutation hops onto the main thread through the app handle
//! (`run_on_main_thread`); the callers are the RTSP/sink worker threads. Geometry arrives in
//! PHYSICAL px (the frontend's devicePixelRatio-scaled rects) and is mapped to points through the
//! window's backing scale factor. AppKit's content view is NOT flipped: y grows upwards, so rects
//! are converted from the DOM's top-left origin here, in one place.

// The CATransform3D C helpers are flagged as renamed (`CATransform3D::concat` & co.); the C
// names are what Apple documents and read identically to the AppKit/QuartzCore literature.
#![allow(deprecated)]

use std::cell::RefCell;
use std::sync::OnceLock;

use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSView, NSWindow, NSWindowOrderingMode};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::CGColor;
use objc2_quartz_core::{CALayer, CATransform3D, CATransform3DConcat, CATransform3DIdentity, CATransform3DMakeRotation, CATransform3DMakeScale};
use tauri::{AppHandle, Manager};

/// Main-thread state; `None` until installed.
struct Host {
    window: Retained<NSWindow>,
    container: Retained<NSView>,
    /// The sink's display layer, while one is attached.
    video: Option<Retained<CALayer>>,
    /// Last rect pushed (physical px: x, y, w, h, cx, cy, cw, ch) — re-applied when a layer is
    /// attached after the rect arrived.
    rect: [i32; 8],
    mirror: bool,
    rotate180: bool,
}

thread_local! {
    static HOST: RefCell<Option<Host>> = const { RefCell::new(None) };
}

/// The app handle, kept for the main-thread hops (`on_main`).
static APP: OnceLock<AppHandle> = OnceLock::new();

/// Install the video layer under the main window's WebView. Call once from Tauri's setup (the
/// window exists by then). Failure leaves the window as it was and only costs the native sink route.
pub fn install(app: &AppHandle) {
    let _ = APP.set(app.clone());
    let handle = app.clone();
    let hop = app.run_on_main_thread(move || {
        if let Err(e) = install_main(&handle) {
            log::warn!("[video] apple host: {e} — the native decode sink is unavailable");
        }
    });
    if let Err(e) = hop {
        log::warn!("[video] apple host: main-thread hop failed: {e}");
    }
}

fn install_main(app: &AppHandle) -> Result<(), String> {
    let Some(mtm) = MainThreadMarker::new() else {
        return Err("not on the main thread".into());
    };
    if HOST.with(|h| h.borrow().is_some()) {
        return Ok(());
    }
    let window = app.get_webview_window("main").ok_or("no main window")?;
    let ns_window = window.ns_window().map_err(|e| format!("ns_window: {e}"))?;
    // SAFETY: tauri hands out the live NSWindow of this window; retaining it keeps it valid for
    // the host's lifetime (the app's main window lives as long as the app).
    let ns_window: Retained<NSWindow> = unsafe {
        Retained::retain(ns_window.cast::<NSWindow>()).ok_or("null NSWindow")?
    };
    let content = ns_window.contentView().ok_or("window has no content view")?;

    let container = NSView::initWithFrame(
        mtm.alloc::<NSView>(),
        CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(1.0, 1.0)),
    );
    container.setWantsLayer(true);
    if let Some(layer) = container.layer() {
        let black = CGColor::new_generic_rgb(0.0, 0.0, 0.0, 1.0);
        layer.setBackgroundColor(Some(&black));
        layer.setMasksToBounds(true);
    }
    container.setHidden(true);
    // Below every existing sibling — the WebView included — so it paints only where the DOM is
    // transparent. Nothing is removed or re-parented.
    content.addSubview_positioned_relativeTo(&container, NSWindowOrderingMode::Below, None);

    log::info!(
        "[video] apple host: video layer installed (backing scale {})",
        ns_window.backingScaleFactor()
    );
    HOST.with(|h| {
        *h.borrow_mut() = Some(Host {
            window: ns_window,
            container,
            video: None,
            rect: [0, 0, 1, 1, 0, 0, 1, 1],
            mirror: false,
            rotate180: false,
        })
    });
    Ok(())
}

/// Run `f` against the host on the main thread. Silently nothing without a host (install failed
/// / not called — the MJPEG route needs none of this).
fn on_main(f: impl FnOnce(&mut Host) + Send + 'static) {
    let Some(app) = APP.get() else { return };
    let _ = app.run_on_main_thread(move || {
        HOST.with(|h| {
            if let Some(host) = h.borrow_mut().as_mut() {
                f(host);
            }
        });
    });
}

/// Apply `host.rect` to the container (visible box) and the video layer (full box), converting
/// physical px with a top-left origin into points with AppKit's bottom-left origin.
fn layout(host: &Host) {
    let s = host.window.backingScaleFactor().max(1.0);
    let p = |v: i32| v as f64 / s;
    let [x, y, w, h, cx, cy, cw, ch] = host.rect;
    let Some(content) = host.window.contentView() else { return };
    let content_h = content.frame().size.height;
    let (cw_pt, ch_pt) = (p(cw).max(1.0), p(ch).max(1.0));
    host.container.setFrame(CGRect::new(
        CGPoint::new(p(cx), content_h - p(cy) - ch_pt),
        CGSize::new(cw_pt, ch_pt),
    ));
    if let Some(video) = &host.video {
        let (w_pt, h_pt) = (p(w).max(1.0), p(h).max(1.0));
        // Inside the container's layer (bottom-left origin): the full box offset by the clip.
        let left = p(x - cx);
        let bottom = ch_pt - p(y - cy) - h_pt;
        video.setBounds(CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(w_pt, h_pt)));
        video.setPosition(CGPoint::new(left + w_pt / 2.0, bottom + h_pt / 2.0));
        video.setContentsScale(s);
        video.setTransform(transform(host.mirror, host.rotate180));
    }
}

/// Mirror = flip x; rotate-180 = half turn about z; both compose. Around the layer's centre
/// (anchor point 0.5/0.5), so the picture stays in its box.
fn transform(mirror: bool, rotate180: bool) -> CATransform3D {
    let mut t = unsafe { CATransform3DIdentity };
    if mirror {
        t = CATransform3DConcat(t, CATransform3DMakeScale(-1.0, 1.0, 1.0));
    }
    if rotate180 {
        t = CATransform3DConcat(t, CATransform3DMakeRotation(std::f64::consts::PI, 0.0, 0.0, 1.0));
    }
    t
}

/// On-screen rect (PHYSICAL px, window client coords, top-left origin): FULL box `x/y/w/h` for the
/// video's aspect-fit layout, VISIBLE box `cx/cy/cw/ch` for the clip.
#[allow(clippy::too_many_arguments)]
pub fn set_rect(x: i32, y: i32, w: i32, h: i32, cx: i32, cy: i32, cw: i32, ch: i32) {
    on_main(move |host| {
        host.rect = [x, y, w, h, cx, cy, cw, ch];
        layout(host);
    });
}

/// Show/hide the layer (no DOM surface wants it right now).
pub fn set_visible(visible: bool) {
    on_main(move |host| host.container.setHidden(!visible));
}

/// Mirror / 180° rotation — a transform on the display layer, no decoder involvement.
pub fn set_orient(mirror: bool, rotate180: bool) {
    on_main(move |host| {
        host.mirror = mirror;
        host.rotate180 = rotate180;
        layout(host);
    });
}

/// Build the sink's display layer ON the main thread (`make` runs there — AppKit/CoreAnimation
/// objects are created where they are used), put it into the container, replacing any previous
/// one, and lay it out at the last known rect. Returns once the main thread has done it (`Ok`)
/// or an error if no host exists.
pub fn attach(make: impl FnOnce() -> Retained<CALayer> + Send + 'static) -> Result<(), String> {
    let (tx, rx) = std::sync::mpsc::channel::<bool>();
    on_main(move |host| {
        detach_layer(host);
        let layer = make();
        if let Some(root) = host.container.layer() {
            root.addSublayer(&layer);
        }
        host.video = Some(layer);
        layout(host);
        let _ = tx.send(true);
    });
    match rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(true) => Ok(()),
        _ => Err("no video host installed (main-thread install failed?)".into()),
    }
}

/// Remove the sink's layer (the container stays for the next stream).
pub fn detach() {
    on_main(|host| detach_layer(host));
}

fn detach_layer(host: &mut Host) {
    if let Some(v) = host.video.take() {
        v.removeFromSuperlayer();
    }
}
