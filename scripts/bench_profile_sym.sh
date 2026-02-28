#!/usr/bin/env bash
set -euo pipefail

# シンボル解決付きプロファイル取得スクリプト
# ロードテスター側をプロファイル、プロキシは通常起動

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

PROXY_BIN="$PROJECT_DIR/target/release/sip-proxy"
LOADTEST_BIN="$PROJECT_DIR/target/release/sip-load-test"

PROXY_CFG="/tmp/bench_proxy.json"
BENCH_CFG="/tmp/bench_config_profile.json"
RESULT_FILE="/tmp/bench_result_sym.json"
PROXY_LOG="/tmp/bench_proxy_sym.log"
USERS_FILE="/tmp/bench_users.json"
PROFILE_FILE="/tmp/profile_sym.json"

PROXY_PID=""

cleanup() {
    if [ -n "$PROXY_PID" ] && kill -0 "$PROXY_PID" 2>/dev/null; then
        kill "$PROXY_PID" 2>/dev/null
        wait "$PROXY_PID" 2>/dev/null || true
    fi
    rm -f "$PROXY_CFG" "$BENCH_CFG" "$USERS_FILE"
    echo "Cleanup done."
}
trap cleanup EXIT

echo "=== Building (release) ==="
cd "$PROJECT_DIR"
cargo build --release 2>&1
echo ""

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

# sustained モードで高負荷（max_stable_cps の 80% = 1350 CPS）を 20 秒間
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
  "duration": 20,
  "scenario": "invite_bye",
  "call_duration": 20,
  "shutdown_timeout": 3,
  "auth_enabled": true,
  "users_file": "/tmp/bench_users.json",
  "session_expires": 10,
  "mode": "sustained"
}
EOF

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

echo "=== Running Benchmark with samply (symbolicated) ==="
samply record --save-only -o "$PROFILE_FILE" -- \
    "$LOADTEST_BIN" run "$BENCH_CFG" --output "$RESULT_FILE" 2>&1
echo ""

echo "Profile saved: $PROFILE_FILE"

echo "=== Results ==="
if [ -f "$RESULT_FILE" ]; then
    if command -v jq &>/dev/null; then
        jq '{
            total_calls: .total_calls,
            successful_calls: .successful_calls,
            failed_calls: .failed_calls,
            latency_p50_ms: .latency_p50_ms,
            latency_p99_ms: .latency_p99_ms
        }' "$RESULT_FILE"
    else
        cat "$RESULT_FILE"
    fi
fi
