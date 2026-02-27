#!/usr/bin/env bash
set -euo pipefail

# === SIP Proxy — Performance Profiling (proxy side) ===
# samply でプロキシ側をプロファイリングする

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

PROXY_BIN="$PROJECT_DIR/target/release/sip-proxy"
LOADTEST_BIN="$PROJECT_DIR/target/release/sip-load-test"

PROXY_CFG="/tmp/bench_proxy.json"
BENCH_CFG="/tmp/bench_config.json"
RESULT_FILE="/tmp/bench_result_proxy_profile.json"
PROXY_LOG="/tmp/bench_proxy_profile.log"
USERS_FILE="/tmp/bench_users.json"
PROFILE_FILE="/tmp/proxy_profile.json"

LOADTEST_PID=""
SAMPLY_PID=""

cleanup() {
    if [ -n "$LOADTEST_PID" ] && kill -0 "$LOADTEST_PID" 2>/dev/null; then
        kill "$LOADTEST_PID" 2>/dev/null
        wait "$LOADTEST_PID" 2>/dev/null || true
    fi
    # samply + proxy は samply 終了時に自動停止
    if [ -n "$SAMPLY_PID" ] && kill -0 "$SAMPLY_PID" 2>/dev/null; then
        kill "$SAMPLY_PID" 2>/dev/null
        wait "$SAMPLY_PID" 2>/dev/null || true
    fi
    rm -f "$PROXY_CFG" "$BENCH_CFG" "$USERS_FILE"
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
cat > "$USERS_FILE" << 'EOF'
{
  "users": [
    {"username": "bench_user", "domain": "localhost", "password": "bench_pass"}
  ]
}
EOF

cat > "$PROXY_CFG" << 'EOF'
{
  "host": "127.0.0.1",
  "port": 5060,
  "forward_host": "127.0.0.1",
  "forward_port": 5080,
  "auth_enabled": true,
  "users_file": "/tmp/bench_users.json",
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
  "target_cps": 1350.0,
  "duration": 30,
  "scenario": "invite_bye",
  "call_duration": 30,
  "shutdown_timeout": 3,
  "auth_enabled": true,
  "users_file": "/tmp/bench_users.json",
  "session_expires": 10,
  "mode": "sustained"
}
EOF

# --- 3. Start proxy under samply profiler ---
echo "=== Starting SIP Proxy under samply profiler ==="
samply record --no-open --unstable-presymbolicate -o "$PROFILE_FILE" -- "$PROXY_BIN" "$PROXY_CFG" > "$PROXY_LOG" 2>&1 &
SAMPLY_PID=$!
sleep 2

echo "Proxy + samply started (PID: $SAMPLY_PID)"
echo ""

# --- 4. Run fixed-rate benchmark at max stable CPS ---
echo "=== Running fixed-rate benchmark at 1350 CPS for 30s ==="
"$LOADTEST_BIN" run "$BENCH_CFG" --output "$RESULT_FILE" 2>&1
echo ""

# --- 5. Stop proxy (send SIGINT to samply, which forwards to proxy) ---
echo "=== Stopping proxy ==="
kill -INT "$SAMPLY_PID" 2>/dev/null || true
wait "$SAMPLY_PID" 2>/dev/null || true
SAMPLY_PID=""
echo ""

echo "Proxy profile saved: $PROFILE_FILE"
echo "(Open with: samply load $PROFILE_FILE)"
echo ""

# --- 6. Show results ---
echo "=== Results ==="
if [ -f "$RESULT_FILE" ]; then
    if command -v jq &>/dev/null; then
        jq '{
            total_calls: .total_calls,
            successful_calls: .successful_calls,
            failed_calls: .failed_calls,
            error_rate: (if .total_calls > 0 then (.failed_calls / .total_calls) else 0 end),
            latency_p50_ms: .latency_p50_ms,
            latency_p90_ms: .latency_p90_ms,
            latency_p95_ms: .latency_p95_ms,
            latency_p99_ms: .latency_p99_ms
        }' "$RESULT_FILE"
    else
        cat "$RESULT_FILE"
    fi
fi
