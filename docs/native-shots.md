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

## After milestone 3: engine upgrades (pc204, pc205)

- **Bake key** (pc204): `build_id` is a hash of the binary's bytes, made once a process (about 0.3 s). The mtime and
  size it used before made two keys for one release installed on two members.
- **JIT for the bake** (pc204, and the default with `--native` from pc205): `runtime/jsengine` runs JavaScriptCore on
  Linux by dlopen of WebKitGTK's `libjavascriptcoregtk-4.1` (the same C API as Apple's framework).
  - ES modules stay Apple's, so a host needing them uses QuickJS.
  - Ch5, one runtime: 17.8 → 3.4 s for 22 frames (5.3×), and a runtime opens in 2.3 s against 4.5.
  - Native on JSC is identical to live on QuickJS (Ch5); Ch2 is unchanged at 57.5 dB.
- **Warm bake runtimes** (measured, not adopted): keeping the runtimes parked (`RAEA_NATIVE_PARK=1`) changes nothing;
  the engine opens in 6.3–6.9 s either way. A 13–15 s open is a new binary's first run on a host. With JSC and lane
  sizing, a production batch opens about 2 runtimes at about 2.5 s, so a cross-process warm pool would save about
  0.03 s a frame.
- **Pipelined readback** (pc205): `Engine::start_read`/`finish_read`, and `over_3d` leaves a frame's readback in
  flight when it's the page over the 3D alone.
  - The film's chapters draw through a layered Canvas3D slot (the page paints with the 3D in place), so they take the
    whole path. Next: hand the slot the 3D's texture on the GPU, as the fed render does, instead of 3D → CPU → page →
    CPU.
- **Fixed-step catch-up** (pc205; found with a chrome trace, `--features raea-render/trace`, `RUST_LOG=info`): see the
  pc205 commit. A shot's first frame now comes 0.7–1 s after its engine opens.
- **Ch2's 57–58 dB residual:** not the mesh entities' spawn order (moving later component rows to the end changed
  nothing). No cheap fix found.
- **Next, by the trace:**
  - Ch2's engine open (19 s: glTF loads, `model_setup`, pipelines);
  - the per-frame render schedule (about 34 ms) and Update (about 29 ms);
  - the slot round trip.
