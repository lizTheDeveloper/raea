//! The 3D kinds raea's 3D SDK writes (sdk/raea/docs/bevy-coverage.md), drawn with Bevy: lights of three types,
//! materials past colour, roughness and metalness, glTF models with their animation clips, the camera's
//! post-processing, and the systems that move all of it frame by frame from the rows. Every value a frame draws is a
//! column of the frame's rows (keys, rules), so a frame is a function of the rows, as everywhere in the engine.
//!
//! Columns (all optional; the defaults are what the engine drew before these existed):
//!   camera: x y z look_x look_y look_z fov near far, ambient ambient_color, bloom (intensity, 0 = none)
//!           bloom_threshold, fog_color fog_start fog_end | fog_density, tonemap ('aces' 'agx' 'tony' 'reinhard'
//!           'none'), exposure (EV100), grade_temperature grade_tint grade_saturation, grade_{s,m,h}_{saturation,
//!           contrast,gamma,gain,lift} (shadows, midtones, highlights), vignette vignette_radius vignette_smooth
//!           vignette_color, aberration, vfog (1: volumetric fog) vfog_steps vfog_dither vfog_resolution vfog_ambient vfog_ambient_color, ssao
//!           ('low' 'medium' 'high' 'ultra') ssao_thickness, aa ('smaa' 'fxaa' 'none'; absent: 4x MSAA), lens_*
//!           (Fisheye and LensDistortion: lens.rs), mb_* (MotionBlur: motion_blur.rs)
//!   light:  type ('directional' default, 'point', 'spot'), color, shadows (1/0), volumetric (1: shafts through
//!           fog volumes; turns shadows on), soft (soft shadows, PCSS: a sun's size in metres as seen from the scene,
//!           a point or spot light's radius);
//!           directional: lux dir_x dir_y dir_z;  point and spot: x y z lumens range radius;
//!           spot: dir_x dir_y dir_z outer inner (radians)
//!   mesh:   shape color rough metal, emissive emissive_k, opacity, unlit, clearcoat clearcoat_rough, reflectance, toon
//!           (a MeshToonMaterial, and its keyed sh_r sh_g sh_b rim_k spec_k outline_w: toon.rs),
//!           map (a picture for the base colour), normal_map, rough_map, metal_map, mr_map (green roughness, blue
//!           metalness), ao_map (red), repeat_u repeat_v (every map tiled that many times a face), x y z rot_x rot_y rot_z (radians, YXZ), scale | scale_x scale_y
//!           scale_z, visible, col_r col_g col_b (an animated colour, sRGB 0..1, wins over color)
//!   model:  src (a .glb or .gltf), scene (index), x y z rot_x rot_y rot_z scale | scale_x.., visible, anim (a clip's
//!           name, or its index as a number), anim_t (seconds into the clip), anim_loop (1 = wrap, 0 = hold the end),
//!           toon (MeshToonMaterials over its glTF materials: toon.rs)
//!   fogvolume: x y z rot_x rot_y rot_z scale | scale_x.. (a unit box of air, scaled), density, color | col_r col_g
//!           col_b, absorption, scattering, anisotropy, visible (Bevy draws every fog volume, so a hidden one is
//!           shrunk to nothing)

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use bevy::prelude::*;
use raea_core_program::eval::Tup;
use raea_core_program::ir::V;
use raea_sys_frame::*;
use raea_sys_frame::bevy_api;
use raea_sys_material::{decal, surface, toon};
use raea_sys_fog::{froxel, froxel_glass};
use raea_sys_post::lens;
use raea_sys_look::paint;
use raea_runtime_plugins as plugins;
use bevy::animation::{AnimationPlayer, graph::{AnimationGraph, AnimationGraphHandle, AnimationNodeIndex}};
use bevy::gltf::Gltf;
use bevy::world_serialization::WorldAssetRoot;

// a row's mesh (raea-sys-material)
pub use raea_sys_material::mesh::*;
// a row's standard material and its maps (raea-sys-material)
pub use raea_sys_material::maps::*;
mod light;
pub use light::*;
mod camera;
pub use camera::*;
mod model;
pub use model::*;
mod sync;
pub use sync::*;
mod shaded;
pub use shaded::*;
mod instance;
pub use instance::*;
mod share;
pub use share::*;
pub use raea_sys_material::clay;

