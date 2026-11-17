//! `raea render EDIT.tsx -o OUT.mp4|OUT.png|DIR|OUT.sql [--frame N | --at A,B]` and the rest of raea's command line
//! (lib.rs's `main`), with the allocator every raea process counts with.

/// ZeroHeap Z1: every allocation counted per thread while RAEA_ZEROHEAP=1 (one relaxed load more otherwise)
#[global_allocator]
static ALLOC: raea_core_alloc::Counting = raea_core_alloc::Counting;

fn main() {
    // the `spans` build's per-span sums (raea-render engine/spans.rs); nothing in any other build
    raea_render::engine::Engine::spans_global();
    let code = raea_cli::main();
    // On Linux a finished raea leaves without the C library's exit handlers: the NVIDIA driver's own teardown ran there
    // while Bevy's task pools and render threads still held the device, and some hosts (driver 575.51, a pool member)
    // segfaulted after every frame was written and the summary printed (exit 139: rq counted the batch failed). raea's
    // own work is joined before main returns; its output is flushed here. RAEA_EXIT_CLEAN=1 exits the usual way.
    #[cfg(target_os = "linux")]
    if std::env::var_os("RAEA_EXIT_CLEAN").is_none() {
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
        unsafe extern "C" {
            fn _exit(code: i32) -> !;
        }
        // SAFETY: the process ends here; nothing after this runs
        unsafe { _exit(code) }
    }
    std::process::exit(code);
}
