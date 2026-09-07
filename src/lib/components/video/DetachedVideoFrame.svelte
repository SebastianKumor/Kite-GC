<!--
  SPDX-License-Identifier: GPL-3.0-or-later
  Copyright (C) 2026 Marc Hoffmann (b14ckyy)
-->

<script lang="ts">
  // The detached video window's whole UI (VIDEO_MULTISINK_WINDOW.md PR B, §5.2). It runs in a
  // SECOND WebView — its own JS context, no app stores, no map, no telemetry — so everything it
  // knows arrives over the event channel from the main window (controllers/detachedVideo.ts).
  //
  // It looks like the in-app floating window (D4): the same glass ring around the picture, the same
  // lighter top-right corner for resizing, and the picture itself is the drag handle. The window is
  // transparent and undecorated, so the ring IS the window's edge.
  //
  // Its chrome is hover-only (D5): close in the top-left — the corner the app's unplug button sits
  // in, so the same corner takes the picture out and brings it back — and fullscreen bottom-right,
  // clear of the resize corner. Double-click does nothing (D7).
  //
  // The picture is the native decode sink's hardware layer showing through a transparent hole, the
  // same as in the app: this window runs its own surface router, publishing ONE surface (`floating`)
  // under its own window label. See controllers/nativeVideo.ts.
  import { onMount } from 'svelte';
  import { t } from 'svelte-i18n';
  import { getCurrentWindow, PhysicalSize } from '@tauri-apps/api/window';
  import { emitTo, listen, type UnlistenFn } from '@tauri-apps/api/event';
  import { invoke } from '@tauri-apps/api/core';
  import {
    nativeSurface,
    activeNativeSurfaces,
    startNativeSurfaceRouter,
    stopNativeSurfaceRouter,
  } from '$lib/controllers/nativeVideo';
  import type { DetachedState } from '$lib/controllers/detachedVideo';

  /** The ring around the picture, per side (css px): the body's 4 px box-shadow plus its 1 px
   *  border — the in-app frame's FLOAT_BEZEL_PX in the same two parts. The PICTURE carries the
   *  stream's aspect, so the aspect snap has to know how much of the window is not picture. */
  const RING_PX = 5;
  /** How long the window may resize before we snap its height back onto the stream's aspect. Live
   *  snapping fights the OS resize loop (it owns the pointer during a drag), so it happens once the
   *  drag settles — the picture letterboxes for those few frames instead. */
  const SNAP_IDLE_MS = 140;
  /** Move/resize reports to the main window (which persists the box) — coarse on purpose. */
  const REPORT_IDLE_MS = 400;

  const win = getCurrentWindow();

  // NOT named `state`: `$state` would then read as that variable's store subscription.
  let feed = $state<DetachedState>({
    status: 'starting',
    error: null,
    aspect: 16 / 9,
    nativeSink: true,
    reconnecting: false,
    reconnectAttempt: 0,
  });
  let fullscreen = $state(false);
  /** The windowed box (physical px) — kept across a fullscreen trip, which must not be saved as it. */
  let box = $state<{ x: number; y: number; w: number; h: number } | null>(null);

  const live = $derived(feed.status === 'live' && feed.nativeSink);
  const armed = $derived($activeNativeSurfaces.has('floating'));

  // The hardware layer only follows a hole while the router runs; it runs only while there is a
  // picture to place.
  $effect(() => {
    if (live) startNativeSurfaceRouter();
    else stopNativeSurfaceRouter();
  });

  let snapTimer = 0;
  let reportTimer = 0;

  // A saved box carries the aspect of whatever stream was running when it was saved, and the default
  // box ignores the ring — so the frame is fitted to the picture once the stream's aspect is known,
  // and again whenever it changes (a different source, a resolution switch).
  $effect(() => {
    const aspect = feed.aspect;
    if (!aspect || fullscreen) return;
    clearTimeout(snapTimer);
    snapTimer = window.setTimeout(() => void snapAspect(), SNAP_IDLE_MS);
  });

  onMount(() => {
    document.documentElement.classList.add('detached-video');
    const offs: UnlistenFn[] = [];
    void listen<DetachedState>('video-detached-state', (e) => {
      feed = e.payload;
    }).then((u) => offs.push(u));
    // The main window may have pushed the state before this page had a listener — ask for it.
    void emitTo('main', 'video-detached-ready', {});
    // The fullscreen state FIRST: `readBox` must know not to record the screen as the window's box.
    void win
      .isFullscreen()
      .then((f) => {
        fullscreen = f;
        return readBox();
      })
      .catch(() => {});
    void win.onResized(onGeometryChanged).then((u) => offs.push(u));
    void win.onMoved(onGeometryChanged).then((u) => offs.push(u));
    return () => {
      clearTimeout(snapTimer);
      clearTimeout(reportTimer);
      for (const u of offs) u();
      stopNativeSurfaceRouter();
      document.documentElement.classList.remove('detached-video');
    };
  });

  function onGeometryChanged(): void {
    clearTimeout(snapTimer);
    clearTimeout(reportTimer);
    snapTimer = window.setTimeout(() => void snapAspect(), SNAP_IDLE_MS);
    reportTimer = window.setTimeout(() => void reportBox(), REPORT_IDLE_MS);
  }

  /** Remember the current windowed box (skipped while fullscreen — that box is the screen). */
  async function readBox(): Promise<void> {
    if (fullscreen) return;
    try {
      const pos = await win.outerPosition();
      const size = await win.innerSize();
      box = { x: pos.x, y: pos.y, w: size.width, h: size.height };
    } catch {
      /* the window is going away */
    }
  }

  async function reportBox(): Promise<void> {
    await readBox();
    if (!box) return;
    void emitTo('main', 'video-detached-geometry', { ...box, fullscreen }).catch(() => {});
  }

  /** Snap the height so the PICTURE (the box inside the ring) carries the stream's aspect exactly —
   *  the same inside-out sizing the in-app frame uses, so neither ever shows bars. */
  async function snapAspect(): Promise<void> {
    if (fullscreen || !feed.aspect) return;
    try {
      const size = await win.innerSize();
      const ring = 2 * RING_PX * (await win.scaleFactor());
      const target = Math.round((size.width - ring) / feed.aspect + ring);
      if (Math.abs(target - size.height) > 2) {
        await win.setSize(new PhysicalSize(size.width, Math.max(120, target)));
      }
    } catch {
      /* the window is going away */
    }
  }

  function onBodyPointerDown(e: PointerEvent): void {
    if (e.button !== 0 || fullscreen) return;
    void win.startDragging();
  }

  function onGripPointerDown(e: PointerEvent): void {
    e.stopPropagation();
    void win.startResizeDragging('NorthEast');
  }

  async function toggleFullscreen(): Promise<void> {
    const next = !fullscreen;
    if (next) await readBox(); // keep the windowed box; fullscreen must not overwrite it
    fullscreen = next;
    await win.setFullscreen(next);
    // macOS drops the window's level on the way out of fullscreen (see video_detached_pin_top),
    // so the always-on-top promise has to be renewed on every toggle.
    void invoke('video_detached_pin_top').catch(() => {});
    if (!next) await readBox(); // back in a window — that box is the truth again
    if (box) void emitTo('main', 'video-detached-geometry', { ...box, fullscreen: next }).catch(() => {});
  }

  function onKey(e: KeyboardEvent): void {
    if (e.key === 'Escape' && fullscreen) void toggleFullscreen();
  }
