//! Native shots (docs/native-shots.md): a shot's React run once, its frames split across runtimes on every core; its
//! 3D folded into a Program over the shot's own frames (not from frame 0); its page kept as the recorded commits. The
//! frames are then drawn with no JavaScript at all: the commits replayed into the ECS page (ReplayPlugin, as a baked
//! timeline plays) and the 3D drawn from the Program. The bake is kept on disk by what made it, so a re-render of the
//! same shot skips it. `raea render ... --at A,B,... -o DIR/ --native` (or RAEA_NATIVE=1).

use super::*;
use raea_core_program::ir::{Program, V};

/// `render --native`: stills drawn from a shot bake, React never running in the draw.
pub(crate) static NATIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// A shot baked: the composition's size and rate, each stepped frame's commits (the first frame of each runtime's
/// piece a RESET and that runtime's whole tree, so a piece replays on its own), and the 3D as a Program keyed at the
/// shot's own frames.
pub(crate) struct Shot {
    w: u32,
    h: u32,
    fps: f64,
    total: i64,
    ops: Vec<(i64, Vec<String>)>,
    prog: Option<Program>,
}

/// One runtime's piece of a shot: each frame's commits, scene and clear.
type Piece = Vec<(i64, Vec<String>, Vec<Node3>, Option<String>)>;

fn t_ms(t: std::time::Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}

/// RAEA_NATIVE_TRACE: a drawn frame's time by part (ms summed): the page synced and painted, the 3D drawn, its target read back
static PARTS: std::sync::Mutex<[f64; 3]> = std::sync::Mutex::new([0.0; 3]);

pub(crate) fn part(k: usize, t: std::time::Instant) {
    if trace() {
        PARTS.lock().unwrap()[k] += t_ms(t);
    }
}

fn trace() -> bool {
    std::env::var_os("RAEA_NATIVE_TRACE").is_some()
}

/// Memory the system has free for new work (MemAvailable), in GB; None where it can't be read.
fn mem_available_gb() -> Option<f64> {
    let t = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kb: f64 = t.lines().find(|l| l.starts_with("MemAvailable:"))?.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb / 1048576.0)
}

/// How many runtimes a shot's bake steps on: RAEA_BAKE_WORKERS, else one a core (at most 32) and one per 1 GB of free
/// memory (a chapter's runtime held about 0.45 GB: Ch5 @2400, 11 runtimes, 7.7 GB peak against 2.7 drawing), each
/// with at least RAEA_BAKE_MIN frames (6: a runtime's opening and its whole first tree are paid once per piece).
fn workers_for(frames: usize) -> usize {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let by_mem = mem_available_gb().map_or(usize::MAX, |g| (g / 1.0).floor().max(1.0) as usize);
    let want = std::env::var("RAEA_BAKE_WORKERS").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(cores.min(32).min(by_mem)).clamp(1, 64);
    let min = std::env::var("RAEA_BAKE_MIN").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(6).max(1);
    want.min(frames.div_ceil(min)).max(1)
}

/// The frames the shot steps: each wanted frame and its neighbours (what reads a frame either side), as scene_frames
/// steps a still's.
fn stepped_of(wanted: &[i64]) -> Vec<i64> {
    let s: std::collections::BTreeSet<i64> = wanted.iter().filter(|f| **f >= 0).flat_map(|&f| f.saturating_sub(1).max(0)..=f + 1).collect();
    s.into_iter().collect()
}

