//! RAEA_CPU_CENSUS=1: where the main world's CPU frame goes, for the gamespeed work (design/engine-gamespeed.md in the
//! film's project). Every 30 updates a line of averages:
//!   - the transforms written since the last update: how many, how many written with the value they already held
//!     (a write that changes nothing still marks the tree dirty, so propagation walks it), by kind and by name;
//!   - the time from just before transform propagation to just after it, and the GlobalTransforms it changed;
//!   - at the end (Engine::census), the systems in each schedule of the main and render worlds, and their entities.
//! Off, nothing is added.

use bevy::ecs::entity::EntityHashMap;
use bevy::mesh::Mesh3d;
use bevy::prelude::*;
use bevy::transform::TransformSystems;
use std::collections::HashMap;
use std::time::Instant;

/// Whether RAEA_CPU_CENSUS is set.
pub(crate) fn wanted() -> bool {
    std::env::var("RAEA_CPU_CENSUS").is_ok_and(|v| v == "1")
}

#[derive(Resource, Default)]
struct Census {
    updates: u64,
    /// per kind: (written, written unchanged)
    kinds: [(u64, u64); 4],
    /// by name (digits dropped): written unchanged, written changed
    spurious_by: HashMap<String, u64>,
    real_by: HashMap<String, u64>,
    propagate_ms: f64,
    globals_changed: u64,
    transforms: u64,
    before: Option<Instant>,
}

/// Updates run so far (after_propagate's count), for the end's per-update figures.
static UPDATES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[cfg(feature = "spans")]
const SPANS_FROM: u64 = 20;

const KINDS: [&str; 4] = ["mesh root", "mesh child", "node (child, no mesh)", "root (no mesh)"];

pub(crate) fn install(app: &mut App) {
    app.init_resource::<Census>()
        .add_systems(PostUpdate, before_propagate.before(TransformSystems::Propagate))
        .add_systems(PostUpdate, after_propagate.after(TransformSystems::Propagate));
}

fn short(n: Option<&Name>) -> String {
    let s: String = n.map_or("(no name)", |n| n.as_str()).chars().filter(|c| !c.is_ascii_digit()).take(40).collect();
    s
}

#[allow(clippy::type_complexity)]
fn before_propagate(mut c: ResMut<Census>, q: Query<(Entity, Ref<Transform>, Has<ChildOf>, Has<Mesh3d>, Option<&Name>)>, mut last: Local<EntityHashMap<Transform>>) {
    let mut n = 0;
    for (e, t, child, mesh, name) in &q {
        n += 1;
        if !t.is_changed() {
            continue;
        }
        let k = match (mesh, child) {
            (true, false) => 0,
            (true, true) => 1,
            (false, true) => 2,
            (false, false) => 3,
        };
        c.kinds[k].0 += 1;
        let same = last.get(&e) == Some(&*t);
        if same {
            c.kinds[k].1 += 1;
            *c.spurious_by.entry(short(name)).or_default() += 1;
        } else {
            *c.real_by.entry(short(name)).or_default() += 1;
        }
        last.insert(e, *t);
    }
    c.transforms += n;
    c.before = Some(Instant::now());
}

fn after_propagate(mut c: ResMut<Census>, q: Query<Ref<GlobalTransform>>) {
    if let Some(t) = c.before.take() {
        c.propagate_ms += t.elapsed().as_secs_f64() * 1000.0;
        raea_sys_frame::sys_time("transform propagate (window)", t);
    }
    c.globals_changed += q.iter().filter(|g| g.is_changed()).count() as u64;
    c.updates += 1;
    UPDATES.store(c.updates, std::sync::atomic::Ordering::Relaxed);
    // the `spans` build's sums start after the warm-up (spans.rs)
    #[cfg(feature = "spans")]
    if c.updates == SPANS_FROM {
        super::spans::reset();
    }
    if c.updates % 30 == 0 {
        let n = c.updates as f64;
        let per = |x: u64| x as f64 / 30.0;
        let mut line = format!("raea census: last 30 updates (of {n:.0}), a frame: transforms {:.0}, written", per(c.transforms));
        for (i, (w, s)) in c.kinds.iter().enumerate() {
            line += &format!(" | {}: {:.1} ({:.1} unchanged)", KINDS[i], per(*w), per(*s));
        }
        line += &format!(" | propagate window {:.2} ms, globals changed {:.1}", c.propagate_ms / 30.0, per(c.globals_changed));
        eprintln!("{line}");
        for (label, by) in [("written unchanged", &c.spurious_by), ("written changed", &c.real_by)] {
            let mut v: Vec<_> = by.iter().collect();
            v.sort_by(|a, b| b.1.cmp(a.1));
            let top: Vec<String> = v.iter().take(10).map(|(k, x)| format!("{k} {:.1}", **x as f64 / 30.0)).collect();
            eprintln!("raea census:   {label} by name: {}", top.join(", "));
        }
        *c = Census { updates: c.updates, ..Default::default() };
    }
}

