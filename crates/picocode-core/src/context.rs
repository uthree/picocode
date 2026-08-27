//! Estimated breakdown of what fills the model's context window, backing
//! the colored detail block `/status` (alias `/usage`) shows in both
//! front ends.
//!
//! Per-category tokens are estimated from character counts (≈4 chars per
//! token, the same heuristic as the live output counter). Media is the
//! exception: character counts say nothing about what an image costs, so
//! [`crate::media`] works it out separately — by asking the provider where
//! one will answer, and from the image's own pixels where none will — and
//! the figure arrives here as a [`crate::media::MediaTally`]. The gap
//! between the provider-reported context size and the sum of estimates is
//! reported as [`ContextKind::Overhead`] (tool schemas, message framing),
//! so the total always matches what the provider actually measured.

use rig::message::{AssistantContent, Message, ToolResultContent, UserContent};

use crate::config::Config;
use crate::media::{MediaSource, MediaTally};

/// One slice of the context window. Order here is the display order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextKind {
    /// The base system prompt.
    System,
    /// Project instruction files appended to the system prompt.
    Instructions,
    /// The user's own messages (prompts, `!` shell records, steering).
    User,
    /// Assistant text and reasoning.
    Assistant,
    /// Tool calls and their outputs.
    Tools,
    /// Attached media. Counted by [`crate::media`] rather than estimated
    /// from characters; [`Breakdown::media_source`] says how.
    Media,
    /// Whatever the provider counted beyond the estimates above — tool
    /// schemas, message framing. Present only when a reported context
    /// size exists to compare against.
    Overhead,
}

/// All kinds in display order.
pub const KINDS: [ContextKind; 7] = [
    ContextKind::System,
    ContextKind::Instructions,
    ContextKind::User,
    ContextKind::Assistant,
    ContextKind::Tools,
    ContextKind::Media,
    ContextKind::Overhead,
];

impl ContextKind {
    /// Stable identifier used in the encoded form and as the tail of the
    /// GUI locale keys (`ctx_<key>`).
    pub fn key(self) -> &'static str {
        match self {
            ContextKind::System => "system",
            ContextKind::Instructions => "instructions",
            ContextKind::User => "user",
            ContextKind::Assistant => "assistant",
            ContextKind::Tools => "tools",
            ContextKind::Media => "media",
            ContextKind::Overhead => "overhead",
        }
    }

    /// English display label (the TUI shows it directly).
    pub fn label(self) -> &'static str {
        match self {
            ContextKind::System => "system prompt",
            ContextKind::Instructions => "project instructions",
            ContextKind::User => "user messages",
            ContextKind::Assistant => "assistant replies",
            ContextKind::Tools => "tool calls & outputs",
            ContextKind::Media => "attachments (est.)",
            ContextKind::Overhead => "tool schemas & overhead",
        }
    }
}

/// The estimated composition of the context window at one point in time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Breakdown {
    /// Estimated tokens per kind, in [`KINDS`] order (zeros included).
    pub segments: Vec<(ContextKind, u64)>,
    /// Provider-reported context tokens of the last request (0 = none yet).
    pub reported: u64,
    /// The configured context window.
    pub window: u64,
    /// How the media figure was arrived at — measured by the provider,
    /// computed from the image, or a flat estimate. `None` when there is
    /// no media in the conversation (or the breakdown came from a version
    /// that did not record it).
    pub media_source: Option<MediaSource>,
}

impl Breakdown {
    /// Total used tokens: the provider report when it exists (the
    /// estimates then sum to it via Overhead), the estimate sum otherwise.
    pub fn used(&self) -> u64 {
        let sum: u64 = self.segments.iter().map(|(_, v)| v).sum();
        self.reported.max(sum)
    }

    pub fn free(&self) -> u64 {
        self.window.saturating_sub(self.used())
    }

