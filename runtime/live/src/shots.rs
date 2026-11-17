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
    /// each bake's frames (first, last) and file, by first frame
    files: Vec<(i64, i64, PathBuf)>,
    /// the shots read in (their commits are in the timeline, their Program here); at most the one playing and its
    /// neighbours: a chapter's bakes parsed whole would be tens of GB
    progs: std::collections::HashMap<usize, Arc<raea_core_program::ir::Program>>,
    /// the next shot, being read on a thread, and prepared there for the engine (its rows, timeline and decoded maps:
    /// raea_render::engine::Prepared, so the cut only installs it)
    ahead: Option<(usize, std::sync::mpsc::Receiver<R<Ahead>>)>,
    /// the next shot as prepared ahead, until the cut takes it
    prepared: Option<(usize, raea_render::engine::Prepared)>,
    /// the shot the engine holds now
    at: Option<usize>,
}

/// A shot read (and prepared) on the read-ahead thread
pub(crate) struct Ahead {
    ops: Vec<(i64, Vec<String>)>,
    prog: Option<Arc<raea_core_program::ir::Program>>,
    prepared: Option<raea_render::engine::Prepared>,
}

/// A bake's first stepped frame, from the start of its JSON (the ops follow the assets), without reading it all
fn first_frame(p: &Path) -> Option<i64> {
    use std::io::Read;
    let f = std::fs::File::open(p).ok()?;
    let mut d = zstd::stream::read::Decoder::new(f).ok()?;
    let mut buf = vec![0u8; 1 << 20];
    let mut n = 0;
    while n < buf.len() {
        let k = d.read(&mut buf[n..]).ok()?;
        if k == 0 {
            break;
        }
        n += k;
        if let Some(i) = buf[..n].windows(8).position(|w| w == b"\"ops\":[[") {
            let rest = &buf[i + 8..n];
            let end = rest.iter().position(|c| !c.is_ascii_digit() && *c != b'-')?;
            return std::str::from_utf8(&rest[..end]).ok()?.parse().ok();
        }
    }
    None
}

