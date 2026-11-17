//! A program loaded into a running Engine: its rows spawned, its clips and media opened, its overlay keyed.

use super::*;

impl Engine {
    /// Re-mirror the tables into the World (after a move). The size must not change (make a new Engine for that).
    pub fn load(&mut self, prog: &Program, inserted: &[Tup]) -> Result<(), String> {
        self.load_shared(prog, inserted, None)
    }

    /// As load; a timeline passed in must be this edit's (its rows in the edit's order), and is used as it is.
    /// Another program of the same size loaded into this engine as a fresh one opens it: the World's rows replaced
    /// (load_shared) and warmed up by the same 8 updates (finish_build), its device, pipelines, models and textures
    /// kept. A served process's next native shot (docs/native-shots.md).
    pub fn reload(&mut self, prog: &Program, inserted: &[Tup]) -> Result<(), String> {
        // the clocks and physics as a fresh engine's, before the new program's rows set any of them
        let t = std::time::Instant::now();
        pristine_restore(self.app.world_mut());
        // the text atlas emptied before the new program's text is spawned, as a fresh engine's is (emptied after the
        // spawn, the new text was never drawn into it: a kept engine's repeat of Ch5 lost its clock's numerals)
        text3d::reset_atlas(self.app.world_mut());
        let t_reset = t.elapsed().as_secs_f64() * 1e3;
        self.load_shared(prog, inserted, None)?;
        let t_load = t.elapsed().as_secs_f64() * 1e3;
        let mut ups = Vec::new();
        for _ in 0..8 {
            let u = std::time::Instant::now();
            self.app.update();
            ups.push(u.elapsed().as_secs_f64() * 1e3);
        }
        if std::env::var_os("RAEA_NATIVE_TRACE").is_some() {
            eprintln!("raea native: reload {:.0} ms: restore {t_reset:.0}, load {:.0}, warm-up updates {}", t.elapsed().as_secs_f64() * 1e3, t_load - t_reset, ups.iter().map(|u| format!("{u:.0}")).collect::<Vec<_>>().join(" "));
        }
        self.drawn = Some(self.app.world().resource::<Clock>().0);
        self.pipe = raea_sys_frame::pipe::Pipe::new();
        state_dump(&mut self.app, "reload");
        Ok(())
    }

