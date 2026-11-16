#!/usr/bin/env python3
"""Two RAEA_ROWS_DUMP folders compared property by property (docs/native-shots.md, parity).

usage: tools/native-diff.py A_DIR B_DIR [--frame F] [--tol 1e-5] [--top 40]

Each folder holds FRAME.json files (raea render with RAEA_ROWS_DUMP=DIR): every 3D row's entity and the entities
under it, each with its reflected components and its material. For each frame both have, it prints the entities only
one side has, then every leaf value that differs beyond --tol (relative to the larger magnitude, floor 1), grouped by
component and field across entities, worst first, so a whole class of difference reads as one line.
"""
import json
import math
import os
import sys
from collections import defaultdict


def leaves(v, path=""):
    if isinstance(v, dict):
        for k, x in v.items():
            yield from leaves(x, f"{path}.{k}" if path else k)
    elif isinstance(v, list):
        for i, x in enumerate(v):
            yield from leaves(x, f"{path}[{i}]")
    else:
        yield path, v


def differs(a, b, tol):
    if isinstance(a, (int, float)) and isinstance(b, (int, float)) and not isinstance(a, bool) and not isinstance(b, bool):
        if math.isnan(a) and math.isnan(b):
            return 0.0
        d = abs(a - b) / max(1.0, abs(a), abs(b))
        return d if d > tol else 0.0
    return 0.0 if a == b else math.inf


def main():
    args = sys.argv[1:]
    opt = {"--frame": None, "--tol": "1e-5", "--top": "40"}
    pos = []
    i = 0
    while i < len(args):
        if args[i] in opt:
            opt[args[i]] = args[i + 1]
            i += 2
        else:
            pos.append(args[i])
            i += 1
    a_dir, b_dir = pos
    tol, top = float(opt["--tol"]), int(opt["--top"])
    frames = sorted({f[:-5] for f in os.listdir(a_dir) if f.endswith(".json")} & {f[:-5] for f in os.listdir(b_dir) if f.endswith(".json")}, key=int)
    if opt["--frame"] is not None:
        frames = [opt["--frame"]]
    for f in frames:
        a = json.load(open(os.path.join(a_dir, f + ".json")))
        b = json.load(open(os.path.join(b_dir, f + ".json")))
        only_a, only_b = sorted(set(a) - set(b)), sorted(set(b) - set(a))
        print(f"== frame {f}: {len(a)} entities in A, {len(b)} in B; only A {len(only_a)}, only B {len(only_b)}")
        for n in only_a[:10]:
            print(f"   only A: {n}")
        for n in only_b[:10]:
            print(f"   only B: {n}")
        groups = defaultdict(lambda: [0, 0.0, None])
        # maps keyed by Entity (a light's cascades per view) differ by entity id alone between two runs: left out
        skip = ("Cascades", "CascadesFrusta", "EguiContextSettings")
        for name in set(a) & set(b):
            ca, cb = a[name], b[name]
            for comp in set(ca) | set(cb):
                if comp.split("::")[-1] in skip:
                    continue
                if comp not in ca or comp not in cb:
                    g = groups[(comp, "<component on one side only>")]
                    g[0] += 1
                    g[1] = math.inf
                    g[2] = g[2] or (name, "A" if comp in ca else "B")
                    continue
                xa, xb = ca[comp], cb[comp]
                # a rotation and its negation are the same turn: B's taken with the sign nearer A's
                if isinstance(xa, dict) and isinstance(xb, dict) and isinstance(xa.get("rotation"), list) and isinstance(xb.get("rotation"), list):
                    qa, qb = xa["rotation"], xb["rotation"]
                    if sum(p * q for p, q in zip(qa, qb)) < 0:
                        xb = dict(xb, rotation=[-q for q in qb])
                la, lb = dict(leaves(xa)), dict(leaves(xb))
                for leaf in set(la) | set(lb):
                    d = differs(la.get(leaf), lb.get(leaf), tol)
                    if d:
                        # array indices out of the group key, so one field across a list reads as one line
                        key = (comp, "".join(c for c in leaf if not c.isdigit()))
                        g = groups[key]
                        g[0] += 1
                        if d > g[1] or g[2] is None:
                            g[1] = d
                            g[2] = (name, leaf, la.get(leaf), lb.get(leaf))
        for (comp, leaf), (n, worst, ex) in sorted(groups.items(), key=lambda kv: (-kv[1][1], -kv[1][0]))[:top]:
            print(f"   {n:6d} x {comp} :: {leaf}  worst {worst:.3g}  e.g. {ex}")


if __name__ == "__main__":
    main()
