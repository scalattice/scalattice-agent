use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Tracks recent invoke demand per runtime model for warm-list prioritization.
#[derive(Debug, Default)]
pub struct DemandTracker {
    hits: HashMap<String, Vec<Instant>>,
}

impl DemandTracker {
    pub fn record_hit(&mut self, runtime_model: &str) {
        let entry = self.hits.entry(runtime_model.to_string()).or_default();
        entry.push(Instant::now());
        // Cap history.
        if entry.len() > 64 {
            let drain = entry.len() - 64;
            entry.drain(0..drain);
        }
    }

    pub fn score(&self, runtime_model: &str, window: Duration) -> u32 {
        let Some(hits) = self.hits.get(runtime_model) else {
            return 0;
        };
        let cutoff = Instant::now().checked_sub(window).unwrap_or_else(Instant::now);
        hits.iter().filter(|t| **t >= cutoff).count() as u32
    }

    /// Sort runtime models by recent demand (highest first), stable for ties.
    pub fn order_by_demand(&self, models: &[String], window: Duration) -> Vec<String> {
        let mut scored: Vec<(u32, String)> = models
            .iter()
            .map(|m| (self.score(m, window), m.clone()))
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        scored.into_iter().map(|(_, m)| m).collect()
    }
}
