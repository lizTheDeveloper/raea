//! `raea play SCENERY.tsx --shots DIR --cast CAST.tsx`: game-time characters over a chapter's baked scenery (the live
//! player, premature-collapse design/engine-live-loop.md, Lane F). The scenery plays from its native-shot bakes with no
//! JS (shots.rs). The cast is a composition of its own, its characters only (no camera, set or lights: the shot has
//! them; its camera, lights and post effects are never sent), run frame by frame as live play runs a game: React steps it at the film's frame, its 3D rows are made from
//! that frame's own state (LiveScene, as a game's are) and set into the shot's engine beside the baked Program. Its
//! rows are numbered clear of the Program's (LiveScene::numbered_from) and its camera row is never sent, so the shot's
//! camera and ambient stay. Entering another shot reloads the engine, and the whole cast is set into it again.
//! The cast's page (its 2D) isn't drawn: the bakes' page is.

use super::*;

/// the cast's rows are numbered from here (`model1000000`...), a baked Program's from 0. Why a million: a heavy Ch5 shot
/// has a few thousand rows of a kind. A Program that reaches it is refused loudly at the cut (Cast::clear_of), never
/// drawn with ids that collide.
const CAST_IDS: usize = 1_000_000;

pub(crate) struct Cast {
    app: App,
    ls: raea_runtime_react::scene::LiveScene,
    js: PathBuf,
    tsx: PathBuf,
    dir: PathBuf,
    input: Option<PathBuf>,
    /// kinds of row the cast made that a baked Program may also name by the render's own ids (said once)
    warned: bool,
    /// the last step's JS and rows, ms (what a frame of live characters costs)
    pub(crate) cost: (f64, f64),
}

impl Cast {
    /// `input`: the input file the scenery was baked with (a render's --input: a story's choices), so the cast's story
    /// state is the bake's
    pub(crate) fn open(tsx: &Path, input: Option<&Path>) -> R<Cast> {
        let tsx = tsx.canonicalize().map_err(|e| format!("{}: {e}", tsx.display()))?;
        let dir = tsx.parent().unwrap().to_path_buf();
        let js = std::env::temp_dir().join(format!("raea-cast-{}-{:x}.js", std::process::id(), rand_id()));
        let mut opts: Vec<String> = Vec::new();
        if std::env::var("RAEA_LIVE_REACT").as_deref() != Ok("1") {
            opts.push("--lite".into());
        }
        if let Some(i) = input {
            opts.push("--input".into());
            opts.push(i.display().to_string());
        }
        let t = std::time::Instant::now();
        raea_runtime_react::scene::bundle_composition(&tsx, &js, &raea_runtime_react::scene::comp_opts(&opts)?, None)?;
        raea_runtime_react::native::open_in(&dir, &js);
        let mut app = App::new();
        app.add_plugins(MinimalPlugins).add_plugins(ReactPlugin::new(js.clone()));
        raea_runtime_react::scene::one_thread(&mut app);
        raea_runtime_react::scene::settle(&mut app, Some(-1), 20.0)?.ok_or("the cast's settings didn't arrive")?;
        eprintln!("raea play --cast: {} opened in {:.0} ms", tsx.display(), t.elapsed().as_secs_f64() * 1e3);
        Ok(Cast { app, ls: raea_runtime_react::scene::LiveScene::numbered_from(CAST_IDS), js, tsx, dir, input: input.map(Path::to_path_buf), warned: false, cost: (0.0, 0.0) })
    }

    /// The same cast opened again from its files (a saved edit): the new one replaces this one only if it opens.
    pub(crate) fn reopen(&self) -> R<Cast> {
        Cast::open(&self.tsx, self.input.as_deref())
    }

    pub(crate) fn tsx(&self) -> &Path {
        &self.tsx
    }

    /// Whether a shot's Program numbers any row at or past CAST_IDS (the cast's own ids would then collide with it):
    /// looked at once a cut, the spawns' ids only.
    pub(crate) fn clear_of(prog: &raea_core_program::ir::Program) -> Result<(), String> {
        for (kind, rows) in &prog.spawns {
            for r in rows {
                if let Some(raea_core_program::ir::V::S(id)) = r.get("id")
                    && let Ok(n) = id.trim_start_matches(|c: char| !c.is_ascii_digit()).parse::<usize>()
                    && n >= CAST_IDS
                {
                    return Err(format!("the shot's {kind} row {id} is numbered past the cast's first id ({CAST_IDS}): the cast would draw over it"));
                }
            }
        }
        Ok(())
    }

