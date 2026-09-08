// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Marc Hoffmann (b14ckyy)

//! AppKit host for the macOS hole-punch video layer (MOBILE_RTSP.md P3) — the macOS counterpart
//! of `linux_host` / the Android `NativeVideo` view: a layer-backed NSView BELOW the transparent
//! WKWebView, visible wherever the DOM cut its hole.
//!
//! Since VIDEO_MULTISINK_WINDOW.md §4.2 there is one such view **per surface**, keyed by the
//! surface's `window/id` — the video widget and the floating window at the same time, and (PR B)
//! a surface that lives in the DETACHED video window, which is a different NSWindow entirely. The
//! window a container belongs to is looked up by its Tauri label, so nothing here is bound to the
//! main window any more.
//!
//! View tree per output (created on demand, nothing is re-parented — wry keeps its own view tree):
//!
//!   NSWindow.contentView                      ← the window the surface was published from
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

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::mpsc::channel;
use std::sync::OnceLock;
use std::time::Duration;

use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSView, NSWindow, NSWindowOrderingMode};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::CGColor;
use objc2_quartz_core::{CALayer, CATransform3D, CATransform3DConcat, CATransform3DIdentity, CATransform3DMakeRotation, CATransform3DMakeScale};
use tauri::{AppHandle, Manager};

/// One on-screen output: the clip container under one window's WebView, plus the display layer the
/// sink enqueues into. Main-thread state throughout.
struct Output {
    /// The window the container hangs in — kept for the backing scale factor and the content box.
    window: Retained<NSWindow>,
    container: Retained<NSView>,
    /// The sink's display layer, while one is attached.
    video: Option<Retained<CALayer>>,
    /// Last rect pushed (physical px: x, y, w, h, cx, cy, cw, ch) — re-applied when a layer is
    /// attached after the rect arrived.
    rect: [i32; 8],
}

thread_local! {
    /// Outputs by surface key (`window/id`). Only ever touched on the main thread.
    static OUTPUTS: RefCell<HashMap<String, Output>> = RefCell::new(HashMap::new());
    /// Picture orientation — a property of the STREAM, so it applies to every output and to any
    /// output created later.
    static ORIENT: Cell<(bool, bool)> = const { Cell::new((false, false)) };
}

/// The app handle, kept for the main-thread hops (`on_main`) and for resolving window labels.
static APP: OnceLock<AppHandle> = OnceLock::new();

/// Remember the app handle. Call once from Tauri's setup; the containers themselves are created
/// per surface when the sink attaches a layer, because a surface can be in a window that does not
/// exist yet (the detached video window).
pub fn install(app: &AppHandle) {
    let _ = APP.set(app.clone());
}

/// Run `f` against the output map on the main thread. Silently nothing without an app handle
/// (install not called — the MJPEG route needs none of this).
fn on_main(f: impl FnOnce(&mut HashMap<String, Output>) + Send + 'static) {
    let Some(app) = APP.get() else { return };
    let _ = app.run_on_main_thread(move || {
        OUTPUTS.with(|m| f(&mut m.borrow_mut()));
    });
}

/// Build the container view for `label`'s window and hang it under that window's WebView.
/// Main thread only.
fn create_container(label: &str) -> Result<Output, String> {
    let Some(mtm) = MainThreadMarker::new() else {
        return Err("not on the main thread".into());
    };
    let app = APP.get().ok_or("no app handle")?;
    let window = app
        .get_webview_window(label)
        .ok_or_else(|| format!("no window labelled {label}"))?;
    let ns_window = window.ns_window().map_err(|e| format!("ns_window: {e}"))?;
    // SAFETY: tauri hands out the live NSWindow of this window; retaining it keeps it valid for
    // as long as the output exists. That matters for the detached video window, whose outputs are
    // dropped a moment AFTER it closed (its surfaces are withdrawn by the destroy hook, and the
    // decode thread acts on the next push) — the retain is what makes that ordering harmless.
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
    // Below every existing sibling — the WebView included — so it paints only where the DOM is
    // transparent. Nothing is removed or re-parented.
    content.addSubview_positioned_relativeTo(&container, NSWindowOrderingMode::Below, None);

    log::info!(
        "[video] apple host: container in window {label} (backing scale {})",
        ns_window.backingScaleFactor()
    );
    Ok(Output {
        window: ns_window,
        container,
        video: None,
        rect: [0, 0, 1, 1, 0, 0, 1, 1],
    })
}