/// The shot's frames stepped across runtimes on threads: the runs of stepped frames cut into pieces of about equal
/// length, one runtime each, its commits tapped. None when a scene carries GPU state from frame to frame (a stateful
/// <Compute>: every frame before must be stepped, so the shot isn't native).
fn bake(js: &Path, dir: &Path, stepped: &[i64]) -> R<Option<(Map<String, Value>, Piece)>> {
    let t0 = std::time::Instant::now();
    let workers = workers_for(stepped.len());
    let per = stepped.len().div_ceil(workers);
    let mut pieces: Vec<(i64, i64)> = Vec::new();
    for (a, b) in raea_runtime_bake::runs(stepped) {
        let mut s = a;
        while s < b {
            let e = (s + per as i64).min(b);
            pieces.push((s, e));
            s = e;
        }
    }
    let threads: Vec<std::thread::JoinHandle<R<(Map<String, Value>, Piece)>>> = pieces
        .iter()
        .enumerate()
        .map(|(k, &(a, b))| {
            let off = (k as u64 + 1) << 36;
            let (js, dir) = (js.to_path_buf(), dir.to_path_buf());
            std::thread::Builder::new()
                .name("raea shot bake".into())
                .spawn(move || {
                    edit_dir(&dir);
                    let t = std::time::Instant::now();
                    let mut app = App::new();
                    app.add_plugins(MinimalPlugins).add_plugins(ReactPlugin::new(js.clone()));
                    app.insert_resource(raea_runtime_react::CommitTap::default()).insert_resource(raea_runtime_react::OneBatch::default());
                    let conf = settle(&mut app, Some(-1), 20.0)?.ok_or("the composition's settings didn't arrive")?;
                    let opened = t_ms(t);
                    std::mem::take(&mut app.world_mut().resource_mut::<raea_runtime_react::CommitTap>().0);
                    let mut out: Piece = Vec::with_capacity((b - a) as usize);
                    let mut record = |app: &mut App, f: i64| -> R<()> {
                        let world = app.world_mut();
                        let scene = scene_of(world);
                        let clear = scene_clear(world);
                        let tapped = std::mem::take(&mut world.resource_mut::<raea_runtime_react::CommitTap>().0);
                        // RAEA_NATIVE_SCENES=DIR: each stepped frame's 3D parts in order (kind, identity, place), by the
                        // frame and the piece it was stepped in, to compare runtimes
                        if let Some(d) = std::env::var_os("RAEA_NATIVE_SCENES").map(PathBuf::from) {
                            let _ = std::fs::create_dir_all(&d);
                            let parts: Vec<Value> = raea_runtime_scene::items_of(&scene).iter().map(|it| { let t = it.world.w_axis; json!([it.kind, raea_core_scene::base_ident(it), [t.x, t.y, t.z]]) }).collect();
                            let _ = std::fs::write(d.join(format!("{f}-{a}.json")), Value::Array(parts).to_string());
                        }
                        // a piece's first frame: what's there goes, then this runtime's whole tree (its node ids are its own)
                        let ops = if f == a { vec![raea_runtime_bake::RESET.to_string(), Value::Array(raea_runtime_react::snapshot_ops(world)).to_string()] } else { tapped };
                        // each piece's node ids in a range of their own: another runtime numbers its tree its own way,
                        // and the page kept what it knew of an id across the RESET (Ch2 @14230: the layered 3D's
                        // holder painted over it from the piece that renumbered it on)
                        let ops = ops.into_iter().map(|j| if j == raea_runtime_bake::RESET { j } else { ids_moved(&j, off) }).collect();
                        out.push((f, ops, scene, clear));
                        Ok(())
                    };
                    let ld = |x: &std::sync::atomic::AtomicU64| x.load(std::sync::atomic::Ordering::Relaxed);
                    let (w0, to0) = (ld(&raea_runtime_react::FLUSH_WAIT_NS), ld(&raea_runtime_react::FLUSH_TIMEOUTS));
                    // RAEA_NATIVE_STEP=range: the frames asked for as one range (raea-frames), the next set off by the
                    // frame's own effect; by default each its own raea-frame event, as a render asks for a frame
                    if std::env::var("RAEA_NATIVE_STEP").as_deref() == Ok("range") {
                        raea_runtime_bake::step_frames(&mut app, a, b, |app, f| record(app, f))?;
                    } else {
                        for f in a..b {
                            app.world().resource::<ReactOutbox>().send(json!({"t": "event", "name": "raea-frame", "value": f}));
                            settle(&mut app, Some(f), 20.0)?;
                            record(&mut app, f)?;
                        }
                    }
                    if trace() {
                        eprintln!("raea native: piece {a}..{b}: runtime open {opened:.0} ms, {} frames stepped in {:.0} ms (flush waits {:.0} ms, {} timed out, every runtime)", b - a, t_ms(t) - opened, (ld(&raea_runtime_react::FLUSH_WAIT_NS) - w0) as f64 / 1e6, ld(&raea_runtime_react::FLUSH_TIMEOUTS) - to0);
                    }
                    Ok((conf, out))
                })
                .map_err(|e| e.to_string())
        })
        .collect::<Result<_, _>>()?;
    let mut conf = None;
    let mut all: Piece = Vec::with_capacity(stepped.len());
    for t in threads {
        let (c, p) = t.join().map_err(|_| "a shot bake runtime panicked".to_string())??;
        conf.get_or_insert(c);
        all.extend(p);
    }
    all.sort_by_key(|p| p.0);
    if trace() {
        eprintln!("raea native: {} frames baked on {} runtimes in {:.0} ms", all.len(), pieces.len(), t_ms(t0));
    }
    if all.iter().any(|p| raea_runtime_scene::carries_state(&p.2)) {
        return Ok(None);
    }
    Ok(conf.map(|c| (c, all)))
}

