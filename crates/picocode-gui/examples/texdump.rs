//! Debug helper: dump sample formulas as PNGs (dark + light foreground),
//! mirroring src/tex.rs's pipeline.
use ratex_layout::{LayoutOptions, layout, to_display_list};
use ratex_render::{RenderOptions, render_to_png};
use ratex_types::color::Color;
use ratex_types::math_style::MathStyle;

fn render(tex: &str, color: Color) -> Vec<u8> {
    let nodes = ratex_parser::parser::parse(tex).expect("parse");
    let options = LayoutOptions::default()
        .with_style(MathStyle::Display)
        .with_color(color);
    let lbox = layout(&nodes, &options);
    render_to_png(
        &to_display_list(&lbox),
        &RenderOptions {
            font_size: 17.0,
            padding: 2.0,
            background_color: Color {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            },
            font_dir: String::new(),
            device_pixel_ratio: 2.0,
        },
    )
    .expect("render")
}

fn main() {
    let out = std::env::args().nth(1).expect("usage: texdump <dir>");
    let white = Color {
        r: 1.0,
        g: 1.0,
        b: 1.0,
        a: 1.0,
    };
    let black = Color {
        r: 0.0,
        g: 0.0,
        b: 0.0,
        a: 1.0,
    };
    let samples = [
        ("quadratic", r"x = \frac{-b \pm \sqrt{b^2 - 4ac}}{2a}"),
        (
            "integral",
            r"\int_0^\infty e^{-x^2}\,dx = \frac{\sqrt{\pi}}{2}",
        ),
        ("matrix", r"\begin{pmatrix} a & b \\ c & d \end{pmatrix}"),
    ];
    for (name, tex) in samples {
        for (variant, color) in [("dark", white), ("light", black)] {
            std::fs::write(format!("{out}/{name}-{variant}.png"), render(tex, color)).unwrap();
        }
    }
    println!("done");
}
