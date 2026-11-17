//! Native shots (docs/native-shots.md): a shot's React run once, its frames split across runtimes on every core; its
//! 3D folded into a Program over the shot's own frames (not from frame 0); its page kept as the recorded commits. The
//! frames are then drawn with no JavaScript at all: the commits replayed into the ECS page (ReplayPlugin, as a baked
//! timeline plays) and the 3D drawn from the Program. The bake is kept on disk by what made it, so a re-render of the
//! same shot skips it. `raea render ... --at A,B,... -o DIR/ --native` (or RAEA_NATIVE=1).

use super::*;
use raea_core_program::ir::{Program, V};

/// `render --native`: stills drawn from a shot bake, React never running in the draw.
pub(crate) static NATIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `raea render-serve`: this process renders many shots. With RAEA_KEEP_ENGINE=1 a native shot's engine is kept for
/// the next one of the same size and drawing (Engine::reload). Opt in: sticky-checked (2026-11-16), a kept engine's
/// next shot differed from the same shot drawn fresh (Ch5 @2400 again: 28.9 dB; Ch3 and Ch5 @3300 too), state the
/// reload doesn't reset yet. A fresh engine per served shot matches (docs/native-shots.md).
/// Bevy's task pools (global, made by the first app that asks) sized to this process's share of the cores, not the
/// host's: a render's first app made a compute pool of ~110 threads on a 256-thread host, every lane of a member alike,
/// where members ran out of threads and memory for them (raea_core_scene::par). Called first thing by a render.
pub fn size_task_pools() {
    bevy::app::TaskPoolOptions::with_num_threads(raea_core_scene::par::share().max(4)).create_default_pools();
}

pub static SERVING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

thread_local! {
    /// the engine a served shot left: its size, whether it reads AOVs, and the engine
    static KEPT_ENGINE: std::cell::RefCell<Option<(u32, u32, bool, raea_render::engine::Engine)>> = const { std::cell::RefCell::new(None) };
}

fn keeping_engine() -> bool {
    SERVING.load(std::sync::atomic::Ordering::Relaxed) && std::env::var("RAEA_KEEP_ENGINE").as_deref() == Ok("1")
}

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
static PARTS: std::sync::Mutex<[f64; 16]> = std::sync::Mutex::new([0.0; 16]);

/// RAEA_GS_SERIAL=1 (with RAEA_NATIVE_TRACE): the GPU waited for after each part of the GPU path, so each part's GPU
/// time shows on its own (parts 12..15), at the cost of the overlap
pub(crate) fn serial_wait(k: usize) {
    if trace() && std::env::var_os("RAEA_GS_SERIAL").is_some() {
        if let Some(g) = raea_runtime_view::gpuview::paint_device() {
            let t = std::time::Instant::now();
            let _ = g.device.poll(wgpu::PollType::Wait { submission_index: None, timeout: Some(std::time::Duration::from_secs(60)) });
            part(k, t);
        }
    }
}

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
/// raea processes running on this machine now, this one included (a member's lanes, which start together): the
/// cores and memory a bake takes are its share.
fn raea_processes() -> usize {
    let Ok(d) = std::fs::read_dir("/proc") else { return 1 };
    d.flatten().filter(|e| e.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit())).filter(|e| std::fs::read_to_string(e.path().join("comm")).is_ok_and(|c| c.trim().starts_with("raea"))).count().max(1)
}