/// Each schedule's systems and the entities, in the main world and the render world: called between updates (a
/// running schedule is out of `Schedules` while it runs).
pub(crate) fn print_worlds(app: &mut App) {
    fn one(label: &str, w: &World) {
        let mut v: Vec<(String, usize)> = w.get_resource::<Schedules>().map(|s| s.iter().map(|(l, s)| (format!("{l:?}"), s.systems_len())).collect()).unwrap_or_default();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        let total: usize = v.iter().map(|x| x.1).sum();
        eprintln!("raea census: {label}: {} entities, {total} systems: {}", w.entities().count_spawned(), v.iter().map(|(l, n)| format!("{l} {n}")).collect::<Vec<_>>().join(", "));
    }
    #[cfg(feature = "spans")]
    super::spans::print(UPDATES.load(std::sync::atomic::Ordering::Relaxed).saturating_sub(SPANS_FROM), 90);
    one("main world", app.world());
    // RAEA_CPU_CENSUS_ORDER=FILE: each schedule's systems in the order its executor starts them (main and render
    // worlds): two runs' files compared show which systems' order a plugin set changes
    if let Some(path) = std::env::var_os("RAEA_CPU_CENSUS_ORDER") {
        let mut out = String::new();
        let mut put = |world: &str, w: &World| {
            if let Some(ss) = w.get_resource::<Schedules>() {
                for (label, sch) in ss.iter() {
                    if let Ok(it) = sch.systems() {
                        for (i, (_, sys)) in it.enumerate() {
                            out += &format!("{world}\t{label:?}\t{i}\t{}\n", sys.name());
                        }
                    }
                }
            }
        };
        put("main", app.world());
        if let Some(r) = app.get_sub_app(bevy::render::RenderApp) {
            put("render", r.world());
        }
        let _ = std::fs::write(path, out);
    }
    // where the rows' entities sit among the indices (a plugin set that spawns fewer entities first shifts them), and
    // every entity's index and generation summed: two runs that allocate alike agree on both
    let w = app.world_mut();
    let mut q = w.query_filtered::<Entity, With<raea_sys_frame::store::RowKey>>();
    let idx: Vec<u32> = q.iter(w).map(|e| e.index_u32()).collect();
    let mut all = w.query::<Entity>();
    let sum: u64 = all.iter(w).map(|e| e.to_bits() % 1_000_003).sum();
    eprintln!("raea census: rows' entities {} (index {}..{}), all entities' bits summed (mod) {sum}", idx.len(), idx.iter().min().unwrap_or(&0), idx.iter().max().unwrap_or(&0));
    // RAEA_CPU_CENSUS_DUMP=FILE: every entity's index, generation and what it is (its name, its nearest row's key, and
    // whether it has a mesh), a line each: two runs' files diffed show where their allocations part
    if let Some(path) = std::env::var_os("RAEA_CPU_CENSUS_DUMP") {
        let mut q = w.query::<(Entity, Option<&Name>, Option<&raea_sys_frame::store::RowKey>, Option<&ChildOf>, Has<Mesh3d>)>();
        let rows: std::collections::HashMap<Entity, String> = q.iter(w).filter_map(|(e, _, k, _, _)| k.map(|k| (e, format!("{}/{}", k.kind, k.id)))).collect();
        let parent: std::collections::HashMap<Entity, Entity> = q.iter(w).filter_map(|(e, _, _, p, _)| p.map(|p| (e, p.parent()))).collect();
        let mut lines: Vec<String> = q
            .iter(w)
            .map(|(e, n, _, _, mesh)| {
                let mut a = e;
                let mut row = String::new();
                for _ in 0..64 {
                    if let Some(r) = rows.get(&a) {
                        row = r.clone();
                        break;
                    }
                    match parent.get(&a) {
                        Some(p) => a = *p,
                        None => break,
                    }
                }
                format!("{} {} {} {} {}", e.index_u32(), e.generation(), if mesh { "mesh" } else { "-" }, n.map_or("", |n| n.as_str()), row)
            })
            .collect();
        lines.sort_by_key(|l| l.split(' ').next().and_then(|x| x.parse::<u32>().ok()).unwrap_or(0));
        let _ = std::fs::write(path, lines.join("\n"));
    }
    if let Some(r) = app.get_sub_app(bevy::render::RenderApp) {
        one("render world", r.world());
    }
}

/// Bevy's ambiguity report (warnings at build) on the frame's schedules, main and render worlds.
pub(crate) fn ambiguity(app: &mut App) {
    use bevy::ecs::schedule::{LogLevel, ScheduleBuildSettings, ScheduleLabel};
    let set = |s: &mut bevy::ecs::schedule::Schedule| {
        s.set_build_settings(ScheduleBuildSettings { ambiguity_detection: LogLevel::Warn, report_sets: false, use_shortnames: false, ..Default::default() });
    };
    let main: [bevy::ecs::intern::Interned<dyn ScheduleLabel>; 5] = [First.intern(), PreUpdate.intern(), Update.intern(), PostUpdate.intern(), Last.intern()];
    for l in main {
        app.edit_schedule(l, set);
    }
    let r = app.sub_app_mut(bevy::render::RenderApp);
    for l in [bevy::render::Render.intern(), bevy::render::ExtractSchedule.intern()] {
        r.edit_schedule(l, set);
    }
}
