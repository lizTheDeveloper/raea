//! Transparent draws in one order whatever came before: Bevy sorts each view's Transparent3d phase by distance alone,
//! with a stable sort over a phase it keeps from frame to frame, so items at the same distance (layered lace rings,
//! engraved letters over their plate, glass plates sharing a centre) kept whatever order they were inserted in, which
//! changed as parts went in and out of view (Ch1 v7.7's lace: whole rings swapping light and dark about 10 times a
//! second) and with where a batch began. Here, after Bevy's sort: distance, then a key fixed per entity (its Name's hash,
//! else its row's, else the entity), so a tie is broken the same way on every frame. (Distances are compared exactly:
//! rounding them made parts a hair apart swap at every rounding boundary, tools/checks/transparent-ties.)
//! RAEA_TRANSPARENT_SORT=bevy keeps Bevy's.

use super::*;
use bevy::core_pipeline::core_3d::Transparent3d;
use bevy::render::render_phase::ViewSortedRenderPhases;
use bevy::render::sync_world::MainEntity;

/// Each mesh entity's tie-break key, by main-world entity (render world).
#[derive(Resource, Default)]
pub(crate) struct TieKeys(bevy::platform::collections::HashMap<MainEntity, u64>);

fn hash(s: &str) -> u64 {
    // FNV-1a: the same on every run and machine
    s.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

/// Keys for meshes new or renamed this frame.
#[allow(clippy::type_complexity)]
pub(crate) fn extract_tie_keys(mut keys: ResMut<TieKeys>, q: bevy::render::Extract<Query<(Entity, Option<&Name>, Option<&raea_sys_frame::store::RowKey>), (With<Mesh3d>, Or<(Added<Mesh3d>, Changed<Name>)>)>>) {
    for (e, name, row) in &q {
        let k = match (name, row) {
            (Some(n), _) => hash(n.as_str()),
            (None, Some(r)) => hash(&format!("{}/{}", r.kind, r.id)),
            (None, None) => e.to_bits(),
        };
        keys.0.insert(MainEntity::from(e), k);
    }
}

/// Then, past the name, the mesh's place in the scene (dsort.rs's SceneKey: its row and child path), and only then the
/// entity: two parts of one glTF share a name (Ch2's glass plates), and an entity's bits differ between a kept engine
/// and a fresh one, and between two fresh ones whose worlds made one more resource first (sub-1/255 on Ch2 @14189).
fn tie(keys: &TieKeys, scene: &super::dsort::SceneKeys, e: &MainEntity) -> (u64, u64, u64) {
    (keys.0.get(e).copied().unwrap_or(0), scene.0.get(e).copied().unwrap_or(0), e.id().to_bits())
}

pub(crate) fn stable_transparent_order(keys: Res<TieKeys>, scene: Option<Res<super::dsort::SceneKeys>>, mut phases: ResMut<ViewSortedRenderPhases<Transparent3d>>) {
    let empty = super::dsort::SceneKeys::default();
    let scene = scene.as_deref().unwrap_or(&empty);
    for phase in phases.values_mut() {
        phase.items.sort_by(|_, a, _, b| a.distance.total_cmp(&b.distance).then_with(|| tie(&keys, scene, &a.entity.1).cmp(&tie(&keys, scene, &b.entity.1))));
    }
}

/// Transmissive draws (glass that refracts what's behind it) the same way: Bevy sorts them by distance alone, ties in
/// the order they were queued.
pub(crate) fn stable_transmissive_order(keys: Res<TieKeys>, scene: Option<Res<super::dsort::SceneKeys>>, mut phases: ResMut<ViewSortedRenderPhases<bevy::pbr::Transmissive3d>>) {
    let empty = super::dsort::SceneKeys::default();
    let scene = scene.as_deref().unwrap_or(&empty);
    for phase in phases.values_mut() {
        phase.items.sort_by(|_, a, _, b| a.distance.total_cmp(&b.distance).then_with(|| tie(&keys, scene, &a.entity.1).cmp(&tie(&keys, scene, &b.entity.1))));
    }
}

/// On the engine's render app (unless RAEA_TRANSPARENT_SORT=bevy).
pub(crate) fn install(app: &mut App) {
    if std::env::var("RAEA_TRANSPARENT_SORT").as_deref() == Ok("bevy") {
        return;
    }
    app.sub_app_mut(bevy::render::RenderApp)
        .init_resource::<TieKeys>()
        .add_systems(bevy::render::ExtractSchedule, extract_tie_keys)
        .add_systems(bevy::render::Render, (stable_transparent_order, stable_transmissive_order).after(bevy::render::RenderSystems::PhaseSort).before(bevy::render::RenderSystems::Prepare));
}
