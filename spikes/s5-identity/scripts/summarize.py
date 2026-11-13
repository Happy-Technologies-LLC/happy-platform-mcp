#!/usr/bin/env python3
"""Render results/run-*.json as markdown tables (measured seconds and production-equivalent minutes)."""
import json
import sys
from collections import defaultdict

d = json.load(open(sys.argv[1]))
scale, window = d["scale"], d["window_s"]
CH = ["a", "b", "c1", "c2", "c3"]
NAMES = {"a": "refresh at hpm", "b": "introspect existing AT", "c1": "new login (fresh)", "c2": "re-auth, hpm SSO only",
         "c3": "re-auth, upstream SSO only"}


def fmt_min(sec):
    m = sec * scale / 60
    return f"{m:.1f} min" if m < 10 else f"{m:.0f} min"


def cell(vals):
    """vals: list of {'t':..,'why':..} over repeats"""
    if not vals:
        return "-"
    if all(v["why"] == "n/a" for v in vals):
        return "n/a (login allowed)"
    ts = [v["t"] for v in vals if v["t"] is not None]
    if len(ts) < len(vals):
        return f"**not cut in {window:.0f} s** (>{fmt_min(window)})"
    lo, hi = min(ts), max(ts)
    txt = f"{lo:g} s" if lo == hi else f"{lo:g}-{hi:g} s"
    why = vals[0]["why"]
    if "expiry only" in why:
        return f"{txt} (= AT expiry)"
    return f"{txt} ≈ {fmt_min(hi)}"


groups = defaultdict(lambda: defaultdict(list))
meta = {}
for r in d["results"]:
    if "harness_error" in r or "setup_error" in r:
        print(f"!! {r['scenario']}: {r.get('harness_error') or r.get('setup_error')}")
        continue
    key = (r["profile"], r["event"], r["offline"])
    for c in CH:
        if c in r:
            groups[key][c].append(r[c])
    meta[key] = d["profiles"][r["profile"]]

cur = None
for key in sorted(groups, key=lambda k: (k[0], k[2], k[1])):
    prof, event, off = key
    if prof != cur:
        cur = prof
        p = meta[key]
        print(f"\n### {prof}\n\nSettings (test values; x{scale} = production): AT {p['at']} s, SSO idle {p['idle']} s, SSO max {p['max']} s, "
              f"sync {p['sync']}, upstream back-channel logout {p['bcl']}, recon {p['recon']} every {p['recon_interval']} s, "
              f"minValidityToken {p['min_validity']}, offline_access {p['offline']}\n")
        print("| event | " + " | ".join(NAMES[c] for c in CH) + " |")
        print("|---|" + "---|" * len(CH))
    print(f"| {event}{' (offline_access)' if off else ''} | " + " | ".join(cell(groups[key].get(c, [])) for c in CH) + " |")
