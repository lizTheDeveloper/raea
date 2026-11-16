# Native shots: compile a shot once, play it with no JavaScript per frame

## The goal

Render a film's shots the way a game draws them:
- run the composition's JavaScript once per shot and export the scene: spawns plus animated columns, which is what
  raea's `Program` IR already is;
- play it in Rust with no React per frame, one persistent process per shot.

## What raea already has: the "programmed" path

- `RAEA_3D=program`. React steps the frames, `scene_frames` reads each frame's 3D as rows, and
  `runtime/export/src/program.rs` folds them into a `Program`: spawns and per-(id, prop) keyed columns, with pages as
  image rows.
- `raea-render` (`Engine::new_live`) then draws the 3D from the Program without the live bridge.
- `scene_frames` can step a long range across several React runtimes on threads (`RAEA_BAKE_WORKERS`).

## Measured on Ch5 @2400

60 frames on a Ryzen 5 3600 with an RTX 3090, raea 0.1.4-pc162, stills:

| path | before frame one | steady | PSNR vs live |
|---|---|---|---|
| live (today) | 15 s | 0.82 s/frame | |
| `RAEA_3D=program` | 177 s | 0.74 s/frame | 41–64 dB (visually close) |

The 177 s before frame one breaks down as:
- React stepping the 62 wanted frames: 68 s, about 1.1 s/frame, on one runtime;
- folding the Program over 2,461 held frames: 92 s;
- opening the engine: 9 s.

## Why it isn't native yet

1. **React still runs every frame in the draw loop**, for the page (the 2D under and over the 3D). So the steady
   frame barely moves. Fix: draw the page from the Program's page rows too, as the 2D video path does, or composite a
   cached page layer.
2. **The bake is one runtime for `--at` stills.** `scene_frames` only splits across worker threads for whole-timeline
   renders. Fix: split the wanted runs across `RAEA_BAKE_WORKERS` runtimes; frames are pure. That's N cores of JS
   per shot instead of one.
3. **The fold covers every frame from 0, not the shot.** `program()` builds over 0..upto (2,461 frames for a
   60-frame shot). Fix: fold only the shot's frames, offset to the shot.
4. **Parity is 41–64 dB rather than inf.** Find which columns differ between live and programmed (Model nodes and IK,
   materials keyed linearly, page compositing). Then either key them exactly at every frame or document the difference.
5. **Persist the Program as a bake** (`shot.prog`, serde on `Program` and `V`), keyed by the shot's element-tree hash.
   A re-render then skips the bake, and a patch re-bakes only its frames.

## Expected payoff

With the bake on N cores and the draw free of JavaScript, a frame costs:

  bake_JS / N + draw

On a 12-core member that's about 1.1 / 10 + ~0.15 s, roughly 4× today's per-lane speed, more with more cores. Cached
bakes make an iteration that only moves the camera or lights nearly free.

## Milestones

1. One Ch5 shot end to end with fixes 1–3: frames/min and PSNR.
2. One Ch2 shore shot with characters (skinning, morphs, cloth bones, uniforms, captions).
3. A coverage report across chapters.
4. rq integration with a per-shot fallback to the live path.

## Milestone 1: one Ch5 shot, native (done; raea 0.1.4-pc201)

`raea render EDIT.tsx --at A,B,… -o DIR/ --native` (or `RAEA_NATIVE=1`), in `runtime/export/src/native.rs`:

1. **The bake.** The shot's stepped frames (each drawn frame and its neighbours) are cut into pieces, one React runtime
   each, on threads. There is one runtime per core, capped at one per GB of free memory, with at least
   `RAEA_BAKE_MIN` (6) frames each. Each piece taps its commits; its first frame is a RESET plus that runtime's whole
   tree, so a piece replays on its own.
   - Frames are asked for one `raea-frame` event at a time, as a render asks. `raea-frames` (what `bake.rs` and
     `scene_frames` use) left the JS thread idle about 0.45 s a frame: 1.12 s a frame against 0.67, for the same JS
     CPU (`RAEA_NATIVE_STEP=range` keeps it, to compare).
2. **The fold.** The Program is folded over the shot's frames alone, from the first drawn frame, then moved there.
   - Its keys are moved, and so are the `from`/`to` of page, screen and sound rows.
   - Component and resource rows are made at their own frame through raea-runtime-look's `KEY_BASE`.
   - The engine's clock, time uniforms and plugin clock stay the composition's own.
   - Folding from the stepped neighbour before the first frame showed that neighbour's `<Entity>` places on the first
     frame (a live engine's first draw shows its rows' own values for that kind).