    pub fn load_shared(&mut self, prog: &Program, inserted: &[Tup], timeline: Option<std::sync::Arc<raea_core_program::timeline::Timeline>>) -> Result<(), String> {
        if (prog.width, prog.height) != self.size() {
            return Err("the edit's size changed: make a new Engine".into());
        }
        // kept (and the frames before a frame stepped through: catch_up) only when the rows need steps (#77)
        self.replay = (plugins::active(self.app.world()) && plugins::steps(inserted)).then(|| (prog.clone(), inserted.to_vec()));
        self.drawn = None;
        let world = self.app.world_mut();
        let g = world.resource::<Gen>().0;
        world.insert_resource(Gen(if g == 255 { 1 } else { g + 1 }));
        world.resource_mut::<Out>().0.clear();
        #[cfg(feature = "native")]
        world.insert_resource(OverlayKey::default());
        // the last load's material maps go with its meshes (scene3d); made only by a load that uses one
        if let Some(mut maps) = world.get_resource_mut::<scene3d::Maps>() {
            maps.0.clear();
        }
        #[cfg(feature = "native")]
        {
                self.warm = false;
        }
        // the rows go through the store; what else the last load spawned (overlay, tag, raea's leftovers) goes too
        store::clear(world);
        world.despawn_all::<With<Mirror>>();
        let ents = store::load(world, inserted);
        mem_mark("load: rows stored");
        #[cfg(feature = "native")]
        let mut rows2d: Vec<(String, serde_json::Value, Entity)> = Vec::new();
        let mut camera = None;
        // an edit with no 3D or 2D mesh rows draws nothing through a mesh pipeline: its camera only clears, so it gets no
        // MSAA (4x targets at full size were most of the start-up memory); a 2D one's shares the 2D camera's (sprite)
        let has_3d = has_3d(inserted) || raea_sys_sprite::has_2d(inserted);
        // cameras first, so UI can target them; otherwise insertion order
        // the rows' map files decoded and mipped across the cores before the spawn takes them one by one
        raea_sys_material::maps::prefetch_maps(inserted, &self.dir);
        mem_mark("load: maps prefetched");
        let mut order: Vec<(&Tup, Entity)> = inserted.iter().zip(ents.iter().copied()).collect();
        // RAEA_MEM_MARKS: each 3D kind's spawn time, summed
        let marks = std::env::var_os("RAEA_MEM_MARKS").is_some();
        let mut by_kind: std::collections::BTreeMap<String, (usize, f64)> = Default::default();
        order.sort_by_key(|(t, _)| t.kind != "camera");
        for (t, e) in order {
            let vals = &t.vals;
            match t.kind.as_str() {
                "camera" => {
                    spawn_camera(world, e, vals, inserted, &self.dir, (prog.width, prog.height), RenderTarget::Image(self.target.clone().into()), has_3d)?;
                    camera.get_or_insert(e); // (the first camera: a second draws over it, two_passes)
                }
                k if is_3d_spawn(k) => {
                    let t0 = std::time::Instant::now();
                    spawn_3d(world, e, t, inserted, &self.dir)?;
                    if marks {
                        let x = by_kind.entry(k.to_string()).or_insert((0, 0.0));
                        x.0 += 1;
                        x.1 += t0.elapsed().as_secs_f64() * 1e3;
                    }
                }
                // without native a clip or a 2D row is a row only (a browser's page draws its own video and text)
                #[cfg(feature = "native")]
                "clip" if world.resource::<Composite>().0 => {} // a row only: clips_gpu reads it
                #[cfg(feature = "native")]
                "clip" => {
                    if raea_runtime_view::masks::mask_of(vals).is_some() {
                        return Err(format!("clip {}: a mask needs the composite (kgpu) path, and this render draws clips as UI images", t.id));
                    }
                    let image = world.resource_mut::<Assets<Image>>().add(Image::new_fill(
                        bevy::render::render_resource::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
                        bevy::render::render_resource::TextureDimension::D2,
                        &[0, 0, 0, 0],
                        TextureFormat::Rgba8UnormSrgb,
                        bevy::asset::RenderAssetUsages::default(),
                    ));
                    let Some(cam) = camera else { return Err("the edit needs a camera before its clips".into()) };
                    world.entity_mut(e).insert((
                        Node { position_type: PositionType::Absolute, ..default() },
                        ImageNode::new(image.clone()),
                        Clip { image, last: None },
                        Visibility::Hidden,
                        GlobalZIndex(0),
                        UiTargetCamera(cam),
                    ));
                }
                #[cfg(feature = "native")]
                k if KINDS_2D.contains(&k) => {
                    let (id, row) = row2d(t, inserted);
                    rows2d.push((id, row, e));
                }
                _ => {} // data kinds (face, cut, ...): rows only, read by rules
            }
        }
        raea_sys_material::maps::prefetch_done();
        mem_mark("load: cameras and 3D spawned");
        if marks {
            let mut v: Vec<_> = by_kind.into_iter().collect();
            v.sort_by(|a, b| b.1 .1.total_cmp(&a.1 .1));
            for (k, (n, ms)) in v.iter().take(8) {
                eprintln!("mem_mark   spawn {k}: {n} rows, {ms:.0} ms");
            }
        }
        // MotionBlur's previous poses: a pre-step before every update (motion_blur.rs)
        self.prestep = inserted.iter().any(|t| t.kind == "camera" && t.vals.contains_key("mb_on"));
        // plugin component and resource rows through the TypeRegistry, and the frame clock back at 0 (plugins.rs)
        plugins::apply_rows(world, inserted, &ents, &self.dir)?;
        // shaders over lit surfaces (docs/shaders.feature SH1-SH4), once their textures' "render:NAME" images exist
        mem_mark("load: plugin rows applied");
        world.remove_resource::<scene3d::SceneShader>();
        scene3d::shade_rows(world, inserted)?;
        // a camera on a bare <Entity> (a second view, a viewport) draws into the edit's frame unless its rows give it a
        // RenderTarget of its own (a "render:NAME" image)
        for (t, &e) in inserted.iter().zip(ents.iter()) {
            if t.kind != "entity" || world.get::<Camera>(e).is_none() {
                continue;
            }
            let own = inserted.iter().any(|c| c.kind == "component" && c.vals.get("target").is_some_and(|v| matches!(v, V::S(x) if *x == t.id)) && c.vals.get("type").is_some_and(|v| matches!(v, V::S(x) if x.ends_with("RenderTarget"))));
            if !own {
                world.entity_mut(e).insert(RenderTarget::Image(self.target.clone().into()));
            }
        }
        // the 2D rows drawn by bevy_ui over the 3D (rows2d.rs); each row's entity keeps its id and constants. None, no
        // page: an empty one was drawn and read back anyway (the fed render's 3D, whose 2D is raea's page; B1d)
        #[cfg(feature = "native")]
        if !rows2d.is_empty() {
            let instances: Vec<serde_json::Value> = rows2d.iter().map(|(_, r, _)| r.clone()).collect();
            let rows = raea_runtime_view::rows2d::Rows::new(&instances, prog.width, prog.height, prog.frames.max(1), &self.dir)?;
            for (id, _, e) in rows2d {
                let base = Base2d(rows.base(&id).cloned().unwrap_or_default());
                world.entity_mut(e).insert((base, Row2d(id)));
            }
            world.insert_non_send(Rows2d(rows));
        } else {
            world.remove_non_send::<Rows2d>();
        }
        mem_mark("load: shaders and 2D rows");
        let base = store::gather(world);
        let ix: bevy::ecs::entity::EntityHashMap<usize> = store::rows(world).iter().enumerate().map(|(r, &e)| (e, r)).collect();
        let media = inserted.iter().filter(|t| t.kind == "media").cloned().collect();
        let timeline = match timeline {
            Some(t) => t,
            None => std::sync::Arc::new(raea_core_program::timeline::Timeline::from_cols(prog, base)?),
        };
        world.insert_resource(Edit { prog: prog.clone(), timeline, media });
        world.insert_resource(raea_sys_frame::tables::Cuts::of(prog));
        world.insert_resource(raea_sys_frame::shots::Shots::of(prog));
        world.insert_resource(FrameRows::new(ix));
        let Some(cam) = camera else { return Err("the edit needs a camera: INSERT INTO camera (id, ...) VALUES (...)".into()) };
        let models = world.query::<&scene3d::Model3d>().iter(world).count();
        let h = prog.height as f32;
        // the page overlay's texture, and a UI node showing it over the 3D when clips draw as UI (native only)
        #[cfg(feature = "native")]
        {
                let w = prog.width as f32;
                let mut img = Image::new_target_texture(prog.width, prog.height, TextureFormat::Rgba8UnormSrgb, None);
                img.texture_descriptor.usage |= TextureUsages::COPY_DST;
                let overlay = world.resource_mut::<Assets<Image>>().add(img);
                world.insert_resource(Overlay(overlay.clone()));
                if !world.resource::<Composite>().0 {
                    world.spawn((
                        Node { position_type: PositionType::Absolute, left: Val::Px(0.0), top: Val::Px(0.0), width: Val::Px(w), height: Val::Px(h), ..default() },
                        ImageNode::new(overlay),
                        GlobalZIndex(OVERLAY_Z),
                        UiTargetCamera(cam),
                        Mirror,
                    ));
                }
        }
        raea_sys_frame::spawn_tag(world, self.target.clone(), h);
        mem_mark("load: timeline made");
        // glTF models loaded and set up before any frame draws
        if models > 0 {
            scene3d::await_models(&mut self.app)?;
        }
        // and the assets a component row's handle fields loaded by path (plugins.rs)
        mem_mark("load: models ready");
        plugins::await_loads(&mut self.app)?;
        Ok(())
    }
}