fn workers_for(frames: usize) -> usize {
    // its share of the cores and the free memory, beside the other lanes rendering here (three lanes on a 12-core
    // member each opened 12 runtimes, a runtime's 3.5 s opening paid three times over)
    let lanes = raea_processes();
    let cores = (std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4) / lanes).max(1);
    let by_mem = mem_available_gb().map_or(usize::MAX, |g| (g / lanes as f64).floor().max(1.0) as usize);
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
    // each runtime's piece comes back on a channel; RAEA_NATIVE_PARK=1 keeps the runtimes, parked, for the rest of the
    // process (measured: the engine's opening is the same either way, 6.3-6.9 s on Ch5; a slow one, 13-15 s, is a
    // new binary's first run on a host, not the runtimes going)
    let (done_tx, done_rx) = std::sync::mpsc::channel::<R<(Map<String, Value>, Piece)>>();
    let park = std::env::var("RAEA_NATIVE_PARK").as_deref() == Ok("1");
    let threads: Vec<std::thread::JoinHandle<()>> = pieces
        .iter()
        .enumerate()
        .map(|(k, &(a, b))| {
            let off = (k as u64 + 1) << 36;
            let (js, dir) = (js.to_path_buf(), dir.to_path_buf());
            let done_tx = done_tx.clone();
            std::thread::Builder::new()
                .name("raea shot bake".into())
                .spawn(move || {
                    let mut kept: Option<App> = None;
                    let r = (|| -> R<(Map<String, Value>, Piece)> {
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
                        // (moved, and the 3D's interior left out, once the piece is whole: page_commits)
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
                    page_commits(&mut out, off);
                    kept = Some(app);
                    Ok((conf, out))
                    })();
                    let ok = r.is_ok();
                    let _ = done_tx.send(r);
                    if ok && park {
                        let _keep = kept;
                        loop {
                            std::thread::park();
                        }
                    }
                })
                .map_err(|e| e.to_string())
        })
        .collect::<Result<_, _>>()?;
    drop(done_tx);
    let mut conf = None;
    let mut all: Piece = Vec::with_capacity(stepped.len());
    for _ in 0..threads.len() {
        let (c, p) = done_rx.recv().map_err(|_| "a shot bake runtime panicked".to_string())??;
        conf.get_or_insert(c);
        all.extend(p);
    }
    if !park {
        for t in threads {
            let _ = t.join();
        }
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

/// RAEA_NATIVE_DROP3D: on unless "0". The page's commits without the 3D's interior (page_commits).
fn drop3d() -> bool {
    std::env::var("RAEA_NATIVE_DROP3D").as_deref() != Ok("0")
}

/// A piece's commits as the page replays them: each id moved up by `off` (ids_moved_ops) and, unless RAEA_NATIVE_DROP3D=0,
/// the 3D's interior left out. A 3D element (data-3d) inside another is the 3D's alone (a native shot draws its 3D from
/// the Program): they were all but 0.1 MB of Ch5's 87.6 MB of commits, replayed every frame (18-143 ms) into a page
/// world of ~12k entities its sync then walked. Decided over the whole piece, so it can see what any frame adds:
///  - the 3D's outermost elements stay, as the page finds its holders by them;
///  - so does every element over a page element inside the 3D: a <Screen>'s picture (data-raea-screen, its own root
///    when the page walks to it), a UiMaterial's or a texture's slot, a layered Canvas3D's slot. Left out, these hung
///    from a parent the page never made and their pictures went (pc208's opt-in: Ch3 @900 at 33.7 dB);
///  - text under an element left out goes with it;
///  - an insert before a sibling left out goes before the next sibling kept, else to the end (the page's replay skips
///    an insert whose `before` it doesn't know, and the node never hung anywhere).
fn page_commits(piece: &mut Piece, off: u64) {
    use std::collections::HashSet;
    let moved = |ops: Vec<Value>| -> String { ids_moved_ops(ops, off) };
    let parse = |j: &str| serde_json::from_str::<Vec<Value>>(j).ok();
    if !drop3d() {
        for (_, ops, _, _) in piece.iter_mut() {
            for j in ops.iter_mut().filter(|j| j.as_str() != raea_runtime_bake::RESET) {
                if let Some(v) = parse(j) {
                    *j = moved(v);
                }
            }
        }
        return;
    }
    let n = |o: &Value, k: &str| o.get(k).and_then(Value::as_u64);
    // what the whole piece says: each node's parent, its 3D elements, its page elements and its text
    let (mut parent, mut d3, mut page, mut text) = (HashMap::<u64, u64>::new(), HashSet::<u64>::new(), Vec::<u64>::new(), Vec::<u64>::new());
    let mut all: Vec<Vec<Option<Vec<Value>>>> = Vec::with_capacity(piece.len());
    for (_, ops, _, _) in piece.iter() {
        let mut frame = Vec::with_capacity(ops.len());
        for j in ops {
            let v = if j.as_str() == raea_runtime_bake::RESET { None } else { parse(j) };
            for o in v.iter().flatten() {
                match o.get("op").and_then(Value::as_str) {
                    Some("create") => {
                        if let Some(i) = n(o, "id") {
                            if o.get("props").is_some_and(|p| p.get("data-3d").is_some()) {
                                d3.insert(i);
                            } else {
                                page.push(i);
                            }
                        }
                    }
                    Some("createText") | Some("createTextSpan") => text.extend(n(o, "id")),
                    Some("append") | Some("insert") => {
                        if let (Some(p), Some(c)) = (n(o, "parent"), n(o, "child")) {
                            parent.insert(c, p);
                        }
                    }
                    _ => {}
                }
            }
            frame.push(v);
        }
        all.push(frame);
    }
    // every element over a page element stays
    let mut needed = HashSet::<u64>::new();
    for &e in &page {
        let mut id = e;
        for _ in 0..4096 {
            let Some(&p) = parent.get(&id) else { break };
            if !needed.insert(p) {
                break;
            }
            id = p;
        }
    }
    let inside = |mut id: u64| -> bool {
        for _ in 0..4096 {
            let Some(&p) = parent.get(&id) else { return false };
            if d3.contains(&p) {
                return true;
            }
            id = p;
        }
        false
    };
    let mut gone: HashSet<u64> = d3.iter().copied().filter(|&i| !needed.contains(&i) && inside(i)).collect();
    for t in text {
        if parent.get(&t).is_some_and(|p| gone.contains(p)) {
            gone.insert(t);
        }
    }
    // each parent's children in order, as replay keeps them (every op, kept or not), to place an insert whose
    // `before` is gone
    let mut kids: HashMap<u64, Vec<u64>> = HashMap::new();
    let mut at: HashMap<u64, u64> = HashMap::new();
    let unhang = |kids: &mut HashMap<u64, Vec<u64>>, at: &mut HashMap<u64, u64>, c: u64| {
        if let Some(p) = at.remove(&c) {
            if let Some(l) = kids.get_mut(&p) {
                l.retain(|&k| k != c);
            }
        }
    };
    for ((_, ops, _, _), frame) in piece.iter_mut().zip(all) {
        for (j, v) in ops.iter_mut().zip(frame) {
            let Some(v) = v else {
                kids.clear();
                at.clear();
                continue;
            };
            let mut keep = Vec::with_capacity(v.len());
            for mut o in v {
                let kind = o.get("op").and_then(Value::as_str).unwrap_or("").to_string();
                let (p, c) = (n(&o, "parent"), n(&o, "child"));
                match (kind.as_str(), p, c) {
                    ("append", Some(p), Some(c)) => {
                        unhang(&mut kids, &mut at, c);
                        kids.entry(p).or_default().push(c);
                        at.insert(c, p);
                    }
                    ("insert", Some(p), Some(c)) => {
                        unhang(&mut kids, &mut at, c);
                        let l = kids.entry(p).or_default();
                        let b = n(&o, "before");
                        let k = b.and_then(|b| l.iter().position(|&x| x == b)).unwrap_or(l.len());
                        l.insert(k, c);
                        at.insert(c, p);
                        if b.is_some_and(|b| gone.contains(&b)) && !gone.contains(&c) {
                            match l[k + 1..].iter().find(|x| !gone.contains(x)) {
                                Some(&next) => o["before"] = json!(next),
                                None => {
                                    o = json!({"op": "append", "parent": p, "child": c});
                                }
                            }
                        }
                    }
                    ("remove", _, Some(c)) => unhang(&mut kids, &mut at, c),
                    _ => {}
                }
                if n(&o, "id").or(c).is_none_or(|i| !gone.contains(&i)) {
                    keep.push(o);
                }
            }
            *j = moved(keep);
        }
    }
}

/// A commit's node ids moved up by `off` (the root's, 0, stays): a piece's ids kept apart from every other piece's.
fn ids_moved_ops(mut ops: Vec<Value>, off: u64) -> String {
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

/// This binary, by its contents: a hash of its bytes, made once a process (about 0.3 s for a 490 MB raea). Its size
/// and modification time were the key before, and the same release installed at two moments on two members made two
/// keys, so a pool's members never reused each other's bakes (rq stamped every binary's mtime to work round it).
fn build_id() -> u64 {
    static ID: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *ID.get_or_init(|| {
        use std::hash::Hasher;
        let mut h = std::collections::hash_map::DefaultHasher::new();
        h.write(env!("CARGO_PKG_VERSION").as_bytes());
        let Some(f) = std::env::current_exe().ok().and_then(|e| std::fs::File::open(e).ok()) else { return h.finish() };
        // in 32 MB chunks across the cores, each chunk's hash then folded in order: 0.3-0.5 s on one core for a
        // 490 MB raea (1.2 s for a 2.7 GB build with its debug info), in front of every shot's bake read
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        const CHUNK: u64 = 32 << 20;
        let n = len.div_ceil(CHUNK) as usize;
        let parts = raea_core_scene::par::map(n, 16, |i| {
            let at = i as u64 * CHUNK;
            let want = CHUNK.min(len - at) as usize;
            let mut buf = vec![0u8; want];
            let mut got = 0;
            while got < want {
                match read_at(&f, &mut buf[got..want], at + got as u64) {
                    Ok(0) | Err(_) => break,
                    Ok(k) => got += k,
                }
            }
            let mut ch = std::collections::hash_map::DefaultHasher::new();
            ch.write(&buf[..got]);
            ch.finish()
        });
        h.write_u64(len);
        for p in parts {
            h.write_u64(p);
        }
        h.finish()
    })
}

#[cfg(unix)]
fn read_at(f: &std::fs::File, buf: &mut [u8], at: u64) -> std::io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(f, buf, at)
}

#[cfg(windows)]
fn read_at(f: &std::fs::File, buf: &mut [u8], at: u64) -> std::io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(f, buf, at)
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
    eat(format!("native-3|{stepped:?}|{}", ALPHA.load(std::sync::atomic::Ordering::Relaxed)).as_bytes());
    for v in ["RAEA_NATIVE_DROP3D", "RAEA_HIDE_LAYERS", "RAEA_CAMERA", "RAEA_ENGINE_READS", "RAEA_SOURCE_STAMPS", "RAEA_LAYERS"] {
        eat(format!("|{v}={:?}", std::env::var(v).ok()).as_bytes());
    }
    Ok(key)
}

