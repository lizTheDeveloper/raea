//! The slot cache: a page whose 3D sits in its layered slot, drawn each frame without Vello.
//!
//! The page is cut at the slot's image (raea-sys-filters split.rs): what's under it (`prefix`) and what's over it
//! (`segments`) are rendered by Vello once, raw (vello's `set_raw_output`: fine's blend state as f32), and again only
//! when the page's print changes. Each frame a compute chain does what fine does across the cut, in fine's own
//! arithmetic: the slot's texel blended over the prefix, each segment over that (a layer through fine's 8-bit blend
//! stack where its clip has tiles), and the result made straight and encoded into the page's storage texture, which
//! the premultiply pass then reads as it reads Vello's. Ch5's page (a 3D slot under a full-frame veil and a caption):
//! about 5 ms of Vello a frame on a 3090, against this chain's passes.
//!
//! RAEA_SLOT_CACHE=0 draws every page with Vello, as before; RAEA_SLOT_CACHE_WHY=1 says why a page can't be cut.

use std::collections::HashMap;

use bevy::asset::AssetId;
use bevy::image::Image;
use bevy::math::UVec2;
use raea_sys_filters::split::{Segment, Split};

const WGSL: &str = r"
struct P { w: u32, h: u32, x0: u32, y0: u32, x1: u32, y1: u32, tx: i32, ty: i32, linear: u32, tiles_w: u32, clip: u32, _a: u32, cx0: u32, cy0: u32, cx1: u32, cy1: u32 }
@group(0) @binding(0) var<uniform> p: P;
@group(0) @binding(1) var state_in: texture_2d<f32>;
@group(0) @binding(2) var seg: texture_2d<f32>;
@group(0) @binding(3) var slot: texture_2d<f32>;
@group(0) @binding(4) var tiles: texture_2d<u32>;
@group(0) @binding(5) var state_out: texture_storage_2d<rgba32float, write>;
@group(0) @binding(6) var page_out: texture_storage_2d<rgba8unorm, write>;
@group(0) @binding(7) var tiles_out: texture_storage_2d<r32uint, write>;

// fine.wgsl's (vello_shaders, raea's linear-light blending), word for word
fn srgb_decode(c: vec3<f32>) -> vec3<f32> {
    return select(pow((c + 0.055) / 1.055, vec3(2.4)), c / 12.92, c <= vec3(0.04045));
}
fn srgb_encode(c: vec3<f32>) -> vec3<f32> {
    return select(1.055 * pow(c, vec3(1.0 / 2.4)) - 0.055, c * 12.92, c <= vec3(0.0031308));
}
fn blend_in(premul: vec4<f32>) -> vec4<f32> {
    if p.linear == 0u || premul.a <= 0.0 {
        return premul;
    }
    return vec4(srgb_decode(clamp(premul.rgb / premul.a, vec3(0.0), vec3(1.0))) * premul.a, premul.a);
}
fn blend_out(premul: vec4<f32>) -> vec4<f32> {
    if p.linear == 0u || premul.a <= 0.0 {
        return premul;
    }
    return vec4(srgb_encode(clamp(premul.rgb / premul.a, vec3(0.0), vec3(1.0))) * premul.a, premul.a);
}
fn straight_out(straight: vec4<f32>) -> vec4<f32> {
    if p.linear == 0u {
        return straight;
    }
    return vec4(srgb_encode(clamp(straight.rgb, vec3(0.0), vec3(1.0))), straight.a);
}
fn premul_alpha(rgba: vec4<f32>) -> vec4<f32> {
    return vec4(rgba.rgb * rgba.a, rgba.a);
}