/// Every model's glTF, held while a kept engine loads its next program (Engine::reload), so a model the next shot
/// names again is the asset already loaded, not read and parsed again.
/// The next glTF asked of the asset server, once the last one asked has loaded (or failed): one at a time, in the
/// models' entity order. Bevy gives a glTF's meshes and materials their asset ids while it loads, so glTFs loading
/// together took them in whatever order their loads ran; those ids order the opaque draws (the phase's bins), and
/// where two surfaces meet at the same depth the later draw wins: Ch4's cut at 5186 drew a few pixels of a lid and a
/// crown one of two ways run to run (84-90 dB). RAEA_PARALLEL_GLTF=1 asks for every one at once, as before.
pub fn issue_gltf_loads(world: &mut World) {
    let server = world.resource::<AssetServer>().clone();
    let parallel = std::env::var_os("RAEA_PARALLEL_GLTF").is_some();
    // RAEA_GLTF_ORDER=root: the next glTF asked once the last one's root (its file parsed, its meshes and materials
    // given their ids) has loaded, not once everything it depends on (its images) has: the ids come in the same order,
    // and the images load alongside the next files
    let root = std::env::var("RAEA_GLTF_ORDER").as_deref() == Ok("root");
    let mut q = world.query::<(Entity, &mut Model3d)>();
    loop {
        if !parallel && q.iter(world).any(|(_, m)| m.queued.is_none() && matches!(m.state, ModelState::Loading) && !if root { root_done(&server, &m.gltf) } else { gltf_done(&server, &m.gltf) }) {
            return;
        }
        let Some(path) = q.iter(world).filter_map(|(e, m)| m.queued.as_ref().map(|p| (e, p.clone()))).min_by_key(|(e, _)| *e).map(|(_, p)| p) else { return };
        let h: Handle<Gltf> = server.load(path.clone());
        for (_, mut m) in q.iter_mut(world) {
            if m.queued.as_deref() == Some(path.as_str()) {
                m.gltf = h.clone();
                m.queued = None;
            }
        }
    }
}

fn root_done(server: &AssetServer, h: &Handle<Gltf>) -> bool {
    matches!(server.get_load_state(h), Some(bevy::asset::LoadState::Loaded) | Some(bevy::asset::LoadState::Failed(_)))
}

fn gltf_done(server: &AssetServer, h: &Handle<Gltf>) -> bool {
    matches!(server.get_load_state(h), Some(bevy::asset::LoadState::Failed(_)))
        || matches!(server.get_recursive_dependency_load_state(h), Some(bevy::asset::RecursiveDependencyLoadState::Loaded) | Some(bevy::asset::RecursiveDependencyLoadState::Failed(_)))
}

pub fn model_handles(world: &mut World) -> Vec<Handle<Gltf>> {
    world.query::<&Model3d>().iter(world).map(|m| m.gltf.clone()).collect()
}

/// A model row's glTF: loaded by the asset server, then its scene spawned under the row's entity and its clip set up.
#[derive(Component)]
#[require(Model)]
pub struct Model3d {
    gltf: Handle<Gltf>,
    /// its file, until `issue_gltf_loads` asks for it (one glTF at a time, in entity order: see there)
    queued: Option<String>,
    scene: usize,
    anim: Option<V>,
    state: ModelState,
    src: String,
    /// its row's id (another model's attach names it)
    id: String,
    /// MeshToonMaterials over its materials (toon.rs), applied once its scene's meshes exist
    toon: Vec<toon::ToonSpec>,
    toon_wait: u32,
    /// its materials re-covered (surface.rs), before its toon surfaces
    surfaces: Vec<surface::Surface>,
    /// shaders over its materials (shaded.rs), after its surfaces and toon: each spec with the glTF material it names
    shaders: Vec<(Option<String>, serde_json::Value)>,
    /// how its own glTF textures are read (<Model sampler>, raea-sys-sampler), given once it's ready
    sampler: Option<raea_sys_sampler::Sampling>,
    /// clips blended by weight (<Model animations>): each clip's name or index (the row's `mix`, JSON); their times and
    /// weights are the keyed mix{i}_t and mix{i}_w
    mix: Vec<V>,
    /// the blend's graph nodes, once set playing
    mixer: Vec<(AnimationNodeIndex, Handle<bevy::animation::AnimationClip>)>,
    /// morph targets set by weight (<Model morphs>): each target's name or index (`morph_names`, JSON); the keyed morph{i}
    morphs: Vec<V>,
    /// an authored animation graph (<Model graph>): its nodes depth-first (the row's `graph`, JSON); weights g{i}_w and
    /// clip times g{i}_t keyed
    graph: Vec<GraphPart>,
    /// the graph built: its handle and each node's index (and clip)
    built: Option<(Handle<AnimationGraph>, Vec<(AnimationNodeIndex, Option<Handle<bevy::animation::AnimationClip>>)>)>,
    /// <Instances>: its placements, until its scene is copied to them (instance.rs's instance_models)
    inst: Option<Vec<Transform>>,
}

