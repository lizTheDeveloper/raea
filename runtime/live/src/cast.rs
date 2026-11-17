//! `raea play SCENERY.tsx --shots DIR --cast CAST.tsx`: game-time characters over a chapter's baked scenery (the live
//! player, premature-collapse design/engine-live-loop.md, Lane F). The scenery plays from its native-shot bakes with no
//! JS (shots.rs). The cast is a composition of its own, its characters only (no camera, set or lights: the shot has
//! them; its camera, lights and post effects are never sent), run frame by frame as live play runs a game: React steps it at the film's frame, its 3D rows are made from
//! that frame's own state (LiveScene, as a game's are) and set into the shot's engine beside the baked Program. Its
//! rows are numbered clear of the Program's (LiveScene::numbered_from) and its camera row is never sent, so the shot's
//! camera and ambient stay. Entering another shot reloads the engine, and the whole cast is set into it again.
//! The cast's page (its 2D) isn't drawn: the bakes' page is.

use super::*;
use raea_core_program::ir::V as Value3;
use std::collections::BTreeMap;

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
    /// the cast's row ids looked at against the shot's Program since the engine was (re)made, and those of them the
    /// Program draws itself (its 3D text's glyphs, a part outside the chapter's layer tags): passed by, the bake has
    /// them. Each id is looked up once, when it first comes (no per-frame key made)
    seen: std::collections::HashSet<String>,
    shadowed: std::collections::HashSet<String>,
    /// the director's half live (on unless `--no-direct`): the cast's camera (its place and lens) and its named lights
    /// laid over the bake's own (Engine::set_over), so an edit to the chapter's camera, cut timing or lights shows while
    /// the bake plays; the bake's ambient, sky and grade stay. With the chapter unedited they're the bake's values.
    direct: bool,
    /// the camera's place and lens as the cast last computed them, and each named light's values (by its name)
    cam_over: BTreeMap<String, Value3>,
    light_over: std::collections::HashMap<String, BTreeMap<String, Value3>>,
    /// the cast's light ids' names, and each name's row in the shot (None: the bake has no light of that name)
    light_names: std::collections::HashMap<String, String>,
    light_rows: std::collections::HashMap<String, Option<String>>,
    /// whether the engine has this shot's laid values yet (a load forgets them: sent whole after each cut)
    over_sent: bool,
    /// the last step's JS and rows, ms (what a frame of live characters costs)
    pub(crate) cost: (f64, f64),
}

impl Cast {
    /// `input`: the input file the scenery was baked with (a render's --input: a story's choices), so the cast's story
    /// state is the bake's
    pub(crate) fn open(tsx: &Path, input: Option<&Path>, direct: bool) -> R<Cast> {
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
        Ok(Cast { app, ls: raea_runtime_react::scene::LiveScene::numbered_from(CAST_IDS), js, tsx, dir, input: input.map(Path::to_path_buf), warned: false, seen: Default::default(), shadowed: Default::default(), direct, cam_over: BTreeMap::new(), light_over: Default::default(), light_names: Default::default(), light_rows: Default::default(), over_sent: false, cost: (0.0, 0.0) })
    }

    /// The same cast opened again from its files (a saved edit): the new one replaces this one only if it opens.
    pub(crate) fn reopen(&self) -> R<Cast> {
        Cast::open(&self.tsx, self.input.as_deref(), self.direct)
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
        self.seen.clear();
        self.shadowed.clear();
        self.cut();
    }

    /// The engine loaded another shot (kept or not): the laid-over values (set_over) are gone from it, and its rows by
    /// name are another shot's: sent whole and looked up again at the next step.
    pub(crate) fn cut(&mut self) {
        self.over_sent = false;
        self.light_rows.clear();
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
        let (mut rows, mut gone) = self.ls.frame(raea_runtime_react::scene::Shot { scene: &scene, clear: None });
        // direct: the camera's place and lens and the named lights, kept to lay over the bake's (after the rows below)
        let mut changed = !self.over_sent;
        if self.direct {
            for t in &rows {
                match t.kind.as_str() {
                    "camera" if t.id == "cam" => {
                        for (k, v) in t.vals.iter().filter(|(k, _)| cam_direct(k)) {
                            self.cam_over.insert(k.clone(), v.clone());
                            changed = true;
                        }
                    }
                    "light" => {
                        if let Some(Value3::S(n)) = t.vals.get("name") {
                            self.light_names.insert(t.id.clone(), n.to_string());
                        }
                        if let Some(n) = self.light_names.get(&t.id) {
                            let o = self.light_over.entry(n.clone()).or_default();
                            for (k, v) in t.vals.iter().filter(|(k, _)| !matches!(k.as_str(), "id" | "name")) {
                                o.insert(k.clone(), v.clone());
                            }
                            changed = true;
                        }
                    }
                    _ => {}
                }
            }
        }
        // the shot's camera (and its ambient, which the camera row carries), lights and post effects (resources) stay
        // the bake's rows: a cast made from its chapter computes them too, and sent as rows they'd light the shot twice
        rows.retain(|t| !matches!(t.kind.as_str(), "camera" | "light" | "resource"));
        if !self.warned {
            // looked at once, on the first frame with rows (not rebuilt every frame)
            self.warned = !rows.is_empty();
            let other: std::collections::BTreeSet<String> = rows.iter().filter(|t| !["light", "mesh", "model", "panel", "decal"].contains(&t.kind.as_str())).map(|t| format!("{} {}", t.kind, t.id)).collect();
            if !other.is_empty() {
                eprintln!("raea play --cast: rows by the render's own ids, which a bake may name too: {}", other.into_iter().take(12).collect::<Vec<_>>().join(", "));
            }
        }
        // a row the shot's Program holds too (by the render's own ids) is the Program's: it follows the bake's timeline,
        // and the cast passes it by (set_rows_over refuses it)
        let before = self.shadowed.len();
        for t in &rows {
            if !self.seen.contains(t.id.as_str()) {
                self.seen.insert(t.id.clone());
                if engine.timeline_row(&t.kind, &t.id) {
                    self.shadowed.insert(t.id.clone());
                }
            }
        }
        if self.shadowed.len() > before {
            eprintln!("raea play --cast: frame {f}: {} of the cast's rows are the shot's own (the bake draws them): passed by", self.shadowed.len() - before);
        }
        if !self.shadowed.is_empty() {
            rows.retain(|t| !self.shadowed.contains(t.id.as_str()));
            gone.retain(|(_, id)| !self.shadowed.contains(id.as_str()));
        }
        if !gone.is_empty() {
            engine.remove_rows(&gone);
        }
        if !rows.is_empty() {
            // over the shot's timeline: its camera, cuts and keyed parts keep moving under the cast (set_rows took every
            // row over at the clock, and the shot's 3D froze at its first frame with the cast on)
            engine.set_rows_over(&rows)?;
            // the textures the cast's rows started loading, loaded before the frame draws (Engine::await_new_loads)
            engine.await_new_loads()?;
        }
        if self.direct && changed {
            self.lay_over(engine)?;
        }
        self.cost = ((t1 - t0).as_secs_f64() * 1e3, t1.elapsed().as_secs_f64() * 1e3);
        Ok(())
    }
}

