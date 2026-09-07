// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Marc Hoffmann (b14ckyy)

//! macOS hardware video decode sink (MOBILE_RTSP.md P3) — the AVFoundation counterpart of
//! `win_sink` / `android_sink`: H.264/HEVC access units from the RTSP client are wrapped as
//! compressed `CMSampleBuffer`s and enqueued into an `AVSampleBufferDisplayLayer`, which decodes
//! them through VideoToolbox and renders straight into the hole-punch layer below the WebView
//! (`apple_host.rs`). One decode thread owns the conversion; the layer lives in the host.
//!
//! The real work is the framing conversion: the depacketizer delivers Annex-B (start codes,
//! in-band parameter sets), CoreMedia wants AVCC/HVCC (4-byte big-endian length prefixes, the
//! parameter sets in a `CMVideoFormatDescription`). Per AU the NAL units are split, VPS/SPS/PPS
//! are captured (the description is rebuilt only when their bytes change — encoders resend
//! identical sets on every keyframe), and the slice/SEI units are re-emitted length-prefixed.
//!
//! Presentation: with the smoothing buffer at 0 (the latency-first default) every sample is
//! tagged `DisplayImmediately`. Depths 1–3 schedule each frame `depth × frame-interval` behind
//! the media timeline on the layer's control timebase — the Android `Pacing` logic (EMA
//! interval, anchor, re-anchor on jumps or depth changes).
//!
//! Resume on keyframe: after a flush (decode error, `requiresFlushToResumeDecoding`) AUs are
//! dropped until one carries an IDR/IRAP, so the decoder never restarts mid-GOP.

// API generation: the direct AVSampleBufferDisplayLayer enqueue/flush/status methods are what
// macOS 13 (our minimum) offers; macOS 14 moved them to `sampleBufferRenderer` and marked these
// deprecated. One generation, targeted cleanly — revisit when the minimum OS moves to 14.
// (`CMTimebase::create_with_master_clock` and `CFDictionarySetValue` are renamed-only.)
#![allow(deprecated)]

use std::ptr::{null, null_mut, NonNull};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use objc2::rc::Retained;
use objc2_av_foundation::{
    AVLayerVideoGravityResizeAspect, AVQueuedSampleBufferRenderingStatus, AVSampleBufferDisplayLayer,
};
use objc2_core_foundation::{kCFBooleanTrue, CFArray, CFDictionarySetValue, CFMutableDictionary, CFRetained};
use objc2_core_media::{
    kCMBlockBufferAssureMemoryNowFlag, kCMSampleAttachmentKey_DisplayImmediately, kCMTimeInvalid,
    CMBlockBuffer, CMClock, CMFormatDescription, CMSampleBuffer, CMSampleTimingInfo, CMTime, CMTimebase,
    CMVideoFormatDescriptionCreateFromH264ParameterSets, CMVideoFormatDescriptionCreateFromHEVCParameterSets,
    CMVideoFormatDescriptionGetPresentationDimensions,
};

use super::apple_host as host;
use super::rtsp::VideoCodec;
use super::surface::SinkSurface;

/// How long the initial layer attach may take (one main-thread hop away) before the sink
/// declares failure.
const FIRST_LAYER_TIMEOUT: Duration = Duration::from_secs(10);
/// RTP video clock — the timestamps `push` receives are 90 kHz ticks.
const TIMESCALE: i32 = 90_000;

enum Cmd {
    /// One access unit (Annex-B, in-band parameter sets) + its unwrapped 90 kHz timestamp.
    Frame(Vec<u8>, u64),
    Orient { mirror: bool, rotate180: bool },
    Stop,
}

#[derive(Default)]
struct Shared {
    error: Mutex<Option<String>>,
    presented: AtomicU64,
    width: AtomicU32,
    height: AtomicU32,
    /// Smoothing-buffer depth in frames (0 = display immediately).
    buffer_frames: AtomicU32,
    stopping: AtomicBool,
    /// No DOM surface on screen — the layer is hidden by the host; decoding continues so the
    /// picture is there the instant it comes back.
    hidden: AtomicBool,
}

