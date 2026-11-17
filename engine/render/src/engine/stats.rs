//! RAEA_SCENE_STATS=FILE: what each frame hands the GPU, one JSON line a frame (design/engine-scene-cost.md in the film's
//! project). The main world's meshes (all, drawn by some view, unique meshes and materials, triangles, transparent,
//! shadow casters), its lights and cascades; the render world's phases (views, bins, draw calls, instances) per phase;
//! the CPU time of the main update and of the render world's stages; and, with RAEA_BEVY_PASSES=1, the GPU time of
//! each of Bevy's passes (timestamp queries; the latest readback, a few frames behind). Off, nothing is added.

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use bevy::prelude::*;
use bevy::render::render_phase::{BinnedPhaseItem, PhaseItem, SortedPhaseItem, ViewBinnedRenderPhases, ViewSortedRenderPhases};
use bevy::render::{Render, RenderApp, RenderSystems as RS};

#[derive(Default)]
struct Shared {
    /// the main world's half of the frame's line (written in Last)
    main: Option<serde_json::Map<String, serde_json::Value>>,
    /// the render world's marks, ms from its start
    marks: Vec<(&'static str, f64)>,
    start: Option<Instant>,
    phases: serde_json::Map<String, serde_json::Value>,
    out: Option<std::fs::File>,
    frames: u64,
}

#[derive(Resource, Clone)]
struct Stats(Arc<Mutex<Shared>>);

#[derive(Resource, Default)]
struct MainStart(Option<Instant>);

/// Whether RAEA_SCENE_STATS is set.
pub(crate) fn wanted() -> bool {
    std::env::var_os("RAEA_SCENE_STATS").is_some()
}

pub(crate) fn install(app: &mut App) {
    let Some(path) = std::env::var_os("RAEA_SCENE_STATS") else { return };
    let out = std::fs::OpenOptions::new().create(true).append(true).open(&path).ok();
    if out.is_none() {
        eprintln!("raea: RAEA_SCENE_STATS: can't open {}", path.to_string_lossy());
    }
    let s = Stats(Arc::new(Mutex::new(Shared { out, ..Default::default() })));
    app.insert_resource(s.clone()).init_resource::<MainStart>().add_systems(First, main_start).add_systems(Last, (main_stats, census, pick));
    fn mark(name: &'static str) -> impl FnMut(Res<Stats>) {
        move |s: Res<Stats>| {
            let mut g = s.0.lock().unwrap();
            let t = g.start.map_or(0.0, |t| t.elapsed().as_secs_f64() * 1000.0);
            g.marks.push((name, t));
        }
    }
    let r = app.sub_app_mut(RenderApp);
    r.insert_resource(s);
    r.add_systems(
        Render,
        (
            render_start.before(RS::ExtractCommands),
            mark("extract_cmds").after(RS::ExtractCommands).before(RS::PrepareAssets),
            mark("prepare_assets").after(RS::PrepareAssets).before(RS::CreateViews),
            mark("views").after(RS::CreateViews).before(RS::Specialize),
            mark("specialize").after(RS::Specialize).before(RS::PrepareViews),
            mark("prepare_views").after(RS::PrepareViews).before(RS::Queue),
            mark("queue").after(RS::QueueSweep).before(RS::PhaseSort),
            mark("sort").after(RS::PhaseSort).before(RS::Prepare),
            mark("prepare").after(RS::PrepareResourcesFlush).before(RS::PrepareBindGroups),
            (mark("bind_groups"), phase_counts).chain().after(RS::PrepareBindGroups).before(RS::Render),
            mark("render").after(RS::Render).before(RS::Cleanup),
        ),
    );
}

/// Once, at the 12th update (the shot's first frames drawn): the mesh entities by their material component types and
/// shadow flags, by archetype, as a `census` line.
fn census(world: &mut World, mut n: Local<u32>) {
    *n += 1;
    if *n != 12 {
        return;
    }
    let Some(mesh) = world.components().component_id::<Mesh3d>() else { return };
    let no_cast = world.components().component_id::<bevy::light::NotShadowCaster>();
    let mut by: std::collections::BTreeMap<String, (usize, usize)> = Default::default();
    for a in world.archetypes().iter() {
        if a.is_empty() || !a.contains(mesh) {
            continue;
        }
        let casts = no_cast.is_none_or(|c| !a.contains(c));
        let mut names: Vec<String> = a.components().iter().filter_map(|c| world.components().get_info(*c)).map(|i| format!("{}", i.name())).filter(|n| n.contains("MeshMaterial3d")).collect();
        names.sort();
        let key = if names.is_empty() { "(no material)".to_string() } else { names.join(" + ") };
        let e = by.entry(key).or_default();
        e.0 += a.len() as usize;
        if casts {
            e.1 += a.len() as usize;
        }
    }
    let line = serde_json::json!({"census": by.into_iter().map(|(k, (n, c))| serde_json::json!({"material": k, "entities": n, "casting": c})).collect::<Vec<_>>()});
    if let Some(f) = world.resource::<Stats>().0.lock().unwrap().out.as_mut() {
        let _ = writeln!(f, "{line}");
    }
}

/// RAEA_PICK="x,y;x,y..." (frame pixels): once, at the 12th update, every mesh triangle under each pixel, nearest
/// first, from the camera drawing the engine's target (its rest pose for a skinned mesh): two hits at one distance
/// are a coplanar pair, whose winner is whichever is drawn first. A `pick` line per pixel.
fn pick(world: &mut World, mut n: Local<u32>) {
    *n += 1;
    let Some(spec) = std::env::var("RAEA_PICK").ok() else { return };
    if *n != 12 {
        return;
    }
    let target = world.get_resource::<super::TagRowTarget>().map(|t| t.0.id());
    let mut cams = world.query::<(&Camera, &GlobalTransform, &bevy::camera::RenderTarget)>();
    // every active camera (the one drawing the engine's target first): a pixel is looked up in each, scaled to its
    // target's size, so a camera into a texture (a pocket-world window) is searched too
    let mut all: Vec<(Camera, GlobalTransform, bool)> = cams.iter(world).filter(|(c, _, _)| c.is_active).map(|(c, g, t)| (c.clone(), *g, matches!(t, bevy::camera::RenderTarget::Image(i) if Some(i.handle.id()) == target))).collect();
    all.sort_by_key(|(c, _, main)| (!*main, -c.order));
    let frame = all.iter().find(|(_, _, m)| *m).and_then(|(c, _, _)| c.logical_viewport_size()).unwrap_or(Vec2::new(1920.0, 1080.0));
    let mut meshes_q = world.query::<(Entity, &Mesh3d, &GlobalTransform, &ViewVisibility)>();
    let items: Vec<(Entity, AssetId<Mesh>, GlobalTransform)> = meshes_q.iter(world).filter(|(_, _, _, v)| v.get()).map(|(e, m, g, _)| (e, m.0.id(), *g)).collect();
    let mut keys = world.query::<&raea_sys_frame::store::RowKey>();
    let mut names = world.query::<&Name>();
    let mut parents = world.query::<&ChildOf>();
    let mut lines = Vec::new();
    for (ci, (cam, cam_t, main)) in all.iter().enumerate() {
    let (cam, cam_t) = (cam, *cam_t);
    let scale = cam.logical_viewport_size().map_or(Vec2::ONE, |v| v / frame);
    for p in spec.split(';') {
        let mut it = p.split(',').filter_map(|v| v.trim().parse::<f32>().ok());
        let (Some(fx), Some(fy)) = (it.next(), it.next()) else { continue };
        let (x, y) = (fx * scale.x, fy * scale.y);
        let Ok(ray) = cam.viewport_to_world(&cam_t, Vec2::new(x + 0.5, y + 0.5)) else { continue };
        let mut hits: Vec<(f32, Entity, usize)> = Vec::new();
        let meshes = world.resource::<Assets<Mesh>>();
        for (e, id, g) in &items {
            let Some(m) = meshes.get(*id) else { continue };
            let Some(bevy::mesh::VertexAttributeValues::Float32x3(pos)) = m.attribute(Mesh::ATTRIBUTE_POSITION) else { continue };
            let mat = g.to_matrix();
            let idx: Vec<usize> = match m.indices() {
                Some(i) => i.iter().collect(),
                None => (0..pos.len()).collect(),
            };
            let dir: Vec3 = *ray.direction;
            for (ti, t) in idx.chunks_exact(3).enumerate() {
                let [a, b, c] = [t[0], t[1], t[2]].map(|i| mat.transform_point3(Vec3::from(pos[i])));
                // Moller-Trumbore, both faces
                let (e1, e2) = (b - a, c - a);
                let pv = dir.cross(e2);
                let det = e1.dot(pv);
                if det.abs() < 1e-12 {
                    continue;
                }
                let inv = 1.0 / det;
                let tv = ray.origin - a;
                let u = tv.dot(pv) * inv;
                if !(0.0..=1.0).contains(&u) {
                    continue;
                }
                let qv = tv.cross(e1);
                let v = dir.dot(qv) * inv;
                if v < 0.0 || u + v > 1.0 {
                    continue;
                }
                let d = e2.dot(qv) * inv;
                if d > 0.0 {
                    hits.push((d, *e, ti));
                }
            }
        }
        hits.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut list = Vec::new();
        for (d, e, ti) in hits.iter().take(8) {
            let mut up = *e;
            let mut path = Vec::new();
            for _ in 0..10 {
                if let Ok(n) = names.get(world, up) {
                    path.push(n.as_str().to_string());
                }
                if let Ok(k) = keys.get(world, up) {
                    path.push(format!("{}/{}", k.kind, k.id));
                    break;
                }
                match parents.get(world, up) {
                    Ok(p) => up = p.parent(),
                    Err(_) => break,
                }
            }
            list.push(serde_json::json!({"dist": d, "entity": format!("{e}"), "tri": ti, "who": path.join(" < ")}));
        }
        // and every drawn mesh whose box, projected by this camera, covers the pixel: its depth range
        let mut boxes = world.query::<(Entity, &bevy::camera::primitives::Aabb, &GlobalTransform, &ViewVisibility)>();
        let mut cover: Vec<(f32, f32, Entity)> = Vec::new();
        for (e, bb, g, v) in boxes.iter(world) {
            if !v.get() {
                continue;
            }
            let (c, h) = (Vec3::from(bb.center), Vec3::from(bb.half_extents));
            let (mut lo, mut hi, mut dmin, mut dmax, mut ok) = (Vec2::splat(f32::MAX), Vec2::splat(f32::MIN), f32::MAX, f32::MIN, true);
            for i in 0..8 {
                let k = Vec3::new(if i & 1 == 0 { -1.0 } else { 1.0 }, if i & 2 == 0 { -1.0 } else { 1.0 }, if i & 4 == 0 { -1.0 } else { 1.0 });
                let w = g.transform_point(c + h * k);
                match cam.world_to_viewport(&cam_t, w) {
                    Ok(s) => {
                        lo = lo.min(s);
                        hi = hi.max(s);
                    }
                    Err(_) => ok = false,
                }
                let d = (w - cam_t.translation()).length();
                dmin = dmin.min(d);
                dmax = dmax.max(d);
            }
            if ok && x + 0.5 >= lo.x && x + 0.5 <= hi.x && y + 0.5 >= lo.y && y + 0.5 <= hi.y {
                cover.push((dmin, dmax, e));
            }
        }
        cover.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut cov = Vec::new();
        for (dmin, dmax, e) in cover.iter().take(12) {
            let mut up = *e;
            let mut path = Vec::new();
            for _ in 0..10 {
                if let Ok(n) = names.get(world, up) {
                    path.push(n.as_str().to_string());
                }
                if let Ok(k) = keys.get(world, up) {
                    path.push(format!("{}/{}", k.kind, k.id));
                    break;
                }
                match parents.get(world, up) {
                    Ok(p) => up = p.parent(),
                    Err(_) => break,
                }
            }
            cov.push(serde_json::json!({"depth": [dmin, dmax], "entity": format!("{e}"), "who": path.join(" < ")}));
        }
        lines.push(serde_json::json!({"pick": [fx, fy], "camera": ci, "main": main, "order": cam.order, "hits": list, "boxes": cov}));
    }
    }
    if let Some(f) = world.resource::<Stats>().0.lock().unwrap().out.as_mut() {
        for l in lines {
            let _ = writeln!(f, "{l}");
        }
    }
}

fn main_start(mut m: ResMut<MainStart>) {
    m.0 = Some(Instant::now());
}

/// The render world's start of a frame: the line for the frame before it (its main half, phases and marks) written.
fn render_start(s: Res<Stats>) {
    let mut g = s.0.lock().unwrap();
    if let Some(mut line) = g.main.take() {
        let mut prev = 0.0;
        let mut marks = serde_json::Map::new();
        for (k, t) in g.marks.drain(..) {
            marks.insert(k.to_string(), serde_json::json!(((t - prev) * 1000.0).round() / 1000.0));
            prev = t;
        }
        marks.insert("total".into(), serde_json::json!((prev * 1000.0).round() / 1000.0));
        line.insert("render_cpu_ms".into(), serde_json::Value::Object(marks));
        line.insert("phases".into(), serde_json::Value::Object(std::mem::take(&mut g.phases)));
        let txt = serde_json::Value::Object(line).to_string();
        if let Some(f) = g.out.as_mut() {
            let _ = writeln!(f, "{txt}");
        }
        g.frames += 1;
    } else {
        g.marks.clear();
    }
    g.start = Some(Instant::now());
}

#[derive(Default)]
struct Ph {
    views: usize,
    bins: usize,
    draws: usize,
    instances: usize,
    /// each view's (draws, instances)
    per_view: Vec<(usize, usize)>,
}

fn binned<B: BinnedPhaseItem>(p: &ViewBinnedRenderPhases<B>) -> Ph {
    let mut o = Ph::default();
    for ph in p.0.values() {
        if ph.is_empty() {
            continue;
        }
        o.views += 1;
        let (d0, i0) = (o.draws, o.instances);
        // NoIndirectDrawing (open.rs): each batchable bin (one mesh and material) is one draw of its instances
        o.bins += ph.batchable_meshes.len() + ph.unbatchable_meshes.len() + ph.multidrawable_meshes.len();
        for b in ph.batchable_meshes.values() {
            o.draws += 1;
            o.instances += b.entities().len();
        }
        for u in ph.unbatchable_meshes.values() {
            o.draws += u.entities.len();
            o.instances += u.entities.len();
        }
        for n in ph.non_mesh_items.values() {
            o.draws += n.entities.len();
            o.instances += n.entities.len();
        }
        // (multidraw only with GPU culling, RAEA_GPU_CULLING=1: counted as one draw a batch set)
        o.draws += ph.multidrawable_meshes.len();
        o.per_view.push((o.draws - d0, o.instances - i0));
    }
    o.per_view.sort_unstable_by(|a, b| b.1.cmp(&a.1));
    o
}

fn sorted<I: SortedPhaseItem>(p: &ViewSortedRenderPhases<I>) -> Ph {
    let mut o = Ph::default();
    for ph in p.0.values() {
        if ph.items.is_empty() {
            continue;
        }
        o.views += 1;
        o.instances += ph.items.len();
        let mut i = 0;
        let items: Vec<&I> = ph.items.values().collect();
        while i < items.len() {
            let r = items[i].batch_range().len();
            if r == 0 {
                i += 1;
            } else {
                o.draws += 1;
                i += r;
            }
        }
        o.bins = o.draws;
    }
    o
}

#[allow(clippy::too_many_arguments)]
fn phase_counts(
    s: Res<Stats>,
    opaque: Option<Res<ViewBinnedRenderPhases<bevy::core_pipeline::core_3d::Opaque3d>>>,
    mask: Option<Res<ViewBinnedRenderPhases<bevy::core_pipeline::core_3d::AlphaMask3d>>>,
    pre: Option<Res<ViewBinnedRenderPhases<bevy::core_pipeline::prepass::Opaque3dPrepass>>>,
    shadow: Option<Res<ViewBinnedRenderPhases<bevy::pbr::Shadow>>>,
    trans: Option<Res<ViewSortedRenderPhases<bevy::core_pipeline::core_3d::Transparent3d>>>,
) {
    let mut m = serde_json::Map::new();
    let mut shadow_keys = None;
    let mut put = |k: &str, p: Ph| {
        if p.views > 0 {
            m.insert(k.into(), serde_json::json!({"views": p.views, "bins": p.bins, "draws": p.draws, "instances": p.instances, "per_view": p.per_view}));
        }
    };
    if let Some(p) = opaque { put("opaque", binned(&p)); }
    if let Some(p) = mask { put("alpha_mask", binned(&p)); }
    if let Some(p) = pre { put("prepass", binned(&p)); }
    if let Some(p) = shadow {
        put("shadow", binned(&p));
        // what splits the shadow phase's batches: its batch sets' distinct pipelines, material bind groups and mesh
        // slabs, and the meshes binned
        let (mut pipes, mut groups, mut slabs, mut meshes, mut sets) = (HashSet::new(), HashSet::new(), HashSet::new(), HashSet::new(), HashSet::new());
        let mut per_set: Vec<usize> = Vec::new();
        for ph in p.0.values() {
            let mut count: HashMap<String, usize> = HashMap::new();
            for (set, bin) in ph.batchable_meshes.keys().chain(ph.unbatchable_meshes.keys()) {
                pipes.insert(format!("{:?}", set.pipeline));
                groups.insert(set.material_bind_group_index);
                slabs.insert(format!("{:?}", set.slabs));
                meshes.insert(format!("{:?}", bin.asset_id));
                let k = format!("{:?}{:?}{:?}{:?}", set.pipeline, set.draw_function, set.material_bind_group_index, set.slabs);
                *count.entry(k.clone()).or_default() += 1;
                sets.insert(k);
            }
            per_set.extend(count.values());
        }
        per_set.sort_unstable_by(|a, b| b.cmp(a));
        shadow_keys = Some(serde_json::json!({"batch_sets": sets.len(), "pipelines": pipes.len(), "material_bind_groups": groups.len(), "slabs": slabs.len(), "meshes": meshes.len(), "unbatchable_bins": p.0.values().map(|v| v.unbatchable_meshes.len()).sum::<usize>(), "largest_sets_in_bins": per_set.iter().take(6).collect::<Vec<_>>()}));
    }
    if let Some(p) = trans { put("transparent", sorted(&p)); }
    if let Some(k) = shadow_keys {
        m.insert("shadow_keys".into(), k);
    }
    s.0.lock().unwrap().phases = m;
}

#[allow(clippy::type_complexity)]
fn main_stats(
    s: Res<Stats>,
    start: Res<MainStart>,
    clock: Option<Res<super::Clock>>,
    meshes: Res<Assets<Mesh>>,
    std_mats: Res<Assets<StandardMaterial>>,
    q: Query<(&Mesh3d, Option<&MeshMaterial3d<StandardMaterial>>, &ViewVisibility, Has<bevy::light::NotShadowCaster>)>,
    dirs: Query<(&DirectionalLight, Option<&bevy::light::CascadeShadowConfig>, &ViewVisibility)>,
    points: Query<&PointLight>,
    spots: Query<&SpotLight>,
    cams: Query<&Camera>,
    store: Option<Res<bevy::diagnostic::DiagnosticsStore>>,
    mut tris_of: Local<HashMap<AssetId<Mesh>, u64>>,
    (mut ev_std, mut ev_mesh, mut ev_img): (MessageReader<AssetEvent<StandardMaterial>>, MessageReader<AssetEvent<Mesh>>, MessageReader<AssetEvent<Image>>),
    moved: Query<(), (With<Mesh3d>, Changed<GlobalTransform>)>,
    (holders, keys, parents): (Query<(Entity, &MeshMaterial3d<StandardMaterial>)>, Query<&raea_sys_frame::store::RowKey>, Query<&ChildOf>),
) {
    // what this update changed that the render world prepares again: assets added or modified, and meshes moved
    fn tally<A: Asset>(r: &mut MessageReader<AssetEvent<A>>, seen: &mut HashMap<AssetId<A>, &'static str>) -> serde_json::Value {
        let (mut added, mut modified, mut removed) = (0u32, 0u32, 0u32);
        let mut ids = HashSet::new();
        for e in r.read() {
            match e {
                AssetEvent::Added { id } => { added += 1; ids.insert(*id); seen.insert(*id, "added"); }
                AssetEvent::Modified { id } => { modified += 1; ids.insert(*id); seen.entry(*id).or_insert("modified"); }
                AssetEvent::Removed { .. } => removed += 1,
                _ => {}
            }
        }
        serde_json::json!({"added": added, "modified": modified, "removed": removed, "distinct": ids.len()})
    }
    let mut std_seen = HashMap::new();
    let changed = serde_json::json!({"std_materials": tally(&mut ev_std, &mut std_seen), "meshes": tally(&mut ev_mesh, &mut HashMap::new()), "images": tally(&mut ev_img, &mut HashMap::new()), "mesh_transforms": moved.iter().count()});
    // RAEA_SCENE_STATS_WHO=1: which rows hold the materials added or modified this update (their row ids, by kind)
    let mut who: HashMap<String, u32> = HashMap::new();
    if std::env::var_os("RAEA_SCENE_STATS_WHO").is_some() {
        for (e, h) in &holders {
            if let Some(k) = std_seen.get(&h.0.id()) {
                let mut up = e;
                let mut name = String::from("?");
                for _ in 0..8 {
                    if let Ok(rk) = keys.get(up) {
                        name = format!("{}/{}", rk.kind, rk.id);
                        break;
                    }
                    match parents.get(up) { Ok(p) => up = p.parent(), Err(_) => break }
                }
                *who.entry(format!("{k} {name}")).or_default() += 1;
            }
        }
    }
    let main_ms = start.0.map_or(0.0, |t| t.elapsed().as_secs_f64() * 1000.0);
    let (mut all, mut vis, mut tris, mut vis_tris, mut blend, mut vis_blend, mut casters, mut other_mat) = (0u64, 0u64, 0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    let (mut um, mut umat, mut vum, mut vumat) = (HashSet::new(), HashSet::new(), HashSet::new(), HashSet::new());
    let mut per_mesh: HashMap<AssetId<Mesh>, u64> = HashMap::new();
    for (m, mat, v, no_cast) in &q {
        all += 1;
        let id = m.0.id();
        let t = *tris_of.entry(id).or_insert_with(|| {
            meshes.get(id).map_or(0, |me| me.indices().map_or(me.count_vertices(), |i| i.len()) as u64 / 3)
        });
        tris += t;
        um.insert(id);
        let b = mat.and_then(|h| std_mats.get(&h.0)).is_some_and(|sm| !matches!(sm.alpha_mode, AlphaMode::Opaque | AlphaMode::Mask(_)));
        if let Some(h) = mat {
            umat.insert(h.0.id());
        } else {
            other_mat += 1;
        }
        blend += b as u64;
        if !no_cast {
            casters += 1;
        }
        if v.get() {
            vis += 1;
            vis_tris += t;
            vum.insert(id);
            if let Some(h) = mat {
                vumat.insert(h.0.id());
            }
            vis_blend += b as u64;
            *per_mesh.entry(id).or_default() += 1;
        }
    }
    // how many drawn meshes share a mesh with another (instancing's reach): the largest groups
    let mut groups: Vec<u64> = per_mesh.values().copied().filter(|n| *n > 1).collect();
    groups.sort_unstable_by(|a, b| b.cmp(a));
    let shared: u64 = groups.iter().sum();
    let lights = serde_json::json!({
        "directional": dirs.iter().count(),
        "directional_shadows": dirs.iter().filter(|(d, _, _)| d.shadow_maps_enabled).count(),
        "cascades": dirs.iter().filter(|(d, _, _)| d.shadow_maps_enabled).map(|(_, c, _)| c.map_or(0, |c| c.bounds.len())).collect::<Vec<_>>(),
        "point": points.iter().count(),
        "point_shadows": points.iter().filter(|p| p.shadow_maps_enabled).count(),
        "spot": spots.iter().count(),
        "spot_shadows": spots.iter().filter(|p| p.shadow_maps_enabled).count(),
    });
    let mut line = serde_json::Map::new();
    line.insert("frame".into(), serde_json::json!(clock.map(|c| c.0)));
    line.insert("main_ms".into(), serde_json::json!((main_ms * 1000.0).round() / 1000.0));
    line.insert("cameras_active".into(), serde_json::json!(cams.iter().filter(|c| c.is_active).count()));
    line.insert(
        "meshes".into(),
        serde_json::json!({
            "entities": all, "drawn": vis, "unique_meshes": um.len(), "unique_std_materials": umat.len(),
            "drawn_unique_meshes": vum.len(), "drawn_unique_std_materials": vumat.len(), "other_materials": other_mat,
            "tris": tris, "drawn_tris": vis_tris, "blend": blend, "drawn_blend": vis_blend, "shadow_casters": casters,
            "drawn_sharing_a_mesh": shared, "largest_mesh_groups": groups.iter().take(8).collect::<Vec<_>>(),
        }),
    );
    line.insert("lights".into(), lights);
    line.insert("changed".into(), changed);
    if !who.is_empty() {
        line.insert("changed_who".into(), serde_json::json!(who));
    }
    if let Some(store) = store {
        let mut gpu = serde_json::Map::new();
        for d in store.iter() {
            let p = d.path().as_str();
            // each pass's GPU time (`.../elapsed_gpu`, ms) and, where the device has pipeline statistics, its counts
            // (fragment and vertex invocations, clipped primitives: overdraw is fragments over the frame's pixels)
            if p.starts_with("render/") && !p.ends_with("elapsed_cpu") {
                if let Some(v) = d.measurement().map(|m| m.value) {
                    gpu.insert(p.trim_start_matches("render/").to_string(), serde_json::json!((v * 1000.0).round() / 1000.0));
                }
            }
        }
        if !gpu.is_empty() {
            line.insert("gpu_ms".into(), serde_json::Value::Object(gpu));
        }
    }
    s.0.lock().unwrap().main = Some(line);
}
