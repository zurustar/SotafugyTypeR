#!/usr/bin/env python3
"""プロファイルJSONのアドレスをバイナリのシンボルテーブルで解決する"""
import json
import subprocess
import sys
from collections import defaultdict
import bisect

def load_symbols(binary_path):
    """nm からシンボルテーブルを読み込む"""
    result = subprocess.run(
        ["nm", "-n", binary_path],
        capture_output=True, text=True
    )
    symbols = []  # (addr, name)
    for line in result.stdout.splitlines():
        parts = line.split()
        if len(parts) >= 3 and parts[1] in ('t', 'T'):
            addr = int(parts[0], 16)
            name = parts[2]
            symbols.append((addr, name))
    symbols.sort(key=lambda x: x[0])
    return symbols

def demangle(name):
    """Rust シンボルをデマングル"""
    try:
        result = subprocess.run(
            ["rustfilt"], input=name, capture_output=True, text=True, timeout=5
        )
        if result.returncode == 0:
            return result.stdout.strip()
    except (FileNotFoundError, subprocess.TimeoutExpired):
        pass
    return name

def lookup(symbols, addr):
    """二分探索でアドレスに対応するシンボルを検索"""
    addrs = [s[0] for s in symbols]
    idx = bisect.bisect_right(addrs, addr) - 1
    if idx >= 0:
        return symbols[idx][1]
    return None

def main():
    profile_path = sys.argv[1]
    binary_path = sys.argv[2]

    print(f"Loading symbols from {binary_path}...")
    symbols = load_symbols(binary_path)
    print(f"Loaded {len(symbols)} symbols")

    # バイナリのベースアドレスを取得（__mh_execute_header）
    base_addr = None
    for addr, name in symbols:
        if name == "__mh_execute_header":
            base_addr = addr
            break
    if base_addr is None:
        base_addr = symbols[0][0] if symbols else 0
    print(f"Base address: 0x{base_addr:x}")

    with open(profile_path) as f:
        prof = json.load(f)

    libs = prof.get("libs", [])

    for ti, thread in enumerate(prof["threads"]):
        thread_name = thread.get("name", f"Thread {ti}")
        samples = thread.get("samples", {})
        stack_table = thread.get("stackTable", {})
        frame_table = thread.get("frameTable", {})
        func_table = thread.get("funcTable", {})
        string_array = thread.get("stringArray", [])

        stacks = samples.get("stack", [])
        if not stacks or len(stacks) < 100:
            continue

        func_counts = defaultdict(int)
        total = len(stacks)

        for stack_idx in stacks:
            if stack_idx is None:
                continue
            frame_idx = stack_table["frame"][stack_idx]
            rva = frame_table["address"][frame_idx]
            # RVA + base = absolute address
            abs_addr = base_addr + rva
            sym_name = lookup(symbols, abs_addr)
            if sym_name:
                func_counts[sym_name] += 1
            else:
                func_counts[f"0x{rva:x}"] += 1

        if not func_counts:
            continue

        print(f"\n=== {thread_name} ({total} samples) ===")
        sorted_funcs = sorted(func_counts.items(), key=lambda x: -x[1])

        # デマングル上位30
        for fn, count in sorted_funcs[:30]:
            pct = count / total * 100
            demangled = demangle(fn) if fn.startswith("_") else fn
            # 短縮表示
            if len(demangled) > 100:
                demangled = demangled[:97] + "..."
            print(f"  {pct:5.1f}%  ({count:6d})  {demangled}")

if __name__ == "__main__":
    main()