// the prefix's state, the slot's texel over it in its box (fine's CMD_IMAGE at one texel a pixel, area 1, alpha 1);
// with the slot's own clip (a rectangle holding the image alone), fine's begin and end of that clip around it in the
// tiles the clip touches: the state through the 8-bit blend stack, the image over 0, then blend_mix_compose
@compute @workgroup_size(16, 16) fn init(@builtin(global_invocation_id) g: vec3u) {
    if g.x >= p.w || g.y >= p.h { return; }
    var s = textureLoad(state_in, vec2<i32>(g.xy), 0);
    let in_fill = g.x >= p.x0 && g.x < p.x1 && g.y >= p.y0 && g.y < p.y1;
    var fg_i = vec4(0.0);
    if in_fill {
        let t = vec2<i32>(i32(g.x - p.x0) + p.tx, i32(g.y - p.y0) + p.ty);
        let fg_rgba = blend_in(premul_alpha(textureLoad(slot, t, 0)));
        fg_i = fg_rgba * 1.0 * 1.0;
    }
    if p.clip == 0u {
        if in_fill {
            s = s * (1.0 - fg_i.a) + fg_i;
        }
    } else {
        let t0 = (g.xy / 16u) * 16u;
        if t0.x < p.cx1 && t0.x + 16u > p.cx0 && t0.y < p.cy1 && t0.y + 16u > p.cy0 {
            let bg = blend_in(unpack4x8unorm(pack4x8unorm(blend_out(s))));
            var inner = vec4(0.0);
            if in_fill {
                inner = inner * (1.0 - fg_i.a) + fg_i;
            }
            let area = select(0.0, 1.0, g.x >= p.cx0 && g.x < p.cx1 && g.y >= p.cy0 && g.y < p.cy1);
            let fg = inner * area * 1.0;
            s = bg * (1.0 - fg.a) + fg;
        }
    }
    textureStore(state_out, vec2<i32>(g.xy), s);
}

// a run of fills over the state
@compute @workgroup_size(16, 16) fn fills(@builtin(global_invocation_id) g: vec3u) {
    if g.x >= p.w || g.y >= p.h { return; }
    let s = textureLoad(state_in, vec2<i32>(g.xy), 0);
    let f = textureLoad(seg, vec2<i32>(g.xy), 0);
    textureStore(state_out, vec2<i32>(g.xy), s * (1.0 - f.a) + f);
}

// a top-level layer: where its clip has tiles, the backdrop through fine's 8-bit blend stack, then fine's
// blend_mix_compose for Normal/SrcOver (the layer's raw render is fg: its content times area times alpha, over 0)
@compute @workgroup_size(16, 16) fn layer(@builtin(global_invocation_id) g: vec3u) {
    if g.x >= p.w || g.y >= p.h { return; }
    var s = textureLoad(state_in, vec2<i32>(g.xy), 0);
    if textureLoad(tiles, vec2<i32>(i32(g.x / 16u), i32(g.y / 16u)), 0).x != 0u {
        let bg = blend_in(unpack4x8unorm(pack4x8unorm(blend_out(s))));
        let fg = textureLoad(seg, vec2<i32>(g.xy), 0);
        s = bg * (1.0 - fg.a) + fg;
    }
    textureStore(state_out, vec2<i32>(g.xy), s);
}

// the state made straight and encoded, as fine's end does
@compute @workgroup_size(16, 16) fn finish(@builtin(global_invocation_id) g: vec3u) {
    if g.x >= p.w || g.y >= p.h { return; }
    let fg = textureLoad(state_in, vec2<i32>(g.xy), 0);
    let a_inv = 1.0 / max(fg.a, 1e-6);
    let rgba_sep = straight_out(vec4(fg.rgb * a_inv, fg.a));
    textureStore(page_out, vec2<i32>(g.xy), rgba_sep);
}

// a layer's clip tiles: a 16x16 tile any pixel of whose mask render has coverage (one invocation a tile)
@compute @workgroup_size(8, 8) fn tile_mask(@builtin(global_invocation_id) g: vec3u) {
    if g.x >= p.tiles_w || g.y >= (p.h + 15u) / 16u { return; }
    var any = 0u;
    for (var y = 0u; y < 16u; y++) {
        for (var x = 0u; x < 16u; x++) {
            let q = vec2<u32>(g.x * 16u + x, g.y * 16u + y);
            if q.x < p.w && q.y < p.h && textureLoad(seg, vec2<i32>(q), 0).a > 0.0 {
                any = 1u;
            }
        }
    }
    textureStore(tiles_out, vec2<i32>(g.xy), vec4(any, 0u, 0u, 0u));
}
";

pub fn enabled() -> bool {
    std::env::var("RAEA_SLOT_CACHE").map_or(true, |v| v != "0")
}

/// One page's pieces on the GPU, each made for its print (`prints`: the prefix's first, then each segment's).
struct Pieces {
    prints: Vec<u64>,
    /// each segment a layer (true) or fills
    kinds: Vec<bool>,
    size: UVec2,
    prefix: wgpu::Texture,
    /// each segment: its raw render, and for a layer its clip's tiles
    segments: Vec<(wgpu::Texture, Option<wgpu::Texture>)>,
    states: [wgpu::Texture; 2],
}