impl Cast {
    /// The camera's place and lens and the named lights laid over the shot's own rows (Engine::set_over). A light the
    /// bake has no row of that name for is said once a shot (an unnamed light can't be matched: name it in the chapter).
    fn lay_over(&mut self, engine: &mut raea_render::engine::Engine) -> R<()> {
        if !self.cam_over.is_empty() {
            engine.set_over("camera", "cam", &self.cam_over)?;
        }
        for (name, vals) in &self.light_over {
            let row = self.light_rows.entry(name.clone()).or_insert_with(|| {
                let r = engine.named_row("light", name);
                if r.is_none() {
                    eprintln!("raea play --cast: the shot has no light named {name}: the cast's isn't laid over");
                }
                r
            });
            if let Some(id) = row {
                engine.set_over("light", id, vals)?;
            }
        }
        self.over_sent = true;
        Ok(())
    }
}

/// The camera columns the cast lays over the bake's (direct): its place, aim and lens, the shot it's on (a cut). Not
/// its ambient, sky, environment or post effects, which the scenery half sets (the cast half lacks the set's).
fn cam_direct(k: &str) -> bool {
    matches!(k, "x" | "y" | "z" | "look_x" | "look_y" | "look_z" | "fov" | "near" | "far" | "focus" | "ortho" | "shot") || ["up_", "lens_", "view_"].iter().any(|p| k.starts_with(p))
}

impl Drop for Cast {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.js);
    }
}

impl Live {
    /// `--cast CAST.tsx`: live characters drawn into the shots' 3D (cast.rs). Only with `--shots`.
    /// `direct`: the cast's camera and named lights laid over the bake's (Cast::direct; `--no-direct` turns it off).
    pub fn add_cast(&mut self, tsx: &Path, input: Option<&Path>, direct: bool) -> R<()> {
        if self.shots.is_none() {
            return Err("--cast plays characters over a chapter's bakes: it needs --shots DIR".into());
        }
        self.cast = Some(Cast::open(tsx, input, direct)?);
        // the frame on screen now gets its characters
        let f = self.frame;
        self.step_cast(f);
        // the open's transients (the bundle, its first settle, the first rows) handed back (shots.rs trim)
        super::shots::trim();
        if let Some(e) = self.scene.as_ref() {
            e.asset_census("cast set");
        }
        eprintln!("raea play --cast: after its open, {}", super::shots::mem());
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
            let ids: Vec<(String, String)> = old.ls.row_ids().into_iter().filter(|(_, id)| !old.shadowed.contains(id.as_str())).collect();
            if !ids.is_empty() {
                e.remove_rows(&ids);
            }
        }
        self.cast = Some(new);
        let f = self.frame;
        self.step_cast(f);
        super::shots::trim();
        self.dirty = true;
        Ok(())
    }

    /// The cast at frame `f`, into the engine the shot holds (an error is said, and the scenery plays on).
    pub(crate) fn step_cast(&mut self, f: i64) {
        let (Some(c), Some(e)) = (self.cast.as_mut(), self.scene.as_mut()) else { return };
        if let Err(why) = c.step(f, e) {
            eprintln!("raea play --cast: frame {f}: {why}");
        }
        if crate::live_trace() && c.cost.0 + c.cost.1 > 10.0 {
            eprintln!("raea play --cast: frame {f}: JS {:.1} ms, rows {:.1} ms", c.cost.0, c.cost.1);
        }
    }

    /// The last cast step's JS and rows, ms
    pub fn cast_cost(&self) -> Option<(f64, f64)> {
        self.cast.as_ref().map(|c| c.cost)
    }
}