impl Live {
    pub fn open_shots(tsx: &Path, shots: &Path) -> R<Live> {
        warm_audio();
        raea_runtime_react::interactive_thread();
        let tsx = tsx.canonicalize().map_err(|e| format!("{}: {e}", tsx.display()))?;
        let dir = tsx.parent().unwrap().to_path_buf();
        *raea_runtime_react::native::ASSET_DIR.lock().unwrap() = Some(dir.clone());
        let t0 = std::time::Instant::now();
        let mut files: Vec<(i64, PathBuf)> = std::fs::read_dir(shots).map_err(|e| format!("{}: {e}", shots.display()))?.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.to_string_lossy().ends_with(".json.zst")).filter_map(|p| first_frame(&p).map(|f| (f, p))).collect();
        if files.is_empty() {
            return Err(format!("no shot bakes in {}", shots.display()));
        }
        files.sort_by_key(|f| f.0);
        // the first one read now: the composition's size, rate and length, and the frames its commits begin
        let first = raea_runtime_export::read_shot(&files[0].1, &dir)?;
        let (w, h, fps, total) = (first.w, first.h, first.fps, first.total);
        // the header is checked before anything is sized by it (a crafted bake's `total` would size the timeline)
        header_ok(w, h, fps, total, first.ops.iter().map(|o| o.0).max().unwrap_or(0)).map_err(|e| format!("{}: {e}", files[0].1.display()))?;
        // each bake runs to the frame before the next one's first (the last: its own last)
        let last0 = first.ops.last().map_or(files[0].0, |o| o.0);
        let spans: Vec<(i64, i64, PathBuf)> = files.iter().enumerate().map(|(i, (lo, p))| (*lo, files.get(i + 1).map_or(if i == 0 { last0 } else { lo + (last0 - files[0].0) }, |n| n.0 - 1), p.clone())).collect();
        // a jump into a shot needs no keyframe of its own: its first frame is a RESET and its whole tree
        let mut keys: Vec<(i64, Vec<Value>)> = vec![(-1, Vec::new())];
        keys.extend(spans.iter().map(|s| (s.0 - 1, Vec::new())));
        keys.sort_by_key(|k| k.0);
        keys.dedup_by_key(|k| k.0);
        let mut conf = serde_json::Map::new();
        for (k, v) in [("width", w as f64), ("height", h as f64), ("fps", fps), ("frames", total as f64)] {
            conf.insert(k.into(), serde_json::json!(v));
        }
        let baked = raea_runtime_bake::Baked { conf, fps, frames: total, w, h, init: Vec::new(), frame_ops: vec![Vec::new(); total.max(1) as usize], keys, comps: Vec::new(), code: Default::default(), has_3d: true };
        eprintln!("raea play --shots: {} shots, frames {}-{} of {total}, opened in {:.0} ms", spans.len(), spans[0].0, spans[spans.len() - 1].1, t0.elapsed().as_secs_f64() * 1e3);
        // the brand and the page's stylesheets come from the composition's bundle (made, never run)
        let js = std::env::temp_dir().join(format!("raea-live-{}-{:x}.js", std::process::id(), rand_id()));
        raea_runtime_react::scene::bundle_composition(&tsx, &js, &crate::open::baked_opts(&dir)?, None)?;
        let mut app = App::new();
        app.add_plugins(MinimalPlugins).add_plugins(raea_runtime_react::ReplayPlugin);
        raea_runtime_react::scene::one_thread(&mut app);
        let page = raea_runtime_page::BevyPage::new(w, h, &dir)?;
        let brand = raea_runtime_react::scene::page_css(&dir, w, &js);
        let start = spans[0].0;
        let mut live = Live { app, dir: dir.clone(), page, brand, dirty: true, mixer: None, mixer_failed: false, lag: (0.0, std::time::Instant::now()), w, h, fps, frames: total, responsive: false, js, master: None, master_wav: None, game_master: None, game_gain: None, scene_rx: None, scene: None, live3d: None, frame: 0, pads: Vec::new(), pointer: Default::default(), made: Default::default(), sound_buf: Vec::new(), holders: Vec::new(), baked: Some(Arc::new(baked)), applied: -1, session: new_session(), shots: Some(Shots { files: spans, progs: Default::default(), ahead: None, prepared: None, at: None }), cast: None };
        live.take_shot(0, first.ops, first.prog.map(Arc::new));
        trim();
        live.frame = start;
        live.replay(start);
        live.draw()?;
        live.warm_sound();
        Ok(live)
    }

    /// The first frame the bakes cover (a chapter's player starts there)
    pub fn shots_first(&self) -> Option<i64> {
        self.shots.as_ref().map(|s| s.files[0].0)
    }

    /// A shot read in: its commits laid into the timeline, its Program kept; every other shot but the next let go (a
    /// Program holds its shot's every frame: about 42 MB a frame held on Ch5, Lane C; a bake is ~2 MB to read again).
    fn take_shot(&mut self, i: usize, ops: Vec<(i64, Vec<String>)>, prog: Option<Arc<raea_core_program::ir::Program>>) {
        let (Some(s), Some(baked)) = (self.shots.as_mut(), self.baked.as_mut()) else { return };
        let Some(t) = Arc::get_mut(baked) else { return };
        for (f, batches) in ops {
            if f >= 0 && (f as usize) < t.frame_ops.len() {
                t.frame_ops[f as usize] = batches;
            }
        }
        if let Some(p) = prog {
            s.progs.insert(i, p);
        }
        if s.prepared.as_ref().is_some_and(|(k, _)| *k != i && *k != i + 1) {
            s.prepared = None;
        }
        let gone: Vec<usize> = s.progs.keys().copied().filter(|&k| k != i && k != i + 1).collect();
        for k in gone {
            s.progs.remove(&k);
            let (lo, hi, _) = s.files[k];
            for f in lo.max(0)..=hi.min(t.frame_ops.len() as i64 - 1) {
                t.frame_ops[f as usize] = Vec::new();
            }
        }
    }

    /// Frame `f`'s shot read in (waited for if it's being read ahead), the next one started on a thread, and the 3D
    /// engine made to hold it. Called before a frame is replayed and drawn.
    pub(crate) fn follow_shot(&mut self, f: i64) {
        let Some(s) = self.shots.as_ref() else { return };
        let Some(i) = s.files.iter().position(|(lo, hi, _)| f >= *lo && f <= *hi) else { return };
        if !s.progs.contains_key(&i) {
            let t = std::time::Instant::now();
            let ahead = self.shots.as_mut().and_then(|s| s.ahead.take());
            let read = match ahead {
                Some((k, rx)) if k == i => rx.recv().map_err(|e| e.to_string()).and_then(|r| r),
                _ => {
                    let p = self.shots.as_ref().unwrap().files[i].2.clone();
                    raea_runtime_export::read_shot(&p, &self.dir).map(|b| Ahead { ops: b.ops, prog: b.prog.map(Arc::new), prepared: None })
                }
            };
            match read {
                Ok(a) => {
                    if let (Some(p), Some(s)) = (a.prepared, self.shots.as_mut()) {
                        s.prepared = Some((i, p));
                    }
                    self.take_shot(i, a.ops, a.prog)
                }
                Err(e) => eprintln!("raea play --shots: shot {i}: {e}"),
            }
            trim();
            raea_io_trace::event("shot_read", serde_json::json!({"shot": i, "waited_ms": t.elapsed().as_secs_f64() * 1e3}));
            eprintln!("raea play --shots: shot {i} read (waited {:.0} ms), {}", t.elapsed().as_secs_f64() * 1e3, mem());
        }
        // the next shot read ahead while this one plays, and prepared there for the engine (Engine::Prepared: the maps
        // it holds now aren't decoded again)
        let dir = self.dir.clone();
        let have = self.scene.as_ref().map(|e| e.map_sources()).unwrap_or_default();
        if let Some(s) = self.shots.as_mut()
            && s.ahead.is_none()
            && i + 1 < s.files.len()
            && !s.progs.contains_key(&(i + 1))
        {
            let p = s.files[i + 1].2.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            let ahead = move || -> R<Ahead> {
                let b = raea_runtime_export::read_shot(&p, &dir)?;
                let prog = b.prog.map(Arc::new);
                let prepared = prog.clone().and_then(|g| match raea_render::engine::Prepared::new(g, &dir, &have) {
                    Ok(p) => Some(p),
                    Err(e) => {
                        eprintln!("raea play --shots: the next shot wasn't prepared ahead ({e}); it reloads at the cut");
                        None
                    }
                });
                Ok(Ahead { ops: b.ops, prog, prepared })
            };
            if std::thread::Builder::new().name("raea shot ahead".into()).spawn(move || { let _ = tx.send(ahead()); }).is_ok() {
                s.ahead = Some((i + 1, rx));
            }
        }
        let Some(s) = self.shots.as_mut() else { return };
        if s.at == Some(i) {
            return;
        }
        let Some(prog) = s.progs.get(&i).cloned() else { return };
        let prepared = match s.prepared.take() {
            Some((k, p)) if k == i => Some(p),
            other => {
                s.prepared = other;
                None
            }
        };
        let t = std::time::Instant::now();
        let was_prepared = prepared.is_some();
        // the cut: the shot prepared ahead installed (switch: Engine::reload_prepared), else reloaded from its shared
        // Program (reload_arc: no copy)
        // with a cast, its rows are kept across the cut (reload_*_keeping: its entities, meshes and models stay, only the
        // Program's rows are reloaded), so the cut doesn't set the whole cast in again. Refused (a kept row colliding with
        // the new shot's, or text): said, and a plain reload with the cast set in afresh
        let casting = self.cast.is_some();
        let inserted_np = prepared.is_none().then(|| raea_render::inserted_of(&prog));
        let keep = casting
            && match (self.scene.as_ref(), prepared.as_ref().map(|p| p.inserted.as_slice()).or(inserted_np.as_deref())) {
                (Some(e), Some(ins)) => match e.keepable(ins) {
                    Ok(()) => true,
                    Err(why) => {
                        eprintln!("raea play --cast: shot {i}: the cast isn't kept across this cut ({why}): set in again");
                        false
                    }
                },
                _ => false,
            };
        let cast_kept = keep;
        let reloaded = match (self.scene.as_mut(), prepared, inserted_np) {
            (Some(e), Some(p), _) => Some(if keep { e.reload_prepared_keeping(p) } else { e.reload_prepared(p) }),
            (Some(e), None, Some(ins)) => Some(if keep { e.reload_arc_keeping(&prog, &ins) } else { e.reload_arc(&prog, &ins) }),
            _ => None,
        };
        let kept = match reloaded {
            Some(r) => match r {
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
            let inserted = raea_render::inserted_of(&prog);
            // the shot's Program shared with the engine, not copied (mem2: Engine::new_live_arc)
            match raea_render::engine::Engine::new_live_arc(prog.clone(), &inserted, &self.dir) {
                Ok(e) => self.scene = Some(e),
                Err(e) => eprintln!("raea play --shots: shot {i}'s 3D didn't open: {e}"),
            }
        }
        if let Some(s) = self.shots.as_mut() {
            s.at = Some(i);
        }
        // the engine holds only the new shot's rows now (unless the cast was kept): the cast is set into it whole at its
        // next step
        if let Some(c) = self.cast.as_mut() {
            if !(kept && cast_kept) {
                c.reset();
            } else {
                // kept: its rows stay, but the values it laid over the last shot's camera and lights went with it
                c.cut();
            }
            // a shot whose rows reach the cast's ids: the cast is stopped, loudly (never drawn colliding)
            if let Err(why) = super::cast::Cast::clear_of(&prog) {
                eprintln!("raea play --cast: {why}; the cast is off from here");
                self.cast = None;
            }
        }
        trim();
        raea_io_trace::event("shot", serde_json::json!({"shot": i, "ms": t.elapsed().as_secs_f64() * 1e3, "reloaded": kept, "prepared": was_prepared}));
        eprintln!("raea play --shots: shot {i} (from frame {}) in {:.0} ms ({}, {})", self.shots.as_ref().map_or(0, |s| s.files[i].0), t.elapsed().as_secs_f64() * 1e3, if kept && was_prepared { "engine reloaded, prepared ahead" } else if kept { "engine reloaded" } else { "engine opened" }, mem());
    }
}

