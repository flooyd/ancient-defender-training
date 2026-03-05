# Build the WASM client and serve it locally
# Usage: .\build_wasm.ps1 [-release]

param([switch]$release)

$ErrorActionPreference = "Stop"

# 1. Ensure the wasm32 target is installed
Write-Host "Ensuring wasm32 target is installed..." -ForegroundColor Cyan
rustup target add wasm32-unknown-unknown

# 2. Build
if ($release) {
    Write-Host "Building (release)..." -ForegroundColor Cyan
    cargo build --target wasm32-unknown-unknown --bin client --release
    $wasm_src = "target\wasm32-unknown-unknown\release\client.wasm"
} else {
    Write-Host "Building (debug)..." -ForegroundColor Cyan
    cargo build --target wasm32-unknown-unknown --bin client
    $wasm_src = "target\wasm32-unknown-unknown\debug\client.wasm"
}

# 3. Copy wasm into the web/ folder
Write-Host "Copying $wasm_src -> web\client.wasm" -ForegroundColor Cyan
Copy-Item -Force $wasm_src "web\client.wasm"

# 4. Copy matching JS bundles from local cargo registry (versions guaranteed to match the compiled WASM)
$registry = "$env:USERPROFILE\.cargo\registry\src"
$registryRoot = Get-ChildItem $registry -Directory | Select-Object -First 1

$miniquadDir = Get-ChildItem $registryRoot.FullName -Filter "miniquad-*" -Directory | Sort-Object Name -Descending | Select-Object -First 1
$macroquadDir = Get-ChildItem $registryRoot.FullName -Filter "macroquad-*" -Directory | Sort-Object Name -Descending | Select-Object -First 1
$quadNetDir = Get-ChildItem $registryRoot.FullName -Filter "quad-net-*" -Directory | Sort-Object Name -Descending | Select-Object -First 1

# Prefer macroquad's bundled mq_js_bundle.js (combines everything), fall back to miniquad's gl.js
$bundleSrc = $null
if ($macroquadDir -and (Test-Path "$($macroquadDir.FullName)\js\mq_js_bundle.js")) {
    $bundleSrc = "$($macroquadDir.FullName)\js\mq_js_bundle.js"
    Write-Host "Using macroquad bundle: $bundleSrc" -ForegroundColor Cyan
} elseif ($miniquadDir -and (Test-Path "$($miniquadDir.FullName)\js\gl.js")) {
    $bundleSrc = "$($miniquadDir.FullName)\js\gl.js"
    Write-Host "Using miniquad gl.js: $bundleSrc" -ForegroundColor Cyan
}

if ($bundleSrc) {
    $bundleContent = Get-Content $bundleSrc -Raw
    # sapp_jsutils defines consume_js_object as local 'a' and js_object as local 't'
    # inside an IIFE. quad_net (also in the bundle) calls them as globals.
    # Patch 1: expose them as globals right before the sapp_jsutils IIFE closes.
    $patched = $bundleContent -replace `
        'function a\(t\)\{var n=e\[t\];return delete e\[t\],n\}function r\(t\)\{return e\[t\]\}\}\(\)', `
        'function a(t){var n=e[t];return delete e[t],n}function r(t){return e[t]}window.consume_js_object=a;window.js_object=t}()'
    if ($patched -eq $bundleContent) {
        Write-Host "WARNING: Could not patch consume_js_object - bundle format may have changed." -ForegroundColor Yellow
    } else {
        Write-Host "Patched consume_js_object/js_object globals into bundle." -ForegroundColor Green
    }
    # Patch 2: quad_net ws_try_recv returns wrapper object {text,data} but Rust's
    # to_byte_buffer() is called on the wrapper instead of the inner data field,
    # producing an empty buffer. Fix: unwrap to just the data payload directly.
    $before = $patched
    $patched = $patched -replace `
        'function c\(\)\{return n\.length!=0\?js_object\(n\.shift\(\)\):-1\}', `
        'function c(){if(n.length!=0){var m=n.shift();return js_object(m.data!=null?m.data:m)}return -1}'
    if ($patched -eq $before) {
        Write-Host "WARNING: Could not patch ws_try_recv - bundle format may have changed." -ForegroundColor Yellow
    } else {
        Write-Host "Patched ws_try_recv to unwrap data payload." -ForegroundColor Green
    }
    $patched | Set-Content "web\mq_js_bundle.js" -NoNewline
} else {
    Write-Host "WARNING: Could not find local JS bundle. Using CDN fallback." -ForegroundColor Yellow
}

# Copy quad-net.js plugin from local crate if available
if ($quadNetDir -and (Test-Path "$($quadNetDir.FullName)\js\quad-net.js")) {
    Write-Host "Using quad-net.js: $($quadNetDir.FullName)\js\quad-net.js" -ForegroundColor Cyan
    Copy-Item -Force "$($quadNetDir.FullName)\js\quad-net.js" "web\quad-net.js"
} else {
    Write-Host "WARNING: quad-net.js not found in local crate." -ForegroundColor Yellow
}

# 5. Serve
Write-Host ""
Write-Host "Build complete. Starting web server at http://127.0.0.1:8000 ..." -ForegroundColor Green
Write-Host "Open http://127.0.0.1:8000 in your browser." -ForegroundColor Yellow
Write-Host "Press Ctrl+C to stop." -ForegroundColor Yellow
Write-Host ""

# Try basic-http-server first, fall back to Python
if (Get-Command basic-http-server -ErrorAction SilentlyContinue) {
    Set-Location web
    basic-http-server -a 127.0.0.1:8000
} elseif (Get-Command python -ErrorAction SilentlyContinue) {
    Set-Location web
    python -m http.server 8000 --bind 127.0.0.1
} elseif (Get-Command python3 -ErrorAction SilentlyContinue) {
    Set-Location web
    python3 -m http.server 8000 --bind 127.0.0.1
} else {
    Write-Host "No web server found. Install one:" -ForegroundColor Red
    Write-Host "  cargo install basic-http-server" -ForegroundColor White
    Write-Host "  -- or --" -ForegroundColor White
    Write-Host "  python -m http.server 8000  (run from the web/ folder)" -ForegroundColor White
    exit 1
}
