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
