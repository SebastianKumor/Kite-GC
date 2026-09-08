// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Marc Hoffmann (b14ckyy)

//! What a window's surface router publishes: every DOM hole the native decode sink has to present
//! into right now (VIDEO_MULTISINK_WINDOW.md §4.1).
//!
//! One decoded stream, several on-screen surfaces — the video widget's tile and the floating window
//! at the same time, and with the detached window a surface that lives in a DIFFERENT native window.
//! A surface is therefore identified by **(window label, surface id)**, and the whole list is
//! replaced on every push: a surface that stops being reported is gone. That makes visibility
//! implicit (no second command that could arrive out of order) and one message per frame enough.

/// One surface as the frontend reports it (PHYSICAL px, its own window's client coords): the FULL
/// box `x/y/w/h` the video is aspect-fitted into, and the VISIBLE box `cx/cy/cw/ch` it is cut at.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct SurfaceRect {
    pub id: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub cx: i32,
    pub cy: i32,
    pub cw: i32,
    pub ch: i32,
}

/// A reported surface resolved against the window it came from — what the sinks work with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkSurface {
    /// `window/id`, stable for as long as the surface exists: the sinks key their outputs on it.
    pub key: String,
    /// Tauri window label the hole lives in (`main`, later `video`). The AppKit/GTK hosts look the
    /// window up by it.
    pub window: String,
    /// Native parent window handle for the platforms that parent a child window to it (Windows).
    /// 0 when the label has no handle registered — such a surface is skipped, never guessed.
    pub parent: isize,
    /// FULL box (x, y, w, h) — the video's aspect-fit layout box.
    pub full: (i32, i32, i32, i32),
    /// VISIBLE box (x, y, w, h) — what is left of it after the DOM's clipping ancestors.
    pub clip: (i32, i32, i32, i32),
}

impl SinkSurface {
    pub fn new(window: &str, parent: isize, r: &SurfaceRect) -> Self {
        Self {
            key: format!("{window}/{}", r.id),
            window: window.to_string(),
            parent,
            full: (r.x, r.y, r.w, r.h),
            clip: (r.cx, r.cy, r.cw, r.ch),
        }
    }
}
