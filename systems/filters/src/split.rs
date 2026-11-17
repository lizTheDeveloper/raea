//! A page cut at its layered slot (raea-page's slot cache, systems/page draw.rs): what's painted before the slot's
//! image, the image itself, and what's painted after it as segments, each a scene of its own. Vello renders each
//! piece once, raw (vello's `set_raw_output`: its blend state as f32), and a compute pass blends the slot's texture
//! between them each frame as Vello's fine stage would, so a page whose only change is its 3D isn't drawn again.
//!
//! The cut holds only where fine's arithmetic carries across it: the slot's image is filled at the page's top level
//! (no clip or layer open), on whole pixels, one texel a pixel; what follows is plain fills and top-level layers of
//! Normal/SrcOver blend (fine's `bg * (1 - fg.a) + fg`, the layer's backdrop through its 8-bit stack). Anything else
//! (a filtered layer, a backdrop filter, another blend, the slot drawn twice or in a layer) leaves `Split::usable`
//! false and the page is drawn by Vello as before.

use vello::kurbo::{Affine, Rect, Shape};
use vello::peniko::{BlendMode, Color, Fill};

/// What follows the slot: a run of fills (each applied as `s * (1 - S.a) + S`, S their raw render), or a top-level
/// layer (its backdrop through fine's 8-bit blend stack where its clip has tiles, then `bg * (1 - S.a) + S`), with
/// `mask`: its clip filled white, whose raw render's coverage says which 16x16 tiles the clip touches.
pub enum Segment {
    Fills(vello::Scene),
    Layer { scene: vello::Scene, mask: vello::Scene },
}

/// The slot's image as the page fills it: its box in device px (whole pixels), the texel of the slot's texture at
/// the box's top left, and the clip that holds the image alone when there is one (blitz-paint clips a replaced
/// element to its content box): a rectangle on whole pixels, in px.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SlotFill {
    pub blob: u64,
    pub rect: [u32; 4],
    pub texel: (i32, i32),
    pub clip: Option<[u32; 4]>,
}

/// A rectangle on whole pixels, inside the page, in px (None: not one).
fn whole_rect(transform: Affine, r: Rect) -> Option<[u32; 4]> {
    let c = transform.as_coeffs();
    if !(c[0] == 1.0 && c[1] == 0.0 && c[2] == 0.0 && c[3] == 1.0) {
        return None;
    }
    let (x0, y0, x1, y1) = (r.x0 + c[4], r.y0 + c[5], r.x1 + c[4], r.y1 + c[5]);
    ([x0, y0, x1, y1].into_iter().all(|v| v.fract() == 0.0) && x0 >= 0.0 && y0 >= 0.0 && x1 > x0 && y1 > y0).then_some([x0 as u32, y0 as u32, x1 as u32, y1 as u32])
}

#[derive(Default)]
enum Stage {
    #[default]
    Before,
    After,
    Unusable,
}

/// The page's pieces, recorded alongside its scene (Painter::split_at).
#[derive(Default)]
pub struct Split {
    pub(crate) slots: Vec<u64>,
    stage: Stage,
    pub prefix: vello::Scene,
    pub slot: Option<SlotFill>,
    pub segments: Vec<Segment>,
    /// a top-level layer open in the suffix: its depth (1 = the layer itself)
    pub(crate) layer_depth: u32,
    /// why the cut doesn't hold (RAEA_SLOT_CACHE_WHY)
    pub why: Option<&'static str>,
    /// RAEA_SLOT_CACHE_WHY=2: the page's calls, one line each (what, depth, box)
    pub trace: Vec<String>,
    /// a top-level clip pushed before the slot, held back: the slot's own clip if the slot's image is all it holds
    /// (then it's in no piece), else put into the prefix at the next call (the root depth before it, its fill rule,
    /// transform and rectangle)
    pending_clip: Option<(u32, Fill, Affine, Rect)>,
    /// the slot's own clip is open (only its pop may follow)
    clip_open: bool,
    /// each piece's print, the prefix's first: the page's running print when the piece closed (it covers every call
    /// up to there, so a piece whose print is unchanged drew the same; a later piece's change leaves it alone)
    pub prints: Vec<u64>,
    /// the page's running print, as the painter last gave it (`at`)
    running: u64,
}