</script>

<svelte:window onkeydown={onKey} />

<div class="dv-root" class:fs={fullscreen}>
  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <div
    class="dv-body"
    class:armed
    onpointerdown={onBodyPointerDown}
  >
    {#if live}
      <!-- The transparent hole the hardware layer shows through — see controllers/nativeVideo. -->
      <div class="dv-hole" class:armed use:nativeSurface={'floating'}>
        {#if !armed}<span>{$t('video.starting')}</span>{/if}
      </div>
    {:else}
      <div class="dv-ph">
        {#if feed.reconnecting}
          <!-- The link dropped; the app is retrying. Stopping belongs in the app, so no button. -->
          {$t('video.reconnecting')}{feed.reconnectAttempt ? ` (${feed.reconnectAttempt})` : ''}
        {:else if feed.status === 'error'}
          ⚠ {feed.error}
        {:else}
          {$t('video.starting')}
        {/if}
      </div>
    {/if}

    <!-- Hover chrome (D5): invisible until the pointer is over the frame. -->
    <button
      class="dv-btn dv-close"
      onpointerdown={(e) => e.stopPropagation()}
      onclick={() => void win.close()}
      title={$t('video.detachedClose')}
      aria-label={$t('video.detachedClose')}
    >
      <!-- chain, whole again: the counterpart of the app's broken-chain unplug button -->
      <svg viewBox="3.3 3.3 17.4 17.4" class="filled" aria-hidden="true">
        <path d="M15.69 12.83 19.23 9.29A3.2 3.2 0 0 0 14.71 4.77L11.17 8.31 12.44 9.58 15.98 6.04A1.4 1.4 0 0 1 17.96 8.02L14.42 11.56Z" />
        <path d="M8.31 11.17 4.77 14.71A3.2 3.2 0 0 0 9.29 19.23L12.83 15.69 11.56 14.42 8.02 17.96A1.4 1.4 0 0 1 6.04 15.98L9.58 12.44Z" />
        <path d="M14.51 10.83 10.83 14.51 9.49 13.17 13.17 9.49Z" />
      </svg>
    </button>
    <button
      class="dv-btn dv-fs"
      onpointerdown={(e) => e.stopPropagation()}
      onclick={() => void toggleFullscreen()}
      title={fullscreen ? $t('video.detachedWindowed') : $t('video.detachedFullscreen')}
      aria-label={fullscreen ? $t('video.detachedWindowed') : $t('video.detachedFullscreen')}
    >
      {#if fullscreen}
        <svg viewBox="0 0 24 24" aria-hidden="true">
          <path d="M9 4v5H4M15 4v5h5M9 20v-5H4M15 20v-5h5" />
        </svg>
      {:else}
        <svg viewBox="0 0 24 24" aria-hidden="true">
          <path d="M4 9V4h5M20 9V4h-5M4 15v5h5M20 15v5h-5" />
        </svg>
      {/if}
    </button>
  </div>

  {#if !fullscreen}
    <!-- svelte-ignore a11y_no_static_element_interactions -->
    <div class="dv-grip" onpointerdown={onGripPointerDown} title={$t('video.resizeWindow')}></div>
  {/if}
</div>

<style>
  /* The window is transparent and undecorated: the page IS the frame. Only this route carries the
     class, so the main window's globals are untouched. */
  :global(html.detached-video),
  :global(html.detached-video body) {
    margin: 0;
    padding: 0;
    height: 100%;
    background: transparent;
    overflow: hidden;
    font-family: 'Segoe UI', Tahoma, sans-serif;
    color: #e0e0e0;
    user-select: none;
    -webkit-user-select: none;
  }

  .dv-root {
    position: fixed;
    inset: 0;
  }
  /* Glass ring around the picture — the in-app frame's `nv-active` look, drawn as a box-shadow so
     the body's own overflow can't clip it. `backdrop-filter` is pointless here: a standalone
     window has no page behind it to blur, only the desktop (which is not part of its backdrop). */
  .dv-body {
    position: absolute;
    inset: 4px;
    box-sizing: border-box;
    background: #000;
    overflow: hidden;
    border-radius: 5px;
    border: 1px solid #393939;
    /* No drop shadow: it would be painted outside the window and clipped to a smear at the edge.
       The ring IS the window's border — an undecorated window has no shadow of its own. */
    box-shadow: 0 0 0 4px rgba(30, 30, 30, 0.92);
    cursor: grab;
    touch-action: none;
  }
  .dv-body:active {
    cursor: grabbing;
  }
  .dv-body.armed {
    background: transparent;
  }
  .dv-root.fs .dv-body {
    inset: 0;
    border: none;
    border-radius: 0;
    box-shadow: none;
    cursor: default;
  }

  .dv-hole {
    position: absolute;
    inset: 0;
    display: flex;
    align-items: center;
    justify-content: center;
    color: #888;
    font-size: 12px;
    background: #000;
    /* The router reads this radius and rounds the hole with it. */
    border-radius: 5px;
  }
  .dv-hole.armed {
    background: transparent;
  }
  .dv-root.fs .dv-hole {
    border-radius: 0;
  }
  .dv-ph {
    position: absolute;
    inset: 0;
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 0 10px;
    color: #888;
    font-size: 12px;
    text-align: center;
  }

  /* Hover-only overlay buttons, in the picture's corners. */
  .dv-btn {
    position: absolute;
    width: 32px;
    height: 32px;
    padding: 6px;
    background: rgba(46, 46, 46, 0.82);
    border: 1px solid rgba(55, 168, 219, 0.5);
    border-radius: 6px;
    color: #37a8db;
    cursor: pointer;
    opacity: 0;
    transition: opacity 0.15s ease, background 0.2s;
    pointer-events: none;
  }
  .dv-root:hover .dv-btn {
    opacity: 1;
    pointer-events: auto;
  }
  .dv-btn:hover {
    background: rgba(55, 168, 219, 0.3);
  }
  .dv-btn svg {
    width: 100%;
    height: 100%;
    fill: none;
    stroke: currentColor;
    stroke-width: 2;
    stroke-linecap: round;
    stroke-linejoin: round;
  }
  /* The chain glyph is a filled shape cropped to its own bounds (see the unplug button in
     FloatingVideoWindow) — hence the smaller padding: it must read at this size. */
  .dv-btn svg.filled {
    fill: currentColor;
    stroke: none;
  }
  .dv-close {
    padding: 4px;
  }
  .dv-close {
    top: 8px;
    left: 8px;
  }
  .dv-fs {
    right: 8px;
    bottom: 8px;
  }

  /* Resize corner — the same L the in-app frame draws in its bezel, a shade lighter than the ring. */
  .dv-grip {
    position: absolute;
    top: 0;
    right: 0;
    width: 26px;
    height: 26px;
    box-sizing: border-box;
    background: transparent;
    border-top: 4px solid #5e5e5e;
    border-right: 4px solid #5e5e5e;
    border-top-right-radius: 8px;
    cursor: nesw-resize;
    touch-action: none;
  }
  .dv-grip:hover {
    border-color: #727272;
  }
</style>