/// One node of a <Model graph>: a clip (by name or index), a blend (weights normalised) or an additive blend, under
/// its parent (an earlier node; None for the graph's root), with the bones whose subtrees it leaves alone.
#[derive(Clone, Debug)]
pub struct GraphPart {
    kind: String,
    clip: Option<V>,
    parent: Option<usize>,
    mask: Vec<String>,
}

fn graph_col(vals: &BTreeMap<String, V>) -> Result<Vec<GraphPart>, String> {
    let Some(V::S(s)) = vals.get("graph") else { return Ok(vec![]) };
    let v: serde_json::Value = serde_json::from_str(s).map_err(|e| format!("graph isn't JSON: {e}"))?;
    let a = v.as_array().ok_or("graph is a list of nodes")?;
    a.iter().enumerate().map(|(i, n)| {
        let kind = n.get("kind").and_then(|k| k.as_str()).unwrap_or("").to_string();
        if !["clip", "blend", "add"].contains(&kind.as_str()) {
            return Err(format!("graph node {i}: kind is clip, blend or add, got {kind:?}"));
        }
        let parent = match n.get("parent").and_then(|p| p.as_i64()) {
            Some(p) if p >= 0 && (p as usize) < i => Some(p as usize),
            Some(p) if p < 0 => None,
            None => None,
            Some(p) => return Err(format!("graph node {i}: its parent {p} isn't an earlier node")),
        };
        let clip = match n.get("clip") {
            Some(serde_json::Value::String(c)) => Some(V::s(c.clone())),
            Some(serde_json::Value::Number(c)) => c.as_f64().map(V::N),
            _ => None,
        };
        if kind == "clip" && clip.is_none() {
            return Err(format!("graph node {i}: a clip node names its clip"));
        }
        let mask = n.get("mask").and_then(|m| m.as_array()).map(|m| m.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
        Ok(GraphPart { kind, clip, parent, mask })
    }).collect()
}

/// A JSON list column of clip or morph-target names and indices.
fn names_col(vals: &BTreeMap<String, V>, k: &str) -> Vec<V> {
    let Some(V::S(s)) = vals.get(k) else { return vec![] };
    let Ok(serde_json::Value::Array(a)) = serde_json::from_str::<serde_json::Value>(s) else { return vec![] };
    a.iter().filter_map(|x| match x {
        serde_json::Value::String(n) => Some(V::s(n.clone())),
        serde_json::Value::Number(n) => n.as_f64().map(V::N),
        _ => None,
    }).collect()
}

impl Model3d {
    /// Loaded, spawned and its clip set up.
    pub fn ready(&self) -> bool {
        matches!(self.state, ModelState::Ready { .. })
    }

    /// Ready, with its own surfaces, toon and shaders on (or given up on): what a scene's <SurfaceShader> waits for.
    pub fn settled(&self) -> bool {
        self.ready() && self.surfaces.is_empty() && self.toon.is_empty() && self.shaders.is_empty() && self.inst.is_none()
    }
}

/// A model's scene root whose hierarchy Bevy has finished spawning (its WorldInstanceReady).
#[derive(Component)]
pub struct SceneSpawned;

#[derive(PartialEq)]
enum ModelState {
    Loading,
    /// its scene's root, spawning until Bevy's WorldInstanceReady marks it `SceneSpawned`
    Spawned(Entity),
    Ready { player: Option<(Entity, AnimationNodeIndex, Handle<bevy::animation::AnimationClip>)> },
}

/// Each model's clip at the frame's time (anim_t seconds): seeked, never advanced by the clock, so the pose is a
/// function of the frame.
/// A model's row is read where its values are (FrameRows::row), and the g{i}_ and mix{i}_ column names are made once,
/// not formatted per node per frame.
pub fn sync_models(frame: Res<FrameRows>, clips: Res<Assets<bevy::animation::AnimationClip>>, models: Query<(Entity, &Model3d)>, mut players: Query<&mut AnimationPlayer>, mut graphs: ResMut<Assets<AnimationGraph>>, mut names: Local<(Vec<[String; 2]>, Vec<[String; 2]>)>) {
    let (gk, mk) = &mut *names;
    for (e, m) in &models {
        let ModelState::Ready { player: Some((p, node, ref clip)) } = m.state else { continue };
        let Some(v) = frame.row(e) else { continue };
        let looped = v.get("anim_loop").is_none_or(|x| x.truthy());
        if let Some((h, made)) = &m.built {
            // an authored graph: each clip seeked to its time and weighted as its player animation; a blend or add
            // node's weight on the graph (changed only when it changes: a changed graph is re-threaded)
            let Ok(mut pl) = players.get_mut(p) else { continue };
            while gk.len() < made.len() {
                let i = gk.len();
                gk.push([format!("g{i}_w"), format!("g{i}_t")]);
            }
            for (i, (n, c)) in made.iter().enumerate() {
                let w = v.num(&gk[i][0], 1.0).max(0.0);
                match c {
                    Some(c) => {
                        let mut at = v.num(&gk[i][1], 0.0).max(0.0);
                        let len = clips.get(c).map(|c| c.duration()).unwrap_or(0.0);
                        if len > 0.0 {
                            at = if looped { at % len } else { at.min(len) };
                        }
                        if let Some(a) = pl.animation_mut(*n) {
                            if a.seek_time() != at || !a.is_paused() {
                                a.seek_to(at).pause();
                            }
                            if a.weight() != w {
                                a.set_weight(w);
                            }
                        }
                    }
                    None => {
                        if graphs.get(h).and_then(|g| g.get(*n)).is_some_and(|g| g.weight != w) {
                            if let Some(mut gr) = graphs.get_mut(h) {
                                if let Some(g) = gr.get_mut(*n) {
                                    g.weight = w;
                                }
                            }
                        }
                    }
                }
            }
            continue;
        }
        if !m.mixer.is_empty() {
            // each blended clip at its own time and weight
            let Ok(mut pl) = players.get_mut(p) else { continue };
            while mk.len() < m.mixer.len() {
                let i = mk.len();
                mk.push([format!("mix{i}_t"), format!("mix{i}_w")]);
            }
            for (i, (n, c)) in m.mixer.iter().enumerate() {
                let mut at = v.num(&mk[i][0], 0.0).max(0.0);
                let len = clips.get(c).map(|c| c.duration()).unwrap_or(0.0);
                if len > 0.0 {
                    at = if looped { at % len } else { at.min(len) };
                }
                let w = v.num(&mk[i][1], 1.0).max(0.0);
                if let Some(a) = pl.animation_mut(*n) {
                    if a.seek_time() != at || !a.is_paused() {
                        a.seek_to(at).pause();
                    }
                    if a.weight() != w {
                        a.set_weight(w);
                    }
                }
            }
            continue;
        }
        let mut at = v.num("anim_t", 0.0).max(0.0);
        // looped (anim_loop, the default) or held on its last pose
        let len = clips.get(clip).map(|c| c.duration()).unwrap_or(0.0);
        if len > 0.0 {
            at = if looped { at % len } else { at.min(len) };
        }
        if let Ok(mut pl) = players.get_mut(p) {
            if let Some(a) = pl.animation_mut(node) {
                if a.seek_time() != at || !a.is_paused() {
                    a.seek_to(at).pause();
                }
            }
        }
    }
}

/// Each model's morph targets at the frame's weights (morph{i}), by target name (the mesh's morph_target_names) or index,
/// on every MorphWeights in its scene that has that target.
pub fn sync_morphs(frame: Res<FrameRows>, models: Query<(Entity, &Model3d)>, kids: Query<&Children>, mut weights: Query<&mut bevy::mesh::morph::MorphWeights>, meshes: Res<Assets<Mesh>>, mut err: ResMut<Error>, mut keys: Local<Vec<String>>) {
    for (e, m) in &models {
        if m.morphs.is_empty() || !matches!(m.state, ModelState::Ready { .. }) {
            continue;
        }
        // read where its values are (FrameRows::row), the morph{i} names made once
        let Some(v) = frame.row(e) else { continue };
        while keys.len() < m.morphs.len() {
            let i = keys.len();
            keys.push(format!("morph{i}"));
        }
        let mut found = vec![false; m.morphs.len()];
        for d in descendants(e, &kids) {
            let Ok(mut mw) = weights.get_mut(d) else { continue };
            let names: Vec<String> = mw.first_mesh().and_then(|h| meshes.get(h)).and_then(|me| me.morph_target_names()).map(|n| n.to_vec()).unwrap_or_default();
            let len = mw.weights().len();
            for (i, key) in m.morphs.iter().enumerate() {
                let ix = match key {
                    V::S(n) => names.iter().position(|x| x.as_str() == &**n),
                    V::N(x) => Some(*x as usize).filter(|&x| x < len),
                };
                let Some(ix) = ix else { continue };
                found[i] = true;
                let w = v.num(&keys[i], 0.0);
                if mw.weights()[ix] != w {
                    mw.weights_mut()[ix] = w;
                }
            }
        }
        if let Some(i) = found.iter().position(|f| !f) {
            err.0 = Some(format!("model {}: no morph target {:?} on any of its meshes", m.src, m.morphs[i]));
        }
    }
}
