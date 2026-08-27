//! What the attached media in the history costs the model, in tokens.
//!
//! Providers report one prompt total and never break it down by content
//! block, so the media share has to come from somewhere else. Two sources,
//! in order of preference:
//!
//! 1. **Measured** — ask the provider. Anthropic's `count_tokens` endpoint
//!    takes the same blocks the Messages API does and answers with a token
//!    count, so one probe per block (minus the cost of the probe's own
//!    wrapper) is the real figure, PDFs included. It is free and rate
//!    limited separately from message creation.
//! 2. **Computed** — from the image's own pixels, through the provider's
//!    documented formula. Exact on Anthropic, whose rule is published with
//!    a reference implementation; per-family on OpenAI; a plain patch count
//!    everywhere else, where the cost belongs to whichever vision encoder
//!    the served model happens to carry.
//!
//! Anything with no dimensions to work from — audio, PDFs when the
//! provider cannot be asked, an image whose header will not parse — keeps a
//! flat estimate. [`MediaTally::source`] says which of the three the
//! figure in hand actually is, so `/status` can label it rather than
//! implying a precision it does not have.

use std::collections::HashMap;

use rig::message::{AssistantContent, Image, Message, ToolResultContent, UserContent};

use crate::config::{Config, Provider};

/// Fallback for a media part with nothing to measure or compute from.
const ESTIMATED_MEDIA_TOKENS: u64 = 1_000;

/// Bytes of a media file decoded before looking for its dimensions. Image
/// headers sit at the front; a file whose size is not found in the first
/// chunk is decoded whole rather than given up on.
const HEADER_BYTES: usize = 64 * 1024;

/// How long a `count_tokens` probe may take before the breakdown gives up
/// and computes the figure instead. The breakdown is drawn at the end of a
/// turn and by `/status`, so the wait is in front of the user.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Where a media token figure came from, worst last: a tally over several
/// parts reports the weakest source among them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum MediaSource {
    /// The provider counted it.
    Measured,
    /// Computed from the image's pixel dimensions.
    Computed,
    /// A flat figure: nothing to measure or compute from.
    Estimated,
}

impl MediaSource {
    /// Stable identifier, used in the encoded breakdown and to name the
    /// locale key that labels the row.
    pub fn key(self) -> &'static str {
        match self {
            MediaSource::Measured => "measured",
            MediaSource::Computed => "computed",
            MediaSource::Estimated => "estimated",
        }
    }

    /// One word for the `/status` media row, in the interface language.
    /// Lives here because `t!` only ever reads its own crate's catalog and
    /// both front ends render the row.
    pub fn label(self) -> String {
        match self {
            MediaSource::Measured => rust_i18n::t!("media_measured"),
            MediaSource::Computed => rust_i18n::t!("media_computed"),
            MediaSource::Estimated => rust_i18n::t!("media_estimated"),
        }
        .to_string()
    }

    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "measured" => Some(MediaSource::Measured),
            "computed" => Some(MediaSource::Computed),
            "estimated" => Some(MediaSource::Estimated),
            _ => None,
        }
    }
}

/// The media in one history: its token cost, and how that was arrived at.
/// `source` is `None` when there is no media at all.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MediaTally {
    pub tokens: u64,
    pub source: Option<MediaSource>,
}

impl MediaTally {
    fn add(&mut self, tokens: u64, source: MediaSource) {
        self.tokens += tokens;
        // The weakest source wins: a measured image next to an estimated
        // audio file does not make the pair measured.
        self.source = Some(self.source.map_or(source, |s| s.max(source)));
    }
}

/// Per-part token counts, kept across turns so the same attachment is
/// measured once rather than on every breakdown.
#[derive(Default)]
pub struct MediaCounter {
    /// Keyed by model and the part's content hash: a model switch changes
    /// both the tokenizer and the resolution tier.
    cache: HashMap<(String, u64), (u64, MediaSource)>,
    /// What an empty `count_tokens` probe costs, per model — the wrapper
    /// that has to come off each measurement.
    baseline: HashMap<String, u64>,
    /// Set when a probe fails, so a missing key or a dead network costs one
    /// timeout per session instead of one per breakdown.
    probes_off: bool,
}

