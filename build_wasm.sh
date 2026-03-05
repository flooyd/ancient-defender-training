#!/usr/bin/env bash
set -e

# Install wasm32 target if not already present
rustup target add wasm32-unknown-unknown

# WS_URL is baked into the WASM at compile time via option_env!("WS_URL").
# On Render this is set as a build env var (see render.yaml).
# Locally it defaults to ws://127.0.0.1:8080 when not set.
echo "Building WASM client with WS_URL=${WS_URL:-ws://127.0.0.1:8080}"

# Build WASM client (release)
cargo build --release --bin client --target wasm32-unknown-unknown

# Copy WASM into web/
cp target/wasm32-unknown-unknown/release/client.wasm web/client.wasm

# ── Locate mq_js_bundle.js ────────────────────────────────────────────────────
# Try common cargo registry locations (local dev, Render, CI, etc.)
MQ_VERSION=$(grep -A1 'name = "macroquad"' Cargo.lock | grep version | head -1 | sed 's/.*"\(.*\)".*/\1/')
echo "macroquad version from Cargo.lock: $MQ_VERSION"

BUNDLE_SRC=""

# Search all known registry root candidates
for REGISTRY_ROOT in \
    "$HOME/.cargo/registry/src/"*/ \
    "/opt/render/.cargo/registry/src/"*/ \
    "/root/.cargo/registry/src/"*/; do
    CANDIDATE="${REGISTRY_ROOT}macroquad-${MQ_VERSION}/js/mq_js_bundle.js"
    if [ -f "$CANDIDATE" ]; then
        BUNDLE_SRC="$CANDIDATE"
        echo "Found local bundle: $BUNDLE_SRC"
        break
    fi
done

if [ -n "$BUNDLE_SRC" ]; then
    cp "$BUNDLE_SRC" web/mq_js_bundle.js
else
    # Fall back to downloading the exact version from GitHub
    DOWNLOAD_URL="https://raw.githubusercontent.com/not-fl3/macroquad/v${MQ_VERSION}/js/mq_js_bundle.js"
    echo "Local bundle not found. Downloading from: $DOWNLOAD_URL"
    curl -fL "$DOWNLOAD_URL" -o web/mq_js_bundle.js
fi

# ── Patch 1: expose consume_js_object / js_object as globals ─────────────────
# sapp_jsutils defines them as locals; quad_net calls them as globals.
sed -i 's/function a(t){var n=e\[t\];return delete e\[t\],n}function r(t){return e\[t\]}}/function a(t){var n=e[t];return delete e[t],n}function r(t){return e[t]}window.consume_js_object=a;window.js_object=t}/' web/mq_js_bundle.js

# ── Patch 2: ws_try_recv — unwrap {text,data} and return the raw Uint8Array ──
# so Rust's to_byte_buffer() reads actual message bytes instead of empty.
sed -i 's/function c(){return n\.length!=0?js_object(n\.shift()):-1}/function c(){if(n.length!=0){var m=n.shift();return js_object(m.data!=null?m.data:m)}return -1}/' web/mq_js_bundle.js

echo "Build complete. web/ is ready to serve."
