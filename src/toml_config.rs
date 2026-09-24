//! Optional TOML file configuration for srtla_send.
//!
//! Read once at startup via `--config <path>` and merged under the command
//! line: an explicitly given flag wins over the file, the file wins over the
//! built-in default. SIGHUP reloads the IP list only, not this file.
//! All fields use `#[serde(default)]` so a partial config file is valid.
//!
//! The congestion/lifecycle/selection tuning keys below are parsed but not yet
//! wired to the sender; setting one logs a startup warning.

use std::path::Path;

use serde::Deserialize;
use tracing::{info, warn};

/// Top-level TOML configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TomlConfig {
    /// Scheduling mode: classic, enhanced.
    pub mode: String,
    /// Disable quality scoring.
    pub no_quality: bool,
    /// Disable the stalled-link deselect guard (on by default).
    pub no_stall_deselect: bool,
    /// In-flight backlog at or above which a link becomes a stall candidate.
    pub stall_min_in_flight: i32,
    /// Delivery-proof staleness window (ms) before a stall candidate is deselected.
    pub stall_ack_stale_ms: u64,
    /// Per-link liveness timeout (ms); silence past this re-registers the link.
    pub conn_timeout_ms: u64,
    /// Reconnect cadence (ms) for an established link's first
    /// `reconnect_fast_retry_attempts` retries after it times out.
    pub reconnect_fast_retry_ms: u64,
    /// Failed reconnects retried at `reconnect_fast_retry_ms` before the
    /// exponential backoff; `0` = backoff from the first miss.
    pub reconnect_fast_retry_attempts: u32,

    // --- Congestion control ---
    /// RTT velocity threshold (ms/sample) above which window recovery is halved.
    pub rtt_velocity_gate: f64,

    // --- Link lifecycle ---
    /// RTT probes required during warming phase before going Live.
    pub warming_rtt_probes: u32,
    /// Maximum time (ms) in warming phase before auto-promoting.
    pub warming_timeout_ms: u64,
    /// Quality threshold below which a Live link becomes Degraded.
    pub degraded_quality_threshold: f64,
    /// NAK burst count threshold for degradation.
    pub degraded_nak_burst_threshold: i32,
    /// Cooldown duration (ms) before re-entering Live from Degraded.
    pub cooldown_duration_ms: u64,

    // --- Selection ---
    /// Minimum time (ms) between connection switches.
    pub min_switch_interval_ms: u64,
    /// Switch hysteresis threshold (1.10 = 10% better required).
    pub switch_hysteresis: f64,
}

impl Default for TomlConfig {
    fn default() -> Self {
        Self {
            mode: "enhanced".to_string(),
            no_quality: false,
            no_stall_deselect: false,
            stall_min_in_flight: crate::config::STALL_MIN_IN_FLIGHT_PACKETS,
            stall_ack_stale_ms: crate::config::STALL_ACK_STALE_MS,
            conn_timeout_ms: crate::config::CONN_TIMEOUT_MS,
            reconnect_fast_retry_ms: crate::config::RECONNECT_FAST_RETRY_MS,
            reconnect_fast_retry_attempts: crate::config::RECONNECT_FAST_RETRY_ATTEMPTS,
            rtt_velocity_gate: 2.0,
            warming_rtt_probes: 2,
            warming_timeout_ms: 5_000,
            degraded_quality_threshold: 0.5,
            degraded_nak_burst_threshold: 5,
            cooldown_duration_ms: 5_000,
            min_switch_interval_ms: 15,
            switch_hysteresis: 1.10,
        }
    }
}

impl TomlConfig {
    /// Load config from a TOML file.
    pub fn load(path: &Path) -> Result<Self, String> {
        let content =
            std::fs::read_to_string(path).map_err(|e| format!("failed to read {path:?}: {e}"))?;
        toml::from_str(&content).map_err(|e| format!("failed to parse {path:?}: {e}"))
    }

    /// Names of the keys set to a non-default value that this build parses but
    /// does not apply (the congestion / lifecycle / selection tuning section),
    /// so the operator is told instead of silently ignored.
    pub fn unwired_keys_set(&self) -> Vec<&'static str> {
        let d = Self::default();
        let mut keys = Vec::new();
        if self.rtt_velocity_gate != d.rtt_velocity_gate {
            keys.push("rtt_velocity_gate");
        }
        if self.warming_rtt_probes != d.warming_rtt_probes {
            keys.push("warming_rtt_probes");
        }
        if self.warming_timeout_ms != d.warming_timeout_ms {
            keys.push("warming_timeout_ms");
        }
        if self.degraded_quality_threshold != d.degraded_quality_threshold {
            keys.push("degraded_quality_threshold");
        }
        if self.degraded_nak_burst_threshold != d.degraded_nak_burst_threshold {
            keys.push("degraded_nak_burst_threshold");
        }
        if self.cooldown_duration_ms != d.cooldown_duration_ms {
            keys.push("cooldown_duration_ms");
        }
        if self.min_switch_interval_ms != d.min_switch_interval_ms {
            keys.push("min_switch_interval_ms");
        }
        if self.switch_hysteresis != d.switch_hysteresis {
            keys.push("switch_hysteresis");
        }
        keys
    }

    /// Load config, logging errors and falling back to defaults.
    pub fn load_or_default(path: &Path) -> Self {
        match Self::load(path) {
            Ok(cfg) => {
                info!("loaded config from {}", path.display());
                cfg
            }
            Err(e) => {
                warn!("config load failed: {e}, using defaults");
                Self::default()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_defaults() {
        let cfg = TomlConfig::default();
        assert_eq!(cfg.mode, "enhanced");
        assert!(!cfg.no_quality);
        assert!((cfg.rtt_velocity_gate - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_unwired_keys_reported() {
        assert!(TomlConfig::default().unwired_keys_set().is_empty());
        let cfg: TomlConfig =
            toml::from_str("switch_hysteresis = 1.5\nconn_timeout_ms = 2000").unwrap();
        assert_eq!(cfg.unwired_keys_set(), vec!["switch_hysteresis"]);
    }

    #[test]
    fn test_partial_toml() {
        let toml_str = r#"
            mode = "classic"
            rtt_velocity_gate = 3.5
        "#;
        let cfg: TomlConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.mode, "classic");
        assert!((cfg.rtt_velocity_gate - 3.5).abs() < f64::EPSILON);
        // Defaults for unspecified fields
        assert!(!cfg.no_quality);
    }

    #[test]
    fn test_full_toml() {
        let toml_str = r#"
            mode = "classic"
            no_quality = true
            rtt_velocity_gate = 1.0
            warming_rtt_probes = 3
            warming_timeout_ms = 10000
            degraded_quality_threshold = 0.3
            degraded_nak_burst_threshold = 10
            cooldown_duration_ms = 8000
            min_switch_interval_ms = 30
            switch_hysteresis = 1.20
            reconnect_fast_retry_ms = 2000
            reconnect_fast_retry_attempts = 0
        "#;
        let cfg: TomlConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.mode, "classic");
        assert!(cfg.no_quality);
        assert_eq!(cfg.warming_rtt_probes, 3);
        assert_eq!(cfg.reconnect_fast_retry_ms, 2000);
        assert_eq!(cfg.reconnect_fast_retry_attempts, 0);
    }
}