impl MediaCounter {
    /// Total tokens the history's media parts occupy. Parts already seen
    /// come from the cache; new ones are measured if the provider can be
    /// asked and computed otherwise.
    pub async fn tally(&mut self, cfg: &Config, history: &[Message]) -> MediaTally {
        let mut tally = MediaTally::default();
        for part in media_parts(history) {
            let key = (cfg.model.clone(), part.hash());
            if let Some((tokens, source)) = self.cache.get(&key) {
                tally.add(*tokens, *source);
                continue;
            }
            let (tokens, source) = self.resolve(cfg, &part).await;
            self.cache.insert(key, (tokens, source));
            tally.add(tokens, source);
        }
        tally
    }

    /// One part's cost: the provider's answer, the formula, or the flat
    /// estimate — whichever is available, in that order.
    async fn resolve(&mut self, cfg: &Config, part: &MediaPart) -> (u64, MediaSource) {
        if let Some(tokens) = self.measure(cfg, part).await {
            return (tokens, MediaSource::Measured);
        }
        if let Some((width, height)) = part.dimensions() {
            return (
                image_tokens(cfg.provider, &cfg.model, width, height),
                MediaSource::Computed,
            );
        }
        (ESTIMATED_MEDIA_TOKENS, MediaSource::Estimated)
    }

    /// Ask the provider what the part costs. `None` when the provider has
    /// no endpoint for the question, or the one it has did not answer.
    async fn measure(&mut self, cfg: &Config, part: &MediaPart) -> Option<u64> {
        if self.probes_off || cfg.provider != Provider::Anthropic {
            return None;
        }
        // No key means there is nothing to ask, which costs nothing to
        // find out — not the kind of failure worth disabling probes over.
        let key = std::env::var("ANTHROPIC_API_KEY").ok()?;
        let baseline = match self.baseline.get(&cfg.model) {
            Some(baseline) => *baseline,
            None => match count_tokens(cfg, &key, None).await {
                Some(baseline) => {
                    self.baseline.insert(cfg.model.clone(), baseline);
                    baseline
                }
                // A dead network or a refused key would otherwise cost a
                // timeout on every part of every breakdown.
                None => {
                    self.probes_off = true;
                    return None;
                }
            },
        };
        match count_tokens(cfg, &key, Some(part)).await {
            // The wrapper the probe needs (a role, a one-character text
            // block) is what the baseline takes back off.
            Some(with_part) => Some(with_part.saturating_sub(baseline)),
            None => {
                self.probes_off = true;
                None
            }
        }
    }
}

// ----- the parts themselves ------------------------------------------------

/// One media block found in the history, in the shape a probe needs.
struct MediaPart {
    content: UserContent,
}

impl MediaPart {
    /// Identity for the cache: the source data, whatever form it took.
    fn hash(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        match &self.content {
            UserContent::Image(i) => source_bytes(&i.data).hash(&mut hasher),
            UserContent::Audio(a) => source_bytes(&a.data).hash(&mut hasher),
            UserContent::Video(v) => source_bytes(&v.data).hash(&mut hasher),
            UserContent::Document(d) => source_bytes(&d.data).hash(&mut hasher),
            _ => 0u8.hash(&mut hasher),
        }
        hasher.finish()
    }

    /// Pixel dimensions, for the parts that have any: images whose header
    /// parses. Everything else (audio, PDFs, an unknown container) is
    /// `None` and falls back to the flat estimate.
    fn dimensions(&self) -> Option<(u32, u32)> {
        let UserContent::Image(image) = &self.content else {
            return None;
        };
        let data = base64_source(&image.data)?;
        // Headers are at the front, so a prefix normally answers it; a
        // JPEG carrying a large thumbnail before its frame header is the
        // case that needs the whole file.
        let head = decode_prefix(data, HEADER_BYTES);
        if let Some(size) = head.as_deref().and_then(|b| imagesize::blob_size(b).ok()) {
            return Some((size.width as u32, size.height as u32));
        }
        let whole = decode_prefix(data, usize::MAX)?;
        let size = imagesize::blob_size(&whole).ok()?;
        Some((size.width as u32, size.height as u32))
    }
}