    /// The engine was reloaded or made again (another shot): every cast row is set into it afresh next step.
    pub(crate) fn reset(&mut self) {
        self.ls = raea_runtime_react::scene::LiveScene::numbered_from(CAST_IDS);
    }

    /// Frame `f` of the cast: React stepped at it, then what changed in its 3D set into `engine`.
    /// The frame goes to it as a render's does (a bare number: its input is the file it was bundled with, its dice the
    /// frame's own), so the cast is the same performance the capture renders.
    pub(crate) fn step(&mut self, f: i64, engine: &mut raea_render::engine::Engine) -> R<()> {
        let t0 = std::time::Instant::now();
        self.app.world().resource::<ReactOutbox>().send(json!({"t": "event", "name": "raea-frame", "value": f}));
        raea_runtime_react::scene::settle(&mut self.app, Some(f), 5.0)?;
        let t1 = std::time::Instant::now();
        let scene = raea_runtime_react::scene::scene_of(self.app.world_mut());
        raea_runtime_react::scene::edit_dir(&self.dir);
        let (mut rows, gone) = self.ls.frame(raea_runtime_react::scene::Shot { scene: &scene, clear: None });
        // the shot's camera (and its ambient, which the camera row carries), lights and post effects (resources) stay
        // the bake's: a cast made from its chapter computes them too, and sent they'd light the shot twice
        rows.retain(|t| !matches!(t.kind.as_str(), "camera" | "light" | "resource"));
        if !self.warned {
            // looked at once, on the first frame with rows (not rebuilt every frame)
            self.warned = !rows.is_empty();
            let other: std::collections::BTreeSet<String> = rows.iter().filter(|t| !["light", "mesh", "model", "panel", "decal"].contains(&t.kind.as_str())).map(|t| format!("{} {}", t.kind, t.id)).collect();
            if !other.is_empty() {
                eprintln!("raea play --cast: rows by the render's own ids, which a bake may name too: {}", other.into_iter().take(12).collect::<Vec<_>>().join(", "));
            }
        }
        if !gone.is_empty() {
            engine.remove_rows(&gone);
        }
        if !rows.is_empty() {
            engine.set_rows(&rows)?;
        }
        self.cost = ((t1 - t0).as_secs_f64() * 1e3, t1.elapsed().as_secs_f64() * 1e3);
        Ok(())
    }
}

impl Drop for Cast {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.js);
    }
}

impl Live {
    /// `--cast CAST.tsx`: live characters drawn into the shots' 3D (cast.rs). Only with `--shots`.
    pub fn add_cast(&mut self, tsx: &Path, input: Option<&Path>) -> R<()> {
        if self.shots.is_none() {
            return Err("--cast plays characters over a chapter's bakes: it needs --shots DIR".into());
        }
        self.cast = Some(Cast::open(tsx, input)?);
        // the frame on screen now gets its characters
        let f = self.frame;
        self.step_cast(f);
        Ok(())
    }

    /// The cast's file, when one plays (what --watch follows)
    pub fn cast_file(&self) -> Option<PathBuf> {
        self.cast.as_ref().map(|c| c.tsx().to_path_buf())
    }

    /// The cast opened again from its saved files, in place: the scenery, the frame and the engine kept. A cast that
    /// doesn't open leaves the old one playing.
    pub fn reload_cast(&mut self) -> R<()> {
        let Some(old) = self.cast.as_ref() else { return Ok(()) };
        let new = old.reopen()?;
        // the old cast's rows are dropped from the engine with it: the new one's are set afresh
        if let Some(e) = self.scene.as_mut() {
            let ids = old.ls.row_ids();
            if !ids.is_empty() {
                e.remove_rows(&ids);
            }
        }
        self.cast = Some(new);
        let f = self.frame;
        self.step_cast(f);
        self.dirty = true;
        Ok(())
    }

    /// The cast at frame `f`, into the engine the shot holds (an error is said, and the scenery plays on).
    pub(crate) fn step_cast(&mut self, f: i64) {
        let (Some(c), Some(e)) = (self.cast.as_mut(), self.scene.as_mut()) else { return };
        if let Err(why) = c.step(f, e) {
            eprintln!("raea play --cast: frame {f}: {why}");
        }
        if std::env::var_os("RAEA_LIVE_TRACE").is_some() && c.cost.0 + c.cost.1 > 10.0 {
            eprintln!("raea play --cast: frame {f}: JS {:.1} ms, rows {:.1} ms", c.cost.0, c.cost.1);
        }
    }

    /// The last cast step's JS and rows, ms
    pub fn cast_cost(&self) -> Option<(f64, f64)> {
        self.cast.as_ref().map(|c| c.cost)
    }
}
