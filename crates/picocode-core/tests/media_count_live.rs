//! Checks the measured media path against a real Anthropic account: the
//! `count_tokens` probe, the baseline subtraction, and the formula the
//! computed path would have used instead.
//!
//! The two should agree — the formula is Anthropic's own published rule —
//! so this is where a drift in either shows up first.
//!
//! Ignored by default: it needs `ANTHROPIC_API_KEY` and a network, so CI
//! would fail on it for the wrong reason. Run it deliberately when
//! touching the probe or the formula:
//!
//! ```sh
//! cargo test -p picocode-core --test media_count_live -- --ignored --nocapture
//! ```

use base64::Engine;
use picocode_core::config::{Args, Config, Provider};
use picocode_core::media::{MediaCounter, MediaSource};

/// A model on the standard resolution tier, where the published table puts
/// a 1000x1000 image at 1296 visual tokens.
const MODEL: &str = "claude-haiku-4-5-20251001";
const EXPECTED: u64 = 1296;

#[tokio::test]
#[ignore = "needs ANTHROPIC_API_KEY and a network"]
async fn anthropic_counts_an_image_the_way_the_formula_does() {
    // The binaries do this in `Config::from_args`; reqwest refuses to
    // build a client without it.
    picocode_core::config::install_tls_provider();
    assert!(
        std::env::var("ANTHROPIC_API_KEY").is_ok(),
        "ANTHROPIC_API_KEY is not set"
    );

    let mut cfg = Config::from_args(Args::for_workspace(None)).unwrap();
    cfg.provider = Provider::Anthropic;
    cfg.model = MODEL.to_string();

    let png = base64::engine::general_purpose::STANDARD.encode(square_png(1000));
    let history = vec![rig::completion::Message::User {
        content: rig::OneOrMany::many([
            rig::message::UserContent::text("what is this"),
            rig::message::UserContent::image_base64(
                png,
                Some(rig::message::ImageMediaType::PNG),
                None,
            ),
        ])
        .unwrap(),
    }];

    let tally = MediaCounter::default().tally(&cfg, &history).await;
    println!("measured {} tokens (formula says {EXPECTED})", tally.tokens);
    assert_eq!(
        tally.source,
        Some(MediaSource::Measured),
        "the probe did not answer — the figure fell back to {:?}",
        tally.source
    );

    // The probe carries a block wrapper the baseline cannot take off to
    // the token, so agreement is close rather than exact.
    let drift = tally.tokens.abs_diff(EXPECTED);
    assert!(
        drift <= EXPECTED / 20,
        "measured {} vs the formula's {EXPECTED} — more than 5% apart",
        tally.tokens
    );
}

/// A solid square PNG of the given size, encoded for real: the endpoint
/// reads the image, so a hand-written header would not do.
fn square_png(size: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, size, size);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().unwrap();
    writer
        .write_image_data(&vec![0x80; (size * size * 3) as usize])
        .unwrap();
    writer.finish().unwrap();
    out
}