/// A commit's node ids moved up by `off` (the root's, 0, stays): a piece's ids kept apart from every other piece's.
fn ids_moved(json: &str, off: u64) -> String {
    let Ok(mut ops) = serde_json::from_str::<Vec<Value>>(json) else { return json.to_string() };
    for op in ops.iter_mut().filter_map(Value::as_object_mut) {
        for k in ["id", "parent", "child", "before"] {
            if let Some(n) = op.get(k).and_then(Value::as_u64).filter(|n| *n != raea_runtime_react::ROOT_ID) {
                op.insert(k.into(), json!(n + off));
            }
        }
    }
    Value::Array(ops).to_string()
}

/// The shot's 3D as a Program over its own frames: folded over `lo..` (the stepped frames, a gap held at the frame
/// before it, as scene_frames holds one) and then moved to `lo`, so its keys sit at the frames they were stepped at
/// (the engine's clock, time uniforms and plugin clock stay the composition's own) and nothing before the shot is folded.
fn fold(piece: &Piece, wanted: &[i64], w: u32, h: u32, fps: f64) -> Option<Program> {
    // from the first frame drawn, not the neighbour stepped before it: a live engine's first draw shows its rows'
    // own values for some kinds (an <Entity>'s place), and those are the fold's first frame's
    let lo = piece.first()?.0.max(wanted.iter().copied().min()?);
    let hi = piece.last()?.0 + 1;
    let mut frames: Vec<(i64, Frame)> = Vec::with_capacity((hi - lo) as usize);
    let mut k = piece.partition_point(|p| p.0 <= lo).saturating_sub(1);
    for f in lo..hi {
        while k + 1 < piece.len() && piece[k + 1].0 <= f {
            k += 1;
        }
        let (_, _, scene, clear) = &piece[k];
        frames.push((f, Frame { screens: Vec::new(), mix: None, page: None, sounds: Vec::new(), scene: scene.clone(), clear: clear.clone(), same: false }));
    }
    if frames.iter().all(|(_, f)| f.scene.is_empty()) {
        return None;
    }
    raea_runtime_look::KEY_BASE.store(lo, std::sync::atomic::Ordering::Relaxed);
    let mut p = program(&frames, w, h, fps);
    raea_runtime_look::KEY_BASE.store(0, std::sync::atomic::Ordering::Relaxed);
    shift(&mut p, lo, hi);
    // the clipping check over the frames drawn, on its own thread, as scene_program_checked runs it: with
    // RAEA_CLIP_CHECK=1 only, as a game's stills (drawn live) never ran it, and over 60 frames of a chapter it
    // outlasted the draw by 40 s
    if std::env::var("RAEA_CLIP_CHECK").as_deref() != Ok("1") {
        return Some(p);
    }
    let want: std::collections::HashSet<i64> = wanted.iter().copied().collect();
    struct Prepassed(Vec<(i64, Frame)>);
    // SAFETY: these frames hold no page (no Painting, the one field that isn't Send), and the thread is their only owner
    unsafe impl Send for Prepassed {}
    let owned = Prepassed(frames.into_iter().filter(|(f, _)| want.contains(f)).collect());
    *FED_CLIPPING.lock().unwrap() = Some(std::thread::spawn(move || {
        let o = owned;
        scene::told(&o.0, w, h, fps)
    }));
    Some(p)
}

