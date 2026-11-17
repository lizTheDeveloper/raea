#!/bin/bash
# Sticky-state census (docs/native-shots.md): a frame drawn in the middle of a batch must equal the same frame drawn
# alone. For each CH:F, renders [F-20, F+10) as one batch and F by itself, compares F (PSNR), and fails below FLOOR dB
# (default 55, the noise bar Ch2 is held to). Run from the project root.
# usage: tools/sticky-census.sh RAEA native|live OUTDIR CH:F... (CH 1-5: chNN-iso.tsx, chapter-N-iso, the film input)
set -u
BIN=$1 MODE=$2 O=$3; shift 3
FLOOR=${FLOOR:-55}
M=--native; [ "$MODE" = live ] && M=
mkdir -p "$O"; fails=0
for cf in "$@"; do
  ch=${cf%%:*} f=${cf#*:}
  F=ch0$ch-iso.tsx C=chapter-$ch-iso I=story/reel-input-film-ch0$ch.json
  at=$(python3 -c "print(','.join(str(i) for i in range($f - 20, $f + 10)))")
  d=$O/ch$ch-$f; mkdir -p "$d/batch" "$d/alone"
  "$BIN" render "$F" --composition "$C" --at "$at" -o "$d/batch/" --input "$I" $M > "$d/batch.log" 2>&1 || { echo "ch$ch @$f: batch failed ($d/batch.log)"; fails=$((fails + 1)); continue; }
  "$BIN" render "$F" --composition "$C" --at "$f" -o "$d/alone/" --input "$I" $M > "$d/alone.log" 2>&1 || { echo "ch$ch @$f: alone failed ($d/alone.log)"; fails=$((fails + 1)); continue; }
  r=$(python3 - "$d/batch/frame-00020.png" "$d/alone/frame-00000.png" "$FLOOR" <<'PY'
import sys, numpy as np
from PIL import Image
a = np.asarray(Image.open(sys.argv[1]).convert("RGB"), dtype=float); b = np.asarray(Image.open(sys.argv[2]).convert("RGB"), dtype=float)
m = ((a - b) ** 2).mean(); p = float("inf") if m == 0 else 10 * np.log10(255 ** 2 / m)
print(f"{'identical' if p == float('inf') else f'{p:.2f} dB'} (max {int(abs(a - b).max())} levels) {'FAIL' if p < float(sys.argv[3]) else 'ok'}")
PY
)
  echo "ch$ch @$f: $r"; [[ $r == *FAIL ]] && fails=$((fails + 1))
done
echo "FAILS $fails"
[ $fails -eq 0 ]
