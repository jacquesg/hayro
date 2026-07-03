use crate::font::Glyph;
use crate::soft_mask::SoftMask;
use crate::{BlendMode, ClipPath, FillRule, Image};
use crate::{DrawMode, DrawProps, ImageDrawProps};
use kurbo::{Affine, BezPath, Rect, Shape};

/// A trait for a device that can be used to process PDF drawing instructions.
///
/// Note: [`begin_marked_content`](Device::begin_marked_content) gained an
/// `actual_text` parameter, a source-breaking change to this trait. This fork
/// has no external implementors, so the break is accepted rather than absorbed
/// behind a struct.
pub trait Device<'a> {
    /// Draw a path.
    fn draw_path(&mut self, path: &BezPath, props: DrawProps<'a>, draw_mode: &DrawMode);
    /// Push a new clip path to the clip stack.
    fn push_clip_path(&mut self, clip_path: &ClipPath);
    /// Push a rectangular clip to the clip stack.
    fn push_clip_rect(&mut self, rect: &Rect) {
        self.push_clip_path(&ClipPath {
            path: rect.to_path(0.1),
            fill: FillRule::NonZero,
        });
    }
    /// Push a new transparency group to the blend stack.
    fn push_transparency_group(
        &mut self,
        opacity: f32,
        mask: Option<SoftMask<'a>>,
        blend_mode: BlendMode,
    );
    /// Draw a glyph.
    fn draw_glyph(
        &mut self,
        glyph: &Glyph<'a>,
        glyph_transform: Affine,
        props: DrawProps<'a>,
        draw_mode: &DrawMode,
    );
    /// Draw an image.
    fn draw_image(&mut self, image: Image<'a, '_>, props: ImageDrawProps<'a>);
    /// Pop the last clip path or clip rectangle from the clip stack.
    fn pop_clip(&mut self);
    /// Pop the last transparency group from the blend stack.
    fn pop_transparency_group(&mut self);
    /// Draw a rectangle directly, without going through the general path pipeline.
    fn draw_rect(&mut self, rect: &Rect, props: DrawProps<'a>, draw_mode: &DrawMode) {
        self.draw_path(&rect.to_path(0.1), props, draw_mode);
    }
    /// Called at the beginning of a marked content sequence (BMC/BDC).
    ///
    /// The tag is the marked content tag (e.g. b"P", b"Span"). The mcid is the
    /// marked content identifier from the properties dict, if present.
    /// `actual_text` carries the raw `/ActualText` string bytes (PDF text-string
    /// encoding — UTF-16BE, UTF-8, or `PDFDocEncoding`; see ISO 32000-2 §7.9.2.2)
    /// from the properties dict, if present; a recording device can use it as the
    /// text for glyphs that lack a `ToUnicode` mapping (e.g. colour-emoji glyphs).
    fn begin_marked_content(
        &mut self,
        _tag: &[u8],
        _mcid: Option<i32>,
        _actual_text: Option<&[u8]>,
    ) {
    }
    /// Called at the end of a marked content sequence (EMC).
    fn end_marked_content(&mut self) {}
    /// Called immediately before and after each single text-showing operator
    /// (`Tj`, `TJ`, `'`, `"`).
    ///
    /// One run corresponds to one text-showing operator; a recording device can
    /// bracket on these to group glyphs into runs that mirror the content
    /// stream's own text-show operators — the same segmentation a viewer's text
    /// layer exposes — instead of inferring run boundaries from glyph geometry.
    /// Rendering devices ignore them. Default: no-op.
    ///
    /// A run may bracket zero [`draw_glyph`](Device::draw_glyph) calls (an empty
    /// string, no resolvable font, glyphs suppressed by an optional-content
    /// group, or a clip-only text-rendering mode). Consumers must key off
    /// `draw_glyph`, not assume a run is non-empty.
    ///
    /// The interpreter emits runs flat, never nested. A device that re-interprets
    /// a Type 3 glyph's content stream may itself nest runs; that is the device's
    /// concern, not the interpreter's.
    fn begin_glyph_run(&mut self) {}
    /// Called after the last glyph of a text-showing operator. See
    /// [`begin_glyph_run`](Device::begin_glyph_run).
    fn end_glyph_run(&mut self) {}
}

/// A device that discards all drawing operations.
pub struct DummyDevice;

impl Device<'_> for DummyDevice {
    fn draw_path(&mut self, _: &BezPath, _: DrawProps<'_>, _: &DrawMode) {}
    fn push_clip_path(&mut self, _: &ClipPath) {}
    fn push_transparency_group(&mut self, _: f32, _: Option<SoftMask<'_>>, _: BlendMode) {}
    fn draw_glyph(&mut self, _: &Glyph<'_>, _: Affine, _: DrawProps<'_>, _: &DrawMode) {}
    fn draw_image(&mut self, _: Image<'_, '_>, _: ImageDrawProps<'_>) {}
    fn pop_clip(&mut self) {}
    fn pop_transparency_group(&mut self) {}
}
