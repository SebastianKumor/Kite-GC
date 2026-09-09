#!/bin/bash
# ============================================================
# Kite Ground Control — iOS / iPad Release Build Script
# Builds a signed .ipa for install/distribution (not live-reload).
# Use scripts/run-ipad.sh for day-to-day dev runs instead.
#
# Runs the full Tauri iOS release flow:
#   npm run build            (SvelteKit web UI -> build/)
#   cargo build ios target   (Rust core -> static lib, release)
#   xcodebuild archive       (wrap + sign -> .ipa)
#
# Min deployment target comes from
# src-tauri/tauri.conf.json -> bundle.iOS.minimumSystemVersion.
# ============================================================
# Prerequisites (one-time):
#   - Node.js (LTS), Rust (via rustup), full Xcode
#   - An Apple ID / signing team configured in the Xcode project
#     (open gen/apple/*.xcodeproj once: target > Signing & Capabilities
#     > Automatically manage signing > pick your Team). Distribution
#     builds need an Apple Developer account.
# ============================================================
# Usage:
#   scripts/build-ios.sh                # release .ipa
#   FORCE_INIT=1 scripts/build-ios.sh   # regenerate gen/apple first
# ============================================================

set -e

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

echo ""
echo "============================================"
echo " Kite Ground Control — iOS Release Build"
echo "============================================"
echo ""

if ! command -v node &> /dev/null; then
    echo "[ERROR] Node.js not found. Install from https://nodejs.org/"
    exit 1
fi

if ! command -v cargo &> /dev/null; then
    echo "[ERROR] Rust/Cargo not found. Install from https://rustup.rs/"
    exit 1
fi

if ! xcode-select -p &> /dev/null || ! command -v xcodebuild &> /dev/null; then
    echo "[ERROR] Xcode not found. Install the full Xcode from the App Store,"
    echo "        then run: sudo xcode-select -s /Applications/Xcode.app"
    exit 1
fi

echo "[1/4] Ensuring iOS Rust target is installed..."
rustup target add aarch64-apple-ios

echo "[2/4] Installing npm dependencies..."
npm install

GEN_APPLE="src-tauri/gen/apple"

# `tauri ios init` lists Externals/ as a plain source group, so XcodeGen puts it in BOTH "Link
# Binary With Libraries" and "Copy Bundle Resources". The second one copies the Rust static library
# into the app bundle, which App Store validation rejects outright:
#
#   Invalid bundle structure. The "Kite Ground Control.app/libapp.a" binary file is not permitted.
#
# It also carried the library's ~37 MB into the ipa for nothing. libapp.a is linked, never a
# resource, so the group needs `buildPhase: none`. src-tauri/gen/apple is generated and gitignored,
# so this cannot be fixed by committing the project file: it has to be re-applied on every build.
patch_externals_build_phase() {
    local yml="$GEN_APPLE/project.yml"
    if [ ! -f "$yml" ]; then
        echo "[ERROR] $yml not found. Run with FORCE_INIT=1 to regenerate the Xcode project."
        exit 1
    fi
    # Already carries a buildPhase? Leave the file alone.
    local has_phase='$0 ~ /^[[:space:]]*- path: Externals$/ { getline nxt; if (nxt ~ /^[[:space:]]*buildPhase:/) found = 1 } END { exit !found }'
    if awk "$has_phase" "$yml"; then
        echo "      Externals already excluded from Copy Bundle Resources."
        return 0
    fi
    awk '
        { print }
        /^[[:space:]]*- path: Externals$/ {
            match($0, /^[[:space:]]*/)
            indent = substr($0, 1, RLENGTH)
            print indent "  buildPhase: none"
        }
    ' "$yml" > "$yml.tmp"
    # Never build on a silent miss: a template change in Tauri that renames or re-indents the group
    # would otherwise put libapp.a back in the bundle and the rejection would only surface at upload.
    if ! awk "$has_phase" "$yml.tmp"; then
        rm -f "$yml.tmp"
        echo "[ERROR] Could not exclude Externals from Copy Bundle Resources in $yml."
        echo "        Add 'buildPhase: none' under the Externals source group by hand, or the ipa"
        echo "        will contain libapp.a and App Store validation will reject it."
        exit 1
    fi
    mv "$yml.tmp" "$yml"
    echo "      Excluded Externals from Copy Bundle Resources (keeps libapp.a out of the app)."
}

if [ ! -d "$GEN_APPLE" ] || [ "${FORCE_INIT:-0}" = "1" ]; then
    echo "[3/4] Generating the Xcode project (tauri ios init)..."
    npm run tauri ios init
else
    echo "[3/4] $GEN_APPLE already present — skipping init (FORCE_INIT=1 to redo)."
fi
patch_externals_build_phase

echo "[4/4] Building signed release .ipa with Tauri..."
npm run tauri ios build

echo ""
echo "[build-ios] Done. Look for the .ipa under:"
echo "  $GEN_APPLE/build/arm64/  (and the Xcode archive/export path Tauri prints above)"
