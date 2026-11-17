//! The slot cache: a page whose 3D sits in its layered slot, drawn each frame without Vello.
//!
//! The page is cut at the slot's image (raea-sys-filters split.rs): what's under it (`prefix`) and what's over it
//! (`segments`) are rendered by Vello, raw (vello's `set_raw_output`: fine's blend state as f32), each again only when
//! its own print changes (its encoding's fingerprint). Each frame one compute pass does what fine does across the cut,
//! in fine's own arithmetic: the slot's texel blended over the prefix (through the slot's own clip where it has one),
//! each segment over that (a layer through fine's 8-bit blend stack where its clip has tiles), and the result made
//! straight and encoded into the page's storage texture, which the premultiply pass then reads as it reads Vello's.
//! Ch5's page (a 3D slot under a full-frame veil and a caption): about 5 ms of Vello a frame on a 3090.
//!
//! RAEA_SLOT_CACHE=0 draws every page with Vello, as before; RAEA_SLOT_CACHE_WHY=1 says why a page can't be cut;
//! RAEA_SLOT_CACHE_TIMES=1 times the pass.

use std::collections::HashMap;

use bevy::asset::AssetId;
use bevy::image::Image;
use bevy::math::UVec2;
use raea_sys_filters::split::{Segment, Split};

/// The most segments one pass blends (a page with more is drawn by Vello).
pub const MAX_SEGMENTS: usize = 8;

const WGSL: &str = r"
struct P { w: u32, h: u32, x0: u32, y0: u32, x1: u32, y1: u32, tx: i32, ty: i32, linear: u32, tiles_w: u32, clip: u32, n: u32, cx0: u32, cy0: u32, cx1: u32, cy1: u32, kinds: array<vec4<u32>, 2> }
@group(0) @binding(0) var<uniform> p: P;
@group(0) @binding(1) var prefix: texture_2d<f32>;
@group(0) @binding(2) var mask: texture_2d<f32>;
@group(0) @binding(3) var slot: texture_2d<f32>;
@group(0) @binding(6) var page_out: texture_storage_2d<rgba8unorm, write>;
@group(0) @binding(7) var tiles_out: texture_storage_2d<r32uint, write>;
@group(0) @binding(8) var segs: texture_2d_array<f32>;
@group(0) @binding(9) var tiles: texture_2d_array<u32>;

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

// one pixel of the page: the prefix's state; the slot's texel over it in its box (fine's CMD_IMAGE at one texel a
// pixel, area 1, alpha 1), through the slot's own clip where it has one (fine's begin and end of that clip in the tiles
// the clip touches); each segment over that; then made straight and encoded, as fine's end does
@compute @workgroup_size(16, 16) fn page(@builtin(global_invocation_id) g: vec3u) {
    if g.x >= p.w || g.y >= p.h { return; }
    var s = textureLoad(prefix, vec2<i32>(g.xy), 0);
    let in_fill = g.x >= p.x0 && g.x < p.x1 && g.y >= p.y0 && g.y < p.y1;
    var fg_i = vec4(0.0);
    if in_fill {
        let t = vec2<i32>(i32(g.x - p.x0) + p.tx, i32(g.y - p.y0) + p.ty);
        let fg_rgba = blend_in(premul_alpha(textureLoad(slot, t, 0)));
        fg_i = fg_rgba * 1.0 * 1.0;
    }
    let tile = vec2<i32>(i32(g.x / 16u), i32(g.y / 16u));
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
    for (var k = 0u; k < p.n; k++) {
        let f = textureLoad(segs, vec2<i32>(g.xy), i32(k), 0);
        if p.kinds[k / 4u][k % 4u] == 1u {
            // a top-level layer: the backdrop through fine's 8-bit blend stack where its clip has tiles, then fine's
            // blend_mix_compose for Normal/SrcOver (its raw render is fg: content times area times alpha, over 0)
            if textureLoad(tiles, tile, i32(k), 0).x != 0u {
                let bg = blend_in(unpack4x8unorm(pack4x8unorm(blend_out(s))));
                s = bg * (1.0 - f.a) + f;
            }
        } else {
            s = s * (1.0 - f.a) + f;
        }
    }
    let a_inv = 1.0 / max(s.a, 1e-6);
    let rgba_sep = straight_out(vec4(s.rgb * a_inv, s.a));
    textureStore(page_out, vec2<i32>(g.xy), rgba_sep);
}