/// A Program folded from frame `lo` on, moved to start there: its keys, and the rows a frame's position made (pages,
/// screens, sounds: their `from` and `to`). Component and resource rows were made at their frames already (KEY_BASE).
fn shift(p: &mut Program, lo: i64, hi: i64) {
    for keys in p.keys.values_mut() {
        for k in keys.iter_mut() {
            k.0 += lo;
        }
    }
    for (kind, rows) in p.spawns.iter_mut().filter(|(k, _)| matches!(k.as_str(), "image" | "screenpage" | "audio")) {
        let _ = kind;
        for r in rows.iter_mut() {
            for c in ["from", "to"] {
                if let Some(V::N(x)) = r.get_mut(c) {
                    *x += lo as f64;
                }
            }
        }
    }
    p.frames = hi;
}

fn v_json(v: &V) -> Value {
    match v {
        V::N(x) if x.is_finite() => json!(x),
        V::N(x) => json!({ "nf": x.to_string() }),
        V::S(s) => Value::String(s.clone()),
    }
}

fn v_of(j: &Value) -> V {
    match j {
        Value::String(s) => V::S(s.clone()),
        Value::Object(o) => V::N(o.get("nf").and_then(Value::as_str).and_then(|s| s.parse().ok()).unwrap_or(f64::NAN)),
        _ => V::N(j.as_f64().unwrap_or(0.0)),
    }
}

fn prog_json(p: &Program) -> Value {
    let spawns: Vec<Value> = p.spawns.iter().map(|(k, rows)| json!([k, rows.iter().map(|r| Value::Object(r.iter().map(|(c, v)| (c.clone(), v_json(v))).collect())).collect::<Vec<_>>()])).collect();
    let keys: Vec<Value> = p.keys.iter().map(|((id, c), ks)| json!([id, c, ks.iter().map(|(f, v, e)| json!([f, if v.is_finite() { json!(v) } else { json!({"nf": v.to_string()}) }, e])).collect::<Vec<_>>()])).collect();
    json!({"fps": p.fps, "width": p.width, "height": p.height, "frames": p.frames, "spawns": spawns, "keys": keys})
}

fn prog_of(j: &Value) -> Option<Program> {
    let mut p = Program { fps: j["fps"].as_f64()?, width: j["width"].as_u64()? as u32, height: j["height"].as_u64()? as u32, frames: j["frames"].as_i64()?, ..Default::default() };
    for s in j["spawns"].as_array()? {
        let rows = s[1].as_array()?.iter().map(|r| r.as_object().map(|o| o.iter().map(|(c, v)| (c.clone(), v_of(v))).collect())).collect::<Option<Vec<BTreeMap<String, V>>>>()?;
        p.spawns.push((s[0].as_str()?.to_string(), rows));
    }
    for k in j["keys"].as_array()? {
        let ks = k[2].as_array()?.iter().map(|e| Some((e[0].as_i64()?, v_of(&e[1]).num(), e[2].as_str()?.to_string()))).collect::<Option<Vec<_>>>()?;
        p.keys.insert((k[0].as_str()?.to_string(), k[1].as_str()?.to_string()), ks);
    }
    Some(p)
}

/// This binary (its size and modification time): a bake from another build may fold differently.
fn build_id() -> u64 {
    let m = std::env::current_exe().ok().and_then(|e| e.metadata().ok());
    let t = m.as_ref().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0);
    t ^ m.map(|m| m.len()).unwrap_or(0).rotate_left(32)
}

