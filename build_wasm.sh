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

# Copy the local macroquad JS bundle (version-matched to the compiled WASM)
REGISTRY="$HOME/.cargo/registry/src"
REGISTRY_ROOT=$(ls -d "$REGISTRY"/*/  | head -1)
MQ_DIR=$(ls -d "${REGISTRY_ROOT}"macroquad-*/ 2>/dev/null | sort -r | head -1)

if [ -n "$MQ_DIR" ] && [ -f "${MQ_DIR}js/mq_js_bundle.js" ]; then
    echo "Using macroquad bundle: ${MQ_DIR}js/mq_js_bundle.js"
    cp "${MQ_DIR}js/mq_js_bundle.js" web/mq_js_bundle.js
else
    echo "ERROR: Could not find local macroquad JS bundle" >&2
    exit 1
fi

# Patch 1: expose consume_js_object and js_object as globals
# (sapp_jsutils defines them as locals; quad_net calls them as globals)
sed -i 's/function a(t){var n=e\[t\];return delete e\[t\],n}function r(t){return e\[t\]}}/function a(t){var n=e[t];return delete e[t],n}function r(t){return e[t]}window.consume_js_object=a;window.js_object=t}/' web/mq_js_bundle.js

# Patch 2: ws_try_recv — unwrap the {text, data} wrapper and return the raw
# Uint8Array so Rust's to_byte_buffer() reads actual message bytes
sed -i 's/function c(){return n\.length!=0?js_object(n\.shift()):-1}/function c(){if(n.length!=0){var m=n.shift();return js_object(m.data!=null?m.data:m)}return -1}/' web/mq_js_bundle.js

echo "Build complete. web/ is ready to serve."
