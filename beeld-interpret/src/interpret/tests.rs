//! Behavioural tests for the glyph-run and marked-content events the
//! interpreter emits to a [`Device`], driven through content streams with a
//! recording device.

use super::interpret;
use crate::font::Glyph;
use crate::{
    BlendMode, ClipPath, Context, Device, DrawMode, DrawProps, Image, ImageDrawProps,
    InterpreterCache, InterpreterSettings, SoftMask,
};
use beeld_syntax::Pdf;
use beeld_syntax::content::TypedIter;
use kurbo::{Affine, BezPath, Rect};

/// A single device call, reduced to the facts under test.
#[derive(Debug, PartialEq)]
enum Event {
    BeginRun,
    EndRun,
    BeginMc {
        mcid: Option<i32>,
        actual_text: Option<Vec<u8>>,
    },
    EndMc,
    Glyph,
}

/// A device that records the event stream and discards all rendering.
#[derive(Default)]
struct Recorder {
    events: Vec<Event>,
}

impl<'a> Device<'a> for Recorder {
    fn draw_path(&mut self, _path: &BezPath, _props: DrawProps<'a>, _mode: &DrawMode) {}
    fn push_clip_path(&mut self, _clip: &ClipPath) {}
    fn push_transparency_group(
        &mut self,
        _opacity: f32,
        _mask: Option<SoftMask<'a>>,
        _blend: BlendMode,
    ) {
    }
    fn draw_glyph(
        &mut self,
        _glyph: &Glyph<'a>,
        _transform: Affine,
        _props: DrawProps<'a>,
        _mode: &DrawMode,
    ) {
        self.events.push(Event::Glyph);
    }
    fn draw_image(&mut self, _image: Image<'a, '_>, _props: ImageDrawProps<'a>) {}
    fn pop_clip(&mut self) {}
    fn pop_transparency_group(&mut self) {}
    fn begin_marked_content(&mut self, _tag: &[u8], mcid: Option<i32>, actual_text: Option<&[u8]>) {
        self.events.push(Event::BeginMc {
            mcid,
            actual_text: actual_text.map(<[u8]>::to_vec),
        });
    }
    fn end_marked_content(&mut self) {
        self.events.push(Event::EndMc);
    }
    fn begin_glyph_run(&mut self) {
        self.events.push(Event::BeginRun);
    }
    fn end_glyph_run(&mut self) {
        self.events.push(Event::EndRun);
    }
}

/// A one-page PDF whose page resources carry a named property list `/P1`
/// (with `/MCID` and `/ActualText`), so the named-`/Properties` path can be
/// exercised. Object byte offsets are captured as the buffer is built, so the
/// xref stays accurate regardless of content.
fn fixture() -> Vec<u8> {
    let mut pdf: Vec<u8> = Vec::new();
    pdf.extend_from_slice(b"%PDF-1.7\n");
    let off1 = pdf.len();
    pdf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
    let off2 = pdf.len();
    pdf.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");
    let off3 = pdf.len();
    pdf.extend_from_slice(
        b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources << /Properties << /P1 << /MCID 7 /ActualText (Emoji) >> >> >> /Contents 4 0 R >>\nendobj\n",
    );
    let off4 = pdf.len();
    pdf.extend_from_slice(b"4 0 obj\n<< /Length 0 >>\nstream\nendstream\nendobj\n");
    let xref_pos = pdf.len();
    pdf.extend_from_slice(b"xref\n0 5\n0000000000 65535 f \n");
    pdf.extend_from_slice(format!("{off1:010} 00000 n \n").as_bytes());
    pdf.extend_from_slice(format!("{off2:010} 00000 n \n").as_bytes());
    pdf.extend_from_slice(format!("{off3:010} 00000 n \n").as_bytes());
    pdf.extend_from_slice(format!("{off4:010} 00000 n \n").as_bytes());
    pdf.extend_from_slice(b"trailer\n<< /Size 5 /Root 1 0 R >>\n");
    pdf.extend_from_slice(format!("startxref\n{xref_pos}\n%%EOF").as_bytes());
    pdf
}

/// Interpret `content` against the fixture page's resources and return the
/// recorded event stream.
fn run(content: &[u8]) -> Vec<Event> {
    let pdf = Pdf::new(fixture()).expect("fixture parses");
    let page = pdf.pages().first().expect("one page");
    let cache = InterpreterCache::new();
    let mut context = Context::new(
        Affine::IDENTITY,
        Rect::new(0.0, 0.0, 200.0, 200.0),
        &cache,
        page.xref(),
        InterpreterSettings::default(),
    );
    let mut device = Recorder::default();
    interpret(
        TypedIter::new(content),
        page.resources(),
        &mut context,
        &mut device,
    );
    device.events
}

#[cfg(feature = "embed-fonts")]
fn glyph_count(events: &[Event]) -> usize {
    events.iter().filter(|e| matches!(e, Event::Glyph)).count()
}

/// Assert the events form exactly one glyph run that brackets `min_glyphs` or
/// more glyphs and nothing else.
#[cfg(feature = "embed-fonts")]
fn assert_single_glyph_run(events: &[Event], min_glyphs: usize) {
    assert!(events.len() >= 2, "run must have brackets: {events:?}");
    assert_eq!(events.first(), Some(&Event::BeginRun), "opens with a run");
    assert_eq!(events.last(), Some(&Event::EndRun), "closes the run");
    assert!(
        events[1..events.len() - 1]
            .iter()
            .all(|e| matches!(e, Event::Glyph)),
        "a run brackets only glyphs: {events:?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::BeginRun))
            .count(),
        1,
        "exactly one run: {events:?}"
    );
    assert!(
        glyph_count(events) >= min_glyphs,
        "expected at least {min_glyphs} glyphs: {events:?}"
    );
}

#[cfg(feature = "embed-fonts")]
#[test]
fn tj_brackets_a_single_glyph_run() {
    assert_single_glyph_run(&run(b"BT /F1 12 Tf (AB) Tj ET"), 2);
}

#[cfg(feature = "embed-fonts")]
#[test]
fn tj_multi_element_is_one_run() {
    // A `TJ` with several string/number elements is one text-showing operator,
    // so it is a single run, not one run per element.
    assert_single_glyph_run(&run(b"BT /F1 12 Tf [(A) -100 (B)] TJ ET"), 2);
}

#[cfg(feature = "embed-fonts")]
#[test]
fn quote_operator_brackets_a_run() {
    assert_single_glyph_run(&run(b"BT /F1 12 Tf (Z) ' ET"), 1);
}

#[cfg(feature = "embed-fonts")]
#[test]
fn double_quote_operator_brackets_a_run() {
    assert_single_glyph_run(&run(b"BT /F1 12 Tf 0 0 (Q) \" ET"), 1);
}

/// Pins the empty-run contract documented on `begin_glyph_run` (device.rs): a
/// text-showing operator brackets a run even when it draws zero glyphs (here an
/// empty string), so consumers must key off `draw_glyph`, not assume the run is
/// non-empty.
#[test]
fn empty_string_still_brackets_a_run() {
    assert_eq!(
        run(b"BT /F1 12 Tf () Tj ET"),
        vec![Event::BeginRun, Event::EndRun]
    );
}

#[test]
fn nested_bdc_emc_nests_marked_content() {
    let events = run(b"/Span << /MCID 1 >> BDC /Span << /MCID 2 >> BDC EMC EMC");
    assert_eq!(
        events,
        vec![
            Event::BeginMc {
                mcid: Some(1),
                actual_text: None,
            },
            Event::BeginMc {
                mcid: Some(2),
                actual_text: None,
            },
            Event::EndMc,
            Event::EndMc,
        ]
    );
}

#[test]
fn inline_span_actual_text_is_reported() {
    let events = run(b"/Span << /ActualText (Hi) >> BDC EMC");
    assert_eq!(
        events,
        vec![
            Event::BeginMc {
                mcid: None,
                actual_text: Some(b"Hi".to_vec()),
            },
            Event::EndMc,
        ]
    );
}

/// Pins D1: a named `/Properties` reference must be resolved so `/MCID` and
/// `/ActualText` reach the device. Before the fix both were dropped, because
/// the operand is a name and `dict_or_stream` returns `None` for a name.
#[test]
fn named_properties_reference_reports_mcid_and_actual_text() {
    let events = run(b"/Span /P1 BDC EMC");
    assert_eq!(
        events,
        vec![
            Event::BeginMc {
                mcid: Some(7),
                actual_text: Some(b"Emoji".to_vec()),
            },
            Event::EndMc,
        ]
    );
}

/// Pins the forwarding contract documented on `begin_marked_content`
/// (device.rs): the interpreter hands the device the raw `/ActualText`
/// text-string bytes undecoded, so a device must detect the text-string
/// encoding itself. A UTF-8 text string is signalled by a leading EF BB BF
/// byte-order mark (ISO 32000-2 §7.9.2.2.1); that BOM and the bytes after it
/// must reach the device verbatim, not stripped or transcoded to UTF-16BE or
/// `PDFDocEncoding`.
#[test]
fn utf8_bom_actual_text_is_forwarded_undecoded() {
    // `<EFBBBF4869>` is a hex string whose decoded bytes are the UTF-8 BOM
    // (EF BB BF) followed by "Hi" (48 69).
    let events = run(b"/Span << /ActualText <EFBBBF4869> >> BDC EMC");
    assert_eq!(
        events,
        vec![
            Event::BeginMc {
                mcid: None,
                actual_text: Some(vec![0xEF, 0xBB, 0xBF, 0x48, 0x69]),
            },
            Event::EndMc,
        ]
    );
}
