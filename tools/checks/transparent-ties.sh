#!/bin/bash
# Transparent draw order per frame (tools/checks/transparent-ties.tsx): renders its 60 frames as one batch and fails if
# the exact tie's top colour ever changes, or the near pair's changes more than once (a strobe, not a crossing).
# usage: tools/checks/transparent-ties.sh RAEA [OUTDIR] [native]
set -u
BIN=$1 O=${2:-out/transparent-ties} M=${3:+--native}
HERE=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$O/one"
"$BIN" render "$HERE/transparent-ties.tsx" --composition transparent-ties --at "$(seq -s, 0 59)" -o "$O/one/" $M > "$O/one.log" 2>&1 || { echo "render failed: $O/one.log"; exit 2; }
python3 - "$O/one" <<'PY'
import sys, os
from PIL import Image
d = sys.argv[1]; seq = {"tie": "", "near": ""}
for f in sorted(x for x in os.listdir(d) if x.endswith(".png")):
    im = Image.open(os.path.join(d, f)).convert("RGB"); w, h = im.size
    for name, x in (("tie", w * 0.25), ("near", w * 0.75)):
        r, g, b = im.getpixel((int(x), h // 2))   # the overlap's centre: the colour on top is the stronger channel
        # both quads there (each over half clear): the stronger channel is the one on top; one alone isn't an order
        seq[name] += ("R" if r > b else "B") if min(r, b) > 30 else "-"
# the tie's red quad is away for frames 20-25 (blue alone there): only its frames before and after are compared
seq["tie"] = seq["tie"][:20] + seq["tie"][26:]
# only frames where the sample has both quads over it are judged (the swing carries each pair off its sample at the ends)
judged = {k: v.replace("-", "") for k, v in seq.items()}
changes = {k: sum(1 for a, b in zip(v, v[1:]) if a != b) for k, v in judged.items()}
for k, v in seq.items(): print(f"{k}: {v} ({changes[k]} changes)")
bad = changes["tie"] > 0 or changes["near"] > 1
print("FLICKER" if bad else "STABLE")
sys.exit(1 if bad else 0)
PY