/// Every media block in the history, in order. Images returned by tools and
/// images in assistant turns cost what a user's image costs, so they are
/// collected in the same shape.
fn media_parts(history: &[Message]) -> Vec<MediaPart> {
    let mut parts = Vec::new();
    fn push_image(parts: &mut Vec<MediaPart>, image: &Image) {
        parts.push(MediaPart {
            content: UserContent::Image(image.clone()),
        })
    }
    for message in history {
        match message {
            Message::System { .. } => {}
            Message::User { content } => {
                for part in content.iter() {
                    match part {
                        UserContent::Image(_)
                        | UserContent::Audio(_)
                        | UserContent::Video(_)
                        | UserContent::Document(_) => parts.push(MediaPart {
                            content: part.clone(),
                        }),
                        UserContent::ToolResult(r) => {
                            for c in r.content.iter() {
                                if let ToolResultContent::Image(image) = c {
                                    push_image(&mut parts, image);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            Message::Assistant { content, .. } => {
                for part in content.iter() {
                    if let AssistantContent::Image(image) = part {
                        push_image(&mut parts, image);
                    }
                }
            }
        }
    }
    parts
}

/// The bytes behind a source, for hashing: the base64 text, the URL, the
/// file id — whichever identifies it.
fn source_bytes(kind: &rig::message::DocumentSourceKind) -> &[u8] {
    use rig::message::DocumentSourceKind::*;
    match kind {
        Url(s) | Base64(s) | FileId(s) | String(s) => s.as_bytes(),
        Raw(bytes) => bytes,
        // `Unknown`, and whatever rig adds next: nothing to identify it by,
        // so every such part hashes alike and shares one cache entry.
        _ => &[],
    }
}

/// The base64 payload of a source, if it is one (attachments always are —
/// see [`crate::attachment`]).
fn base64_source(kind: &rig::message::DocumentSourceKind) -> Option<&str> {
    match kind {
        rig::message::DocumentSourceKind::Base64(data) => Some(data),
        _ => None,
    }
}

/// Decode at most `limit` bytes of base64, cutting on a 4-character group
/// so the tail decodes cleanly.
fn decode_prefix(data: &str, limit: usize) -> Option<Vec<u8>> {
    use base64::Engine;
    let engine = base64::engine::general_purpose::STANDARD;
    if limit == usize::MAX || data.len() <= limit {
        return engine.decode(data).ok();
    }
    let groups = (limit / 3).max(1) * 4;
    let end = groups.min(data.len() - data.len() % 4);
    engine.decode(&data[..end]).ok()
}

// ----- computing from the image --------------------------------------------

/// Tokens an image of these dimensions costs on this provider, by the
/// provider's own documented rule.
pub fn image_tokens(provider: Provider, model: &str, width: u32, height: u32) -> u64 {
    match provider {
        Provider::Anthropic => anthropic_image_tokens(model, width, height),
        Provider::Openai => openai_image_tokens(model, width, height),
        // Ollama serves whatever vision model was pulled, and the cost is
        // the projector's, not the server's: llava spends a fixed 576 on
        // any image, the Qwen-VL family scales with the pixels. A plain
        // patch count is the shape most of them have.
        Provider::Ollama => patches(width, 32) * patches(height, 32),
    }
}

/// `⌈size / patch⌉`, the patch count along one edge.
fn patches(size: u32, patch: u32) -> u64 {
    (size.max(1) as u64).div_ceil(patch as u64)
}

/// Claude sees an image as 28x28 patches, one visual token each, after
/// downscaling it to fit the model's resolution tier.
fn anthropic_image_tokens(model: &str, width: u32, height: u32) -> u64 {
    let (max_edge, max_tokens) = anthropic_tier(model);
    let (width, height) = anthropic_resize(width, height, max_edge, max_tokens);
    patches(width, 28) * patches(height, 28)
}

/// The tier's (max edge, max visual tokens). Claude 4.7 and later see
/// images at the higher resolution; everything else, including a name this
/// cannot read a version out of, is standard.
fn anthropic_tier(model: &str) -> (u32, u64) {
    match model_version(model) {
        Some(version) if version >= (4, 7) => (2576, 4784),
        _ => (1568, 1568),
    }
}

/// The `(major, minor)` version in a model name: the first number in it,
/// and the number after that when it is short enough to be a minor rather
/// than a date. Reads `claude-opus-4-1-20250805` as 4.1, `claude-opus-5`
/// as 5.0, and the older `claude-3-5-sonnet` as 3.5.
fn model_version(model: &str) -> Option<(u32, u32)> {
    let mut parts = model
        .split(['-', '.'])
        .skip_while(|p| p.parse::<u32>().is_err());
    let major: u32 = parts.next()?.parse().ok()?;
    let minor = parts
        .next()
        .filter(|p| p.len() <= 2)
        .and_then(|p| p.parse().ok())
        .unwrap_or(0);
    Some((major, minor))
}

/// The size Claude resizes an image to: the largest aspect-preserving size
/// that fits both the tier's edge limit and its visual-token budget, found
/// the way the published reference implementation finds it — by binary
/// search along the long edge.
fn anthropic_resize(width: u32, height: u32, max_edge: u32, max_tokens: u64) -> (u32, u32) {
    let fits = |w: u32, h: u32| {
        patches(w, 28) * 28 <= max_edge as u64
            && patches(h, 28) * 28 <= max_edge as u64
            && patches(w, 28) * patches(h, 28) <= max_tokens
    };
    if fits(width, height) {
        return (width, height);
    }
    if height > width {
        let (w, h) = anthropic_resize(height, width, max_edge, max_tokens);
        return (h, w);
    }
    let aspect = width as f64 / height as f64;
    // The short edge rounds half to even, matching the live API at exact
    // ties; rounding halves up computes a different size for some images.
    let short = |long: u32| (round_half_even(long as f64 / aspect) as u32).max(1);
    let (mut lo, mut hi) = (1u32, width); // lo always fits; hi never does
    while lo + 1 < hi {
        let mid = lo + (hi - lo) / 2;
        if fits(mid, short(mid)) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    (lo, short(lo))
}

/// Round half to even (what Python's `round` does, and what the API does at
/// an exact tie).
fn round_half_even(value: f64) -> f64 {
    let floor = value.floor();
    if value - floor != 0.5 {
        return value.round();
    }
    if floor as i64 % 2 == 0 {
        floor
    } else {
        floor + 1.0
    }
}

/// How one OpenAI model family charges for an image.
enum OpenaiImageRule {
    /// 512-pixel tiles over a base: `base + tiles * per_tile`.
    Tiles { base: u64, per_tile: u64 },
    /// 32-pixel patches, shrunk to a budget, times a per-model multiplier.
    Patches {
        multiplier: f64,
        budget: Option<u64>,
    },
}

/// Model-name prefix to rule, longest prefix first so `gpt-5-mini` is not
/// read as `gpt-5`. A name that matches nothing gets the tile rule the
/// GPT-4o and 4.1 families use, which is also what an OpenAI-compatible
/// server most often stands in for; the served model's own encoder is
/// beyond anything documented here.
const OPENAI_IMAGE_RULES: &[(&str, OpenaiImageRule)] = &[
    (
        "gpt-5.6",
        OpenaiImageRule::Patches {
            multiplier: 1.2,
            budget: None,
        },
    ),
    (
        "gpt-5.5",
        OpenaiImageRule::Patches {
            multiplier: 1.2,
            budget: Some(2_500),
        },
    ),
    (
        "gpt-5.4",
        OpenaiImageRule::Patches {
            multiplier: 1.2,
            budget: Some(2_500),
        },
    ),
    (
        "gpt-5.2",
        OpenaiImageRule::Patches {
            multiplier: 1.2,
            budget: Some(6_144),
        },
    ),
    (
        "gpt-5.1",
        OpenaiImageRule::Tiles {
            base: 70,
            per_tile: 140,
        },
    ),
    (
        "gpt-5-nano",
        OpenaiImageRule::Patches {
            multiplier: 1.5,
            budget: Some(6_144),
        },
    ),
    (
        "gpt-5-mini",
        OpenaiImageRule::Patches {
            multiplier: 1.2,
            budget: Some(6_144),
        },
    ),
    (
        "gpt-5",
        OpenaiImageRule::Tiles {
            base: 70,
            per_tile: 140,
        },
    ),
    (
        "gpt-4.1-mini",
        OpenaiImageRule::Patches {
            multiplier: 1.62,
            budget: Some(6_144),
        },
    ),
];

/// The tile rule of the GPT-4o / 4.1 families, and the fallback.
const OPENAI_DEFAULT_RULE: OpenaiImageRule = OpenaiImageRule::Tiles {
    base: 85,
    per_tile: 170,
};

/// Largest dimension OpenAI keeps at the default (`auto`, which behaves as
/// `high`) detail level.
const OPENAI_MAX_EDGE: u32 = 2048;

fn openai_image_tokens(model: &str, width: u32, height: u32) -> u64 {
    let rule = OPENAI_IMAGE_RULES
        .iter()
        .find(|(prefix, _)| model.starts_with(prefix))
        .map(|(_, rule)| rule)
        .unwrap_or(&OPENAI_DEFAULT_RULE);
    let (width, height) = fit_within(width, height, OPENAI_MAX_EDGE, OPENAI_MAX_EDGE);
    match rule {
        OpenaiImageRule::Tiles { base, per_tile } => {
            // Then the shortest side comes down to 768.
            let (width, height) = shrink_short_edge(width, height, 768);
            base + patches(width, 512) * patches(height, 512) * per_tile
        }
        OpenaiImageRule::Patches { multiplier, budget } => {
            let count = patches(width, 32) * patches(height, 32);
            let count = match budget {
                Some(budget) if count > *budget => {
                    let (width, height) = shrink_to_patch_budget(width, height, *budget);
                    patches(width, 32) * patches(height, 32)
                }
                _ => count,
            };
            (count as f64 * multiplier).ceil() as u64
        }
    }
}

/// Scale down to fit a box, preserving the aspect ratio. Smaller images are
/// left alone.
fn fit_within(width: u32, height: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    if width <= max_width && height <= max_height {
        return (width, height);
    }
    let scale = (max_width as f64 / width as f64).min(max_height as f64 / height as f64);
    (
        ((width as f64 * scale).round() as u32).max(1),
        ((height as f64 * scale).round() as u32).max(1),
    )
}

/// Scale down until the shorter side is `target`. A already-smaller image
/// is left alone.
fn shrink_short_edge(width: u32, height: u32, target: u32) -> (u32, u32) {
    let short = width.min(height);
    if short <= target {
        return (width, height);
    }
    let scale = target as f64 / short as f64;
    (
        ((width as f64 * scale).round() as u32).max(1),
        ((height as f64 * scale).round() as u32).max(1),
    )
}

/// The documented shrink for an image over a model's patch budget: scale so
/// the patches fit, then trim the factor so both edges land on whole
/// patches.
fn shrink_to_patch_budget(width: u32, height: u32, budget: u64) -> (u32, u32) {
    let (w, h) = (width as f64, height as f64);
    let shrink = ((32.0 * 32.0 * budget as f64) / (w * h)).sqrt();
    let whole = |edge: f64| {
        let patches = edge * shrink / 32.0;
        patches.floor() / patches
    };
    let adjusted = shrink * whole(w).min(whole(h));
    (
        ((w * adjusted) as u32).max(1),
        ((h * adjusted) as u32).max(1),
    )
}

// ----- asking the provider -------------------------------------------------

/// Anthropic's token-counting endpoint: the same blocks the Messages API
/// takes, answered with a count. Free, and rate limited separately from
/// message creation, so a probe per new attachment costs nothing but the
/// round trip. `part` is `None` for the baseline probe — the wrapper alone.
async fn count_tokens(cfg: &Config, key: &str, part: Option<&MediaPart>) -> Option<u64> {
    let base = crate::models::base_url(Provider::Anthropic, cfg.base_url.as_deref());
    let base = base.trim_end_matches('/');

    // A text block rides along because a message needs content the model
    // could read; its cost is what the baseline probe takes back off.
    let mut content = rig::OneOrMany::one(UserContent::text(PROBE_TEXT));
    if let Some(part) = part {
        content.insert(0, part.content.clone());
    }
    let message = Message::User { content };
    // rig owns the conversion into Anthropic's wire shape, media included,
    // so the probe carries exactly the blocks a real request would.
    let message: rig::providers::anthropic::completion::Message = message.try_into().ok()?;
    let body = serde_json::json!({
        "model": cfg.model,
        "messages": [message],
    });

    let response = reqwest::Client::new()
        .post(format!("{base}/v1/messages/count_tokens"))
        .header("x-api-key", key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .timeout(PROBE_TIMEOUT)
        .json(&body)
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let counted: serde_json::Value = response.json().await.ok()?;
    counted.get("input_tokens")?.as_u64()
}

/// The one-character text block every probe carries.
const PROBE_TEXT: &str = "x";

#[cfg(test)]
mod tests {
    use super::*;

    /// The token costs published alongside the patch rule, standard tier
    /// and high-resolution tier. Getting these right is the whole point of
    /// computing from dimensions rather than guessing a flat figure.
    #[test]
    fn anthropic_matches_the_published_table() {
        let standard = |w, h| anthropic_image_tokens("claude-sonnet-4-5", w, h);
        let high = |w, h| anthropic_image_tokens("claude-opus-5", w, h);

        assert_eq!(standard(200, 200), 64);
        assert_eq!(high(200, 200), 64);
        assert_eq!(standard(1000, 1000), 1296);
        assert_eq!(high(1000, 1000), 1296);
        assert_eq!(standard(1092, 1092), 1521);
        assert_eq!(standard(1920, 1080), 1560);
        assert_eq!(high(1920, 1080), 2691);
        assert_eq!(standard(2000, 1500), 1564);
        assert_eq!(high(2000, 1500), 3888);
        assert_eq!(standard(3840, 2160), 1560);
        assert_eq!(high(3840, 2160), 4784);
    }

    /// The resize the documentation walks through: an A4 page scanned at
    /// 130 DPI is under the edge limit on both sides and still resized,
    /// because its patches are over the budget.
    #[test]
    fn the_token_budget_resizes_an_image_within_the_edge_limit() {
        assert_eq!(anthropic_resize(1075, 1520, 1568, 1568), (924, 1307));
        // The same scan on a high-resolution model is left alone.
        assert_eq!(anthropic_resize(1075, 1520, 2576, 4784), (1075, 1520));
        assert_eq!(anthropic_resize(1920, 1080, 1568, 1568), (1456, 819));
    }

    #[test]
    fn model_versions_survive_date_suffixes_and_old_name_shapes() {
        assert_eq!(model_version("claude-opus-4-1-20250805"), Some((4, 1)));
        assert_eq!(model_version("claude-sonnet-4-6"), Some((4, 6)));
        assert_eq!(model_version("claude-opus-5"), Some((5, 0)));
        assert_eq!(model_version("claude-opus-5-20260101"), Some((5, 0)));
        assert_eq!(model_version("claude-3-5-sonnet-20241022"), Some((3, 5)));
        assert_eq!(model_version("some-local-model"), None);
    }

    /// A name with no version in it must not be read as a high-resolution
    /// model: that would triple the figure for every image.
    #[test]
    fn an_unreadable_model_name_gets_the_standard_tier() {
        assert_eq!(anthropic_tier("claude-latest"), (1568, 1568));
        assert_eq!(anthropic_tier("claude-opus-4-7"), (2576, 4784));
        assert_eq!(anthropic_tier("claude-opus-5"), (2576, 4784));
    }

    /// The worked example from OpenAI's patch rule: a square that fits the
    /// budget, and one that has to be shrunk into it.
    #[test]
    fn openai_patch_rule_matches_the_worked_example() {
        assert_eq!(openai_image_tokens("gpt-5.4", 1024, 1024), 1229);
        assert_eq!(openai_image_tokens("gpt-5.4", 2048, 2048), 3000);
    }

    /// Tiles: 1024x1024 comes down to 768x768, which is four 512-pixel
    /// tiles over the family's base.
    #[test]
    fn openai_tile_rule_counts_512_pixel_tiles() {
        assert_eq!(openai_image_tokens("gpt-4o", 1024, 1024), 85 + 4 * 170);
        assert_eq!(openai_image_tokens("gpt-5", 1024, 1024), 70 + 4 * 140);
        // An unknown name falls back to the 4o/4.1 rates rather than to a
        // flat figure.
        assert_eq!(
            openai_image_tokens("some-vllm-model", 1024, 1024),
            85 + 4 * 170
        );
    }

    /// Prefix matching is longest-first: the mini and nano rules must not
    /// be shadowed by the family they start with.
    #[test]
    fn the_longer_model_prefix_wins() {
        assert_ne!(
            openai_image_tokens("gpt-5-nano", 1024, 1024),
            openai_image_tokens("gpt-5", 1024, 1024)
        );
        assert_eq!(openai_image_tokens("gpt-5-nano", 1024, 1024), 1536);
    }

    /// A tally reports the weakest source it saw, so one estimated part
    /// cannot be presented as a measurement.
    #[test]
    fn a_tally_reports_its_weakest_source() {
        let mut tally = MediaTally::default();
        assert_eq!(tally.source, None);
        tally.add(100, MediaSource::Measured);
        assert_eq!(tally.source, Some(MediaSource::Measured));
        tally.add(1_000, MediaSource::Estimated);
        assert_eq!(tally.source, Some(MediaSource::Estimated));
        tally.add(50, MediaSource::Computed);
        assert_eq!(tally.tokens, 1_150);
        assert_eq!(tally.source, Some(MediaSource::Estimated));
    }

    #[test]
    fn media_source_keys_round_trip() {
        for source in [
            MediaSource::Measured,
            MediaSource::Computed,
            MediaSource::Estimated,
        ] {
            assert_eq!(MediaSource::from_key(source.key()), Some(source));
        }
        assert_eq!(MediaSource::from_key("guessed"), None);
    }

    /// A PNG header is enough to size an image, and it is read from a
    /// prefix rather than by decoding the whole attachment.
    #[test]
    fn an_image_part_reads_its_dimensions_from_the_header() {
        use base64::Engine;
        // 3x1 PNG, padded with trailing data the prefix decode will cut.
        let mut png = vec![
            0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0x0d, b'I', b'H', b'D', b'R',
            0, 0, 0, 3, 0, 0, 0, 1, 8, 6, 0, 0, 0,
        ];
        png.extend(std::iter::repeat_n(0u8, 4096));
        let data = base64::engine::general_purpose::STANDARD.encode(&png);
        let part = MediaPart {
            content: UserContent::image_base64(data, Some(rig::message::ImageMediaType::PNG), None),
        };
        assert_eq!(part.dimensions(), Some((3, 1)));
    }

    /// Audio has no dimensions to work from, so it falls to the estimate
    /// rather than to a computed figure of zero.
    #[test]
    fn a_part_without_dimensions_has_none() {
        let part = MediaPart {
            content: UserContent::audio(
                "AAAA".to_string(),
                Some(rig::message::AudioMediaType::MP3),
            ),
        };
        assert_eq!(part.dimensions(), None);
    }

    #[test]
    fn media_parts_are_found_in_every_role() {
        use rig::OneOrMany;
        let image = Image {
            data: rig::message::DocumentSourceKind::Base64("AAAA".into()),
            media_type: Some(rig::message::ImageMediaType::PNG),
            detail: None,
            additional_params: None,
        };
        let history = vec![
            Message::User {
                content: OneOrMany::many([
                    UserContent::text("hi"),
                    UserContent::Image(image.clone()),
                ])
                .unwrap(),
            },
            Message::Assistant {
                id: None,
                content: OneOrMany::one(AssistantContent::Image(image.clone())),
            },
        ];
        assert_eq!(media_parts(&history).len(), 2);
        // Text-only history has nothing to tally.
        assert!(media_parts(&[Message::user("hi")]).is_empty());
    }
}