pub struct SlotCache {
    pipelines: HashMap<&'static str, wgpu::ComputePipeline>,
    pages: HashMap<AssetId<Image>, Pieces>,
}

fn float_target(device: &wgpu::Device, size: UVec2, label: &'static str) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d { width: size.x, height: size.y, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba32Float,
        usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    })
}

impl SlotCache {
    pub fn new(device: &wgpu::Device) -> SlotCache {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("raea-page slot cache"), source: wgpu::ShaderSource::Wgsl(WGSL.into()) });
        let mut pipelines = HashMap::new();
        for e in ["init", "fills", "layer", "finish", "tile_mask"] {
            let pl = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor { label: Some("raea-page slot cache"), layout: None, module: &module, entry_point: Some(e), compilation_options: Default::default(), cache: None });
            pipelines.insert(e, pl);
        }
        SlotCache { pipelines, pages: HashMap::new() }
    }

    /// Pages no longer drawn forgotten.
    pub fn retain(&mut self, live: impl Fn(&AssetId<Image>) -> bool) {
        self.pages.retain(|k, _| live(k));
    }

    fn params(device: &wgpu::Device, queue: &wgpu::Queue, size: UVec2, split: &Split, linear: bool, origin: (u32, u32)) -> wgpu::Buffer {
        let s = split.slot.expect("a usable split has its slot");
        let c = s.clip.unwrap_or([0; 4]);
        let v: [u32; 16] = [size.x, size.y, s.rect[0], s.rect[1], s.rect[2], s.rect[3], s.texel.0 as u32 + origin.0, s.texel.1 as u32 + origin.1, linear as u32, size.x.div_ceil(16), s.clip.is_some() as u32, 0, c[0], c[1], c[2], c[3]];
        let b = device.create_buffer(&wgpu::BufferDescriptor { label: Some("raea-page slot cache"), size: 64, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
        queue.write_buffer(&b, 0, &v.iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<u8>>());
        b
    }

    fn pass(&self, device: &wgpu::Device, encoder: &mut wgpu::CommandEncoder, entry: &'static str, groups: (u32, u32), entries: &[(u32, wgpu::BindingResource<'_>)]) {
        let pl = &self.pipelines[entry];
        let layout = pl.get_bind_group_layout(0);
        let entries: Vec<wgpu::BindGroupEntry> = entries.iter().map(|(b, r)| wgpu::BindGroupEntry { binding: *b, resource: r.clone() }).collect();
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor { label: Some("raea-page slot cache"), layout: &layout, entries: &entries });
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some(entry), timestamp_writes: None });
        pass.set_pipeline(pl);
        pass.set_bind_group(0, &bind, &[]);
        pass.dispatch_workgroups(groups.0, groups.1, 1);
    }

    /// The page `id` (`size` px, painted as `print`, cut as `split`) drawn into `out` (its Vello storage texture) with
    /// the slot's texture `slot` from `origin` (its bytes read as Vello's atlas copy of them is read). `render` renders a scene raw
    /// into a float target (Vello with set_raw_output). Errors leave the page to Vello.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, id: AssetId<Image>, size: UVec2, print: u64, split: &Split, linear: bool, (slot, origin): (&wgpu::Texture, (u32, u32)), out: &wgpu::Texture, mut render: impl FnMut(&vello::Scene, &wgpu::Texture) -> Result<(), String>) -> Result<(), String> {
        let kinds: Vec<bool> = split.segments.iter().map(|s| matches!(s, Segment::Layer { .. })).collect();
        let params = Self::params(device, queue, size, split, linear, origin);
        let groups = (size.x.div_ceil(16), size.y.div_ceil(16));
        // a page of another size or shape is made again whole; otherwise only the pieces whose print changed
        if self.pages.get(&id).is_none_or(|p| p.size != size || p.kinds != kinds || p.prints.len() != split.prints.len()) {
            let blank = || float_target(device, size, "raea-page slot cache piece");
            let segments = kinds.iter().map(|_| (blank(), None)).collect();
            let states = [blank(), blank()];
            self.pages.insert(id, Pieces { prints: vec![!0; split.prints.len()], kinds: kinds.clone(), size, prefix: blank(), segments, states });
        }
        let pieces = self.pages.get_mut(&id).expect("made above");
        if std::env::var_os("RAEA_SLOT_CACHE_WHY").is_some() {
            let again: Vec<usize> = (0..split.prints.len()).filter(|k| pieces.prints[*k] != split.prints[*k]).collect();
            eprintln!("raea-page: slot cache: page print {print:x}, pieces {:x?}, made again {again:?}", split.prints);
        }
        if pieces.prints[0] != split.prints[0] {
            render(&split.prefix, &pieces.prefix)?;
            pieces.prints[0] = split.prints[0];
        }
        let mut masks = Vec::new();
        for (k, s) in split.segments.iter().enumerate() {
            if pieces.prints[k + 1] == split.prints[k + 1] {
                continue;
            }
            match s {
                Segment::Fills(scene) => render(scene, &pieces.segments[k].0)?,
                Segment::Layer { scene, mask } => {
                    render(scene, &pieces.segments[k].0)?;
                    let m = float_target(device, size, "raea-page slot cache mask");
                    render(mask, &m)?;
                    let tiles = device.create_texture(&wgpu::TextureDescriptor {
                        label: Some("raea-page slot cache tiles"),
                        size: wgpu::Extent3d { width: groups.0, height: groups.1, depth_or_array_layers: 1 },
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: wgpu::TextureFormat::R32Uint,
                        usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING,
                        view_formats: &[],
                    });
                    masks.push((k, m, tiles));
                }
            }
            pieces.prints[k + 1] = split.prints[k + 1];
        }
        if !masks.is_empty() {
            let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("raea-page slot cache tiles") });
            for (k, m, tiles) in masks {
                let mv = m.create_view(&Default::default());
                let tv = tiles.create_view(&Default::default());
                let pl = &self.pipelines["tile_mask"];
                let bind = device.create_bind_group(&wgpu::BindGroupDescriptor { label: Some("raea-page slot cache"), layout: &pl.get_bind_group_layout(0), entries: &[wgpu::BindGroupEntry { binding: 0, resource: params.as_entire_binding() }, wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&mv) }, wgpu::BindGroupEntry { binding: 7, resource: wgpu::BindingResource::TextureView(&tv) }] });
                {
                    let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("tile_mask"), timestamp_writes: None });
                    pass.set_pipeline(pl);
                    pass.set_bind_group(0, &bind, &[]);
                    pass.dispatch_workgroups(groups.0.div_ceil(8), groups.1.div_ceil(8), 1);
                }
                self.pages.get_mut(&id).expect("made above").segments[k].1 = Some(tiles);
            }
            queue.submit([enc.finish()]);
        }
        let pieces = &self.pages[&id];
        let slot_view = slot.create_view(&wgpu::TextureViewDescriptor { format: Some(wgpu::TextureFormat::Rgba8Unorm), ..Default::default() });
        let out_view = out.create_view(&Default::default());
        let sv: [wgpu::TextureView; 2] = [pieces.states[0].create_view(&Default::default()), pieces.states[1].create_view(&Default::default())];
        let pv = pieces.prefix.create_view(&Default::default());
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("raea-page slot cache") });
        self.pass(device, &mut enc, "init", groups, &[(0, params.as_entire_binding()), (1, wgpu::BindingResource::TextureView(&pv)), (3, wgpu::BindingResource::TextureView(&slot_view)), (5, wgpu::BindingResource::TextureView(&sv[0]))]);
        let mut cur = 0;
        for (t, tiles) in &pieces.segments {
            let segv = t.create_view(&Default::default());
            match tiles {
                None => self.pass(device, &mut enc, "fills", groups, &[(0, params.as_entire_binding()), (1, wgpu::BindingResource::TextureView(&sv[cur])), (2, wgpu::BindingResource::TextureView(&segv)), (5, wgpu::BindingResource::TextureView(&sv[1 - cur]))]),
                Some(tl) => {
                    let tv = tl.create_view(&Default::default());
                    self.pass(device, &mut enc, "layer", groups, &[(0, params.as_entire_binding()), (1, wgpu::BindingResource::TextureView(&sv[cur])), (2, wgpu::BindingResource::TextureView(&segv)), (4, wgpu::BindingResource::TextureView(&tv)), (5, wgpu::BindingResource::TextureView(&sv[1 - cur]))]);
                }
            }
            cur = 1 - cur;
        }
        self.pass(device, &mut enc, "finish", groups, &[(0, params.as_entire_binding()), (1, wgpu::BindingResource::TextureView(&sv[cur])), (6, wgpu::BindingResource::TextureView(&out_view))]);
        queue.submit([enc.finish()]);
        Ok(())
    }
}