- **GPU-composited draw** (pc206): the native draw makes each frame as a fed render does (`bevy_fed_gpu`: the page
  drawn with the 3D's target in its layered slot into one frame buffer) and reads that buffer back a frame behind.
  - Ch5 cached: 0.170 → 0.109 s/frame, identical to live. Ch2: 0.184 → 0.117 s/frame, 57.1 dB as before.
  - `RAEA_NATIVE_GPU=0` keeps the old path; `--alpha`, `--aov` and `RAEA_PAGE_DUMP` take it too.
- **Parallel fold** (pc207): the fold walks each frame's 3D once, across the cores. Ch5 3.9 → 1.8 s, Ch2 4.4 → 1.8 s.
- **Measured and not kept:**
  - building the page while the bake runs, or while its cache is read: cold frame one 19.5 → 18.4 s, cached no
    better, and steady frames 20–30% slower in alternating runs;
  - the kept bake written on a thread.
- **A binary's first render on a host** pays its bundle (about 7 s) and the GPU's per-binary shader caches (engine
  open 13–19 s against 6.5–10 s later).

---

## HANDOFF (2026-11-16): read this first

### Current build and pin

- **Latest:** `0.1.4-pc209` (commit e08c756; release with sha256 on the fork). pc208 plus portable bakes (project
  root marked, project files hashed: a pruned rq snapshot no longer breaks reuse), DROP3D on by default, and the bake
  kept on a thread beside the engine's opening. Handed to the queue agent to pin, rollback pc208.
- **Pin:** pc203 was pinned for new reels. pc204–pc207 were each marked "ready to pin" in the project's
  design/render-perf.md status lines, and the queue agent pins with rollback.
- **Install:** `tools/render-vast-setup.sh 0.1.4-pcN` (project). It installs raea 0.1.4, swaps in the fork binary
  (checked against the sha256), and installs `libjavascriptcoregtk-4.1-0` for the JIT (optional).
- **Use:** `raea render FILE.tsx --composition C --at A,B,… -o DIR/ --input I.json --native`.
  - `--native` brings `--jit` when JavaScriptCore is installed; `--no-jit` opts out.
  - rq jobs pass it as an extra arg (milestone 4, `--native` by default and a shared bake cache, is the queue agent's).
- **Batch size:** keep 150 for `--native`. A 600-frame native batch drifted on Ch2 @6000–6600 (27 dB): the
  render-to-texture "seven alternatives" panels carry state across a batch, live as much as native.

### Done, with numbers

One host (EPYC 7713, RTX 3090), 60 frames, unless noted.

| shot | path | frame one | steady s/frame | frames/min | PSNR to live |
|---|---|---|---|---|---|
| Ch5 @2400 | pc203 live | 38 s | 1.01 | 35 | |
| | pc205+ live | 15–18 s | 0.93–1.05 | 43–49 | identical to pc203 live |
| | pc207 native, cold | about 19.5 s | 0.10–0.14 | 112–122 | identical |
| | pc206/207 native, cached | 12–13 s | 0.10 | 154–168 | identical |
| Ch2 @14189 shore | pc203 live | 49 s | 2.36 | 19 | |
| | pc206 native, cold | 41 s (bundle and shader-cache first run included) | 0.12 | 69 | 57.4 dB (live against live: 62–69) |
| | pc206 native, cached | 17.7 s | 0.117 | 127 | 57.1 dB |

**Coverage (pc203):** 32 sample shots across Ch1, Ch2, Ch3 and Ch5. All compile. 31 match live at 55 dB or better, 13
of them identical on every frame. The outlier, Ch1 @27800, is the live path's 3D-text bug; native is closer to the
frame drawn alone.

**Fixed on the way** (commit messages have the detail):
- LiveScene's frozen `<Model nodes>` poses and `<Model>` surfaces;
- node ids reused across bake pieces;
- the fold's first-frame base;
- emissive colour keyed (`em_r..`);
- `Engine::warm` on a timeline engine's first frame;
- `raea-frames` stepping idling the JS thread;
- the bake key as a hash of the binary;
- JavaScriptCore on Linux (dlopen of WebKitGTK; 5.3× on the bake);
- **the clock jump as one fixed step** (Bevy's fixed loop had replayed ~frame-number steps on every batch's first draw:
  live and native);
- the GPU-composited native draw (one readback a frame);
- the parallel fold;
- a light's target placed by its groups.

### In progress, and exactly where it stands

**2026-11-16, second engine agent (live log; newest first):**
- **Toward pc210 (committed on native-shots: c966ee3, 7fa6beb; in the tree: the rest):**
  - glTF texture mips across the cores (mip_gltfs_textures): model_setup's 1.77 s call gone.
  - Map files prefetched (decoded and mipped across the cores) from the bake's rows, beside the page's and engine's
    opening (prefetch_start / prefetch_maps; cached_map takes them): Ch2's 3D spawn 3.5 -> 0.55 s.
  - Each glTF's skin weights normalised once, not per model.
  - build_id (the bake key's binary hash) in 32 MB chunks across threads (0.3-1.2 s on one core).
  - Exposure after a folder render: frames decoded and measured across the cores. It was 2.4-2.9 s after the last
    frame of a 60-frame native render (20 PNGs re-read, a powf a pixel on one thread).
  - Vello's shaders initialised on many threads off Apple (it was forced to 1 everywhere).
  - Measured, medians of 3 interleaved (host shared, load ~70): engine open Ch5 8.4 -> 6.5 s, Ch2 9.6 -> 6.2 s.
  - Not taken: RAEA_BEVY_ASYNC_PIPELINES=1 (Bevy's task pool compiles): Ch2 -1.2 s wall, Ch5 nothing; opt-in only,
    for the Windows open stalls on record.
- **Coordinator's queue (scout pass 3, design/research-performance.md), in order:** (1) Vello init threads (done
  above) and a persisted Vello pipeline cache; (2) held-frame skip (count the held share in the pool's bakes first:
  /tmp/held.py on Hetzner; time-driven content is never held; 32 coverage shots pixel-identical on and off);
  (3) a blake3-keyed texture/mip cache on disk; (4) a long-lived raea per lane, sticky-checked on Ch5, Ch2 @6000 and
  Ch3 @900.
- Box notes: /root/ab.sh CH START N ROUNDS BIN... (interleaved A/B, medians; binaries stripped in /root/bins).
- **pc209 shipped** (e2b84a1 + e08c756). Ch5 cached 158 -> 184 frames/min, identical; Ch2 cached 145 -> 157,
  cold 96 -> 108, 56.3 dB. Next: engine open. Cached open on Ch5 is 7.6 s: plugins 0.9, 3D spawn 1.3, timeline
  0.46, await_models 4.6 (290 models). In the chrome trace, model_setup is one 1.77 s call: glTF texture mips made
  serially. Making them across the cores (mip_gltfs_textures), in test.
- **Bake portability** (the queue agent's bug: bakes held `/root/snap/NAME/media/...` paths, so a pruned or other
  snapshot refused them). Now the root is `@@raea-project@@` in the kept bake, placed at the current root on read, and
  `assets` lists each project file the bake names with its blake3. A changed or missing one is "stale" (re-baked, the
  line names the file); an unrelated edit reuses. Tested: baked in a hard-linked copy, copy moved away, drawn from the
  project: 24/24 identical. Edit elsewhere: reused. One byte added to a referenced atlas: re-baked.
- **DROP3D coverage, 32 samples x 24 frames, 6+ pieces, drop against keep (same binary):** 13 identical on every
  frame; all others 56.5 dB or better, with nothing visible, except Ch3 @900.
  - **Ch3 @900 is not DROP3D.** Two keep runs differ by the same 32.4 dB: shadow banding (striped acne) on the sloped
    wall, top right, on alternating frames, and which frames get it changes from run to run. This is the flicker
    coverage item 4 names. It's visible: a known bug, open (below). pc208's "33.7 dB" was this, plus keep and drop
    bakes sharing one key.
  - Ch1 @9000 (57.8) and @12500 (60.1): keep is deterministic and drop differs at skinned edges (an eye highlight,
    glasses rim). The 3D rows are identical, and so are the pages. The same shot differs by the same 60.1 dB between
    keep with the GPU composite and keep without it (RAEA_NATIVE_GPU=0), so it is the engine's update sequence, not a
    missing element. Within the rule; worth a look later.

Parity tools: `RAEA_ROWS_DUMP=DIR` (3D rows) with tools/native-diff.py; `RAEA_PAGE_DUMP=DIR` (note: it turns the GPU
composite off, so it changes the draw path it is measuring).

### Ordered next steps

1. ~~Finish DROP3D and make it the default~~: done in e2b84a1 (pc209 once benchmarked).
2. Engine open (6.5–10 s cached, 13–19 s on a binary's first run on a host).
   - Per a chrome trace: model_setup about 1.7 s, GPU image upload about 1.3 s, upscaling pipelines 0.7 s, mesh
     allocation 0.6 s, glTF loads about 1.2 s.
   - Candidates: a persistent wgpu pipeline cache (needs a Bevy patch), and loading the glTFs in parallel ahead of the
     engine.
3. `BevyPage::new` is 2.8 s on frame one. Making it alongside the bake was measured slower, so find why the page is slow.
4. The steady draw is about 0.10 s/frame: Render about 34 ms, Update about 29 ms. Trace with
   `--features raea-render/trace`, `RUST_LOG=info`, then analyse trace-*.json (spans.py in the dev-box notes below).
5. Batch-sticky render-to-texture screens (Ch2 @6000–6600). Fixing it allows `--batch 600` and spreads frame one over
   4× the frames.
6. Ch2's 57–58 dB residual: no reflected property differs, and it isn't spawn order. Low priority, as nothing is visible.
7. Upstream issues and PRs (only after production validation):
   - LiveScene node/surface freezing;
   - the clock-jump fixed-step catch-up;
   - the emissive colour fold;
   - the light target in groups;
   - `raea-frames` idling;
   - the global World-asks queue across runtimes;
   - bake RESET id reuse in `bake_incremental`.

### Known bugs

- **Ch3 @900 shadow banding flicker** (both paths, run to run): striped acne on the sloped wall on alternating
  frames. Visible. Likely shadow-map state carried per update (cascades or a temporal filter). Open.
- **A sun (directional or spot light) in a moved or turned group ignored the group's aim.** Fixed in pc208 (8ce2a35,
  with a unit test) for render, live and native alike. Ch5's old-person helper has a workaround that can now come out;
  re-check it.
- The live path garbles changing 3D text (RichText atlas) within a batch. Native is right.
- Batch-sticky render-to-texture screens, in both paths (above).
- The World-asks queue (`ASKS` in runtime/shared) is process-global: several React runtimes in one process can cross
  asks. Harmless while a bake meets none (the probe line counts them).
- A native shot that makes layout or World reads, or has a stateful `<Compute>`, falls back to the live path. None of
  the 32 coverage samples did.

### How to build and validate

**Build box:** rent a vast GPU box with 64 cores (image nvidia/cuda:12.6.3-base-ubuntu24.04, label `pc-native-dev`,
never a pool label).
1. apt the packages in tools/release/linux/Dockerfile plus `libjavascriptcoregtk-4.1-0 time`; rustup 1.99.1; bun; uv.
2. rsync this worktree to /root/raea (excluding .git, target and node_modules).
3. `bun install --frozen-lockfile` in sdk/react/js and sdk/react/tw.
4. `cargo build --profile dist -p raea-cli --bin raea --no-default-features --features "$(bash tools/bevy-plugins.sh features sdk/raea/bench/bevy-plugins)"`:
   about 4–5 min cold. A release build with `CARGO_PROFILE_RELEASE_INCREMENTAL=true` rebuilds in about 15 s.
5. Strip, sha256, `gh release create v0.1.4-pcN -R lizTheDeveloper/raea --target <commit> --prerelease` with the
   binary and the .sha256.

**Test assets:**
- the project via `tools/vast-relay.sh mirror` then `push HOST PORT /root/pc`;
- raea 0.1.4 via `sh render-vast-setup.sh 0.1.4-pcN`, then `python3 tools/raea-patches/apply.py` in /root/pc;
- a dev binary needs `libraea_bundle.so` beside it (symlink from ~/.raea/bin).

**Validate:** a 60-frame shot rendered live and `--native` on one host; per-frame PSNR of the PNGs.
- Ch5 @2400 must be identical.
- Ch2 @14189 at about 57 dB or better (its live/live noise is 62–69).
- Parity tools: `RAEA_ROWS_DUMP=DIR` on both paths, tools/native-diff.py A B.
- Timing: `RAEA_NATIVE_TRACE=1`, `RAEA_STARTUP_TRACE=<ms epoch>`.
- Through rq: `tools/rq submit ch05-iso.tsx chapter-5-iso 60 out/perf/native/X.mp4 --test --start 2400 --raea 0.1.4-pcN --input story/reel-input-film-ch05.json --native`, then `rq wait`.

**Rules:**
- fork branch only, never main;
- destroy your boxes;
- never rm (`tools/trash.sh` in the project, mv to .trash elsewhere);
- keys in env only;
- uv for Python;
- no time estimates.
