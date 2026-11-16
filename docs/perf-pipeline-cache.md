# Process startup across stills batches: a shared shader and pipeline cache (planned)

A long film renders as many short `raea render --at` processes, each 150-600 frames, often several to a GPU. Measured
on raea 0.1.4-pc162 (RTX 3090, Linux), a process's first frame costs 20-68 s before the first frame lands. Steady frames
are 1.4-3 s. At 150 frames that's 5-15% of the GPU-time.

Plan, in order:
1. **Split the startup** with RAEA_STARTUP_TRACE marks:
   - bundling the composition;
   - React's first commit;
   - asset and glTF loads;
   - WGSL compiles;
   - the render pipelines specialising on first draw.
2. **Cache the pipelines.** If pipelines dominate: wgpu's `PipelineCache` (Vulkan `VK_KHR_pipeline_cache` via
   `Features::PIPELINE_CACHE`), saved to `~/.cache/raea/pipelines/<adapter>-<driver>-<raea>` after the first frame and
   handed to the device at startup. Plus a content-keyed WGSL/naga cache for the shaders raea composes (look and
   material extensions).
3. **Cache the bundle.** If the bundle dominates: key the bundled JS by the composition's import graph hash (the
   export cache already does this for mp4 outputs), so a batch after the first reuses it.
4. **Measure and validate** on the same chapter ranges: first-frame time, steady s/frame, and PSNR against the
   uncached build (pixels must be identical).