3. **The draw.** A `ReplayPlugin` App, no JS thread: the recorded commits are fed in, the page is synced and painted
   from them, and the 3D is drawn from the Program.
   - The page step skips the 3D elements' props, and the scene isn't read each frame: page 42 → 24 ms.
4. **The cache.** The bake (each frame's commits and the Program as JSON, 124 MB for 62 frames of Ch5) is kept by the
   bundle's bytes (the edit, its imports, its `--input`), this binary, the frames and the settings that change a step.
   - `RAEA_SHOT_CACHE` (`0`: none). A read-back cache is bit-identical to a fresh bake.
5. **The fallback.** A shot whose bake makes layout reads (`page.layout`, answered "nothing drawn" in a bake) or World
   reads (useResource, useQuery, useSystem, useObserver: no Engine answers them in a bake), or has a stateful
   `<Compute>`, renders the usual way.
   - The render's `NATIVE  probe:` line says what it met, and `RAEA_NATIVE_FORCE=1` draws it natively anyway.
   - `RAEA_NATIVE_PROBE=1` stops after the bake (the coverage report).
6. **Parity tools.** `RAEA_ROWS_DUMP=DIR` writes each drawn frame's 3D as the engine holds it: every row's entity and
   the entities under it, reflected, with their materials. `tools/native-diff.py A B` compares two dumps property by
   property; quaternion sign and entity-keyed maps are noise.

### Parity: what differed, and why

On Ch5 @2400 native came out 41–46 dB against today's path (pc162). Property by property:

- **`<Model nodes>` (per-bone poses: Noys's leg IK) froze in the live path.** LiveScene sent a model's
  `node{i}_*` columns only when the engine opened, so a stills batch held its first frame's pose for the whole batch.
  Native keyed them every frame, as React computed them.
  - Fixed in LiveScene (the columns are sent when they change; the fast path hands a posed model to the full diff).
  - Against each frame rendered alone in a fresh process, pc162 is 41.7–45.8 dB, and the fixed live path and native
    are 63–72 dB.
- **The first drawn frame showed the stepped neighbour's `<Entity>` places** (`sea-g34`, 2 mm). Fixed by folding from
  the first drawn frame.
- **Not fixed, and the same in both paths:** a `<Model>`'s surface overrides (`surfaces`, the chorometer strings'
  emissive) are the first frame's in the fold and in LiveScene.
  - That's the residual 63–72 dB against frames rendered alone: batch-sticky, as render-perf.md found.

**Result: native is PSNR-identical to the fixed live path on all 60 frames** (PNG on one host; the rq mp4s are
identical too).

### Measured

Chapter 5 @2400, 60 frames, PNG stills:

| host | path | frame one | steady s/frame | raea wall | frames/min |
|---|---|---|---|---|---|
| vast EPYC 7B13 (64 cores), RTX 4090 | pc162 live | 17.7–31.9 s | 0.87 | 72–86 s | 42–50 |
| | pc201 live | 32.4 s | 0.91 | 88.8 s | 40.6 |
| | pc201 `--native`, cold (11 runtimes) | 27.5 s | 0.21 | 43.5 s | 82.7 |
| | pc201 `--native`, cached bake | 15.6 s | 0.215 | 31.3 s | 115 |
| rq pool member 60862935 (16 cores, 3090) | pc201 live | | | 94 s | 38 |
| | pc201 `--native`, cold | | | 50 s | 72 |

Where a native frame goes (EPYC 7B13, 4090, cached):
- replay of the commits: 19 ms;
- page: 24 ms;
- 3D draw: 123 ms;
- read back: 38 ms;
- PNG: 7.5 ms.

Where the cold bake goes (11 runtimes):
- each runtime opens in 3.3–3.9 s (bundle parse and module top level);
- its first frame is a whole tree;
- then 0.67 s a frame of QuickJS;
- the Program folds 62 frames in 2.9 s;
- the engine opens in 5.6 s, or 11 s right after a many-runtime bake (the runtimes' teardown overlaps it).

**What a frame costs in CPU.** A cold native frame still pays React once, at about the live path's cost (0.67 s of JS).
- It's spread across cores, so a shot's wall time falls.
- Each piece also pays its runtime's opening and a whole first tree.
- So a member whose cores are all busy with lanes gains less than these single-process numbers. The win there is the
  cache, and the draw (0.2 s a frame against 0.9).

**Found on the way (upstream issues to file):**
1. LiveScene's frozen model nodes.
2. `raea-frames` stepping idles the JS thread.
3. World asks (`world.watch`/`system`/`observe`) go through one process-global queue (`ASKS`), drained by whichever
   App's `asks` system runs. With several React runtimes in a process (`scene_frames`' workers, a native bake), one
   runtime's asks can land in another's App.
