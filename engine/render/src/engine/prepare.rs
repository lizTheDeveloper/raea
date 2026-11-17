//! The next shot made ready before the cut, off the draw thread: its rows, its timeline (every frame evaluated) and its
//! map files decoded and mipped. `Engine::reload_prepared` then swaps it in, so a cut pays only the World's spawn and
//! warm-up. What's made here is a pure function of the program and the project's files, made exactly as the load would
//! make it, so a shot reached through a Prepared draws the same frames as one loaded by reload or opened fresh.

use super::*;
use std::sync::Arc;

/// A shot made ready by `Prepared::new` (Send: made on any thread), for `Engine::reload_prepared`.
pub struct Prepared {
    pub prog: Arc<Program>,
    pub inserted: Vec<Tup>,
    pub(crate) timeline: Arc<raea_core_program::timeline::Timeline>,
    pub(crate) maps: raea_sys_material::maps::Prefetched,
    /// how long it took to make (ms)
    pub ms: f64,
}

impl Prepared {
    /// The shot `prog` made ready, its project at `dir`. `have`: the maps the engine it's for holds already
    /// (Engine::map_sources), which it doesn't decode again.
    pub fn new(prog: Arc<Program>, dir: &Path, have: &std::collections::HashSet<(String, bool)>) -> Result<Prepared, String> {
        let inserted = crate::inserted_of(&prog);
        Prepared::with_rows(prog, inserted, dir, have)
    }

    /// As new, with the program's rows made already (raea_render::inserted_of).
    pub fn with_rows(prog: Arc<Program>, inserted: Vec<Tup>, dir: &Path, have: &std::collections::HashSet<(String, bool)>) -> Result<Prepared, String> {
        let t = std::time::Instant::now();
        // the maps on a thread beside the timeline (each spreads across the cores: par)
        let (maps, timeline) = std::thread::scope(|s| {
            let m = s.spawn(|| raea_sys_material::maps::decode_maps(&inserted, dir, have));
            // the base the load would gather: the rows stored in a World of their own, as the engine stores them
            let base = store::Store::new(&inserted).gather();
            let tl = raea_core_program::timeline::Timeline::from_cols(&prog, base);
            (m.join().unwrap_or_default(), tl)
        });
        let timeline = Arc::new(timeline?);
        Ok(Prepared { prog, inserted, timeline, maps, ms: t.elapsed().as_secs_f64() * 1e3 })
    }
}

impl Engine {
    /// The map files this engine holds made (source, colour): pass to Prepared::new for the shot after this one.
    pub fn map_sources(&self) -> std::collections::HashSet<(String, bool)> {
        raea_sys_material::maps::held_sources(self.app.world())
    }
}

impl Opening {
    /// What a Prepared shot made, for this opening's load (its timeline and maps taken, not made again): an engine
    /// opened while the shot was being prepared on another thread.
    pub fn prepared(&mut self, p: Prepared) {
        raea_sys_material::maps::prefetch_install(p.maps);
        self.timeline = Some(p.timeline);
    }
}