/// Where shot bakes are kept: RAEA_SHOT_CACHE ("0" keeps none), else raea's caches' shots/.
fn cache_dir() -> Option<PathBuf> {
    match std::env::var("RAEA_SHOT_CACHE") {
        Ok(v) if v == "0" => None,
        Ok(v) => Some(PathBuf::from(v)),
        Err(_) => std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache"))).map(|c| c.join("raea").join("shots")),
    }
}

/// The bake's key: the bundle's bytes (the edit, everything it imports, its --input, the runtime bundled in), this
/// build, the frames stepped, and the settings that change what a frame steps to.
fn key_of(js: &Path, stepped: &[i64]) -> R<u64> {
    let mut key: u64 = 0xcbf2_9ce4_8422_2325 ^ build_id();
    let mut eat = |bytes: &[u8]| {
        for b in bytes {
            key = (key ^ *b as u64).wrapping_mul(0x100_0000_01b3);
        }
    };
    eat(&std::fs::read(js).map_err(|e| format!("{}: {e}", js.display()))?);
    eat(format!("native-1|{stepped:?}|{}", ALPHA.load(std::sync::atomic::Ordering::Relaxed)).as_bytes());
    for v in ["RAEA_HIDE_LAYERS", "RAEA_CAMERA", "RAEA_ENGINE_READS", "RAEA_SOURCE_STAMPS", "RAEA_LAYERS"] {
        eat(format!("|{v}={:?}", std::env::var(v).ok()).as_bytes());
    }
    Ok(key)
}