impl Shared {
    fn fail(&self, msg: String) {
        log::warn!("[video] apple sink: {msg}");
        self.error.lock().unwrap().get_or_insert(msg);
    }
}

/// The display layer, shared between the main thread (tree, geometry) and the decode thread
/// (enqueue / flush / status — AVFoundation's queued-rendering entry points are thread-safe).
struct Layer(Retained<AVSampleBufferDisplayLayer>);
// SAFETY: the layer's tree membership and geometry are only touched on the main thread (host);
// the decode thread uses the enqueue/flush/status calls, which AVFoundation documents as
// callable from any thread.
unsafe impl Send for Layer {}
unsafe impl Sync for Layer {}

pub struct AppleVideoSink {
    tx: Sender<Cmd>,
    thread: Option<JoinHandle<()>>,
    shared: Arc<Shared>,
}

impl AppleVideoSink {
    /// Bring the sink up for `codec`: create the display layer (on the main thread, inside the
    /// host's container) and start the decode thread. Decoder problems surface later through
    /// [`Self::error`] — the same contract as the other sinks.
    pub fn start(codec: VideoCodec) -> Result<Self, String> {
        let hevc = matches!(codec, VideoCodec::H265);
        let shared = Arc::new(Shared::default());
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = {
            let shared = shared.clone();
            std::thread::spawn(move || {
                decode_loop(hevc, &rx, &shared);
                host::detach();
            })
        };
        Ok(Self { tx, thread: Some(thread), shared })
    }

    /// Queue one access unit (Annex-B) with its unwrapped 90 kHz timestamp.
    pub fn push(&self, au: Vec<u8>, ts90k: u64) {
        let _ = self.tx.send(Cmd::Frame(au, ts90k));
    }

    /// First fatal sink error, if any — the stream ends on it (see rtsp_native).
    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().unwrap().clone()
    }

    /// The surfaces to present into (VIDEO_MULTISINK_WINDOW.md §4.1). This platform drives ONE
    /// output for now, so the first entry wins — the router publishes them highest-priority first —
    /// and an empty list hides the layer. Two outputs here are the follow-up (§4.2).
    pub fn set_surfaces(&self, surfaces: &[SinkSurface]) {
        match surfaces.first() {
            Some(s) => {
                self.set_rect(s.full.0, s.full.1, s.full.2, s.full.3, s.clip.0, s.clip.1, s.clip.2, s.clip.3);
                self.set_visible(true);
            }
            None => self.set_visible(false),
        }
    }

    /// On-screen video rect (PHYSICAL px, window coords): the FULL box `x/y/w/h` for the
    /// aspect-fit layout plus the VISIBLE part `cx/cy/cw/ch` — the host clips the video at that
    /// edge (scrolled panels), it never shrinks into the remainder.
    #[allow(clippy::too_many_arguments)]
    fn set_rect(&self, x: i32, y: i32, w: i32, h: i32, cx: i32, cy: i32, cw: i32, ch: i32) {
        host::set_rect(x, y, w, h, cx, cy, cw, ch);
    }

    fn set_visible(&self, visible: bool) {
        self.shared.hidden.store(!visible, Ordering::Relaxed);
        host::set_visible(visible);
    }

    /// Smoothing-buffer depth in frames (0 = display immediately — the latency-first default).
    pub fn set_buffer(&self, frames: u32) {
        self.shared.buffer_frames.store(frames.min(3), Ordering::Relaxed);
    }

    /// Mirror / 180° rotation — a layer transform, applied live.
    pub fn set_orient(&self, mirror: bool, rotate180: bool) {
        let _ = self.tx.send(Cmd::Orient { mirror, rotate180 });
    }

    pub fn frames_presented(&self) -> u64 {
        self.shared.presented.load(Ordering::Relaxed)
    }

    /// Display picture size (clean aperture, not the coded size), once the parameter sets arrived.
    pub fn picture_size(&self) -> Option<(u32, u32)> {
        let w = self.shared.width.load(Ordering::Relaxed);
        let h = self.shared.height.load(Ordering::Relaxed);
        (w > 0 && h > 0).then_some((w, h))
    }
}

