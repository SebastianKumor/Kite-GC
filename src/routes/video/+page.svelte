<!--
  SPDX-License-Identifier: GPL-3.0-or-later
  Copyright (C) 2026 Marc Hoffmann (b14ckyy)
-->

<script lang="ts">
  // The detached video window's route (VIDEO_MULTISINK_WINDOW.md PR B). A second WebView opens it
  // as `/video`; the whole UI is DetachedVideoFrame. Deliberately thin: this page must not pull the
  // app in — no map, no telemetry, no video store (its prefs belong to the main window alone).
  import DetachedVideoFrame from '$lib/components/video/DetachedVideoFrame.svelte';
</script>

<DetachedVideoFrame />