/// The shot baked, or read back from the cache.
fn shot_of(js: &Path, dir: &Path, wanted: &[i64]) -> R<Option<Shot>> {
    let stepped = stepped_of(wanted);
    if stepped.is_empty() {
        return Ok(None);
    }
    let t0 = std::time::Instant::now();
    let path = match cache_dir() {
        Some(d) => Some(d.join(format!("{:016x}.json", key_of(js, &stepped)?))),
        None => None,
    };
    if let Some(p) = path.as_ref().filter(|p| p.exists()) {
        let read = std::fs::read(p).map_err(|e| e.to_string()).and_then(|b| serde_json::from_slice::<Value>(&b).map_err(|e| e.to_string()));
        match read {
            Ok(v) => {
                let ops = v["ops"].as_array().map(|a| a.iter().filter_map(|e| Some((e[0].as_i64()?, e[1].as_array()?.iter().filter_map(|b| b.as_str().map(str::to_string)).collect()))).collect()).unwrap_or_default();
                let prog = if v["prog"].is_null() { None } else { prog_of(&v["prog"]) };
                let num = |k: &str| v[k].as_f64().unwrap_or(0.0);
                let _ = std::fs::File::options().append(true).open(p).and_then(|f| f.set_modified(std::time::SystemTime::now()));
                raea_runtime_react::startup_mark(&format!("the shot's bake read back ({:.0} ms)", t_ms(t0)));
                return Ok(Some(Shot { w: num("w") as u32, h: num("h") as u32, fps: num("fps"), total: num("total") as i64, ops, prog }));
            }
            Err(e) => eprintln!("raea native: the kept bake {} is unreadable ({e}); baking again", p.display()),
        }
    }
    raea_runtime_react::startup_mark("the shot's bake: stepping React across runtimes");
    let ld = |x: &std::sync::atomic::AtomicU64| x.load(std::sync::atomic::Ordering::Relaxed);
    let (lr0, wr0) = (ld(&raea_runtime_react::layout::READS), ld(&raea_runtime_react::shared::ASKED));
    let baked = bake(js, dir, &stepped)?;
    // what the bake met that only a drawn page or a running Engine answers: page.layout reads (answered "nothing drawn"
    // here) and the World's reads (useResource, useQuery, useSystem, useObserver: answered by no Engine here). A shot
    // that makes either isn't native: it renders the usual way, and the line says why (the coverage report reads it)
    let (layout, world) = (ld(&raea_runtime_react::layout::READS) - lr0, ld(&raea_runtime_react::shared::ASKED) - wr0);
    let stateful = baked.is_none();
    eprintln!("NATIVE  probe: {} frames stepped, layout reads {layout}, world reads {world}, stateful compute {}", stepped.len(), if stateful { "yes" } else { "no" });
    if std::env::var("RAEA_NATIVE_PROBE").as_deref() == Ok("1") {
        return Err(format!("NATIVE  probe only (RAEA_NATIVE_PROBE=1): layout reads {layout}, world reads {world}, stateful compute {stateful}"));
    }
    if stateful || ((layout > 0 || world > 0) && std::env::var("RAEA_NATIVE_FORCE").as_deref() != Ok("1")) {
        eprintln!("NATIVE  not native: {}; rendered the usual way (RAEA_NATIVE_FORCE=1 draws it natively anyway)", if stateful { "a <Compute> carries state from frame to frame".to_string() } else { format!("{layout} layout reads and {world} World reads need a drawn page or a running Engine") });
        return Ok(None);
    }
    let Some((conf, piece)) = baked else { return Ok(None) };
    let (w, h) = (num(&conf, "width").unwrap_or(1080.0) as u32, num(&conf, "height").unwrap_or(1920.0) as u32);
    let fps = num(&conf, "fps").unwrap_or(30.0);
    let total = num(&conf, "frames").unwrap_or(60.0) as i64;
    raea_runtime_react::startup_mark("the shot's bake: stepped");
    let t1 = std::time::Instant::now();
    let prog = fold(&piece, wanted, w, h, fps);
    if trace() {
        eprintln!("raea native: the Program folded over {} frames in {:.0} ms", piece.len(), t_ms(t1));
    }
    raea_runtime_react::startup_mark("the shot's bake: Program folded");
    let ops: Vec<(i64, Vec<String>)> = piece.into_iter().map(|(f, o, _, _)| (f, o)).collect();
    let shot = Shot { w, h, fps, total, ops, prog };
    if let Some(p) = path {
        let t2 = std::time::Instant::now();
        let doc = json!({"w": shot.w, "h": shot.h, "fps": shot.fps, "total": shot.total, "ops": shot.ops, "prog": shot.prog.as_ref().map(prog_json)});
        if let Some(d) = p.parent().filter(|d| std::fs::create_dir_all(d).is_ok()) {
            let tmp = p.with_extension(format!("{}.tmp", std::process::id()));
            if std::fs::write(&tmp, serde_json::to_vec(&doc).unwrap_or_default()).is_ok() && std::fs::rename(&tmp, &p).is_ok() {
                prune_cache(d, "json", 64);
            } else {
                let _ = std::fs::remove_file(&tmp);
            }
        }
        if trace() {
            eprintln!("raea native: the bake kept at {} in {:.0} ms", p.display(), t_ms(t2));
        }
    }
    Ok(Some(shot))
}

/// The frames' commits fed to a ReplayPlugin app: a RESET removes what's under the root first (another runtime's tree
/// follows), as a baked timeline's replay does (raea-runtime-live).
fn feed(app: &mut App, batches: &[String]) {
    for j in batches {
        if j == raea_runtime_bake::RESET {
            app.update();
            let world = app.world();
            let root = world.resource::<raea_runtime_react::ReactEntities>().by_id.get(&raea_runtime_react::ROOT_ID).copied();
            let ops: Vec<Value> = root.and_then(|r| world.get::<Children>(r)).map(|c| c.iter().filter_map(|e| world.get::<raea_runtime_react::ReactNode>(e)).map(|n| json!({"op": "remove", "parent": raea_runtime_react::ROOT_ID, "child": n.0})).collect()).unwrap_or_default();
            world.resource::<raea_runtime_react::ReplayFeed>().feed(Value::Array(ops).to_string());
        } else {
            app.world().resource::<raea_runtime_react::ReplayFeed>().feed(j.clone());
        }
    }
}

