//! Opaque draws in an order of the scene's own, not of the engine's history. Bevy sorts a binned phase by its keys:
//! pipeline id, material slot, mesh slab and mesh asset id, each handed out in the order things were first made in
//! this engine. Where two surfaces meet at the same depth the later draw wins, so a kept engine (Engine::reload), which
//! made its pipelines, materials and meshes in another shot's order, drew a few tie pixels differently from the same
//! shot opened fresh (Ch5 2460 after 2400: 1-4 pixels on 10 of 60 frames). Here, after Bevy's sort, each bin goes by the
//! least scene key of its meshes (the mesh's row and its path of child places under it), bins of one batch set kept
//! together by their set's least key, so the order is a function of the shot alone. RAEA_DRAW_ORDER=bevy keeps Bevy's.

use super::*;
use bevy::platform::collections::HashMap;
use bevy::render::render_phase::{BinnedPhaseItem, ViewBinnedRenderPhases};
use bevy::render::sync_world::MainEntity;

/// A mesh entity's place in the scene as a number: its row (kind and id) and the child places from that row down to it
/// (an entity under no row: its Name, else the entity itself).
#[derive(Component, Clone, Copy)]
pub(crate) struct SceneKey(pub u64);

fn fnv(h: u64, bytes: &[u8]) -> u64 {
    bytes.iter().fold(h, |h, b| (h ^ *b as u64).wrapping_mul(0x100000001b3))
}

/// An entity's scene key: its row, and the child places from that row down to it.
fn place_key(e: Entity, parents: &Query<&ChildOf>, children: &Query<&Children>, rows: &Query<&raea_sys_frame::store::RowKey>, names: &Query<&Name>) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    let mut x = e;
    loop {
        if let Ok(r) = rows.get(x) {
            return fnv(fnv(fnv(h, r.kind.as_bytes()), b"/"), r.id.as_bytes());
        }
        match parents.get(x) {
            Ok(p) => {
                let parent = p.parent();
                let at = children.get(parent).ok().and_then(|c| c.iter().position(|c| c == x)).unwrap_or(usize::MAX);
                h = fnv(h, &(at as u64).to_le_bytes());
                x = parent;
            }
            Err(_) => {
                return match names.get(x) {
                    Ok(n) => fnv(fnv(h, b"name:"), n.as_str().as_bytes()),
                    Err(_) => fnv(fnv(h, b"entity:"), &x.to_bits().to_le_bytes()),
                };
            }
        }
    }
}

/// Keys for meshes and lights new or moved under another parent this update (main world, before extraction).
#[allow(clippy::type_complexity)]
pub(crate) fn scene_keys(
    mut commands: Commands,
    meshes: Query<Entity, (With<Mesh3d>, Or<(Added<Mesh3d>, Changed<ChildOf>)>)>,
    lights: Query<Entity, (Or<(With<bevy::light::PointLight>, With<bevy::light::SpotLight>, With<bevy::light::DirectionalLight>)>, Or<(Added<bevy::light::PointLight>, Added<bevy::light::SpotLight>, Added<bevy::light::DirectionalLight>, Changed<ChildOf>)>)>,
    parents: Query<&ChildOf>,
    children: Query<&Children>,
    rows: Query<&raea_sys_frame::store::RowKey>,
    names: Query<&Name>,
) {
    for e in meshes.iter().chain(lights.iter()) {
        commands.entity(e).insert(SceneKey(place_key(e, &parents, &children, &rows, &names)));
    }
}

/// Each mesh's scene key by main-world entity (render world).
#[derive(Resource, Default)]
pub(crate) struct SceneKeys(pub(crate) HashMap<MainEntity, u64>);

/// (and the lights' into bevy_pbr's LightOrderKeys: the order the shader sums them in, vendor/bevy_pbr)
pub(crate) fn extract_scene_keys(mut keys: ResMut<SceneKeys>, mut light_keys: ResMut<bevy::pbr::LightOrderKeys>, q: bevy::render::Extract<Query<(Entity, &SceneKey, Has<Mesh3d>), Changed<SceneKey>>>) {
    for (e, k, mesh) in &q {
        if mesh {
            keys.0.insert(MainEntity::from(e), k.0);
        } else {
            light_keys.0.insert(MainEntity::from(e), k.0);
        }
    }
}

fn key(keys: &SceneKeys, e: &MainEntity) -> u64 {
    keys.0.get(e).copied().unwrap_or_else(|| e.id().to_bits())
}

/// A binned phase's bins in scene order (after Bevy's PhaseSort, before batching).
pub(crate) fn scene_order<P: BinnedPhaseItem>(keys: Res<SceneKeys>, mut phases: ResMut<ViewBinnedRenderPhases<P>>) {
    for phase in phases.values_mut() {
        if phase.batchable_meshes.len() > 1 {
            let mut set_least: HashMap<P::BatchSetKey, u64> = HashMap::default();
            for ((set, _), bin) in phase.batchable_meshes.iter() {
                let least = bin.entities().keys().map(|e| key(&keys, e)).min().unwrap_or(u64::MAX);
                let s = set_least.entry(set.clone()).or_insert(u64::MAX);
                *s = (*s).min(least);
            }
            phase.batchable_meshes.sort_by_cached_key(|(set, _), bin| (set_least[set], bin.entities().keys().map(|e| key(&keys, e)).min().unwrap_or(u64::MAX)));
        }
        if phase.unbatchable_meshes.len() > 1 {
            let mut set_least: HashMap<P::BatchSetKey, u64> = HashMap::default();
            for ((set, _), bin) in phase.unbatchable_meshes.iter() {
                let least = bin.entities.keys().map(|e| key(&keys, e)).min().unwrap_or(u64::MAX);
                let s = set_least.entry(set.clone()).or_insert(u64::MAX);
                *s = (*s).min(least);
            }
            phase.unbatchable_meshes.sort_by_cached_key(|(set, _), bin| (set_least[set], bin.entities.keys().map(|e| key(&keys, e)).min().unwrap_or(u64::MAX)));
        }
    }
}

/// On the engine (unless RAEA_DRAW_ORDER=bevy).
pub(crate) fn install(app: &mut App) {
    if std::env::var("RAEA_DRAW_ORDER").as_deref() == Ok("bevy") {
        return;
    }
    use bevy::core_pipeline::core_3d::{AlphaMask3d, Opaque3d};
    use bevy::core_pipeline::prepass::{AlphaMask3dPrepass, Opaque3dPrepass};
    use bevy::render::{Render, RenderSystems as RS};
    app.add_systems(PostUpdate, scene_keys.after(bevy::transform::TransformSystems::Propagate));
    app.sub_app_mut(bevy::render::RenderApp)
        .init_resource::<SceneKeys>()
        .init_resource::<bevy::pbr::LightOrderKeys>()
        .add_systems(bevy::render::ExtractSchedule, extract_scene_keys)
        .add_systems(
            Render,
            (scene_order::<Opaque3d>, scene_order::<AlphaMask3d>, scene_order::<Opaque3dPrepass>, scene_order::<AlphaMask3dPrepass>, scene_order::<bevy::pbr::Shadow>)
                .after(RS::PhaseSort)
                .before(RS::Prepare),
        );
}
