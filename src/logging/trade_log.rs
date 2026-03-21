//! Trade logger — writes JSONL to disk + prints to console.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use tokio::sync::mpsc;
use tracing::info;

use crate::config_v2::Config;
use crate::types_v2::LogEvent;

pub async fn run_trade_log_task(
    config: Config,
    mut log_rx: mpsc::Receiver<LogEvent>,
) {
    let _ = fs::create_dir_all(&config.logs_dir);
    let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
    let path = format!("{}/trades_{}.jsonl", config.logs_dir, timestamp);

    let file = File::create(&path).expect("failed to create log file");
    let mut writer = BufWriter::new(file);

    info!("trade_log: writing to {}", path);

    while let Some(event) = log_rx.recv().await {
        if let Ok(json) = serde_json::to_string(&event) {
            let _ = writeln!(writer, "{}", json);
            let _ = writer.flush();
        }
    }
}