// a layer's clip tiles: a 16x16 tile any pixel of whose mask render has coverage (one invocation a tile)
@compute @workgroup_size(8, 8) fn tile_mask(@builtin(global_invocation_id) g: vec3u) {
    if g.x >= p.tiles_w || g.y >= (p.h + 15u) / 16u { return; }
    var any = 0u;
    for (var y = 0u; y < 16u; y++) {
        for (var x = 0u; x < 16u; x++) {
            let q = vec2<u32>(g.x * 16u + x, g.y * 16u + y);
            if q.x < p.w && q.y < p.h && textureLoad(mask, vec2<i32>(q), 0).a > 0.0 {
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
    /// the segments' raw renders, a layer each, and each layer segment's clip tiles at the same layer
    segs: wgpu::Texture,
    tiles: wgpu::Texture,
}

pub struct SlotCache {
    page: wgpu::ComputePipeline,
    tile_mask: wgpu::ComputePipeline,
    pages: HashMap<AssetId<Image>, Pieces>,
}

fn float_target(device: &wgpu::Device, size: UVec2, layers: u32, label: &'static str) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d { width: size.x, height: size.y, depth_or_array_layers: layers },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba32Float,
        usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    })
}

/// One array layer of `t` as a 2D view (what Vello renders a piece into, and the tile pass writes).
fn layer(t: &wgpu::Texture, k: u32) -> wgpu::TextureView {
    t.create_view(&wgpu::TextureViewDescriptor { dimension: Some(wgpu::TextureViewDimension::D2), base_array_layer: k, array_layer_count: Some(1), ..Default::default() })
}

fn array(t: &wgpu::Texture) -> wgpu::TextureView {
    t.create_view(&wgpu::TextureViewDescriptor { dimension: Some(wgpu::TextureViewDimension::D2Array), ..Default::default() })
}

impl SlotCache {
    pub fn new(device: &wgpu::Device) -> SlotCache {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("raea-page slot cache"), source: wgpu::ShaderSource::Wgsl(WGSL.into()) });
        let make = |e: &str| device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor { label: Some("raea-page slot cache"), layout: None, module: &module, entry_point: Some(e), compilation_options: Default::default(), cache: None });
        SlotCache { page: make("page"), tile_mask: make("tile_mask"), pages: HashMap::new() }
    }

    /// Pages no longer drawn forgotten.
    pub fn retain(&mut self, live: impl Fn(&AssetId<Image>) -> bool) {
        self.pages.retain(|k, _| live(k));
    }

    fn params(device: &wgpu::Device, queue: &wgpu::Queue, size: UVec2, split: &Split, linear: bool, origin: (u32, u32)) -> wgpu::Buffer {
        let s = split.slot.expect("a usable split has its slot");
        let c = s.clip.unwrap_or([0; 4]);
        let mut v: Vec<u32> = vec![size.x, size.y, s.rect[0], s.rect[1], s.rect[2], s.rect[3], s.texel.0 as u32 + origin.0, s.texel.1 as u32 + origin.1, linear as u32, size.x.div_ceil(16), s.clip.is_some() as u32, split.segments.len() as u32, c[0], c[1], c[2], c[3]];
        let mut kinds = [0u32; 8];
        for (k, seg) in split.segments.iter().enumerate().take(MAX_SEGMENTS) {
            kinds[k] = matches!(seg, Segment::Layer { .. }) as u32;
        }
        v.extend(kinds);
        let b = device.create_buffer(&wgpu::BufferDescriptor { label: Some("raea-page slot cache"), size: (v.len() * 4) as u64, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
        queue.write_buffer(&b, 0, &v.iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<u8>>());
        b
    }

    /// The page `id` (`size` px, cut as `split`) drawn into `out` (its Vello storage texture) with the slot's texture
    /// `slot` from `origin` (its bytes read as Vello's atlas copy of them is read). `render` renders a scene raw into a
    /// float target's view (Vello with set_raw_output). Errors (more than MAX_SEGMENTS segments, a failed render)
    /// leave the page to Vello.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, id: AssetId<Image>, size: UVec2, split: &Split, linear: bool, (slot, origin): (&wgpu::Texture, (u32, u32)), out: &wgpu::Texture, mut render: impl FnMut(&vello::Scene, &wgpu::TextureView) -> Result<(), String>) -> Result<(), String> {
        if split.segments.len() > MAX_SEGMENTS {
            return Err(format!("{} segments over the slot (at most {MAX_SEGMENTS})", split.segments.len()));
        }
        let kinds: Vec<bool> = split.segments.iter().map(|s| matches!(s, Segment::Layer { .. })).collect();
        let params = Self::params(device, queue, size, split, linear, origin);
        let groups = (size.x.div_ceil(16), size.y.div_ceil(16));
        // a page of another size or shape is made again whole; otherwise only the pieces whose print changed
        if self.pages.get(&id).is_none_or(|p| p.size != size || p.kinds != kinds || p.prints.len() != split.prints.len()) {
            let n = (kinds.len() as u32).max(1);
            let tiles = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("raea-page slot cache tiles"),
                size: wgpu::Extent3d { width: groups.0, height: groups.1, depth_or_array_layers: n },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::R32Uint,
                usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            let pieces = Pieces { prints: vec![!0; split.prints.len()], kinds: kinds.clone(), size, prefix: float_target(device, size, 1, "raea-page slot cache prefix"), segs: float_target(device, size, n, "raea-page slot cache segments"), tiles };
            self.pages.insert(id, pieces);
        }
        let pieces = self.pages.get_mut(&id).expect("made above");
        if std::env::var_os("RAEA_SLOT_CACHE_WHY").is_some() {
            let again: Vec<usize> = (0..split.prints.len()).filter(|k| pieces.prints[*k] != split.prints[*k]).collect();
            eprintln!("raea-page: slot cache: pieces {:x?}, made again {again:?}", split.prints);
        }
        if pieces.prints[0] != split.prints[0] {
            render(&split.prefix, &pieces.prefix.create_view(&Default::default()))?;
            pieces.prints[0] = split.prints[0];
        }
        let mut masks = Vec::new();
        for (k, s) in split.segments.iter().enumerate() {
            if pieces.prints[k + 1] == split.prints[k + 1] {
                continue;
            }
            match s {
                Segment::Fills(scene) => render(scene, &layer(&pieces.segs, k as u32))?,
                Segment::Layer { scene, mask } => {
                    render(scene, &layer(&pieces.segs, k as u32))?;
                    let m = float_target(device, size, 1, "raea-page slot cache mask");
                    render(mask, &m.create_view(&Default::default()))?;
                    masks.push((k as u32, m));
                }
            }
            pieces.prints[k + 1] = split.prints[k + 1];
        }
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("raea-page slot cache") });
        for (k, m) in &masks {
            let mv = m.create_view(&Default::default());
            let tv = layer(&pieces.tiles, *k);
            let bind = device.create_bind_group(&wgpu::BindGroupDescriptor { label: Some("raea-page slot cache tiles"), layout: &self.tile_mask.get_bind_group_layout(0), entries: &[wgpu::BindGroupEntry { binding: 0, resource: params.as_entire_binding() }, wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&mv) }, wgpu::BindGroupEntry { binding: 7, resource: wgpu::BindingResource::TextureView(&tv) }] });
            let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("raea-page slot cache tiles"), timestamp_writes: None });
            pass.set_pipeline(&self.tile_mask);
            pass.set_bind_group(0, &bind, &[]);
            pass.dispatch_workgroups(groups.0.div_ceil(8), groups.1.div_ceil(8), 1);
        }
        if std::env::var_os("RAEA_SLOT_CACHE_TIMES").is_some() {
            // (timed alone: what's queued before it finished first)
            queue.submit([]);
            let _ = device.poll(wgpu::PollType::Wait { submission_index: None, timeout: Some(std::time::Duration::from_secs(60)) });
        }
        let t = std::time::Instant::now();
        {
            let (pv, sv, segv, tilev, ov) = (pieces.prefix.create_view(&Default::default()), slot.create_view(&wgpu::TextureViewDescriptor { format: Some(wgpu::TextureFormat::Rgba8Unorm), ..Default::default() }), array(&pieces.segs), array(&pieces.tiles), out.create_view(&Default::default()));
            let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("raea-page slot cache"),
                layout: &self.page.get_bind_group_layout(0),
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: params.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&pv) },
                    wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(&sv) },
                    wgpu::BindGroupEntry { binding: 6, resource: wgpu::BindingResource::TextureView(&ov) },
                    wgpu::BindGroupEntry { binding: 8, resource: wgpu::BindingResource::TextureView(&segv) },
                    wgpu::BindGroupEntry { binding: 9, resource: wgpu::BindingResource::TextureView(&tilev) },
                ],
            });
            let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("raea-page slot cache"), timestamp_writes: None });
            pass.set_pipeline(&self.page);
            pass.set_bind_group(0, &bind, &[]);
            // (timing: the same pass 20 times, so a clocked-down GPU's ramp is spread out; the result is the same)
            for _ in 0..if std::env::var_os("RAEA_SLOT_CACHE_TIMES").is_some() { 20 } else { 1 } {
                pass.dispatch_workgroups(groups.0, groups.1, 1);
            }
        }
        queue.submit([enc.finish()]);
        if std::env::var_os("RAEA_SLOT_CACHE_TIMES").is_some() {
            let s1 = t.elapsed().as_secs_f64() * 1000.0;
            let w = std::time::Instant::now();
            let _ = device.poll(wgpu::PollType::Wait { submission_index: None, timeout: Some(std::time::Duration::from_secs(60)) });
            eprintln!("raea-page: slot cache pass: encode and submit {s1:.3} ms, GPU {:.3} ms a pass over 20 (pieces made again: {})", w.elapsed().as_secs_f64() * 1000.0 / 20.0, masks.len());
        }
        Ok(())
    }
}
