// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Marc Hoffmann (b14ckyy)

//! Aspect-locked interactive resize for the detached video window (Windows).
//!
//! The page alone can only correct the window AFTER a drag: inside the OS's modal resize loop every
//! `setSize` is overwritten by the next mouse move, which is why the frame used to snap into shape
//! the moment the pointer stopped. Windows asks the window itself instead — `WM_SIZING` carries the
//! rectangle the user is dragging out, and whatever the window writes back is what the drag shows.
//! So the window is subclassed and the rectangle is bent there, live.
//!
//! The page still snaps when the drag settles (`DetachedVideoFrame`): that stays the authority for
//! the exact size, and with this in place it has nothing left to correct.

use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Mutex;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, DefWindowProcW, SetWindowLongPtrW, GWLP_WNDPROC, WMSZ_BOTTOM, WMSZ_BOTTOMLEFT,
    WMSZ_LEFT, WMSZ_RIGHT, WMSZ_TOP, WMSZ_TOPLEFT, WMSZ_TOPRIGHT, WM_SIZING,
};

/// The picture's width/height and the chrome around it (physical px, the same on both axes).
/// Aspect `0` = no lock, which is also the state while the window is fullscreen.
static SHAPE: Mutex<(f64, f64)> = Mutex::new((0.0, 0.0));
/// The window procedure this one replaced. `0` until a window is subclassed.
static PREV: AtomicIsize = AtomicIsize::new(0);

/// Smallest picture the lock will produce — below the window's own minimum size, so the minimum is
/// still the one Tauri set.
const MIN_PICTURE: f64 = 80.0;

/// Take over the window's messages. Called once per detached window, right after it was created.
pub fn install(hwnd: isize) {
    let prev = unsafe { SetWindowLongPtrW(HWND(hwnd as *mut _), GWLP_WNDPROC, wndproc as *const () as usize as isize) };
    // A window created before this one is gone by now; only the live handle's procedure is chained.
    PREV.store(prev, Ordering::SeqCst);
}

/// The shape to hold. `aspect <= 0` releases the window (fullscreen).
pub fn set_shape(aspect: f64, ring: f64) {
    if let Ok(mut s) = SHAPE.lock() {
        *s = (aspect, ring);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let prev = PREV.load(Ordering::SeqCst);
    let result = if prev == 0 {
        DefWindowProcW(hwnd, msg, wparam, lparam)
    } else {
        // SAFETY: `prev` is the procedure Windows handed back when this one was installed.
        let prev: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT =
            std::mem::transmute(prev);
        CallWindowProcW(Some(prev), hwnd, msg, wparam, lparam)
    };
    if msg != WM_SIZING || lparam.0 == 0 {
        return result;
    }
    let (aspect, ring) = SHAPE.lock().map(|s| *s).unwrap_or((0.0, 0.0));
    if aspect <= 0.0 {
        return result;
    }
    // The rectangle the drag is proposing, in screen px — bent in place. The chain ran first, so
    // whatever tao wants to say about the size is said before this.
    let rect = &mut *(lparam.0 as *mut RECT);
    let edge = wparam.0 as u32;
    let w = f64::from(rect.right - rect.left) - ring;
    let h = f64::from(rect.bottom - rect.top) - ring;
    // The side being dragged leads. A corner drags both, and then the one that reaches further
    // wins — the same rule the page applies when it snaps.
    let picture = match edge {
        WMSZ_TOP | WMSZ_BOTTOM => h * aspect,
        WMSZ_LEFT | WMSZ_RIGHT => w,
        _ => w.max(h * aspect),
    }
    .max(MIN_PICTURE);
    let width = (picture + ring).round() as i32;
    let height = (picture / aspect + ring).round() as i32;
    // Grow away from the corner the drag is NOT holding.
    match edge {
        WMSZ_LEFT | WMSZ_TOPLEFT | WMSZ_BOTTOMLEFT => rect.left = rect.right - width,
        _ => rect.right = rect.left + width,
    }
    match edge {
        WMSZ_TOP | WMSZ_TOPLEFT | WMSZ_TOPRIGHT => rect.top = rect.bottom - height,
        _ => rect.bottom = rect.top + height,
    }
    LRESULT(1)
}