/// Apply an output's rect to its container (visible box) and video layer (full box), converting
/// physical px with a top-left origin into points with AppKit's bottom-left origin.
fn layout(out: &Output) {
    let s = out.window.backingScaleFactor().max(1.0);
    let p = |v: i32| v as f64 / s;
    let [x, y, w, h, cx, cy, cw, ch] = out.rect;
    let Some(content) = out.window.contentView() else { return };
    let content_h = content.frame().size.height;
    let (cw_pt, ch_pt) = (p(cw).max(1.0), p(ch).max(1.0));
    out.container.setFrame(CGRect::new(
        CGPoint::new(p(cx), content_h - p(cy) - ch_pt),
        CGSize::new(cw_pt, ch_pt),
    ));
    if let Some(video) = &out.video {
        let (w_pt, h_pt) = (p(w).max(1.0), p(h).max(1.0));
        // Inside the container's layer (bottom-left origin): the full box offset by the clip.
        let left = p(x - cx);
        let bottom = ch_pt - p(y - cy) - h_pt;
        video.setBounds(CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(w_pt, h_pt)));
        video.setPosition(CGPoint::new(left + w_pt / 2.0, bottom + h_pt / 2.0));
        video.setContentsScale(s);
        let (mirror, rotate180) = ORIENT.with(|o| o.get());
        video.setTransform(transform(mirror, rotate180));
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

/// Create output `key`'s display layer ON the main thread (`make` runs there — AppKit and
/// CoreAnimation objects are created where they are used), building the container in `window`'s
/// NSWindow first if this is the surface's first appearance, and lay it out at the last known
/// rect. Returns once the main thread is done, so the caller can start enqueueing.
pub fn attach(
    key: String,
    window: String,
    make: impl FnOnce() -> Retained<CALayer> + Send + 'static,
) -> Result<(), String> {
    let (tx, rx) = channel::<Result<(), String>>();
    on_main(move |outs| {
        let built = match outs.entry(key.clone()) {
            std::collections::hash_map::Entry::Occupied(e) => Ok(e.into_mut()),
            std::collections::hash_map::Entry::Vacant(e) => {
                create_container(&window).map(|out| e.insert(out))
            }
        };
        let res = match built {
            Ok(out) => {
                if let Some(old) = out.video.take() {
                    old.removeFromSuperlayer();
                }
                let layer = make();
                if let Some(root) = out.container.layer() {
                    root.addSublayer(&layer);
                }
                out.video = Some(layer);
                layout(out);
                Ok(())
            }
            Err(e) => Err(e),
        };
        let _ = tx.send(res);
    });
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(res) => res,
        Err(_) => Err("display layer attach timed out".into()),
    }
}

/// On-screen rect of one output (PHYSICAL px, ITS window's client coords, top-left origin):
/// FULL box `x/y/w/h` for the video's aspect-fit layout, VISIBLE box `cx/cy/cw/ch` for the clip.
pub fn place(key: String, rect: [i32; 8]) {
    on_main(move |outs| {
        if let Some(out) = outs.get_mut(&key) {
            out.rect = rect;
            layout(out);
        }
    });
}

/// Drop one output: its layer leaves the tree and its container leaves the window. macOS does NOT
/// cache a hidden output the way the Windows sink does — there, a hidden output costs nothing
/// because one decoder feeds every output; here each layer decodes for itself, so keeping one for
/// a surface that is not on screen would burn a whole VideoToolbox session for nothing.
pub fn remove(key: String) {
    on_main(move |outs| {
        if let Some(out) = outs.remove(&key) {
            if let Some(v) = &out.video {
                v.removeFromSuperlayer();
            }
            out.container.removeFromSuperview();
        }
    });
}

/// Drop every output (the stream ended).
pub fn remove_all() {
    on_main(|outs| {
        for (_, out) in outs.drain() {
            if let Some(v) = &out.video {
                v.removeFromSuperlayer();
            }
            out.container.removeFromSuperview();
        }
    });
}

/// The real `NSFloatingWindowLevel`. Worth spelling out: tao defines its `NSFloatingWindowLevel`
/// as `kCGFloatingWindowLevelKey`, and the `kCG*WindowLevelKey` constants are KEYS to be passed to
/// `CGWindowLevelForKey()`, not levels — so `always_on_top` lands the window at 5 instead of 3.
/// Both are above normal (0), which is why it mostly works, but it is not the level AppKit means.
const NS_FLOATING_WINDOW_LEVEL: isize = 3;

