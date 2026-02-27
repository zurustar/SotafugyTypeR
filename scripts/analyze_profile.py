#!/usr/bin/env python3
"""samply プロファイル + syms.json からセルフタイム上位関数を集計する

使い方: python3 analyze_profile.py <profile.json> <profile.syms.json>

frameTable.address はライブラリ相対アドレス (RVA) なので、
func -> resource -> lib の経路でライブラリを特定し、
syms.json の symbol_table (RVA) と直接照合する。
"""
import json
import sys
from collections import defaultdict

profile_path = sys.argv[1]
syms_path = sys.argv[2]

with open(profile_path) as f:
    prof = json.load(f)
with open(syms_path) as f:
    syms = json.load(f)

# ── syms.json からライブラリ別シンボルテーブルを構築 ──
sym_strings = syms["string_table"]
lib_symbols = {}  # debug_name -> sorted list of (rva, end_rva, symbol_name)

for lib_entry in syms["data"]:
    lib_name = lib_entry["debug_name"]
    entries = []
    for sym_entry in lib_entry.get("symbol_table", []):
        rva = sym_entry["rva"]
        size = sym_entry.get("size", 0)
        sym_idx = sym_entry["symbol"]
        sym_name = sym_strings[sym_idx] if sym_idx < len(sym_strings) else f"<unknown:{sym_idx}>"
        # インライン展開がある場合はリーフ (最初) のフレームの関数名を使う
        if "frames" in sym_entry and sym_entry["frames"]:
            leaf = sym_entry["frames"][0]
            fn_idx = leaf["function"]
            sym_name = sym_strings[fn_idx] if fn_idx < len(sym_strings) else sym_name
        entries.append((rva, rva + size, sym_name))
    entries.sort(key=lambda x: x[0])
    lib_symbols[lib_name] = entries


def lookup_symbol(lib_name, rva):
    """二分探索で RVA を含むシンボルを検索"""
    entries = lib_symbols.get(lib_name, [])
    if not entries:
        return None
    lo, hi = 0, len(entries) - 1
    result = None
    while lo <= hi:
        mid = (lo + hi) // 2
        if entries[mid][0] <= rva:
            result = mid
            lo = mid + 1
        else:
            hi = mid - 1
    if result is not None:
        start, end, name = entries[result]
        if rva < end:
            return name
    return None


# ── プロファイルのライブラリ情報 ──
libs = prof.get("libs", [])

# ── 各スレッドを処理 ──
for ti, thread in enumerate(prof["threads"]):
    thread_name = thread.get("name", f"Thread {ti}")
    samples = thread.get("samples", {})
    stack_table = thread.get("stackTable", {})
    frame_table = thread.get("frameTable", {})
    func_table = thread.get("funcTable", {})
    resource_table = thread.get("resourceTable", {})
    string_array = thread.get("stringArray", [])

    stacks = samples.get("stack", [])
    if not stacks:
        continue

    # resource -> lib の対応
    rt_libs = resource_table.get("lib", [])

    func_counts = defaultdict(int)
    total = len(stacks)

    for stack_idx in stacks:
        if stack_idx is None:
            continue
        # リーフフレーム
        frame_idx = stack_table["frame"][stack_idx]
        rva = frame_table["address"][frame_idx]
        func_idx = frame_table["func"][frame_idx]
        res_idx = func_table["resource"][func_idx]

        sym_name = None
        if res_idx is not None and 0 <= res_idx < len(rt_libs):
            lib_idx = rt_libs[res_idx]
            if lib_idx is not None and 0 <= lib_idx < len(libs):
                lib_debug_name = libs[lib_idx].get("debugName", "")
                sym_name = lookup_symbol(lib_debug_name, rva)

        if sym_name is None:
            # フォールバック: funcTable の名前 (16進アドレス文字列)
            name_idx = func_table["name"][func_idx]
            sym_name = string_array[name_idx] if name_idx < len(string_array) else f"0x{rva:x}"

        func_counts[sym_name] += 1

    if not func_counts:
        continue

    print(f"=== {thread_name} ({total} samples) ===")
    sorted_funcs = sorted(func_counts.items(), key=lambda x: -x[1])
    for fn, count in sorted_funcs[:25]:
        pct = count / total * 100
        print(f"  {pct:5.1f}%  ({count:6d})  {fn}")
    print()
