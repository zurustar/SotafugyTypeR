#!/usr/bin/env bash
set -euo pipefail

# === SIP Load Tester — Performance Benchmark ===
# binary-search モードで最大安定 CPS を探索する

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

PROXY_BIN="$PROJECT_DIR/target/release/sip-proxy"
LOADTEST_BIN="$PROJECT_DIR/target/release/sip-load-test"

PROXY_CFG="/tmp/bench_proxy.json"
BENCH_CFG="/tmp/bench_config.json"
RESULT_FILE="/tmp/bench_result.json"
PROXY_LOG="/tmp/bench_proxy.log"

PROXY_PID=""

cleanup() {
    if [ -n "$PROXY_PID" ] && kill -0 "$PROXY_PID" 2>/dev/null; then
        kill "$PROXY_PID" 2>/dev/null
        wait "$PROXY_PID" 2>/dev/null || true
    fi
    rm -f "$PROXY_CFG" "$BENCH_CFG"
    echo ""
    echo "Cleanup done."
}
trap cleanup EXIT

# --- 1. Release build ---
echo "=== Building (release) ==="
cd "$PROJECT_DIR"
cargo build --release 2>&1
echo ""

# --- 2. Config files ---
cat > "$PROXY_CFG" << 'EOF'
{
  "host": "127.0.0.1",
  "port": 5060,
  "forward_host": "127.0.0.1",
  "forward_port": 5080,
  "auth_enabled": false,
  "debug": false
}
EOF

cat > "$BENCH_CFG" << 'EOF'
{
  "proxy_host": "127.0.0.1",
  "proxy_port": 5060,
  "uac_host": "127.0.0.1",
  "uac_port": 5070,
  "uac_port_count": 4,
  "uas_host": "127.0.0.1",
  "uas_port": 5080,
  "uas_port_count": 4,
  "target_cps": 500.0,
  "max_dialogs": 50000,
  "duration": 30,
  "scenario": "invite_bye",
  "call_duration": 1,
  "mode": "binary_search",
  "binary_search": {
    "initial_cps": 100.0,
    "step_size": 50.0,
    "step_duration": 10,
    "error_threshold": 0.01,
    "convergence_threshold": 5.0,
    "cooldown_duration": 2
  }
}
EOF

# --- 3. Start proxy ---
echo "=== Starting SIP Proxy ==="
"$PROXY_BIN" "$PROXY_CFG" > "$PROXY_LOG" 2>&1 &
PROXY_PID=$!
sleep 1

if ! kill -0 "$PROXY_PID" 2>/dev/null; then
    echo "ERROR: Proxy failed to start. Log:"
    cat "$PROXY_LOG"
    exit 1
fi
echo "Proxy started (PID: $PROXY_PID)"
echo ""

# --- 4. Run benchmark ---
echo "=== Running Benchmark (binary-search) ==="
"$LOADTEST_BIN" run "$BENCH_CFG" --mode binary-search --output "$RESULT_FILE" 2>&1
echo ""

# --- 5. Show results ---
echo "=== Results ==="
if [ -f "$RESULT_FILE" ]; then
    if command -v jq &>/dev/null; then
        jq '{
            max_stable_cps: .max_stable_cps,
            total_calls: .total_calls,
            successful_calls: .successful_calls,
            failed_calls: .failed_calls,
            auth_failures: .auth_failures,
            latency_p50_ms: .latency_p50_ms,
            latency_p90_ms: .latency_p90_ms,
            latency_p95_ms: .latency_p95_ms,
            latency_p99_ms: .latency_p99_ms,
            status_codes: .status_codes
        }' "$RESULT_FILE"
    else
        cat "$RESULT_FILE"
    fi
    echo ""
    echo "Full result: $RESULT_FILE"
    echo "Proxy log:   $PROXY_LOG"
else
    echo "ERROR: Result file not found at $RESULT_FILE"
    exit 1
fi