// The CLI is a thin shell over the `srtla_send` library: it parses arguments,
// starts the optional sidecars, picks an egress binder for the platform, and
// hands off to `sender::run_sender_with_config`. Consuming the library rather
// than re-declaring its modules keeps one compilation of the tree instead of
// two, and keeps the library's embedder-facing surface (the Android and Apple
// binders no CLI ever constructs) from reading as dead code here.
use anyhow::{Context, Result};
use clap::builder::{PossibleValuesParser, TypedValueParser};
use clap::parser::ValueSource;
use clap::{ArgMatches, CommandFactory, FromArgMatches, Parser};
use srtla_core::mode::SchedulingMode;
use srtla_send::{
    config, control_socket, metrics, net, priority_listener, sender, stats, subscriptions,
    toml_config, version,
};
use tracing_subscriber::EnvFilter;

// Use mimalloc as the global allocator for the binary (non-Windows only)
#[cfg(not(windows))]
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Parser, Debug)]
#[command(
    name = "srtla_send",
    author,
    version,
    disable_version_flag = true,
    about = "SRTLA sender CLI",
    override_usage = "srtla_send [OPTIONS] SRT_LISTEN_PORT SRTLA_HOST SRTLA_PORT BIND_IPS_FILE"
)]
struct Cli {
    /// Print the version and exit
    #[arg(short = 'v', long = "version", action = clap::ArgAction::SetTrue)]
    print_version: bool,

    /// Local UDP port to listen for SRT packets (from srt-live-transmit or SRT
    /// app)
    #[arg(required_unless_present = "print_version")]
    local_srt_port: Option<u16>,
    /// Receiver host (srtla_rec or SRT listener)
    #[arg(required_unless_present = "print_version")]
    receiver_host: Option<String>,
    /// Receiver UDP port to send SRTLA packets to
    #[arg(required_unless_present = "print_version")]
    receiver_port: Option<u16>,
    /// Path to file containing newline-separated local source IPs to use for
    /// uplinks
    #[arg(required_unless_present = "print_version")]
    ips_file: Option<String>,

    /// Unix domain socket path for remote toggle control (e.g.,
    /// /tmp/srtla.sock)
    #[arg(long = "control-socket")]
    control_socket: Option<String>,

    /// Path to a TOML config file, read once at startup. Its values override
    /// the built-in defaults; flags given on the command line override it.
    /// (SIGHUP reloads the IP list only; use the control socket to change
    /// settings at runtime.)
    #[arg(long = "config")]
    config_file: Option<String>,

