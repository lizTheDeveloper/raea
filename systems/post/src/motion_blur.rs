//! The camera's motion blur: raea/3d's MotionBlur, Bevy's own MotionBlur on the camera, its values keyed per
//! frame, so a blur ramp plays as it renders; a frame without it has the component removed, so off is Bevy without it.
//!
//! Camera columns: mb_on (keyed: 1 where the frame has a MotionBlur), mb_shutter (the shutter angle, a fraction of a
//!   frame, keyed), mb_samples
//!
//! Motion blur is made a function of the frame: Bevy takes each mesh's, skin's and the camera's previous pose from the
//! update before (its GPU instance builder keeps the last update's transforms), which in raea may be the same frame (a
//! careful readback updates a frame several times) or a frame jumped from. So with MotionBlur the engine draws frame
//! f - 1 before every update of frame f (`Engine::pre_step`, its readback ignored), and `pin_frame_count` sets the
//! shaders' frame counter (the blur's noise) to the frame: every update of frame f draws the same blur.

use super::*;
use bevy::core_pipeline::prepass::{DepthPrepass, MotionVectorPrepass};
use bevy::post_process::motion_blur::MotionBlur;

/// Whether a camera row has a MotionBlur in some frame.
pub fn has_blur(vals: &BTreeMap<String, V>) -> bool {
    vals.contains_key("mb_on")
}

/// What a camera row with motion blur needs from its load: the depth and motion vector prepasses the blur reads.
pub fn load(world: &mut World, cam: Entity, vals: &BTreeMap<String, V>) {
    if has_blur(vals) {
        world.entity_mut(cam).insert((DepthPrepass, MotionVectorPrepass));
    }
}

/// The frame's blur, None where it is off.
fn blur_of(v: &BTreeMap<String, V>) -> Option<MotionBlur> {
    let b = MotionBlur { shutter_angle: num(v, "mb_shutter", 0.5).max(0.0), samples: num(v, "mb_samples", 4.0).round().clamp(1.0, 32.0) as u32 };
    (num(v, "mb_on", 0.0) > 0.5 && b.shutter_angle > 0.0).then_some(b)
}

/// The frame's blur onto the camera: inserted, changed or removed as the frame's columns say.
pub fn sync_motion_blur(frame: Res<FrameRows>, mut commands: Commands, cams: Query<(Entity, Option<&MotionBlur>), With<Camera3d>>) {
    for (e, mb) in &cams {
        let Some(v) = frame.vals(e) else { continue };
        if !has_blur(&v) {
            continue;
        }
        match (blur_of(&v), mb) {
            (Some(want), Some(m)) if m.shutter_angle == want.shutter_angle && m.samples == want.samples => {}
            (Some(want), _) => {
                commands.entity(e).insert(want);
            }
            (None, Some(_)) => {
                commands.entity(e).remove::<MotionBlur>();
            }
            (None, None) => {}
        }
    }
}

/// The shaders' frame counter at the frame being drawn, so every update of frame f, in any batch, samples the same:
/// Bevy seeds noise with it (motion blur, specular transmission's blur, SSAO, contact shadows, volumetric jitter),
/// and counting updates made a frame's noise depend on where its batch began (Ch2 @2700's glass panels: 35 dB between
/// the frame mid-batch and drawn alone). Pinned with motion blur only, before. RAEA_FRAME_COUNT=updates counts updates
/// (Bevy's own), except under motion blur.
pub fn pin_frame_count(clock: Res<Clock>, cams: Query<Has<MotionBlur>, With<Camera3d>>, mut count: ResMut<bevy::diagnostic::FrameCount>) {
    if !cams.iter().any(|mb| mb) && std::env::var("RAEA_FRAME_COUNT").as_deref() == Ok("updates") {
        return;
    }
    let want = clock.0.max(0) as u32;
    if count.0 != want {
        count.0 = want;
    }
}

/// The composition's clock in seconds (frame / fps), extracted into the render world for pin_shader_time.
#[derive(Resource, Default, Clone, Copy)]
pub struct ShaderClock(Option<f64>, f64);

fn extract_shader_clock(mut commands: Commands, clock: bevy::render::Extract<Option<Res<Clock>>>, edit: bevy::render::Extract<Option<Res<Edit>>>) {
    let fps = edit.as_ref().map_or(0.0, |e| e.prog.fps);
    let secs = clock.as_ref().filter(|_| fps > 0.0).map(|c| c.0.max(0) as f64 / fps);
    commands.insert_resource(ShaderClock(secs, fps));
}

/// The shaders' `globals.time` at the frame being drawn (frame / fps, the composition's clock) and its delta one frame,
/// in the render world (the main world's Time, which plugins step, is left be). Bevy's was the engine's own elapsed
/// time, so a frame's time-driven shading (fx/look's holo, fuzz and dissolve noise, its baked curves' steps; a stills
/// render measured 0 at frames 60, 61 and 140 of one batch) depended on where its batch began. RAEA_SHADER_TIME=real
/// keeps Bevy's.
fn pin_shader_time(clock: Res<ShaderClock>, mut time: ResMut<Time>) {
    let (Some(secs), fps) = (clock.0, clock.1) else { return };
    let want = std::time::Duration::from_secs_f64(secs);
    if time.elapsed() == want {
        return;
    }
    let mut t = Time::<()>::default();
    let step = std::time::Duration::from_secs_f64(1.0 / fps);
    if want >= step {
        t.advance_to(want - step);
    }
    t.advance_to(want);
    *time = t;
}

/// The systems, on every engine (they do nothing without a MotionBlur).
pub fn install(app: &mut App) {
    if std::env::var("RAEA_SHADER_TIME").as_deref() != Ok("real") {
        if let Some(r) = app.get_sub_app_mut(bevy::render::RenderApp) {
            r.init_resource::<ShaderClock>()
                .add_systems(bevy::render::ExtractSchedule, extract_shader_clock)
                .add_systems(bevy::render::Render, pin_shader_time.in_set(bevy::render::RenderSystems::CreateViews));
        }
    }
    app.add_systems(Last, pin_frame_count.after(bevy::diagnostic::update_frame_count));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_frames_and_zero_shutters_are_no_blur() {
        let mut v = BTreeMap::new();
        v.insert("mb_on".to_string(), V::N(0.0));
        v.insert("mb_shutter".to_string(), V::N(1.0));
        assert!(blur_of(&v).is_none());
        v.insert("mb_on".to_string(), V::N(1.0));
        let b = blur_of(&v).unwrap();
        assert_eq!((b.shutter_angle, b.samples), (1.0, 4));
        v.insert("mb_shutter".to_string(), V::N(0.0));
        assert!(blur_of(&v).is_none());
    }
}
