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

## Milestone 2: a Ch2 shore shot with characters (done; raea 0.1.4-pc202, pc203)

The shot is Ch2 @14189–14249: Pell speaking ("Director Marra of the Basin"), with Noys, Marra, the procession and the
child (skinned rigs, face morphs, crowns, posed nodes), the sea's uniforms (`own:uniforms` component rows), a layered
Canvas3D and captions.

**Two native bugs found and fixed on the way:**
- **Node ids reused across a RESET.** A piece from another runtime numbers its tree its own way. The bevy_ui page keys
  its elements by React node id, so a reused id kept the old element: from the piece starting at 14230, the layered
  3D's holder painted its gradient over the 3D (15 dB). Each piece's ids are now moved into a range of their own.
- **Folding from the stepped neighbour.** See milestone 1.

**Parity.** Native against live is 57.4–58.7 dB on all 60 frames, with no visible difference. Live against live is
62.3–69.5 dB.
- No reflected property differs on any row entity or its descendants (`RAEA_ROWS_DUMP`).
- The gap grows with the number of extra component-row entities (live 6,930; native over 21 frames 8,295 at
  63–72 dB; over 60 frames 10,834 at 57–58 dB), which points at draw-order noise.

**Measured on one host** (EPYC 7B13, RTX 4090):

| path | frame one | steady s/frame | wall | frames/min |
|---|---|---|---|---|
| pc162 live | 45.6 s | 2.01 | 167.5 s | 21.5 |
| pc201 live | 36.8–49.4 s | 2.08 | 163–176 s | 20.5–22.1 |
| `--native`, cold | 71–98 s | 0.20 | 87–114 s | 31–41 |
| `--native`, cached | 40.9 s | 0.19 | 55.6 s | 65 |

- **Frame one** is Ch2's engine open (its models) in both paths, about 40 s. A cold bake adds its 22 s on 11 runtimes.
- **Through rq on a loaded pool** (three members, each with other jobs' lanes): pc162 live 202 s, pc202 live 240 s,
  pc202 `--native` 91 s of raea time for the 60 frames.

## Milestone 3: coverage across Chapters 1, 2, 3 and 5 (done; raea 0.1.4-pc203)

Each chapter has 8 sample shots spread through it, 24 frames each. Each is rendered live and native with
`RAEA_BAKE_MIN=4`, so every shot is baked in 6+ pieces and the joins are exercised. PNGs on one host, PSNR native
against live:

| chapter | samples | compile natively | identical on all 24 frames | worst frame |
|---|---|---|---|---|
| Ch1 | 8 | 8 | 2 | 30.97 dB (@27800: the live path's 3D text, below) |
| Ch2 | 8 | 8 | 0 (Ch2 never repeats exactly, live against live) | 55.0 dB (@2700) |
| Ch3 | 8 | 8 | 2 | 58.7 dB (@3200) |
| Ch5 | 8 | 8 | 4 | 76.9 dB (@6000) |

**Every sample compiles.** No bake met a layout read, a World read or a stateful `<Compute>` (the `NATIVE  probe`
line), so none fell back.

**What the sweep found, and where each was fixed:**
1. **A mesh's emissive colour was the first frame's in the fold** (strength only was keyed, and a mesh black at
   first had no glow at all). Ch1 @20500 went from 47.8 to 66.1 dB.
   - Fixed with keyed `em_r..em_b`. The fold is also live play's 3D (`scene_program`), so play had this too.
2. **A timeline engine's first frame wasn't warmed up.** Ch5's facet windows (second cameras into textures) came up
   empty on a shot's first frame: 19–24 dB on five Ch5 samples.
   - Fixed with `Engine::warm`, armed as a live engine's first rows arm it. Now identical.
3. **The live path garbles changing 3D text (RichText) within a batch** (bevy_rich_text3d's "atlas expansion" warning).
   On Ch1 @27800, frame 27812: live batch 30.9 dB against the frame drawn alone, native 43.3 dB.
   - Native is the closer one. Not fixed: it's in the live path's text.
4. **Per-update engine state in a live batch flickers** on Ch3 @900: frames alternate with the batch's start.
   - Native matched the frame drawn alone (96 dB) where the live batch didn't (42 dB). After fix 2 the sample is
     74.8 dB.
5. **`<Model>` surface overrides** (`<MeshStandardMaterial>` children) were the first frame's in both paths. They are
   now keyed: fold `sf{i}_*`, LiveScene, `sync_surfaces`.
   - Ch5 @2400 frame 2459 against the frame drawn alone went from 63.5 to 69.2 dB, native identical to live.
   - The live half of this and the node fix (0a16018 on `fix/live-node-overrides`, pc163) are the same family. That
     commit covers nodes only.

**Still not native-exact, and why:**
- **Ch2's residual** (55–58 dB, below its own 62 dB run noise): no property differs; it tracks the number of extra
  component-row entities. The next step is to make component rows data, not entities.
- **Constructs no sample used, which the bake refuses rather than compiles:**
  - layout reads (`page.layout`): the bake has no page;
  - World reads (useResource, useQuery, useSystem, useObserver, raycasts through the Engine's reads): the bake has no
    Engine;
  - a stateful `<Compute>`;
  - a camera inside a page element (S3a), whose box is known only once the page is laid out. Not detected yet.
  - Building these into the exporter means answering them in the bake: a page laid out per piece for layout reads, or
    a headless Engine per piece fed the piece's rows for World reads. That's the next step if a chapter needs them;
    none of the 32 does.

## Scout pass 2 (2026-11-16), tried

- **Bake runtimes sized to the lanes.** In raea rather than rq: a bake takes cores ÷ (raea processes running on the
  machine) runtimes, and the same share of free memory. `RAEA_BAKE_MIN` still sets the least frames a piece.
- **The kept bake compressed.** zstd over the JSON: 124 MB → 11.3 MB for Ch5's 62 frames, read back in 0.9 s,
  bit-identical. About 4 GB a chapter, not 44.
  - Sharing it across the pool through the relay is rq's side: the cache is `$XDG_CACHE_HOME/raea/shots`, or
    `RAEA_SHOT_CACHE`.
- **`RAEA_BEVY_EXECUTOR=multi` for the native draw: slower.** 0.276 against 0.196 s/frame on a 64-core host, so it stays
  single-threaded (`new_live`'s choice).

**Next, by expected payoff:**
1. Warm bake runtimes kept across a process's shots (removes the 3.3–3.9 s opening each).
2. A JIT (JavaScriptCore or V8) for the bake only. It's now the one place JavaScript runs.
3. An async readback ring and pipelined draw: about 0.2 → 0.15 s/frame.
4. Component rows as data rather than entities.
5. rq: `--native` by default for chapter renders, and the shared bake cache (milestone 4).