/// `wanted` drawn natively into the folder `out` (or one PNG): None when the shot has no 3D or isn't native (the
/// caller renders it the usual way).
pub(crate) fn render_native(js: &Path, dir: &Path, out: &Path, wanted: &[i64]) -> R<Option<String>> {
    edit_dir(dir);
    raea_gpu_device::check::scene_black(Some(false));
    raea_runtime_react::native::rendering_in(dir);
    let Some(shot) = shot_of(js, dir, wanted)? else {
        raea_runtime_react::startup_mark("native: the shot has no 3D or carries state; rendered the usual way");
        return Ok(None);
    };
    let Some(prog) = shot.prog.as_ref() else { return Ok(None) };
    let keep_dir = std::env::var_os("RAEA_KEEP").map(PathBuf::from).filter(|v| v.starts_with("/"));
    let work = keep_dir.unwrap_or_else(|| std::env::temp_dir().join(format!("raea-cli-n{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
    let _gone = Gone(work.clone());
    sweep_stale_work();
    let (w, h, fps) = (shot.w, shot.h, shot.fps);
    let mut app = App::new();
    app.add_plugins(MinimalPlugins).add_plugins(raea_runtime_react::ReplayPlugin);
    one_thread(&mut app);
    raea_gpu_device::check::mem_fits(w, h, None)?;
    let mut page = raea_runtime_page::BevyPage::new(w, h, dir)?;
    let mut cx = Ctx { dir, work: &work, sounds: Vec::new(), scene: Vec::new(), clear: None, marker: None, mix: None, audible: HashMap::new(), brand: page_css(dir, w, js), manifest: MANIFEST.lock().unwrap().as_ref().map(|_| Vec::new()), now: 0, said: Default::default(), made: HashMap::new(), scene3d: None, live3d: None };
    let t_open = std::time::Instant::now();
    cx.scene3d = Some(aov_on(raea_render::engine::Engine::new_live(prog, &raea_render::inserted_of(prog), dir)?));
    raea_runtime_react::startup_mark(&format!("native: the 3D's engine open ({:.0} ms)", t_ms(t_open)));
    let folder = out_ext(out).is_empty();
    if folder {
        std::fs::create_dir_all(out).map_err(|e| format!("{}: {e}", out.display()))?;
    }
    // the next stepped frame to feed; a frame before the last one fed goes back to its piece's start (a RESET)
    let mut next = 0usize;
    let (mut t_replay, mut t_draw, mut t_png) = (0f64, 0f64, 0f64);
    let pg = &mut page;
    for (i, &f) in wanted.iter().enumerate() {
        let Some(at) = shot.ops.iter().position(|(g, _)| *g == f) else { return Err(format!("native: frame {f} wasn't baked")) };
        if at < next {
            next = shot.ops[..=at].iter().rposition(|(_, o)| o.first().is_some_and(|b| b == raea_runtime_bake::RESET)).unwrap_or(0);
        }
        let t = std::time::Instant::now();
        for (_, ops) in &shot.ops[next..=at] {
            feed(&mut app, ops);
        }
        app.update();
        next = at + 1;
        t_replay += t_ms(t);
        let t = std::time::Instant::now();
        let px = raea_gpu_device::check::frame_caught(f, || {
            cx.now = f;
            pg.at_frame(f, fps);
            let px = bevy_over_3d(app.world_mut(), pg, &mut cx, fps)?;
            let (px, twice) = raea_gpu_device::check::black_checked(f, px, raea_sys_page::redraw_full, || bevy_over_3d(app.world_mut(), pg, &mut cx, fps))?;
            if twice {
                raea_gpu_device::check::black_twice(f, pg.canary().and_then(|()| cx.scene3d.as_ref().map_or(Ok(()), |e| e.canary())))?;
                lost(f, pg.visible_paint());
            }
            Ok(px)
        })
        .map_err(|e| if i == 0 { format!("{e}; no frame was written") } else { format!("{e}; the {i} frames before it are in {}", out.display()) })?;
        t_draw += t_ms(t);
        let t = std::time::Instant::now();
        let dest = if folder { out.join(format!("frame-{i:05}.png")) } else { out.to_path_buf() };
        image::save_buffer(&dest, &px, w, h, image::ColorType::Rgba8).map_err(|e| format!("{}: {e}", dest.display()))?;
        if folder {
            write_aov(out, i, cx.scene3d.as_mut())?;
        }
        t_png += t_ms(t);
        if i == 0 {
            raea_runtime_react::startup_mark("native: the first frame written");
        }
    }
    if trace() {
        let n = wanted.len().max(1) as f64;
        let p = *PARTS.lock().unwrap();
        eprintln!("raea native: per frame: replay {:.1} ms, draw {:.1} ms (page {:.1}, 3D {:.1}, read back {:.1}), png {:.1} ms", t_replay / n, t_draw / n, p[0] / n, p[1] / n, p[2] / n, t_png / n);
    }
    let clipping = FED_CLIPPING.lock().unwrap().take().and_then(|t| t.join().ok()).unwrap_or_default();
    write_manifest(&mut cx, (w, h, fps, shot.total), clipping, |_| json!([]))?;
    Ok(Some(if folder {
        format!("APPLIED  {} frames rendered to {} (native: the shot baked once, drawn with no JavaScript)", wanted.len(), out.display())
    } else {
        format!("APPLIED  frame {} rendered to {} (native)", wanted.last().copied().unwrap_or(0), out.display())
    }))
}

/// RAEA_ROWS_DUMP=DIR: each drawn frame's 3D as the engine holds it, DIR/FRAME.json: every row's entity and the
/// entities under it (a model's nodes, a skin's joints), each with its reflected components and its material's
/// fields, so two paths' frames can be compared property by property (tools/native-diff).
pub(crate) fn rows_dump(world: &mut World, f: i64) {
    let Some(dir) = std::env::var_os("RAEA_ROWS_DUMP").map(PathBuf::from) else { return };
    let _ = std::fs::create_dir_all(&dir);
    let reg = world.resource::<AppTypeRegistry>().clone();
    let reg = reg.read();
    let mut q = world.query::<(Entity, &raea_sys_frame::store::RowKey)>();
    let roots: Vec<(Entity, String)> = q.iter(world).map(|(e, k)| (e, format!("{}/{}", k.kind, k.id))).collect();
    let mut out = Map::new();
    let mut stack: Vec<(Entity, String)> = roots;
    while let Some((e, name)) = stack.pop() {
        let Ok(er) = world.get_entity(e) else { continue };
        let mut comps = Map::new();
        for cid in er.archetype().components() {
            let Some(info) = world.components().get_info(*cid) else { continue };
            let Some(tid) = info.type_id() else { continue };
            let Some(r) = reg.get(tid) else { continue };
            let Some(rc) = r.data::<bevy::ecs::reflect::ReflectComponent>() else { continue };
            let Some(v) = rc.reflect(er) else { continue };
            let j = serde_json::to_value(bevy::reflect::serde::TypedReflectSerializer::new(v.as_partial_reflect(), &reg)).unwrap_or(Value::Null);
            comps.insert(info.name().to_string(), j);
        }
        if let Some(m) = er.get::<MeshMaterial3d<StandardMaterial>>() {
            if let Some(mat) = world.resource::<Assets<StandardMaterial>>().get(&m.0) {
                let j = serde_json::to_value(bevy::reflect::serde::TypedReflectSerializer::new(mat.as_partial_reflect(), &reg)).unwrap_or(Value::Null);
                comps.insert("material".into(), j);
            }
        }
        if let Some(kids) = er.get::<Children>() {
            for (i, k) in kids.iter().enumerate() {
                let n = world.get::<Name>(k).map(|n| n.as_str().to_string()).unwrap_or_else(|| i.to_string());
                stack.push((k, format!("{name}>{n}")));
            }
        }
        out.insert(name, Value::Object(comps));
    }
    let _ = std::fs::write(dir.join(format!("{f}.json")), Value::Object(out).to_string());
}