/// Where the project's root stands in a kept bake: its files' paths are kept relative to it, so a bake made in one copy
/// of the project (an rq snapshot, /root/snap/NAME) draws in another. The SDK resolves media to absolute paths as it
/// runs, so they were in the bake as the copy's own, and once that copy was pruned the bake's models "didn't exist".
const ROOT_MARK: &str = "@@raea-project@@";

/// The project's root as a bake's paths start: as given and as the files resolve (a link to a snapshot).
fn roots(dir: &Path) -> Vec<String> {
    let mut r = vec![dir.to_string_lossy().trim_end_matches('/').to_string()];
    if let Ok(c) = dir.canonicalize() {
        let c = c.to_string_lossy().trim_end_matches('/').to_string();
        if !r.contains(&c) {
            r.insert(0, c);
        }
    }
    r.retain(|s| s.len() > 1);
    r
}

/// The JSON-escaped body of a path, as it sits inside a JSON string (and inside a string in one, escaped again).
fn escaped(s: &str) -> String {
    let q = serde_json::to_string(s).unwrap_or_default();
    q[1..q.len().saturating_sub(1)].to_string()
}

/// A bake as kept: the project's root (followed by "/") marked, and each project file it names listed with its content
/// hash ("assets"), so stale() can tell a changed model from an unrelated edit.
fn portable(doc: &Value, dir: &Path) -> String {
    let mut text = doc.to_string();
    for r in roots(dir) {
        text = text.replace(&format!("{}/", escaped(&r)), &format!("{ROOT_MARK}/"));
    }
    let mut named: Vec<String> = Vec::new();
    let mut at = 0;
    while let Some(i) = text[at..].find(ROOT_MARK) {
        let a = at + i + ROOT_MARK.len() + 1;
        let end = text[a..].find(|c: char| matches!(c, '"' | '\\' | ')' | '\'' | '?' | '#')).map_or(text.len(), |e| a + e);
        named.push(text[a..end].to_string());
        at = end;
    }
    named.sort();
    named.dedup();
    let assets: Map<String, Value> = hashes(dir, &named).into_iter().filter_map(|(k, h)| Some((k, Value::String(h?)))).collect();
    let body = text.strip_prefix('{').unwrap_or(&text);
    format!("{{\"assets\":{},{body}", Value::Object(assets))
}