pub(crate) fn tracing() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("RAEA_SLOT_CACHE_WHY").as_deref() == Ok("2"))
}

impl Split {
    pub(crate) fn new(slots: Vec<u64>) -> Split {
        Split { slots, ..Default::default() }
    }

    /// The page's running print, given before each call is recorded.
    pub(crate) fn at(&mut self, running: u64) {
        self.running = running;
    }

    /// The page painted: the last piece closed at `print`.
    pub fn finish(&mut self, print: u64) {
        if matches!(self.stage, Stage::After) {
            self.prints.truncate(self.segments.len());
            self.prints.push(print);
        }
    }

    /// A piece closes (another begins): its print is the running print.
    fn close_piece(&mut self) {
        self.prints.push(self.running);
    }

    /// The cut holds: the slot was found at the top level and nothing after it is out of reach.
    pub fn usable(&self) -> bool {
        matches!(self.stage, Stage::After) && self.slot.is_some() && self.layer_depth == 0 && !self.clip_open
    }

    pub(crate) fn unusable(&mut self, why: &'static str) {
        if !matches!(self.stage, Stage::Unusable) {
            self.why = Some(why);
        }
        self.stage = Stage::Unusable;
    }

    /// A rectangle clip pushed at the top level before the slot (`depth` open before it): held back (true: the caller
    /// paints it into the page only) until the next call says whether it's the slot's own.
    pub(crate) fn defer_clip(&mut self, depth: u32, fill: Fill, transform: Affine, clip: Option<Rect>) -> bool {
        if !matches!(self.stage, Stage::Before) {
            return false;
        }
        self.flush();
        let Some(r) = clip else { return false };
        self.pending_clip = Some((depth, fill, transform, r));
        true
    }

    /// A held-back clip put into the prefix (the call after it wasn't the slot).
    fn flush(&mut self) {
        if let Some((_, fill, transform, r)) = self.pending_clip.take() {
            use anyrender::PaintScene as _;
            anyrender_vello::VelloScenePainter::new(&mut self.prefix).push_clip_layer(fill, transform, &r);
        }
    }

    /// A pop at the top level: true when it closes the slot's own clip (in no piece).
    pub(crate) fn pop_is_slot_clip(&mut self) -> bool {
        if self.clip_open && self.layer_depth == 0 {
            self.clip_open = false;
            return true;
        }
        false
    }

    /// The scene a top-level paint call goes to (None: none, the cut no longer holds).
    pub(crate) fn scene(&mut self) -> Option<&mut vello::Scene> {
        if self.clip_open {
            self.unusable("the slot's clip holds more than the slot");
            return None;
        }
        self.flush();
        match self.stage {
            Stage::Before => Some(&mut self.prefix),
            Stage::After => {
                if self.layer_depth == 0 && !matches!(self.segments.last(), Some(Segment::Fills(_))) {
                    if !self.segments.is_empty() {
                        self.close_piece();
                    }
                    self.segments.push(Segment::Fills(vello::Scene::new()));
                }
                match self.segments.last_mut() {
                    Some(Segment::Fills(s)) | Some(Segment::Layer { scene: s, .. }) => Some(s),
                    None => None,
                }
            }
            Stage::Unusable => None,
        }
    }