/// macOS: pin `label`'s window above ordinary windows so it stays visible over other applications
/// (the detached video window's whole point on a multi-monitor desk). Re-asserted here, after the
/// window is realised and shown, because the builder flag is applied during creation.
pub fn float_window(label: String) {
    on_main(move |_| {
        let Some(app) = APP.get() else { return };
        let Some(window) = app.get_webview_window(&label) else { return };
        let Ok(ptr) = window.ns_window() else { return };
        // SAFETY: tauri hands out the live NSWindow of this window.
        let Some(ns_window) = (unsafe { Retained::retain(ptr.cast::<NSWindow>()) }) else { return };
        let before = ns_window.level();
        ns_window.setLevel(NS_FLOATING_WINDOW_LEVEL);
        // Only worth a line when it actually moved: this is re-asserted after every fullscreen
        // toggle, and a no-op pin is not news.
        if before != NS_FLOATING_WINDOW_LEVEL {
            log::info!("[video] apple host: {label} window level {before} -> {NS_FLOATING_WINDOW_LEVEL}");
        }
    });
}

/// Hold `label`'s window to the picture's shape while the user resizes it. AppKit enforces a
/// content aspect ratio inside its own resize loop, which is the only place it can be enforced
/// without fighting the drag.
///
/// The ratio is the WINDOW's, not the picture's: the page's chrome (`ring`, physical px) is a fixed
/// border around the picture, so it shifts the ratio slightly, and by more the smaller the window
/// is. It is therefore worked out from the window's size at the time — the page re-sends the shape
/// whenever a resize settles, so the ratio it is held to is always the one it currently has. What
/// remains is a pixel or two at the far end of a long drag, which the page's own snap takes care
/// of. `aspect <= 0` releases the window (fullscreen).
pub fn set_aspect(label: String, aspect: f64, ring: f64) {
    on_main(move |_| {
        let Some(app) = APP.get() else { return };
        let Some(window) = app.get_webview_window(&label) else { return };
        let Ok(ptr) = window.ns_window() else { return };
        // SAFETY: tauri hands out the live NSWindow of this window.
        let Some(ns_window) = (unsafe { Retained::retain(ptr.cast::<NSWindow>()) }) else { return };
        if aspect <= 0.0 {
            ns_window.setContentAspectRatio(CGSize::new(0.0, 0.0));
            return;
        }
        let size = ns_window.contentView().map(|v| v.frame().size).unwrap_or(CGSize::new(0.0, 0.0));
        let scale = ns_window.backingScaleFactor().max(1.0);
        let border = ring / scale; // the ring arrives in physical px, AppKit works in points
        let picture = (size.width - border).max(160.0);
        ns_window.setContentAspectRatio(CGSize::new(picture + border, picture / aspect + border));
    });
}

/// Stack the containers the way the DOM stacks their surfaces. `keys` arrives highest-priority
/// FIRST (`main` > `floating` > `widget`), and DOM stacking is the reverse of that — the widget dock
/// paints over the floating window, which paints over the fullscreen swap. So the list is walked
/// BACKWARDS and each container sent below all its siblings: the last one moved ends up lowest,
/// which leaves the first entry at the bottom and the widget on top. `Below` with no reference view
/// also keeps every container under the WebView, which is what makes the hole punch work at all.
///
/// Only called when the order actually changed (re-adding a subview moves it, which is a real
/// compositing update), and per window — containers in different windows never compete.
pub fn restack(keys: Vec<String>) {
    on_main(move |outs| {
        for key in keys.iter().rev() {
            let Some(out) = outs.get(key) else { continue };
            let Some(content) = out.window.contentView() else { continue };
            content.addSubview_positioned_relativeTo(
                &out.container,
                NSWindowOrderingMode::Below,
                None,
            );
        }
    });
}

/// Mirror / 180° rotation — a transform on every display layer, no decoder involvement.
pub fn set_orient(mirror: bool, rotate180: bool) {
    on_main(move |outs| {
        ORIENT.with(|o| o.set((mirror, rotate180)));
        for out in outs.values() {
            layout(out);
        }
    });
}