    /// Scheduling mode: classic, enhanced (default)
    //
    // `SchedulingMode` is a pure core type and deliberately knows nothing about
    // clap. The CLI coupling lives here in the shell: parse the allowed strings
    // (which drive --help text and completion) and map through the core type's
    // own `FromStr`.
    #[arg(
        long = "mode",
        default_value = "enhanced",
        value_parser = PossibleValuesParser::new(["classic", "enhanced"])
            .map(|s| s.parse::<SchedulingMode>().expect("possible values are valid modes")),
    )]
    mode: SchedulingMode,
    /// Disable quality scoring (enhanced only)
    #[arg(long = "no-quality")]
    no_quality: bool,

    /// Disable the stalled-link deselect guard (on by default). The guard skips
    /// a link whose in-flight backlog is high while its last delivery proof has
    /// gone stale, provided a healthier link can carry the traffic. While
    /// gated the link carries keepalives plus a sparse duplicate-packet probe
    /// trickle; it rejoins after delivery proof has been sustained for the
    /// rejoin dwell (quick to drop, conservative to rejoin).
    #[arg(long = "no-stall-deselect")]
    no_stall_deselect: bool,
    /// In-flight packet backlog at or above which a link becomes a stall
    /// candidate for `--no-stall-deselect`.
    #[arg(long = "stall-min-in-flight", default_value_t = config::STALL_MIN_IN_FLIGHT_PACKETS)]
    stall_min_in_flight: i32,
    /// Ceiling (ms) on the delivery-proof staleness window after which a
    /// stall-candidate link is deselected. The effective window is
    /// RTT-adaptive (4x smoothed RTT, floored at 1000 ms) and this value caps
    /// it; links without an RTT baseline use the ceiling directly.
    #[arg(long = "stall-ack-stale-ms", default_value_t = config::STALL_ACK_STALE_MS)]
    stall_ack_stale_ms: u64,
    /// Per-link liveness timeout (ms): silence past this tears the link down
    /// and re-registers it. Also settable at runtime over JSON-RPC
    /// (`set_conn_timeout`) so a latency-aware client can scale it to
    /// `max(default, 2 x latency budget)` — an outage the receiver buffer
    /// can absorb should resume warm, not re-handshake. Clamped to
    /// 1000..=60000.
    #[arg(long = "conn-timeout-ms", default_value_t = config::CONN_TIMEOUT_MS)]
    conn_timeout_ms: u64,
    /// Reconnect cadence (ms) for an uplink that was established and has
    /// timed out, for its first `--reconnect-fast-retry-attempts` retries
    /// (the C reference retries on every 1 s housekeeping pass). A short
    /// modem/tether blip is then re-registered inside the SRT peer-idle
    /// timeout. Also settable at runtime (`set_reconnect_fast_retry`).
    /// Clamped to 1000..=5000.
    #[arg(long = "reconnect-fast-retry-ms", default_value_t = config::RECONNECT_FAST_RETRY_MS)]
    reconnect_fast_retry_ms: u64,
    /// Failed reconnect attempts retried at `--reconnect-fast-retry-ms` before
    /// the exponential backoff (5 s doubling, 120 s cap) takes over. `0`
    /// restores the previous behaviour: backoff from the first miss.
    /// Clamped to 0..=10.
    #[arg(
        long = "reconnect-fast-retry-attempts",
        default_value_t = config::RECONNECT_FAST_RETRY_ATTEMPTS
    )]
    reconnect_fast_retry_attempts: u32,

    /// Disable whole-bond re-home (on by default). When every uplink has been
    /// down for longer than the all-links-failed window AND the receiver
    /// hostname no longer resolves to the address the bond is pinned to, the
    /// sender moves every uplink together to the newly-resolved address and
    /// re-registers from scratch over the ordinary REG1/REG2/REG3 flow. It
    /// never touches a bond with a live uplink, never follows a merely
    /// reordered DNS answer, and attempts at most one migration per minute.
    /// Pass this to keep the pre-existing behaviour of staying on the cached
    /// address until the process is restarted.
    #[arg(long = "no-rehome")]
    no_rehome: bool,

    /// UDP bind address for the keyframe priority sidecar. The encoder
    /// front-end sends 5-byte datagrams here to open a critical routing
    /// window. Unauthenticated same-device IPC: bind loopback. Omit to
    /// disable the sidecar (the keyframe-priority override is then inactive).
    /// Example: `127.0.0.1:7000`.
    #[arg(long = "priority-bind")]
    priority_bind: Option<std::net::SocketAddr>,

    /// TCP bind address for the Prometheus `/metrics` scrape endpoint.
    /// Unauthenticated: bind loopback. Omit to disable.
    /// Example: `127.0.0.1:9099`.
    #[arg(long = "metrics-bind")]
    metrics_bind: Option<std::net::SocketAddr>,
}

/// Warn when a sidecar is bound to a non-loopback address. These endpoints
/// are unauthenticated same-device IPC (encoder front-end and local scrapers),
/// so a routable bind exposes an open control / scrape surface. We warn rather
/// than refuse so an operator can still bind elsewhere on a trusted network if
/// they explicitly choose to.
fn warn_if_not_loopback(what: &str, addr: std::net::SocketAddr) {
    if !addr.ip().is_loopback() {
        tracing::warn!(
            %addr,
            "{what} bound to a non-loopback address; it is unauthenticated and \
             should normally bind 127.0.0.1 / ::1"
        );
    }
}

