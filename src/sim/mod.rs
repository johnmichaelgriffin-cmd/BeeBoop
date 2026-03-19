//! Simulation / replay module.
//!
//! Reads recorded JSONL event files and replays them through the
//! strategy pipeline for backtesting and validation.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use tracing::{info, warn};

use crate::types::RecordedEvent;

/// Load recorded events from a JSONL file.
pub fn load_events(path: &Path) -> Result<Vec<RecordedEvent>, Box<dyn std::error::Error>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut events = Vec::new();

    for (i, line) in reader.lines().enumerate() {
        match line {
            Ok(text) => {
                match serde_json::from_str::<RecordedEvent>(&text) {
                    Ok(event) => events.push(event),
                    Err(e) => {
                        if i < 5 {
                            warn!("sim: failed to parse line {}: {}", i, e);
                        }
                    }
                }
            }
            Err(e) => {
                warn!("sim: read error at line {}: {}", i, e);
            }
        }
    }

    info!("sim: loaded {} events from {}", events.len(), path.display());
    Ok(events)
}

/// Replay events through a callback, respecting original timing.
pub async fn replay<F>(
    events: &[RecordedEvent],
    mut handler: F,
    speedup: f64,
) where
    F: FnMut(&RecordedEvent),
{
    if events.is_empty() {
        return;
    }

    let start_ts = events[0].local_recv_us;
    let wall_start = chrono::Utc::now().timestamp_micros();

    for event in events {
        let event_offset_us = event.local_recv_us - start_ts;
        let target_wall_offset = (event_offset_us as f64 / speedup) as i64;
        let target_wall_time = wall_start + target_wall_offset;

        let now = chrono::Utc::now().timestamp_micros();
        if target_wall_time > now {
            let sleep_us = (target_wall_time - now) as u64;
            tokio::time::sleep(std::time::Duration::from_micros(sleep_us)).await;
        }

        handler(event);
    }
}