impl Drop for AppleVideoSink {
    fn drop(&mut self) {
        self.shared.stopping.store(true, Ordering::SeqCst);
        let _ = self.tx.send(Cmd::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Create the display layer on the main thread (inside the host's attach) and return the shared
/// handle the decode thread enqueues through.
fn create_layer() -> Result<Arc<Layer>, String> {
    let slot: Arc<Mutex<Option<Arc<Layer>>>> = Arc::new(Mutex::new(None));
    let fill = slot.clone();
    let (tx, rx) = std::sync::mpsc::channel::<Result<(), String>>();
    std::thread::spawn(move || {
        let res = host::attach(move || {
            // SAFETY: plain AVFoundation object creation + property setup on the main thread.
            let layer = unsafe { AVSampleBufferDisplayLayer::new() };
            unsafe {
                if let Some(g) = AVLayerVideoGravityResizeAspect {
                    layer.setVideoGravity(g);
                }
            }
            *fill.lock().unwrap() = Some(Arc::new(Layer(layer.clone())));
            Retained::into_super(layer)
        });
        let _ = tx.send(res);
    });
    rx.recv_timeout(FIRST_LAYER_TIMEOUT)
        .map_err(|_| "display layer attach timed out".to_string())??;
    let layer = slot.lock().unwrap().take();
    layer.ok_or_else(|| "display layer was not created".into())
}

/// The decode thread: attach the layer, then convert + enqueue AUs until stop.
fn decode_loop(hevc: bool, rx: &Receiver<Cmd>, shared: &Shared) {
    let layer = match create_layer() {
        Ok(l) => l,
        Err(e) => {
            shared.fail(e);
            return;
        }
    };
    log::info!("[video] apple sink: display layer up ({})", if hevc { "HEVC" } else { "H.264" });

    let mut sets = ParameterSets::default();
    let mut format: Option<CFRetained<CMFormatDescription>> = None;
    // The first session starts on the stream's first intra frame; a flush restarts the wait.
    let mut wait_for_idr = true;
    let mut pacing = Pacing::default();
    let mut timebase: Option<CFRetained<CMTimebase>> = None;

    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Cmd::Stop) | Err(RecvTimeoutError::Disconnected) => {
                unsafe { layer.0.flushAndRemoveImage() };
                return;
            }
            Ok(Cmd::Orient { mirror, rotate180 }) => host::set_orient(mirror, rotate180),
            Ok(Cmd::Frame(au, ts90k)) => {
                // Parameter sets first — a rebuilt description is what makes the next IDR decodable.
                let changed = sets.absorb(hevc, &au);
                if changed || format.is_none() {
                    match sets.format_description(hevc) {
                        Ok(Some(desc)) => {
                            // SAFETY: valid description from the create call above.
                            let dims = unsafe {
                                CMVideoFormatDescriptionGetPresentationDimensions(&desc, false, true)
                            };
                            if dims.width > 0.0 && dims.height > 0.0 {
                                shared.width.store(dims.width as u32, Ordering::Relaxed);
                                shared.height.store(dims.height as u32, Ordering::Relaxed);
                                log::info!("[video] apple sink: picture {}x{}", dims.width as u32, dims.height as u32);
                            }
                            format = Some(desc);
                            if changed {
                                // New sets mid-stream: the decoder must restart on the next IDR.
                                wait_for_idr = true;
                            }
                        }
                        Ok(None) => {} // sets incomplete — keep waiting
                        Err(e) => {
                            shared.fail(e);
                            return;
                        }
                    }
                }
                let Some(desc) = format.as_ref() else { continue };
                if wait_for_idr && !has_intra_frame(hevc, &au) {
                    continue;
                }
                wait_for_idr = false;

                // A layer that failed decoding needs a flush before it accepts anything again.
                // SAFETY: thread-safe queued-rendering calls.
                unsafe {
                    if layer.0.status() == AVQueuedSampleBufferRenderingStatus::Failed {
                        let msg = layer
                            .0
                            .error()
                            .map(|e| e.localizedDescription().to_string())
                            .unwrap_or_else(|| "display layer failed".into());
                        shared.fail(format!("decode failed: {msg}"));
                        return;
                    }
                    if layer.0.requiresFlushToResumeDecoding() {
                        log::warn!("[video] apple sink: layer requires a flush — resuming on the next keyframe");
                        layer.0.flush();
                        wait_for_idr = true;
                        pacing.anchor = None;
                        continue;
                    }
                }

                let depth = shared.buffer_frames.load(Ordering::Relaxed);
                let pts = ts90k as i64;
                // Depth > 0 paces on the control timebase; depth 0 displays immediately. Switching
                // depth re-anchors the timeline.
                if depth == 0 {
                    if timebase.take().is_some() {
                        unsafe { layer.0.setControlTimebase(None) };
                    }
                } else {
                    let tb = match timebase.as_ref() {
                        Some(tb) => tb.clone(),
                        None => match new_timebase() {
                            Ok(tb) => {
                                unsafe { layer.0.setControlTimebase(Some(&tb)) };
                                timebase = Some(tb.clone());
                                tb
                            }
                            Err(e) => {
                                shared.fail(e);
                                return;
                            }
                        },
                    };
                    pacing.schedule(&tb, pts, depth);
                }

                let sample = match make_sample(hevc, desc, &au, pts, depth == 0) {
                    Ok(s) => s,
                    Err(e) => {
                        shared.fail(e);
                        return;
                    }
                };
                // SAFETY: valid sample buffer; enqueue is thread-safe.
                unsafe { layer.0.enqueueSampleBuffer(&sample) };
                shared.presented.fetch_add(1, Ordering::Relaxed);
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

/// A control timebase on the host clock, running at rate 1 — the layer displays a sample when
/// the timebase reaches its PTS.
fn new_timebase() -> Result<CFRetained<CMTimebase>, String> {
    // SAFETY: CoreMedia create call with an out-pointer; the retained result is taken over below.
    unsafe {
        let clock = CMClock::host_time_clock();
        let mut out: *mut CMTimebase = null_mut();
        let st = CMTimebase::create_with_master_clock(None, &clock, NonNull::new_unchecked(&mut out));
        if st != 0 || out.is_null() {
            return Err(format!("CMTimebaseCreate failed ({st})"));
        }
        let tb = CFRetained::from_raw(NonNull::new_unchecked(out));
        tb.set_rate(1.0);
        Ok(tb)
    }
}

/// Presentation pacing for smoothing-buffer depths > 0 (the Android sink's logic): the
/// timebase is anchored so that a frame's PTS falls `depth × frame-interval` after now; the EMA
/// of the media frame interval sizes that cushion; a timeline jump or a changed depth re-anchors.
#[derive(Default)]
struct Pacing {
    /// (media pts at anchor, depth) — once anchored, the timebase runs on its own.
    anchor: Option<(i64, u32)>,
    /// EMA of the media-time frame interval (90 kHz ticks); seeds at 60 fps.
    interval: i64,
    last_pts: Option<i64>,
}

impl Pacing {
    fn schedule(&mut self, tb: &CMTimebase, pts: i64, depth: u32) {
        if self.interval == 0 {
            self.interval = 1500; // 60 fps in 90 kHz ticks
        }
        if let Some(last) = self.last_pts {
            let delta = pts - last;
            if (450..=9000).contains(&delta) {
                self.interval += (delta - self.interval) / 8;
            }
        }
        self.last_pts = Some(pts);
        let lead = depth as i64 * self.interval;
        // SAFETY: plain timebase reads/writes.
        unsafe {
            let now = tb.time();
            let now_ticks = if now.timescale == TIMESCALE { now.value } else { now.value * TIMESCALE as i64 / now.timescale.max(1) as i64 };
            let re_anchor = match self.anchor {
                Some((_, d)) if d == depth => {
                    // Drifted past the cushion or behind the clock: re-anchor (no runaway schedule).
                    let due_in = pts - now_ticks;
                    !(0..=lead + 9000).contains(&due_in)
                }
                _ => true,
            };
            if re_anchor {
                tb.set_time(CMTime::new(pts - lead, TIMESCALE));
                self.anchor = Some((pts, depth));
            }
        }
    }
}

/// VPS/SPS/PPS bytes as last seen (emulation prevention kept — CoreMedia wants raw NAL bytes).
#[derive(Default)]
struct ParameterSets {
    vps: Vec<Vec<u8>>,
    sps: Vec<Vec<u8>>,
    pps: Vec<Vec<u8>>,
}

impl ParameterSets {
    /// Capture the parameter sets in `au`. Returns true when the set changed byte-wise.
    fn absorb(&mut self, hevc: bool, au: &[u8]) -> bool {
        let mut changed = false;
        for nal in nal_units(au) {
            let Some(&b0) = nal.first() else { continue };
            let slot = if hevc {
                match (b0 >> 1) & 0x3f {
                    32 => &mut self.vps,
                    33 => &mut self.sps,
                    34 => &mut self.pps,
                    _ => continue,
                }
            } else {
                match b0 & 0x1f {
                    7 => &mut self.sps,
                    8 => &mut self.pps,
                    _ => continue,
                }
            };
            if !slot.iter().any(|s| s == nal) {
                // A re-sent set with the same id but different content replaces; a genuinely new
                // one (different id) is added. Ids are not parsed: for the streams a GCS meets,
                // the newest instance per type is what the encoder uses.
                slot.clear();
                slot.push(nal.to_vec());
                changed = true;
            }
        }
        changed
    }

    /// Build the format description once all required sets are present.
    fn format_description(&self, hevc: bool) -> Result<Option<CFRetained<CMFormatDescription>>, String> {
        let mut all: Vec<&[u8]> = Vec::new();
        if hevc {
            if self.vps.is_empty() || self.sps.is_empty() || self.pps.is_empty() {
                return Ok(None);
            }
            all.extend(self.vps.iter().map(Vec::as_slice));
        } else if self.sps.is_empty() || self.pps.is_empty() {
            return Ok(None);
        }
        all.extend(self.sps.iter().map(Vec::as_slice));
        all.extend(self.pps.iter().map(Vec::as_slice));
        let mut ptrs: Vec<NonNull<u8>> = all.iter().map(|s| NonNull::from(&s[0])).collect();
        let mut sizes: Vec<usize> = all.iter().map(|s| s.len()).collect();
        let mut out: *const CMFormatDescription = null();
        // SAFETY: the pointer/size arrays outlive the call; CoreMedia copies the sets.
        let st = unsafe {
            if hevc {
                CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                    None,
                    all.len(),
                    NonNull::new_unchecked(ptrs.as_mut_ptr()),
                    NonNull::new_unchecked(sizes.as_mut_ptr()),
                    4,
                    None,
                    NonNull::new_unchecked(&mut out),
                )
            } else {
                CMVideoFormatDescriptionCreateFromH264ParameterSets(
                    None,
                    all.len(),
                    NonNull::new_unchecked(ptrs.as_mut_ptr()),
                    NonNull::new_unchecked(sizes.as_mut_ptr()),
                    4,
                    NonNull::new_unchecked(&mut out),
                )
            }
        };
        if st != 0 || out.is_null() {
            return Err(format!("format description from parameter sets failed ({st})"));
        }
        // SAFETY: a +1 retained object from a Create call.
        Ok(Some(unsafe { CFRetained::from_raw(NonNull::new_unchecked(out as *mut CMFormatDescription)) }))
    }
}

/// One compressed sample: the AU's slice/SEI units with 4-byte length prefixes (parameter sets
/// and access-unit delimiters dropped — they live in the description / carry nothing).
fn make_sample(
    hevc: bool,
    desc: &CMFormatDescription,
    au: &[u8],
    pts: i64,
    display_immediately: bool,
) -> Result<CFRetained<CMSampleBuffer>, String> {
    let mut data: Vec<u8> = Vec::with_capacity(au.len() + 16);
    for nal in nal_units(au) {
        let Some(&b0) = nal.first() else { continue };
        let skip = if hevc {
            matches!((b0 >> 1) & 0x3f, 32 | 33 | 34 | 35)
        } else {
            matches!(b0 & 0x1f, 7 | 8 | 9)
        };
        if skip {
            continue;
        }
        data.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        data.extend_from_slice(nal);
    }
    if data.is_empty() {
        return Err("access unit without slices".into());
    }

    // SAFETY: CoreMedia create calls with out-pointers; every retained result is taken over.
    unsafe {
        let mut bb: *mut CMBlockBuffer = null_mut();
        let st = CMBlockBuffer::create_with_memory_block(
            None,
            null_mut(),
            data.len(),
            None,
            null(),
            0,
            data.len(),
            kCMBlockBufferAssureMemoryNowFlag,
            NonNull::new_unchecked(&mut bb),
        );
        if st != 0 || bb.is_null() {
            return Err(format!("CMBlockBufferCreate failed ({st})"));
        }
        let bb = CFRetained::from_raw(NonNull::new_unchecked(bb));
        let st = CMBlockBuffer::replace_data_bytes(
            NonNull::new_unchecked(data.as_ptr() as *mut std::ffi::c_void),
            &bb,
            0,
            data.len(),
        );
        if st != 0 {
            return Err(format!("CMBlockBufferReplaceDataBytes failed ({st})"));
        }

        let timing = CMSampleTimingInfo {
            duration: kCMTimeInvalid,
            presentationTimeStamp: CMTime::new(pts, TIMESCALE),
            decodeTimeStamp: kCMTimeInvalid,
        };
        let size = data.len();
        let mut sb: *mut CMSampleBuffer = null_mut();
        let st = CMSampleBuffer::create_ready(
            None,
            Some(&bb),
            Some(desc),
            1,
            1,
            &timing,
            1,
            &size,
            NonNull::new_unchecked(&mut sb),
        );
        if st != 0 || sb.is_null() {
            return Err(format!("CMSampleBufferCreateReady failed ({st})"));
        }
        let sb = CFRetained::from_raw(NonNull::new_unchecked(sb));

        if display_immediately {
            if let Some(atts) = sb.sample_attachments_array(true) {
                let atts: &CFArray = &atts;
                let dict = atts.value_at_index(0) as *const CFMutableDictionary;
                if let (Some(dict), Some(yes)) = (dict.as_ref(), kCFBooleanTrue) {
                    CFDictionarySetValue(
                        Some(dict),
                        (kCMSampleAttachmentKey_DisplayImmediately as *const _) as *const std::ffi::c_void,
                        (yes as *const _) as *const std::ffi::c_void,
                    );
                }
            }
        }
        Ok(sb)
    }
}

/// Split an Annex-B byte stream into NAL unit payloads (start codes of 3 or 4 bytes).
fn nal_units(au: &[u8]) -> Vec<&[u8]> {
    let mut starts: Vec<usize> = Vec::new();
    let mut i = 0usize;
    while i + 3 <= au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (k, &s) in starts.iter().enumerate() {
        let mut e = if k + 1 < starts.len() { starts[k + 1] - 3 } else { au.len() };
        // A 4-byte start code leaves a trailing zero on the previous unit.
        while e > s && au[e - 1] == 0 {
            e -= 1;
        }
        if e > s {
            out.push(&au[s..e]);
        }
    }
    out
}

/// Does this AU contain an intra frame (H.264 IDR / HEVC IRAP)?
fn has_intra_frame(hevc: bool, au: &[u8]) -> bool {
    nal_units(au).iter().any(|nal| {
        let Some(&b0) = nal.first() else { return false };
        if hevc {
            (16..=21).contains(&((b0 >> 1) & 0x3f))
        } else {
            b0 & 0x1f == 5
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_annex_b_and_finds_idr() {
        // SPS (7), PPS (8), IDR (5) with 4- and 3-byte start codes.
        let au = [
            0, 0, 0, 1, 0x67, 1, 2, // SPS
            0, 0, 1, 0x68, 3, // PPS
            0, 0, 0, 1, 0x65, 9, 9, 9, // IDR
        ];
        let units = nal_units(&au);
        assert_eq!(units.len(), 3);
        assert_eq!(units[0], &[0x67, 1, 2]);
        assert_eq!(units[1], &[0x68, 3]);
        assert_eq!(units[2], &[0x65, 9, 9, 9]);
        assert!(has_intra_frame(false, &au));
        assert!(!has_intra_frame(true, &au));

        let mut sets = ParameterSets::default();
        assert!(sets.absorb(false, &au));
        assert!(!sets.absorb(false, &au), "identical sets must not count as a change");
        assert_eq!(sets.sps.len(), 1);
    }
}