/// A kept bake's marked paths placed at this project's root.
fn placed(text: &str, dir: &Path) -> String {
    text.replace(&format!("{ROOT_MARK}/"), &format!("{}/", escaped(roots(dir).last().map(String::as_str).unwrap_or(""))))
}

/// Why a kept bake no longer matches the project's files: one it names changed or went (None when all match).
fn stale(v: &Value, dir: &Path) -> Option<String> {
    let Some(assets) = v.get("assets").and_then(Value::as_object) else { return Some("it lists no project files".into()) };
    let names: Vec<String> = assets.keys().cloned().collect();
    for (k, (_, h)) in names.iter().zip(hashes(dir, &names)) {
        if h.as_deref() != assets[k].as_str() {
            return Some(format!("{k} {}", if h.is_none() { "is gone" } else { "changed" }));
        }
    }
    None
}

/// Each project file's content hash (blake3), across the cores; None for a name that isn't a file.
fn hashes(dir: &Path, names: &[String]) -> Vec<(String, Option<String>)> {
    let one = |n: &String| {
        let p = dir.join(n);
        let h = p.is_file().then(|| {
            let mut x = blake3::Hasher::new();
            std::fs::File::open(&p).ok().and_then(|f| x.update_reader(f).ok()).map(|x| x.finalize().to_hex().to_string())
        });
        (n.clone(), h.flatten())
    };
    raea_core_scene::par::map(names.len(), 16, |i| one(&names[i]))
}

/// Bakes being kept (written to the shot cache) on threads of their own, joined before a render returns.
static KEEPING: std::sync::Mutex<Vec<std::thread::JoinHandle<()>>> = std::sync::Mutex::new(Vec::new());

fn kept_all() {
    for h in std::mem::take(&mut *KEEPING.lock().unwrap()) {
        let _ = h.join();
    }
}

