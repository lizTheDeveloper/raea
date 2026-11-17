//! `raea play EDIT.tsx --shots DIR`: a chapter played from its native-shot bakes (the .json.zst a `render --native`
//! keeps, made on vast), as a baked timeline plays: each frame's page commits replayed into the page and its 3D drawn
//! from the shot's Program, no JavaScript per frame for any of it (the live player, premature-collapse
//! design/engine-live-loop.md, Lane F). The bakes are read by path (raea_runtime_export::read_shot: the render's own
//! reader; a bake made by another build or on another machine is placed at this project's root and refused if a file
//! it names changed). The 3D engine follows the frame: crossing into another shot's frames reloads it with that
//! shot's Program (Engine::reload; a fresh engine if the reload refuses).

use super::*;
use std::sync::Arc;

pub(crate) struct Shots {
    /// each shot's frames (first, last) and its Program, by first frame
    list: Vec<(i64, i64, Arc<raea_core_program::ir::Program>)>,
    /// the shot the engine holds now
    at: Option<usize>,
}

impl Live {
    pub fn open_shots(tsx: &Path, shots: &Path) -> R<Live> {
        warm_audio();
        raea_runtime_react::interactive_thread();
        let tsx = tsx.canonicalize().map_err(|e| format!("{}: {e}", tsx.display()))?;
        let dir = tsx.parent().unwrap().to_path_buf();
        *raea_runtime_react::native::ASSET_DIR.lock().unwrap() = Some(dir.clone());
        let t0 = std::time::Instant::now();
        let mut files: Vec<PathBuf> = std::fs::read_dir(shots).map_err(|e| format!("{}: {e}", shots.display()))?.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.to_string_lossy().ends_with(".json.zst")).collect();
        files.sort();
        let (mut w, mut h, mut fps, mut total) = (0u32, 0u32, 30.0, 0i64);
        let mut frame_ops: Vec<Vec<String>> = Vec::new();
        let mut keys: Vec<(i64, Vec<Value>)> = vec![(-1, Vec::new())];
        let mut list = Vec::new();
        let mut refused = Vec::new();
        for p in &files {
            let b = match raea_export_read(p, &dir) {
                Ok(b) => b,
                Err(e) => {
                    refused.push(e);
                    continue;
                }
            };
            (w, h, fps, total) = (b.w, b.h, b.fps, total.max(b.total));
            if frame_ops.len() < total as usize {
                frame_ops.resize(total as usize, Vec::new());
            }
            let (lo, hi) = (b.ops.first().map_or(0, |o| o.0), b.ops.last().map_or(0, |o| o.0));
            for (f, batches) in b.ops {
                if f < 0 || f >= total {
                    continue;
                }
                // a piece starts with RESET and its runtime's whole tree: a jump into it needs no keyframe of its own
                // (an empty one before it, then this frame's commits)
                if batches.first().is_some_and(|x| x == raea_runtime_bake::RESET) {
                    keys.push((f - 1, Vec::new()));
                }
                frame_ops[f as usize] = batches;
            }
            if let Some(prog) = b.prog {
                list.push((lo, hi, Arc::new(prog)));
            }
        }
        for e in &refused {
            eprintln!("raea play --shots: {e}");
        }
        if list.is_empty() {
            return Err(format!("no usable shot bakes in {} ({} files, {} refused)", shots.display(), files.len(), refused.len()));
        }
        keys.sort_by_key(|k| k.0);
        keys.dedup_by_key(|k| k.0);
        list.sort_by_key(|s| s.0);
        eprintln!("raea play --shots: {} shots, frames {}-{} of {total}, read in {:.0} ms", list.len(), list[0].0, list[list.len() - 1].1, t0.elapsed().as_secs_f64() * 1e3);
        let mut conf = serde_json::Map::new();
        for (k, v) in [("width", w as f64), ("height", h as f64), ("fps", fps), ("frames", total as f64)] {
            conf.insert(k.into(), serde_json::json!(v));
        }
        let baked = raea_runtime_bake::Baked { conf, fps, frames: total, w, h, init: Vec::new(), frame_ops, keys, comps: Vec::new(), code: Default::default(), has_3d: true };
        // the brand and the page's stylesheets come from the composition's bundle (made, never run)
        let js = std::env::temp_dir().join(format!("raea-live-{}-{:x}.js", std::process::id(), rand_id()));
        raea_runtime_react::scene::bundle_composition(&tsx, &js, &crate::open::baked_opts(&dir)?, None)?;
        let mut app = App::new();
        app.add_plugins(MinimalPlugins).add_plugins(raea_runtime_react::ReplayPlugin);
        raea_runtime_react::scene::one_thread(&mut app);
        let page = raea_runtime_page::BevyPage::new(w, h, &dir)?;
        let brand = raea_runtime_react::scene::page_css(&dir, w, &js);
        let mut live = Live { app, dir: dir.clone(), page, brand, dirty: true, mixer: None, mixer_failed: false, lag: (0.0, std::time::Instant::now()), w, h, fps, frames: total, responsive: false, js, master: None, master_wav: None, game_master: None, game_gain: None, scene_rx: None, scene: None, live3d: None, frame: 0, pads: Vec::new(), pointer: Default::default(), made: Default::default(), sound_buf: Vec::new(), holders: Vec::new(), baked: Some(Arc::new(baked)), applied: -1, session: new_session(), shots: Some(Shots { list, at: None }) };
        let first = live.shots.as_ref().map_or(0, |s| s.list[0].0);
        live.frame = first;
        live.replay(first);
        live.draw()?;
        live.warm_sound();
        Ok(live)
    }

    /// The first frame the bakes cover (a chapter's player starts there)
    pub fn shots_first(&self) -> Option<i64> {
        self.shots.as_ref().map(|s| s.list[0].0)
    }

    /// The engine made to hold the shot that frame `f` is in (draw calls it first).
    pub(crate) fn follow_shot(&mut self, f: i64) {
        let Some(s) = self.shots.as_mut() else { return };
        let Some(i) = s.list.iter().position(|(lo, hi, _)| f >= *lo && f <= *hi) else { return };
        if s.at == Some(i) {
            return;
        }
        let t = std::time::Instant::now();
        let prog = s.list[i].2.clone();
        let inserted = raea_render::inserted_of(&prog);
        let kept = match self.scene.as_mut() {
            Some(e) => match e.reload(&prog, &inserted) {
                Ok(()) => true,
                Err(why) => {
                    eprintln!("raea play --shots: the engine didn't take shot {i} ({why}); a fresh one");
                    false
                }
            },
            None => false,
        };
        if !kept {
            self.scene = None;
            match raea_render::engine::Engine::new_live(&prog, &inserted, &self.dir) {
                Ok(e) => self.scene = Some(e),
                Err(e) => eprintln!("raea play --shots: shot {i}'s 3D didn't open: {e}"),
            }
        }
        s.at = Some(i);
        raea_io_trace::event("shot", serde_json::json!({"shot": i, "from": s.list[i].0, "ms": t.elapsed().as_secs_f64() * 1e3, "reloaded": kept}));
    }
}

fn raea_export_read(p: &Path, dir: &Path) -> R<raea_runtime_export::ShotBake> {
    raea_runtime_export::read_shot(p, dir)
}
