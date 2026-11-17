#!/usr/bin/env python3
# rows-namediff.py A.json B.json : two RAEA_ROWS_DUMP frames compared with entities matched by Name (tools/batch-boundary.sh)
import json, sys, collections
def load(p):
    d = json.load(open(p)); out = collections.defaultdict(list)
    for k, v in d.items(): out[v.get("bevy_ecs::name::Name") or k].append(v)
    return out
def leaves(v, p=""):
    if isinstance(v, dict):
        for k, x in v.items(): yield from leaves(x, f"{p}.{k}" if p else k)
    elif isinstance(v, list):
        for i, x in enumerate(v): yield from leaves(x, f"{p}[{i}]")
    else: yield p, v
A, B = load(sys.argv[1]), load(sys.argv[2])
print("only A:", sorted(set(A) - set(B))[:20]); print("only B:", sorted(set(B) - set(A))[:20])
agg = collections.defaultdict(list)
for n in set(A) & set(B):
    if len(A[n]) != len(B[n]): agg[("count", "")].append((n, len(A[n]), len(B[n]))); continue
    for a, b in zip(A[n], B[n]):
        for comp in set(a) | set(b):
            la, lb = dict(leaves(a.get(comp))), dict(leaves(b.get(comp)))
            for k in set(la) | set(lb):
                x, y = la.get(k), lb.get(k)
                if isinstance(x, float) and isinstance(y, float):
                    if abs(x - y) > 1e-4 * max(1, abs(x), abs(y)): agg[(comp.split("::")[-1], k)].append((n, x, y))
                elif x != y: agg[(comp.split("::")[-1], k)].append((n, x, y))
for (c, k), v in sorted(agg.items(), key=lambda kv: -len(kv[1]))[:400]: print(f"{len(v):5} x {c} :: {k}  e.g. {v[0]}")