/// The shot baked, or read back from the cache.
fn shot_of(js: &Path, dir: &Path, wanted: &[i64]) -> R<Option<Shot>> {
    let stepped = stepped_of(wanted);
    if stepped.is_empty() {
        return Ok(None);
    }
    let t0 = std::time::Instant::now();
    let path = match cache_dir() {
        Some(d) => Some(d.join(format!("{:016x}.json.zst", key_of(js, &stepped)?))),
        None => None,
    };
    if let Some(p) = path.as_ref().filter(|p| p.exists()) {
        let mut lap = std::time::Instant::now();
        let mut laps: Vec<(&str, f64)> = Vec::new();
        let mut mark = |what: &'static str| {
            laps.push((what, t_ms(lap)));
            lap = std::time::Instant::now();
        };
        let read = std::fs::read(p)
            .map_err(|e| e.to_string())
            .and_then(|b| zstd::decode_all(&b[..]).map_err(|e| e.to_string()))
            .and_then(|b| String::from_utf8(b).map_err(|e| e.to_string()))
            .map(|t| {
                mark("zstd");
                let t = placed(&t, dir);
                mark("root placed");
                t
            })
            .and_then(|t| serde_json::from_str::<Value>(&t).map_err(|e| e.to_string()))
            .and_then(|v| {
                mark("parsed");
                let s = stale(&v, dir);
                mark("files hashed");
                match s {
                    Some(why) => Err(format!("stale: {why}")),
                    None => Ok(v),
                }
            });
        match read {
            Ok(v) => {
                let ops = v["ops"].as_array().map(|a| a.iter().filter_map(|e| Some((e[0].as_i64()?, e[1].as_array()?.iter().filter_map(|b| b.as_str().map(str::to_string)).collect()))).collect()).unwrap_or_default();
                let prog = if v["prog"].is_null() { None } else { prog_of(&v["prog"]) };
                mark("Program made");
                if trace() {
                    eprintln!("raea native: bake read back: {}", laps.iter().map(|(w, ms)| format!("{w} {ms:.0} ms")).collect::<Vec<_>>().join(", "));
                }
                let num = |k: &str| v[k].as_f64().unwrap_or(0.0);
                let _ = std::fs::File::options().append(true).open(p).and_then(|f| f.set_modified(std::time::SystemTime::now()));
                raea_runtime_react::startup_mark(&format!("the shot's bake read back ({:.0} ms)", t_ms(t0)));
                return Ok(Some(Shot { w: num("w") as u32, h: num("h") as u32, fps: num("fps"), total: num("total") as i64, ops, prog }));
            }
            Err(e) => eprintln!("raea native: the kept bake {} can't be used ({e}); baking again", p.display()),
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
        // kept on a thread of its own while the engine opens (1.6-2.1 s on Ch5 that frame one waited for); the render
        // joins it before it returns (KEEPING)
        let (ops, prog, dir) = (shot.ops.clone(), shot.prog.clone(), dir.to_path_buf());
        let (sw, sh, sfps, stotal) = (shot.w, shot.h, shot.fps, shot.total);
        let h = std::thread::Builder::new().name("raea shot keep".into()).spawn(move || {
            let t2 = std::time::Instant::now();
            let doc = json!({"w": sw, "h": sh, "fps": sfps, "total": stotal, "ops": ops, "prog": prog.as_ref().map(prog_json)});
            if let Some(d) = p.parent().filter(|d| std::fs::create_dir_all(d).is_ok()) {
                let tmp = p.with_extension(format!("{}.tmp", std::process::id()));
                let packed = zstd::encode_all(portable(&doc, &dir).as_bytes(), 3).unwrap_or_default();
                if !packed.is_empty() && std::fs::write(&tmp, packed).is_ok() && std::fs::rename(&tmp, &p).is_ok() {
                    prune_cache(d, "zst", 64);
                } else {
                    let _ = std::fs::remove_file(&tmp);
                }
            }
            if trace() {
                eprintln!("raea native: the bake kept at {} in {:.0} ms (beside the engine's opening)", p.display(), t_ms(t2));
            }
        });
        if let Ok(h) = h {
            KEEPING.lock().unwrap().push(h);
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
    let r = render_shot(js, dir, out, wanted);
    raea_runtime_react::startup_mark("native: shot returned");
    kept_all();
    r
}

fn render_shot(js: &Path, dir: &Path, out: &Path, wanted: &[i64]) -> R<Option<String>> {
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
    // the 3D's rows, and their map files decoded and mipped beside the page's and the engine's opening
    let inserted = raea_render::inserted_of(prog);
    raea_sys_material::maps::prefetch_start(inserted.clone(), dir.to_path_buf());
    let mut page = raea_runtime_page::BevyPage::new(w, h, dir)?;
    let mut cx = Ctx { dir, work: &work, sounds: Vec::new(), scene: Vec::new(), clear: None, marker: None, mix: None, audible: HashMap::new(), brand: page_css(dir, w, js), manifest: MANIFEST.lock().unwrap().as_ref().map(|_| Vec::new()), now: 0, said: Default::default(), made: HashMap::new(), scene3d: None, live3d: None };
    let t_open = std::time::Instant::now();
    let aov = AOV.load(std::sync::atomic::Ordering::Relaxed);
    let kept = if keeping_engine() { KEPT_ENGINE.with(|k| k.borrow_mut().take()).filter(|k| (k.0, k.1, k.2) == (w, h, aov)) } else { None };
    let reused = kept.is_some();
    // a film shot's engine without the plugins its rows don't use (raea_runtime_plugins::install_lean)
    raea_render::engine::plugins::LEAN.store(true, std::sync::atomic::Ordering::Relaxed);
    cx.scene3d = Some(match kept {
        Some((_, _, _, mut e)) => match e.reload(prog, &inserted) {
            Ok(()) => e,
            Err(why) => {
                eprintln!("raea native: the kept engine didn't take this shot ({why}); a fresh one");
                drop(e);
                aov_on(raea_render::engine::Engine::new_live(prog, &inserted, dir)?)
            }
        },
        None => aov_on(raea_render::engine::Engine::new_live(prog, &inserted, dir)?),
    });
    raea_runtime_react::startup_mark(&format!("native: the 3D's engine {} ({:.0} ms)", if reused { "reloaded" } else { "open" }, t_ms(t_open)));
    // its first frame warmed up as a live engine's is (Engine::warm)
    if let Some(e) = cx.scene3d.as_mut() {
        e.warm();
    }
    let folder = out_ext(out).is_empty();
    if folder {
        std::fs::create_dir_all(out).map_err(|e| format!("{}: {e}", out.display()))?;
    }
    // frames written on two threads of their own, a few queued, so the next frame draws while the last one encodes
    // (a PNG was 7.5 ms of a 0.2 s frame on the draw's thread); or, with RAEA_FRAME_PIPE, sent in order to an encoder
    // with no files (raea_runtime_encode::sink). A still (one file) is written here
    let sink = if folder { Some(raea_runtime_encode::sink::Sink::open(out, w, h, wanted.len(), 2, 4)?) } else { None };
    // the draw pipelined (RAEA_NATIVE_PIPE=0: not): a frame that's the page over its 3D alone leaves its readback in
    // flight, and it's taken after the next frame is drawn, so the GPU's work and the copy overlap the next frame's
    // replay, page and draw. A frame of another kind (a layered slot, a camera in a page box) is drawn whole
    let pipe = folder && !AOV.load(std::sync::atomic::Ordering::Relaxed) && std::env::var("RAEA_NATIVE_PIPE").as_deref() != Ok("0");
    // the frame made on the GPU (RAEA_NATIVE_GPU=0: not), as a fed video render makes it (bevy_fed_gpu): the page
    // drawn with the 3D's target in its layered slot, or laid over it, into one frame buffer, which alone is read
    // back, pipelined a frame behind. The whole path read the 3D back, sent it up into the page, drew the page and
    // read that back: two full-frame round trips a frame
    let gpu = pipe && !ALPHA.load(std::sync::atomic::Ordering::Relaxed) && std::env::var_os("RAEA_PAGE_DUMP").is_none() && std::env::var("RAEA_NATIVE_GPU").as_deref() != Ok("0");
    let dev = if gpu { raea_runtime_view::gpuview::paint_device() } else { None };
    let fbuf = match &dev {
        Some(_) => Some(bevy_fed_buffer(w, h)?),
        None => None,
    };
    // the frames being read back, oldest first: each frame's copy is waited for only once RAEA_READBACK_DEPTH (2) newer
    // frames are queued behind it, so the draw never waits on the GPU for the frame it just drew; the copies go into
    // staging buffers that are reused, each handed mapped to the writer and given back once its frame is written
    let depth = std::env::var("RAEA_READBACK_DEPTH").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(2).max(1);
    let mut copying: std::collections::VecDeque<(usize, i64, Copy)> = std::collections::VecDeque::new();
    let ring = Ring::new();
    let mut pending: Option<(usize, i64, Vec<u8>, raea_render::engine::ReadBack)> = None;
    let send = |i: usize, px: Box<dyn raea_runtime_encode::sink::Frame>| -> R<()> {
        match &sink {
            Some(s) => s.put(i, px),
            None => px.pixels(&mut |px| raea_render::save_png(out, px, w, h)),
        }
    };
    // a readback finished and laid under its page; one all black is the scene's own black if the GPUs still pass
    // their canaries (as black_twice keeps one), else the render stops there, the frames before it written
    let check_black = |cx: &mut Ctx, pg: &mut raea_runtime_page::BevyPage, i: usize, f: i64, px: &dyn raea_runtime_encode::sink::Frame| -> R<()> {
        let mut black = false;
        px.pixels(&mut |px| {
            black = px.chunks_exact(4).all(|p| p[0] == 0 && p[1] == 0 && p[2] == 0);
            Ok(())
        })?;
        if black {
            let e = cx.scene3d.as_ref().ok_or("no 3D")?;
            raea_gpu_device::check::black_twice(f, pg.canary().and_then(|()| e.canary())).map_err(|err| format!("{err}; the {i} frames before it are in {}", out.display()))?;
        }
        Ok(())
    };
    let finish = |cx: &mut Ctx, pg: &mut raea_runtime_page::BevyPage, i: usize, f: i64, page: Vec<u8>, read: raea_render::engine::ReadBack| -> R<Vec<u8>> {
        let px = page_over(cx.scene3d.as_ref().ok_or("no 3D")?.finish_read(read)?, &page);
        check_black(cx, pg, i, f, &px)?;
        Ok(px)
    };
    let (mut t_read, mut t_send) = (0f64, 0f64);
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
        if let (Some(g), Some(buf)) = (&dev, &fbuf) {
            let c = raea_gpu_device::check::frame_caught(f, || {
                cx.now = f;
                pg.at_frame(f, fps);
                bevy_fed_gpu(app.world_mut(), pg, &mut cx, buf, fps, &g.device, &g.queue)?;
                Ok(start_copy(&g.device, &g.queue, buf, &ring))
            })
            .map_err(|e| if i == 0 { format!("{e}; no frame was written") } else { format!("{e}; the {i} frames before it are in {}", out.display()) })?;
            copying.push_back((i, f, c));
            while copying.len() > depth {
                let (pi, pf, prev) = copying.pop_front().ok_or("no readback")?;
                let tr = std::time::Instant::now();
                let px = finish_copy(&g.device, prev, &ring)?;
                t_read += t_ms(tr);
                check_black(&mut cx, pg, pi, pf, &px)?;
                let ts = std::time::Instant::now();
                send(pi, Box::new(px))?;
                t_send += t_ms(ts);
            }
            t_draw += t_ms(t);
            if i == 0 {
                raea_runtime_react::startup_mark("native: the first frame drawn");
            }
            continue;
        }
        let step = raea_gpu_device::check::frame_caught(f, || {
            cx.now = f;
            pg.at_frame(f, fps);
            if pipe {
                return over_3d(app.world_mut(), pg, &mut cx, fps, true);
            }
            let px = bevy_over_3d(app.world_mut(), pg, &mut cx, fps)?;
            let (px, twice) = raea_gpu_device::check::black_checked(f, px, raea_sys_page::redraw_full, || bevy_over_3d(app.world_mut(), pg, &mut cx, fps))?;
            if twice {
                raea_gpu_device::check::black_twice(f, pg.canary().and_then(|()| cx.scene3d.as_ref().map_or(Ok(()), |e| e.canary())))?;
                lost(f, pg.visible_paint());
            }
            Ok(Over::Done(px))
        })
        .map_err(|e| if i == 0 { format!("{e}; no frame was written") } else { format!("{e}; the {i} frames before it are in {}", out.display()) })?;
        // the frame before's readback, waited for now that this frame's work is queued behind it
        if let Some((pi, pf, page, read)) = pending.take() {
            let px = finish(&mut cx, pg, pi, pf, page, read)?;
            send(pi, Box::new(px))?;
        }
        match step {
            Over::Done(px) => send(i, Box::new(px))?,
            Over::Reading { page, read } => pending = Some((i, f, page, read)),
        }
        t_draw += t_ms(t);
        let t = std::time::Instant::now();
        if folder {
            write_aov(out, i, cx.scene3d.as_mut())?;
        }
        t_png += t_ms(t);
        if i == 0 {
            raea_runtime_react::startup_mark("native: the first frame drawn");
        }
    }
    if let Some((pi, pf, page, read)) = pending.take() {
        let px = finish(&mut cx, pg, pi, pf, page, read)?;
        send(pi, Box::new(px))?;
    }
    if let Some(g) = &dev {
        while let Some((pi, pf, prev)) = copying.pop_front() {
            let px = finish_copy(&g.device, prev, &ring)?;
            check_black(&mut cx, pg, pi, pf, &px)?;
            send(pi, Box::new(px))?;
        }
    }
    let piped = sink.as_ref().is_some_and(|s| s.piped_out());
    let tw = std::time::Instant::now();
    if let Some(s) = sink {
        s.finish()?;
    }
    if trace() {
        let n = wanted.len().max(1) as f64;
        let p = *PARTS.lock().unwrap();
        eprintln!("raea native: per frame: replay {:.1} ms, draw {:.1} ms (page {:.1}, 3D {:.1}, read back {:.1}), png {:.1} ms", t_replay / n, t_draw / n, p[0] / n, p[1] / n, p[2] / n, t_png / n);
        eprintln!(
            "raea native: capture per frame: readback wait {:.2} ms, handed to the {} {:.2} ms (in the draw), staging buffers {}, writers' tail {:.0} ms",
            t_read / n,
            if piped { "pipe" } else { "PNG writers" },
            t_send / n,
            ring.made(),
            t_ms(tw)
        );
        if let Some(e) = cx.scene3d.as_mut() {
            let w = e.world_mut();
            let mut q = w.query::<(Option<&bevy::light::PointLight>, Option<&bevy::light::SpotLight>, Option<&bevy::light::DirectionalLight>)>();
            let (mut p, mut ps, mut sp, mut sps, mut d, mut ds) = (0, 0, 0, 0, 0, 0);
            for (a, b, c) in q.iter(w) {
                if let Some(a) = a { p += 1; ps += a.shadow_maps_enabled as usize; }
                if let Some(b) = b { sp += 1; sps += b.shadow_maps_enabled as usize; }
                if let Some(c) = c { d += 1; ds += c.shadow_maps_enabled as usize; }
            }
            let meshes = w.query::<&bevy::mesh::Mesh3d>().iter(w).count();
            let casters = w.query_filtered::<&bevy::mesh::Mesh3d, bevy::prelude::Without<bevy::light::NotShadowCaster>>().iter(w).count();
            eprintln!("raea native: lights: point {p} ({ps} with shadows), spot {sp} ({sps}), directional {d} ({ds}); meshes {meshes}, {casters} casting shadows");
        }
        let pc = |i: usize| raea_sys_shader::own_live::PATCHED[i].load(std::sync::atomic::Ordering::Relaxed) as f64 / n;
        eprintln!("raea native: own:uniforms a frame: {:.1} applied, {:.1} changed, {:.1} storage buffers written", pc(0), pc(1), pc(2));
        let fast = |i: usize| bevy::render::material_bind_groups::RAEA_FAST[i].load(std::sync::atomic::Ordering::Relaxed) as f64 / n;
        eprintln!("raea native: materials a frame: {:.1} updated in place, {:.1} made again", fast(0), fast(1));
        let mut by: Vec<_> = raea_sys_frame::VALS_BY.lock().unwrap().iter().map(|(k, v)| (*v, *k)).collect();
        by.sort_by(|a, b| b.0.cmp(&a.0));
        for (c, (f, l)) in by.iter().take(12) {
            eprintln!("raea native: vals from {f}:{l}: {:.0} a frame", *c as f64 / n);
        }
        for (k, ms, calls) in raea_sys_frame::sys_times() {
            eprintln!("raea native: system {k}: {:.2} ms a frame ({calls} calls)", ms / n);
        }
        eprintln!("raea native: gpu path per frame (ms): live3d {:.2}, page sync {:.2}, page draw {:.2}, screens {:.2}, 3D update {:.2}, feed {:.2}, start copy {:.2}, finish copy {:.2}, black+send {:.2}, png encode (writer threads) {:.2}; serial GPU waits: page {:.2}, 3D {:.2}, feed+copy {:.2}", p[3] / n, p[4] / n, p[5] / n, p[6] / n, p[7] / n, p[12] / n, p[8] / n, p[9] / n, p[10] / n, p[11] / n, p[13] / n, p[14] / n, p[15] / n);
    }
    raea_runtime_react::startup_mark("native: frames written");
    let clipping = FED_CLIPPING.lock().unwrap().take().and_then(|t| t.join().ok()).unwrap_or_default();
    raea_runtime_react::startup_mark("native: clipping check joined");
    write_manifest(&mut cx, (w, h, fps, shot.total), clipping, |_| json!([]))?;
    // a served process keeps the engine for its next shot
    if keeping_engine() {
        if let Some(e) = cx.scene3d.take() {
            KEPT_ENGINE.with(|k| *k.borrow_mut() = Some((w, h, aov, e)));
        }
    }
    Ok(Some(if folder {
        format!("APPLIED  {} frames rendered to {} (native: the shot baked once, drawn with no JavaScript)", wanted.len(), out.display())
    } else {
        format!("APPLIED  frame {} rendered to {} (native)", wanted.last().copied().unwrap_or(0), out.display())
    }))
}

/// A frame buffer's copy for reading back, queued and not waited for (the native draw's GPU path).
struct Copy {
    staging: wgpu::Buffer,
    rx: std::sync::mpsc::Receiver<Result<(), wgpu::BufferAsyncError>>,
    sub: wgpu::SubmissionIndex,
}

/// The staging buffers frames are read back into, reused: a buffer goes out mapped with its frame ([Staged]) and comes
/// back once the frame is written. As many are made as are out at once (the frames in flight, queued and being
/// written: about 8), where a buffer was made and freed for every frame.
struct Ring {
    back: std::sync::mpsc::Sender<wgpu::Buffer>,
    free: std::sync::mpsc::Receiver<wgpu::Buffer>,
    made: std::cell::Cell<usize>,
}

impl Ring {
    fn new() -> Ring {
        let (back, free) = std::sync::mpsc::channel();
        Ring { back, free, made: std::cell::Cell::new(0) }
    }

    fn take(&self, device: &wgpu::Device, size: u64) -> wgpu::Buffer {
        if let Some(b) = self.free.try_iter().find(|b| b.size() == size) {
            return b;
        }
        self.made.set(self.made.get() + 1);
        device.create_buffer(&wgpu::BufferDescriptor { label: Some("native frame readback"), size, usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false })
    }

    fn made(&self) -> usize {
        self.made.get()
    }
}

/// A frame read back, its pixels still in their mapped staging buffer: written from there (no copy on the draw's
/// thread), then unmapped and given back to the ring.
struct Staged {
    buf: Option<wgpu::Buffer>,
    back: std::sync::mpsc::Sender<wgpu::Buffer>,
}

impl raea_runtime_encode::sink::Frame for Staged {
    fn pixels(&self, f: &mut dyn FnMut(&[u8]) -> R<()>) -> R<()> {
        let b = self.buf.as_ref().ok_or("a frame given back")?;
        let view = b.slice(..).get_mapped_range().map_err(|e| format!("the frame's readback: {e:?}"))?;
        f(&view)
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if let Some(b) = self.buf.take() {
            b.unmap();
            let _ = self.back.send(b);
        }
    }
}

fn start_copy(device: &wgpu::Device, queue: &wgpu::Queue, src: &wgpu::Buffer, ring: &Ring) -> Copy {
    let staging = ring.take(device, src.size());
    let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("native frame readback") });
    enc.copy_buffer_to_buffer(src, 0, &staging, 0, src.size());
    let sub = queue.submit([enc.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    staging.slice(..).map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    Copy { staging, rx, sub }
}

/// A copy's frame, once mapped: the device polled without blocking, then (if it still isn't) waited for up to this
/// copy's own submission, never the frames queued after it.
fn finish_copy(device: &wgpu::Device, c: Copy, ring: &Ring) -> R<Staged> {
    let mapped = match c.rx.try_recv() {
        Ok(m) => m,
        Err(_) => {
            let _ = device.poll(wgpu::PollType::Poll);
            match c.rx.try_recv() {
                Ok(m) => m,
                Err(_) => {
                    device.poll(wgpu::PollType::Wait { submission_index: Some(c.sub), timeout: Some(std::time::Duration::from_secs(60)) }).map_err(|e| format!("the frame's readback: {e}"))?;
                    c.rx.recv().map_err(|e| e.to_string())?
                }
            }
        }
    };
    mapped.map_err(|e| format!("the frame's readback: {e}"))?;
    Ok(Staged { buf: Some(c.staging), back: ring.back.clone() })
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
        // a toon surface's (its StandardMaterial base and toon params), which hid a batch-sticky colour (Ch1 @11550)
        if let Some(m) = er.get::<MeshMaterial3d<raea_sys_material::toon::ToonMaterial>>() {
            if let Some(mat) = world.resource::<Assets<raea_sys_material::toon::ToonMaterial>>().get(&m.0) {
                let j = serde_json::to_value(bevy::reflect::serde::TypedReflectSerializer::new(mat.base.as_partial_reflect(), &reg)).unwrap_or(Value::Null);
                comps.insert("material".into(), j);
                let t = serde_json::to_value(bevy::reflect::serde::TypedReflectSerializer::new(mat.extension.as_partial_reflect(), &reg)).unwrap_or(Value::Null);
                comps.insert("toon".into(), t);
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