    /// Serialize as `key value` lines — the text body of a transcript
    /// entry, so saved sessions can re-render the block.
    pub fn encode(&self) -> String {
        let mut out = format!("window {}\nreported {}", self.window, self.reported);
        for (kind, tokens) in &self.segments {
            out.push_str(&format!("\n{} {tokens}", kind.key()));
        }
        if let Some(source) = self.media_source {
            out.push_str(&format!("\nmedia_source {}", source.key()));
        }
        out
    }

    /// Parse [`encode`](Self::encode)'s output; `None` if the text isn't a
    /// breakdown (unknown keys are ignored for forward compatibility).
    pub fn decode(text: &str) -> Option<Self> {
        let mut window = None;
        let mut reported = 0;
        let mut media_source = None;
        let mut tokens = [0u64; KINDS.len()];
        for line in text.lines() {
            let (key, value) = line.split_once(' ')?;
            let value = value.trim();
            // The one line whose value is a word rather than a count.
            if key == "media_source" {
                media_source = MediaSource::from_key(value);
                continue;
            }
            let value: u64 = value.parse().ok()?;
            match key {
                "window" => window = Some(value),
                "reported" => reported = value,
                key => {
                    if let Some(i) = KINDS.iter().position(|k| k.key() == key) {
                        tokens[i] = value;
                    }
                }
            }
        }
        Some(Self {
            segments: KINDS.iter().copied().zip(tokens).collect(),
            reported,
            window: window?,
            media_source,
        })
    }
}

/// ≈4 chars per token.
fn est(chars: usize) -> u64 {
    (chars as u64).div_ceil(4)
}

/// Estimate the context composition for the current configuration and
/// conversation history. `media` is the media tally from
/// [`crate::media::MediaCounter::tally`] — the one part of the breakdown
/// that cannot be worked out from the text.
pub fn breakdown(cfg: &Config, history: &[Message], reported: u64, media: MediaTally) -> Breakdown {
    let (base, instructions) = crate::agent::system_prompt_parts(cfg);
    breakdown_from(
        &base,
        &instructions,
        history,
        reported,
        cfg.context_window.get(),
        media,
    )
}