/// RAEA_STATE_DUMP=DIR (debug): a World's state as a shot is about to draw, DIR/TAG-N.json: every reflectable main-world
/// resource's value, and both worlds' entity counts per component, so a kept engine's state after `reload` can be diffed
/// against a fresh engine's (docs/native-shots.md, engine reuse).
pub fn state_dump(app: &mut App, tag: &str) {
    let Some(dir) = std::env::var_os("RAEA_STATE_DUMP").map(std::path::PathBuf::from) else { return };
    let _ = std::fs::create_dir_all(&dir);
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let world = app.world();
    let reg = world.resource::<AppTypeRegistry>().clone();
    let reg = reg.read();
    let mut res = serde_json::Map::new();
    for (_, info, ptr) in world.iter_resources() {
        let name = info.name().to_string();
        let v = info.type_id().and_then(|t| reg.get(t)).and_then(|r| r.data::<bevy::reflect::ReflectFromPtr>()).map(|rfp| {
            // SAFETY: the pointer is this resource's, of the registered type rfp was made for, read only
            let v: &dyn bevy::reflect::Reflect = unsafe { &*(rfp.raw_pointer_cast())(ptr.as_ptr() as *mut ()) };
            serde_json::to_value(bevy::reflect::serde::TypedReflectSerializer::new(v.as_partial_reflect(), &reg)).unwrap_or(serde_json::Value::Null)
        });
        res.insert(name, v.unwrap_or(serde_json::Value::String("(not reflected)".into())));
    }
    let counts = |w: &World| {
        let mut m = std::collections::BTreeMap::<String, usize>::new();
        for a in w.archetypes().iter() {
            for c in a.components() {
                if let Some(i) = w.components().get_info(*c) {
                    *m.entry(i.name().to_string()).or_default() += a.len() as usize;
                }
            }
        }
        m
    };
    let main = counts(world);
    let render = counts(app.sub_app(bevy::render::RenderApp).world());
    let out = serde_json::json!({"resources": res, "main_counts": main, "render_counts": render, "entities": world.entities().len()});
    let _ = std::fs::write(dir.join(format!("{tag}-{n}.json")), out.to_string());
}