/// The most frames a chapter's timeline is made for (about 3.8 days at 30 fps): a timeline is a Vec per frame
const MAX_FRAMES: i64 = 10_000_000;

/// A bake header's size, rate and length, refused unless sane: w and h in 1..=16384, fps finite in (0, 1000], total
/// in 1..=MAX_FRAMES and past the last frame its commits name (read_shot turns a NaN total into 0 and an infinite one
/// into i64::MAX, both refused here).
fn header_ok(w: u32, h: u32, fps: f64, total: i64, last_op: i64) -> R<()> {
    if !(1..=16384).contains(&w) || !(1..=16384).contains(&h) {
        return Err(format!("the bake's size {w}x{h} is out of range (1..=16384)"));
    }
    if !fps.is_finite() || fps <= 0.0 || fps > 1000.0 {
        return Err(format!("the bake's rate {fps} is out of range (0..=1000 fps)"));
    }
    if !(1..=MAX_FRAMES).contains(&total) {
        return Err(format!("the bake's length {total} frames is out of range (1..={MAX_FRAMES})"));
    }
    if last_op < 0 || last_op >= total {
        return Err(format!("the bake's commits reach frame {last_op}, outside its {total} frames"));
    }
    Ok(())
}

/// The process's memory now: resident (Linux) or physical footprint (macOS)
pub(crate) fn mem() -> String {
    let rss = std::fs::read_to_string("/proc/self/status").ok().and_then(|t| t.lines().find(|l| l.starts_with("VmRSS:")).and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok()));
    match rss {
        Some(kb) => format!("RSS {:.2} GB", kb as f64 / 1048576.0),
        None => format!("footprint {:.2} GB", raea_io_trace::footprint_mb() / 1024.0),
    }
}