/// The low-level variant of [`breakdown`], taking the system-prompt parts
/// directly instead of a [`Config`].
pub fn breakdown_from(
    system_base: &str,
    instructions: &str,
    history: &[Message],
    reported: u64,
    window: u64,
    media: MediaTally,
) -> Breakdown {
    use ContextKind::*;
    let mut tokens = [0u64; KINDS.len()];
    let mut add = |kind: ContextKind, n: u64| {
        let i = KINDS.iter().position(|k| *k == kind).expect("kind listed");
        tokens[i] += n;
    };
    add(System, est(system_base.len()));
    if !instructions.is_empty() {
        add(Instructions, est(instructions.len()));
    }
    for message in history {
        match message {
            Message::System { content } => add(System, est(content.len())),
            Message::User { content } => {
                for part in content.iter() {
                    match part {
                        UserContent::Text(t) => add(User, est(t.text.len())),
                        UserContent::ToolResult(r) => {
                            for c in r.content.iter() {
                                match c {
                                    ToolResultContent::Text(t) => add(Tools, est(t.text.len())),
                                    // Media is tallied whole, below.
                                    ToolResultContent::Image(_) => {}
                                }
                            }
                        }
                        UserContent::Image(_)
                        | UserContent::Audio(_)
                        | UserContent::Video(_)
                        | UserContent::Document(_) => {}
                    }
                }
            }
            Message::Assistant { content, .. } => {
                for part in content.iter() {
                    match part {
                        AssistantContent::Text(t) => add(Assistant, est(t.text.len())),
                        AssistantContent::ToolCall(c) => add(
                            Tools,
                            est(c.function.name.len() + c.function.arguments.to_string().len()),
                        ),
                        AssistantContent::Reasoning(r) => {
                            use rig::message::ReasoningContent;
                            for block in &r.content {
                                let len = match block {
                                    ReasoningContent::Text { text, .. } => text.len(),
                                    ReasoningContent::Summary(s) => s.len(),
                                    ReasoningContent::Encrypted(s) => s.len(),
                                    ReasoningContent::Redacted { data } => data.len(),
                                    _ => 0,
                                };
                                add(Assistant, est(len));
                            }
                        }
                        AssistantContent::Image(_) => {}
                    }
                }
            }
        }
    }
    // Media arrives already counted, per part, by whichever source could
    // answer for it.
    add(Media, media.tokens);
    let sum: u64 = tokens.iter().sum();
    if reported > sum {
        let i = KINDS.iter().position(|k| *k == Overhead).expect("listed");
        tokens[i] = reported - sum;
    }
    Breakdown {
        segments: KINDS.iter().copied().zip(tokens).collect(),
        reported,
        window,
        media_source: media.source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(b: &Breakdown, kind: ContextKind) -> u64 {
        b.segments.iter().find(|(k, _)| *k == kind).unwrap().1
    }

    #[test]
    fn estimates_by_category_and_overhead() {
        let history = vec![
            Message::user("a".repeat(400)),
            Message::Assistant {
                id: None,
                content: rig::OneOrMany::many(vec![
                    AssistantContent::text("b".repeat(200)),
                    AssistantContent::ToolCall(rig::message::ToolCall {
                        id: "1".into(),
                        call_id: None,
                        function: rig::message::ToolFunction {
                            name: "read_file".into(),
                            arguments: serde_json::json!({"path": "src/main.rs"}),
                        },
                        signature: None,
                        additional_params: None,
                    }),
                ])
                .unwrap(),
            },
            Message::User {
                content: rig::OneOrMany::one(UserContent::tool_result(
                    "1",
                    rig::OneOrMany::one(ToolResultContent::text("c".repeat(800))),
                )),
            },
        ];
        let media = MediaTally {
            tokens: 64,
            source: Some(MediaSource::Computed),
        };
        let b = breakdown_from("s".repeat(100).as_str(), "", &history, 500, 4096, media);
        // The tally lands in its own segment, whole.
        assert_eq!(get(&b, ContextKind::Media), 64);
        assert_eq!(b.media_source, Some(MediaSource::Computed));
        assert_eq!(get(&b, ContextKind::System), 25);
        assert_eq!(get(&b, ContextKind::Instructions), 0);
        assert_eq!(get(&b, ContextKind::User), 100);
        assert_eq!(get(&b, ContextKind::Assistant), 50);
        assert_eq!(get(&b, ContextKind::Tools), 200 + 8); // output + call args
        // Reported (500) exceeds the estimate sum → the rest is overhead.
        let sum: u64 = b.segments.iter().map(|(_, v)| v).sum();
        assert_eq!(sum, 500);
        assert!(get(&b, ContextKind::Overhead) > 0);
        assert_eq!(b.used(), 500);
        assert_eq!(b.free(), 4096 - 500);
    }

    #[test]
    fn encode_decode_roundtrip() {
        let b = breakdown_from(
            "system",
            "instr",
            &[Message::user("hi")],
            0,
            8192,
            MediaTally::default(),
        );
        assert_eq!(Breakdown::decode(&b.encode()), Some(b.clone()));
        // The media source survives the round trip when there is one, and
        // a breakdown written before the field existed still loads.
        let measured = Breakdown {
            media_source: Some(MediaSource::Measured),
            ..b.clone()
        };
        assert_eq!(Breakdown::decode(&measured.encode()), Some(measured));
        assert_eq!(
            Breakdown::decode("window 10\nmedia 5"),
            Some(Breakdown {
                segments: KINDS
                    .iter()
                    .copied()
                    .map(|k| (k, if k == ContextKind::Media { 5 } else { 0 }))
                    .collect(),
                reported: 0,
                window: 10,
                media_source: None,
            })
        );
        // Reported below the estimate sum leaves overhead at zero.
        assert_eq!(get(&b, ContextKind::Overhead), 0);
        assert_eq!(Breakdown::decode("not a breakdown"), None);
        assert_eq!(Breakdown::decode("reported 5"), None); // no window
    }
}