impl Cli {
    /// Merge a TOML config into the parsed arguments. Precedence is
    /// command-line flag > TOML > built-in default: a key is taken from the
    /// TOML unless its flag was given explicitly on the command line
    /// (`ArgMatches::value_source`). `TomlConfig` fills absent keys with the
    /// same defaults as the flags, so a partial file changes only what it sets.
    fn apply_toml(&mut self, toml: &toml_config::TomlConfig, matches: &ArgMatches) {
        let from_toml = |id: &str| matches.value_source(id) != Some(ValueSource::CommandLine);
        if from_toml("mode") {
            match toml.mode.parse::<SchedulingMode>() {
                Ok(mode) => self.mode = mode,
                Err(_) => tracing::warn!("config: ignoring unknown mode {:?}", toml.mode),
            }
        }
        if from_toml("no_quality") {
            self.no_quality = toml.no_quality;
        }
        if from_toml("no_stall_deselect") {
            self.no_stall_deselect = toml.no_stall_deselect;
        }
        if from_toml("stall_min_in_flight") {
            self.stall_min_in_flight = toml.stall_min_in_flight;
        }
        if from_toml("stall_ack_stale_ms") {
            self.stall_ack_stale_ms = toml.stall_ack_stale_ms;
        }
        if from_toml("conn_timeout_ms") {
            self.conn_timeout_ms = toml.conn_timeout_ms;
        }
        if from_toml("reconnect_fast_retry_ms") {
            self.reconnect_fast_retry_ms = toml.reconnect_fast_retry_ms;
        }
        if from_toml("reconnect_fast_retry_attempts") {
            self.reconnect_fast_retry_attempts = toml.reconnect_fast_retry_attempts;
        }
        let unused = toml.unwired_keys_set();
        if !unused.is_empty() {
            tracing::warn!(
                "config: these keys are parsed but not used by this build: {}",
                unused.join(", ")
            );
        }
    }