/// The resources a fresh engine has that a kept one (Engine::reload, a served process's next shot) must have again:
/// every Bevy clock (Time, Time<Virtual> whose max_delta a clock jump raises, Time<Fixed>, physics' and particles'),
/// and the physics' own state. A kept engine began each shot 80 s into its clocks with the last shot's physics tick, so
/// a shot served after another differed from it served first (Ch5 @2400 again: 27.8 dB).
const PRISTINE: [&str; 3] = ["bevy_time::time::Time", "avian3d::", "bevy_hanabi::time::"];

type Cast = fn(*mut ()) -> *mut dyn bevy::reflect::Reflect;

/// Those resources' values as the engine's build left them (Engine::build), for `pristine_restore`.
#[derive(Resource, Default)]
pub(crate) struct Pristine(Vec<(bevy::ecs::component::ComponentId, Cast, Box<dyn bevy::reflect::PartialReflect>)>);

pub(crate) fn pristine_take(world: &mut World) {
    let reg = world.resource::<AppTypeRegistry>().clone();
    let reg = reg.read();
    let mut out = Vec::new();
    for (cid, info, ptr) in world.iter_resources() {
        if !PRISTINE.iter().any(|p| info.name().to_string().starts_with(p)) {
            continue;
        }
        let Some(rfp) = info.type_id().and_then(|t| reg.get(t)).and_then(|r| r.data::<bevy::reflect::ReflectFromPtr>()) else { continue };
        let cast = rfp.raw_pointer_cast();
        // SAFETY: the pointer is this resource's, of the registered type rfp was made for, read only
        let v: &dyn bevy::reflect::Reflect = unsafe { &*cast(ptr.as_ptr() as *mut ()) };
        if let Ok(d) = v.as_partial_reflect().to_dynamic() {
            out.push((cid, cast, d));
        }
    }
    world.insert_resource(Pristine(out));
}

pub(crate) fn pristine_restore(world: &mut World) {
    let Some(p) = world.remove_resource::<Pristine>() else { return };
    for (cid, cast, v) in &p.0 {
        if let Some(mut m) = world.get_resource_mut_by_id(*cid) {
            // SAFETY: as pristine_take's, the same resource, written through its reflection
            let r: &mut dyn bevy::reflect::Reflect = unsafe { &mut *cast(m.as_mut().as_ptr() as *mut ()) };
            let _ = r.as_partial_reflect_mut().try_apply(v.as_ref());
        }
    }
    world.insert_resource(p);
}
