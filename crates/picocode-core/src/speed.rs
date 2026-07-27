//! Rolling-window generation-speed meter backing the "tok/s" readout next
//! to the output-token counters in both front ends.
//!
//! The UIs feed it their per-delta token estimates; the rate is tokens
//! over the recent window, measured between the first and last sample —
//! so while the stream stalls (a tool call between steps, a slow
//! provider) the readout freezes at its last value instead of blinking
//! out and back. It disappears only on [`SpeedMeter::reset`] (turn end).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How much history the rate is computed over.
const WINDOW: Duration = Duration::from_secs(10);
/// No rate is reported until this much of the window has data (a single
/// early delta would otherwise read as an absurd spike).
const MIN_SPAN: Duration = Duration::from_millis(500);

#[derive(Default)]
pub struct SpeedMeter {
    /// (arrival time, estimated tokens) per streamed delta.
    samples: VecDeque<(Instant, u64)>,
}

impl SpeedMeter {
    /// Record tokens arriving now.
    pub fn record(&mut self, tokens: u64) {
        self.record_at(Instant::now(), tokens);
    }

    /// Tokens per second over the recent window; `None` without enough
    /// history (idle, or the stream just started). Stable between calls:
    /// the value only moves when new samples arrive.
    pub fn rate(&self) -> Option<f64> {
        let (first, _) = self.samples.front()?;
        let (last, _) = self.samples.back()?;
        let span = last.duration_since(*first);
        if span < MIN_SPAN {
            return None;
        }
        // The first sample's tokens arrived *before* the measured span
        // starts (its instant marks the end of that delta), so they are
        // excluded — otherwise short spans overestimate.
        let total: u64 = self.samples.iter().skip(1).map(|(_, n)| n).sum();
        Some(total as f64 / span.as_secs_f64())
    }

    /// Forget everything (turn ended or was cancelled).
    pub fn reset(&mut self) {
        self.samples.clear();
    }

    fn record_at(&mut self, now: Instant, tokens: u64) {
        self.samples.push_back((now, tokens));
        while let Some((t, _)) = self.samples.front() {
            if now.duration_since(*t) > WINDOW {
                self.samples.pop_front();
            } else {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_over_the_window() {
        let mut m = SpeedMeter::default();
        let t0 = Instant::now();
        assert_eq!(m.rate(), None);
        // 20 tokens over 2 seconds → 10 tok/s (the first sample only
        // anchors the span).
        for i in 0..=20u64 {
            m.record_at(t0 + Duration::from_millis(i * 100), 1);
        }
        let rate = m.rate().unwrap();
        assert!((rate - 10.0).abs() < 0.01, "rate = {rate}");
        // A stalled stream holds the last reading instead of decaying —
        // the readout must not blink out between tool calls.
        assert_eq!(m.rate(), Some(rate));
        m.reset();
        assert_eq!(m.rate(), None);
    }

    #[test]
    fn a_single_early_delta_reports_nothing() {
        let mut m = SpeedMeter::default();
        let t0 = Instant::now();
        m.record_at(t0, 5);
        assert_eq!(m.rate(), None);
        m.record_at(t0 + Duration::from_millis(100), 5);
        // Still under the minimum span.
        assert_eq!(m.rate(), None);
    }

    #[test]
    fn old_samples_fall_out_of_the_window() {
        let mut m = SpeedMeter::default();
        let t0 = Instant::now();
        m.record_at(t0, 1000);
        // A burst long after the first sample prunes it away.
        m.record_at(t0 + Duration::from_secs(30), 10);
        m.record_at(t0 + Duration::from_secs(31), 10);
        m.record_at(t0 + Duration::from_secs(32), 10);
        let rate = m.rate().unwrap();
        // Only the recent tokens over the 2s span count, not the old 1000.
        assert!((rate - 10.0).abs() < 0.01, "rate = {rate}");
    }
}
