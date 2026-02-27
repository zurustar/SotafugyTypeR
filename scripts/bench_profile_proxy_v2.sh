#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

PROXY_BIN="$PROJECT_DIR/target/release/sip-proxy"
LOADTEST_BIN="$PROJECT_DIR/target/release/sip-load-test"

PROXY_CFG="/tmp/bench_proxy2.json"
BENCH_CFG="/tmp/bench_config2.json"
RESULT_FILE="/tmp/bench_result_proxy2.json"
PROXY_LOG="/tmp/bench_proxy2.log"
USERS_FILE="/tmp/bench_users2.json"
PROFILE_FILE="/tmp/proxy_profile2.json"

LOADTEST_PID=""
SAMPLY_PID=""

cleanup() {
    if [ -n "$LOADTEST_PID" ] && kill -0 "$LOADTEST_PID" 2>/dev/null; then
        kill "$LOADTEST_PID" 2>/dev/null
        wait "$LOADTEST_PID" 2>/dev/null || true
    fi
    if [ -n "$SAMPLY_PID" ] && kill -0 "$SAMPLY_PID" 2>/dev/null; then
        kill -INT "$SAMPLY_PID" 2>/dev/null
        wait "$SAMPLY_PID" 2>/dev/null || true
    fi
    rm -f "$PROXY_CFG" "$BENCH_CFG" "$USERS_FILE"
    echo "Cleanup done."
}
trap cleanup EXIT

cd "$PROJECT_DIR"
cargo build --release 2>&1

cat > "$USERS_FILE" << 'EOF'
{"users": [{"username": "bench_user", "domain": "localhost", "password": "bench_pass"}]}
EOF

cat > "$PROXY_CFG" << 'EOF'
{"host":"127.0.0.1","port":5060,"forward_host":"127.0.0.1","forward_port":5080,"auth_enabled":true,"users_file":"/tmp/bench_users2.json","debug":false}
EOF

cat > "$BENCH_CFG" << 'EOF'
{"proxy_host":"127.0.0.1","proxy_port":5060,"uac_host":"127.0.0.1","uac_port":5070,"uac_port_count":4,"uas_host":"127.0.0.1","uas_port":5080,"uas_port_count":4,"target_cps":1350.0,"duration":20,"scenario":"invite_bye","call_duration":20,"shutdown_timeout":3,"auth_enabled":true,"users_file":"/tmp/bench_users2.json","session_expires":10,"mode":"sustained"}
EOF

echo "=== Starting proxy under samply ==="
samply record --no-open --unstable-presymbolicate -o "$PROFILE_FILE" -- "$PROXY_BIN" "$PROXY_CFG" > "$PROXY_LOG" 2>&1 &
SAMPLY_PID=$!
sleep 3

if ! kill -0 "$SAMPLY_PID" 2>/dev/null; then
    echo "ERROR: samply/proxy failed to start"
    cat "$PROXY_LOG"
    exit 1
fi
echo "Proxy+samply started (PID: $SAMPLY_PID)"

echo "=== Running load test (1350 CPS, 20s) ==="
"$LOADTEST_BIN" run "$BENCH_CFG" --output "$RESULT_FILE" 2>&1
echo ""

echo "=== Stopping proxy ==="
kill -INT "$SAMPLY_PID" 2>/dev/null || true
wait "$SAMPLY_PID" 2>/dev/null || true
SAMPLY_PID=""

echo "=== Results ==="
if [ -f "$RESULT_FILE" ]; then
    jq '{total_calls,successful_calls,failed_calls,latency_p50_ms,latency_p99_ms}' "$RESULT_FILE" 2>/dev/null || cat "$RESULT_FILE"
fi
echo ""
echo "Profile: $PROFILE_FILE"
ls -la "${PROFILE_FILE}"* 2>/dev/null