    /// Build the runtime configuration from the (merged) arguments.
    fn dynamic_config(&self) -> config::DynamicConfig {
        config::DynamicConfig::from_cli(
            self.mode,
            self.no_quality,
            self.no_stall_deselect,
            self.stall_min_in_flight,
            self.stall_ack_stale_ms,
            self.conn_timeout_ms,
            self.reconnect_fast_retry_ms,
            self.reconnect_fast_retry_attempts,
            self.no_rehome,
        )
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .init();

    let matches = Cli::command().get_matches();
    let mut args = Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
    if args.print_version {
        println!("{}", version::version_line());
        return Ok(());
    }

    // Load TOML config (if specified); explicit CLI flags override it.
    if let Some(path) = args.config_file.clone() {
        let toml_cfg = toml_config::TomlConfig::load_or_default(std::path::Path::new(&path));
        tracing::debug!("TOML config loaded: {:?}", toml_cfg);
        args.apply_toml(&toml_cfg, &matches);
    }

    let local_srt_port = args.local_srt_port.expect("required");
    let receiver_host = args.receiver_host.as_deref().expect("required");
    let receiver_port = args.receiver_port.expect("required");
    let ips_file = args.ips_file.as_deref().expect("required");

    let config = args.dynamic_config();
    {
        let snap = config.snapshot();
        tracing::info!(
            "link liveness: conn_timeout_ms={} reconnect_fast_retry_ms={} \
             reconnect_fast_retry_attempts={}",
            snap.conn_timeout_ms,
            snap.reconnect_fast_retry_ms,
            snap.reconnect_fast_retry_attempts
        );
    }

    // Create shared stats for telemetry export
    let shared_stats = stats::SharedStats::new();

    let subscription_hub = subscriptions::SubscriptionHub::new();

    let critical_window = srtla_core::priority::CriticalWindow::new();
    if let Some(bind) = args.priority_bind {
        warn_if_not_loopback("priority sidecar (--priority-bind)", bind);
        priority_listener::spawn_listener(
            bind,
            critical_window.clone(),
            Some(subscription_hub.clone()),
        );
    }

    if let Some(bind) = args.metrics_bind {
        warn_if_not_loopback("metrics endpoint (--metrics-bind)", bind);
        metrics::spawn_server(
            bind,
            shared_stats.clone(),
            config.clone(),
            critical_window.clone(),
        );
    }

    // Stdin reader stays blocking; Unix socket goes async to support
    // subscription pushes.
    config::spawn_stdin_listener(
        config.clone(),
        shared_stats.clone(),
        critical_window.clone(),
    );
    if let Some(sock_path) = args.control_socket {
        control_socket::spawn(
            sock_path,
            config.clone(),
            shared_stats.clone(),
            critical_window.clone(),
            subscription_hub.clone(),
        );
    }

    // The CLI binds each uplink by its source IP, which on a multi-homed host
    // selects the egress via source-based routing. Darwin has no source-based
    // routing, so there the same IP list is resolved to interfaces and pinned
    // with IP_BOUND_IF instead; binding the source address alone would leave
    // every uplink on the default route.
    #[cfg(not(target_vendor = "apple"))]
    let binder: std::sync::Arc<dyn net::UplinkBinder> = std::sync::Arc::new(net::SourceIpBinder);
    #[cfg(target_vendor = "apple")]
    let binder: std::sync::Arc<dyn net::UplinkBinder> =
        std::sync::Arc::new(net::AppleInterfaceBinder::new());

    sender::run_sender_with_config(
        local_srt_port,
        receiver_host,
        receiver_port,
        ips_file,
        config,
        shared_stats,
        critical_window,
        subscription_hub,
        binder,
    )
    .await
    .context("srtla_send failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    const POSITIONAL: [&str; 4] = ["6000", "127.0.0.1", "5000", "/tmp/ips"];

    /// Parse `flags` like the binary does, merge `toml`, return the snapshot.
    fn effective(flags: &[&str], toml: &str) -> config::ConfigSnapshot {
        let argv = std::iter::once("srtla_send")
            .chain(flags.iter().copied())
            .chain(POSITIONAL);
        let matches = Cli::command().try_get_matches_from(argv).unwrap();
        let mut args = Cli::from_arg_matches(&matches).unwrap();
        args.apply_toml(&toml::from_str(toml).unwrap(), &matches);
        args.dynamic_config().snapshot()
    }

    #[test]
    fn toml_values_apply_when_no_flag_is_given() {
        let snap = effective(
            &[],
            "reconnect_fast_retry_attempts = 0\nconn_timeout_ms = 2000\nmode = \"classic\"\n",
        );
        assert_eq!(snap.reconnect_fast_retry_attempts, 0);
        assert_eq!(snap.conn_timeout_ms, 2000);
        assert_eq!(snap.mode, SchedulingMode::Classic);
        // Keys the file does not set keep their defaults.
        assert_eq!(
            snap.reconnect_fast_retry_ms,
            config::RECONNECT_FAST_RETRY_MS
        );
    }

    #[test]
    fn explicit_flags_override_toml() {
        let snap = effective(
            &[
                "--conn-timeout-ms",
                "3000",
                "--reconnect-fast-retry-attempts",
                "2",
            ],
            "reconnect_fast_retry_attempts = 0\nconn_timeout_ms = 2000\n",
        );
        assert_eq!(snap.conn_timeout_ms, 3000);
        assert_eq!(snap.reconnect_fast_retry_attempts, 2);
    }

    #[test]
    fn a_flag_equal_to_the_default_still_overrides_toml() {
        // Precedence is by source, not by value: `--conn-timeout-ms 5000`
        // (the default) must win over a TOML value.
        let snap = effective(&["--conn-timeout-ms", "5000"], "conn_timeout_ms = 2000\n");
        assert_eq!(snap.conn_timeout_ms, 5000);
    }

    #[test]
    fn toml_bool_flags_apply() {
        let snap = effective(&[], "no_quality = true\nno_stall_deselect = true\n");
        assert!(!snap.quality_enabled);
        assert!(!snap.stall_deselect);
    }
}
