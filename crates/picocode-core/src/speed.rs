//! Rolling-window generation-speed meter backing the "tok/s" readout next
//! to the output-token counters in both front ends.
//!
//! The UIs feed it their per-delta token estimates; the rate is tokens
//! over the recent window, measured against "now" — so it decays toward
//! zero while the stream stalls instead of freezing at the last value.

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
    /// history (idle, or the stream just started).
    pub fn rate(&self) -> Option<f64> {
        self.rate_at(Instant::now())
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

    fn rate_at(&self, now: Instant) -> Option<f64> {
        let (first, _) = self.samples.front()?;
        let span = now.duration_since(*first);
        if span < MIN_SPAN || span > WINDOW {
            return None;
        }
        let total: u64 = self.samples.iter().map(|(_, n)| n).sum();
        Some(total as f64 / span.as_secs_f64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_over_the_window() {
        let mut m = SpeedMeter::default();
        let t0 = Instant::now();
        assert_eq!(m.rate_at(t0), None);
        // 20 tokens over 2 seconds → 10 tok/s.
        for i in 0..=20u64 {
            m.record_at(t0 + Duration::from_millis(i * 100), 1);
        }
        let rate = m.rate_at(t0 + Duration::from_secs(2)).unwrap();
        assert!((rate - 10.5).abs() < 0.01, "rate = {rate}");
        // Right after the first sample there is not enough history.
        let mut early = SpeedMeter::default();
        early.record_at(t0, 5);
        assert_eq!(early.rate_at(t0 + Duration::from_millis(100)), None);
        // A stalled stream decays instead of freezing …
        let decayed = m.rate_at(t0 + Duration::from_secs(4)).unwrap();
        assert!(decayed < rate);
        // … and stops reporting once the whole window is stale.
        assert_eq!(m.rate_at(t0 + Duration::from_secs(60)), None);
        m.reset();
        assert_eq!(m.rate_at(t0 + Duration::from_secs(2)), None);
    }

    #[test]
    fn old_samples_fall_out_of_the_window() {
        let mut m = SpeedMeter::default();
        let t0 = Instant::now();
        m.record_at(t0, 1000);
        // A burst long after the first sample prunes it away.
        m.record_at(t0 + Duration::from_secs(30), 10);
        m.record_at(t0 + Duration::from_secs(31), 10);
        let rate = m.rate_at(t0 + Duration::from_secs(32)).unwrap();
        // Only the recent 20 tokens over 2s count, not the old 1000.
        assert!((rate - 10.0).abs() < 0.01, "rate = {rate}");
    }
}
