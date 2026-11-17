#!/bin/bash
# The batch-boundary check (docs/native-shots.md): a frame must come out the same whatever batch it is drawn in. Renders
# [B-PRE, B+4) as one batch (PRE 150: a reel's batch) and [B, B+4) as a fresh one, then compares frames B..B+3 by pixels
# (psnr) and by every reflected property of every 3D row (RAEA_ROWS_DUMP; tools/rows-namediff.py matches entities by
# name, as row ids may be numbered differently). Anything that differs is a column applied only when its row spawns:
# camera, lights, fog, environment, materials (StandardMaterial and toon surfaces) and placements alike.
# usage: tools/batch-boundary.sh RAEA EDIT.tsx COMPOSITION INPUT.json B [native|live] [PRE] [OUTDIR]
# (run from the project root; needs python3 with numpy and Pillow)
set -u
BIN=$1 F=$2 C=$3 I=$4 B=$5 MODE=${6:-native} PRE=${7:-150} O=${8:-out/batch-boundary/$(basename "$F" .tsx)-$B-$MODE}
HERE=$(cd "$(dirname "$0")" && pwd)
M=--native; [ "$MODE" = live ] && M=
mkdir -p "$O"
at() { python3 -c "print(','.join(str(i) for i in range($1, $2)))"; }
RAEA_ROWS_DUMP=$O/one-rows "$BIN" render "$F" --composition "$C" --at "$(at $((B - PRE)) $((B + 4)))" -o "$O/one/" --input "$I" $M > "$O/one.log" 2>&1 || { echo "one batch failed: $O/one.log"; exit 1; }
RAEA_ROWS_DUMP=$O/fresh-rows "$BIN" render "$F" --composition "$C" --at "$(at "$B" $((B + 4)))" -o "$O/fresh/" --input "$I" $M > "$O/fresh.log" 2>&1 || { echo "fresh batch failed: $O/fresh.log"; exit 1; }
python3 - "$O" "$PRE" <<'PY'
import sys, os, numpy as np
from PIL import Image
o, pre = sys.argv[1], int(sys.argv[2])
worst = float("inf")
for k in range(4):
    a = np.asarray(Image.open(f"{o}/one/frame-{pre + k:05d}.png").convert("RGB"), dtype=float)
    b = np.asarray(Image.open(f"{o}/fresh/frame-{k:05d}.png").convert("RGB"), dtype=float)
    m = ((a - b) ** 2).mean(); p = float("inf") if m == 0 else 10 * np.log10(255 ** 2 / m); worst = min(worst, p)
    print(f"frame +{k}: {'identical' if p == float('inf') else f'{p:.2f} dB'}  (max {int(abs(a - b).max())} levels)")
print(f"WORST {worst:.2f} dB")
PY
[ -f "$O/one-rows/$((B + 1)).json" ] && python3 "$HERE/rows-namediff.py" "$O/one-rows/$((B + 1)).json" "$O/fresh-rows/$((B + 1)).json"