/// Freed memory handed back to the system. glibc keeps it in its arenas: a parsed bake's JSON tree (several GB) stays
/// resident after it's dropped. macOS's allocator keeps it too, as freed large blocks still dirty in the footprint
/// (vmmap "MALLOC_LARGE (empty)"): with --cast, 2.6 GB of a 7.1 GB footprint on Ch5 (M3 Max, 2026-11-17), the cast's
/// open and each cut's transients. Called after a shot is read or switched and after the cast opens (not per frame:
/// it walks the zones).
pub(crate) fn trim() {
    #[cfg(target_os = "linux")]
    {
        unsafe extern "C" {
            fn malloc_trim(pad: usize) -> i32;
        }
        unsafe { malloc_trim(0) };
    }
    #[cfg(target_os = "macos")]
    {
        unsafe extern "C" {
            fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
        }
        // every zone (null), as much as it can (goal 0)
        unsafe { malloc_zone_pressure_relief(std::ptr::null_mut(), 0) };
    }
}

fn raea_export_read(p: &Path, dir: &Path) -> R<raea_runtime_export::ShotBake> {
    raea_runtime_export::read_shot(p, dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sane_header_passes() {
        assert!(header_ok(1920, 1080, 30.0, 6000, 4050).is_ok());
        assert!(header_ok(1, 1, 1000.0, MAX_FRAMES, MAX_FRAMES - 1).is_ok());
    }

    #[test]
    fn a_size_out_of_range_is_refused() {
        for (w, h) in [(0, 1080), (1920, 0), (16385, 1080), (1920, 16385), (u32::MAX, u32::MAX)] {
            assert!(header_ok(w, h, 30.0, 6000, 10).is_err(), "{w}x{h}");
        }
    }

    #[test]
    fn a_rate_out_of_range_is_refused() {
        for fps in [0.0, -30.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 1000.5] {
            assert!(header_ok(1920, 1080, fps, 6000, 10).is_err(), "{fps}");
        }
    }

    #[test]
    fn a_length_out_of_range_is_refused() {
        // read_shot's `as i64`: NaN -> 0, +inf -> i64::MAX, -inf -> i64::MIN
        for total in [0, -1, i64::MIN, i64::MAX, MAX_FRAMES + 1] {
            assert!(header_ok(1920, 1080, 30.0, total, 0).is_err(), "{total}");
        }
        assert_eq!(f64::NAN as i64, 0);
        assert_eq!(f64::INFINITY as i64, i64::MAX);
    }

    #[test]
    fn commits_outside_the_length_are_refused() {
        assert!(header_ok(1920, 1080, 30.0, 6000, 6000).is_err());
        assert!(header_ok(1920, 1080, 30.0, 6000, -1).is_err());
    }
}
