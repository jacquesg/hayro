//! Convert a PDF file into a series of SVG files.

use beeld_interpret::InterpreterSettings;
use beeld_svg::beeld_syntax::Pdf;
use beeld_svg::{RenderCache, SvgRenderSettings, convert};

fn main() {
    let pdf = std::fs::read(std::env::args().nth(1).unwrap()).unwrap();
    let pdf = Pdf::new(pdf).unwrap();

    let cache = RenderCache::new();
    let interpreter_settings = InterpreterSettings::default();
    let render_settings = SvgRenderSettings::default();

    for (idx, page) in pdf.pages().iter().enumerate() {
        let svg = convert(page, &cache, &interpreter_settings, &render_settings);
        std::fs::write(format!("rendered_{idx}.svg"), svg).unwrap();
    }
}