    /// A top-level fill: the slot's image (the cut) when its brush is a slot's, at the top level, on whole pixels.
    /// True when it was the slot (the caller paints it into the page only).
    pub(crate) fn fill_is_slot(&mut self, blob: Option<u64>, transform: Affine, brush_transform: Option<Affine>, shape_rect: Option<Rect>, open: u32, image_size: (u32, u32), alpha_one: bool, quality_ok: bool) -> bool {
        let Some(blob) = blob.filter(|b| self.slots.contains(b)) else { return false };
        if !matches!(self.stage, Stage::Before) {
            self.unusable("the slot's image is filled twice");
            return true;
        }
        // the slot's own clip: the held-back clip right under it, on whole pixels
        let mut clip = None;
        if let Some((d, _, t, r)) = self.pending_clip {
            if d + 1 == open && let Some(px) = whole_rect(t, r) {
                clip = Some(px);
                self.pending_clip = None;
            } else {
                self.flush();
            }
        }
        let open = if clip.is_some() { open - 1 } else { open };
        if open > 0 {
            self.unusable("the slot's image is inside a clip or layer");
            return true;
        }
        // whole pixels, one texel a pixel: the fill's transform a whole-pixel translation, the image's the identity
        let c = transform.as_coeffs();
        let b = brush_transform.unwrap_or(Affine::IDENTITY).as_coeffs();
        let whole = |v: f64| v.fract() == 0.0;
        let Some(r) = shape_rect else {
            self.unusable("the slot's image isn't filled into a rectangle");
            return true;
        };
        if !(c[0] == 1.0 && c[1] == 0.0 && c[2] == 0.0 && c[3] == 1.0 && whole(c[4]) && whole(c[5]) && b[0] == 1.0 && b[1] == 0.0 && b[2] == 0.0 && b[3] == 1.0 && whole(b[4]) && whole(b[5])) || !alpha_one || !quality_ok {
            self.unusable("the slot's image isn't drawn one texel a pixel, opaque");
            return true;
        }
        let (x0, y0, x1, y1) = (r.x0 + c[4], r.y0 + c[5], r.x1 + c[4], r.y1 + c[5]);
        if ![x0, y0, x1, y1].into_iter().all(whole) || x0 < 0.0 || y0 < 0.0 || x1 <= x0 || y1 <= y0 {
            self.unusable("the slot's box isn't on whole pixels");
            return true;
        }
        // the texel at a pixel: (pixel - fill translation) mapped by the brush transform's inverse (its translation)
        let texel = ((x0 - c[4] - b[4]) as i32, (y0 - c[5] - b[5]) as i32);
        if texel.0 < 0 || texel.1 < 0 || texel.0 as f64 + (x1 - x0) > image_size.0 as f64 || texel.1 as f64 + (y1 - y0) > image_size.1 as f64 {
            self.unusable("the slot's box reaches past its image");
            return true;
        }
        self.slot = Some(SlotFill { blob, rect: [x0 as u32, y0 as u32, x1 as u32, y1 as u32], texel, clip });
        self.close_piece();
        self.clip_open = clip.is_some();
        self.stage = Stage::After;
        true
    }

    /// A layer pushed at the page's top level (`open` before it): after the slot, a new Layer segment (Normal/SrcOver
    /// only) with its clip's mask.
    pub(crate) fn push(&mut self, open: u32, blend: BlendMode, fill: Fill, transform: Affine, clip: &impl Shape) {
        if !matches!(self.stage, Stage::After) {
            return;
        }
        if self.layer_depth > 0 {
            self.layer_depth += 1;
            return;
        }
        if open > 0 {
            return;
        }
        if blend != BlendMode::default() {
            return self.unusable("a layer after the slot blends other than Normal/SrcOver");
        }
        let mut mask = vello::Scene::new();
        mask.push_layer(fill, BlendMode::default(), 1.0, transform, clip);
        let b = transform.transform_rect_bbox(clip.bounding_box());
        mask.fill(Fill::NonZero, Affine::IDENTITY, Color::WHITE, None, &b.inflate(1.0, 1.0));
        mask.pop_layer();
        if !self.segments.is_empty() {
            self.close_piece();
        }
        self.segments.push(Segment::Layer { scene: vello::Scene::new(), mask });
        self.layer_depth = 1;
    }

    /// A layer popped: a top-level layer after the slot closed.
    pub(crate) fn pop(&mut self) {
        if matches!(self.stage, Stage::After) && self.layer_depth > 0 {
            self.layer_depth -= 1;
        }
    }
}
