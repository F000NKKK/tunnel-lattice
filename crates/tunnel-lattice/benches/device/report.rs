//! Statistics and the JSON/Markdown output of the real-device benchmark.
//! Pure: no I/O, so `tests/device_bench.rs` covers it without privilege.

use std::fmt::Write as _;

/// Identifies the JSON layout; additive fields do not change it.
pub const SCHEMA: &str = "tunnel-lattice-device-bench/1";

/// Round-trip time statistics, in nanoseconds.
#[derive(Clone, Debug, PartialEq)]
pub struct Latency {
    /// Round trips measured (lost ones excluded).
    pub samples: usize,
    /// Datagrams sent whose echo did not arrive in time.
    pub lost: u64,
    pub min_ns: u64,
    pub p50_ns: u64,
    pub p90_ns: u64,
    pub p99_ns: u64,
    pub max_ns: u64,
    pub mean_ns: u64,
}

/// Summarizes round-trip times; `None` without any sample.
pub fn latency(mut rtts_ns: Vec<u64>, lost: u64) -> Option<Latency> {
    if rtts_ns.is_empty() {
        return None;
    }
    rtts_ns.sort_unstable();
    let sum: u128 = rtts_ns.iter().map(|&rtt| u128::from(rtt)).sum();
    let samples = rtts_ns.len();
    Some(Latency {
        samples,
        lost,
        min_ns: rtts_ns[0],
        p50_ns: percentile(&rtts_ns, 50.0),
        p90_ns: percentile(&rtts_ns, 90.0),
        p99_ns: percentile(&rtts_ns, 99.0),
        max_ns: rtts_ns[samples - 1],
        mean_ns: u64::try_from(sum / samples as u128).unwrap_or(u64::MAX),
    })
}

/// The nearest-rank percentile `p` (0 < `p` <= 100) of a sorted, non-empty
/// slice: the smallest value with at least `p` percent of the samples at
/// or below it.
pub fn percentile(sorted: &[u64], p: f64) -> u64 {
    assert!(!sorted.is_empty(), "percentile of no samples");
    let rank = (p / 100.0 * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// Windowed echo throughput.
#[derive(Clone, Debug, PartialEq)]
pub struct Throughput {
    /// Length of the measured interval.
    pub secs: f64,
    /// Echoes received in the interval; each is one device receive plus
    /// one device send.
    pub round_trips: u64,
    /// Datagrams sent and echoes received over the whole phase (warm-up
    /// and drain included), for the loss figure.
    pub sent_total: u64,
    pub received_total: u64,
    /// Round trips per second.
    pub pps: f64,
    /// Megabits per second of device-level packets in each direction: the
    /// IP packet for TUN, the Ethernet frame (IP packet plus 14-byte
    /// header) for TAP.
    pub mbps: f64,
}

/// Rates for `round_trips` echoes of `wire_len`-byte device packets in
/// `secs`.
pub fn throughput(
    secs: f64,
    round_trips: u64,
    sent_total: u64,
    received_total: u64,
    wire_len: usize,
) -> Throughput {
    let pps = if secs > 0.0 {
        round_trips as f64 / secs
    } else {
        0.0
    };
    Throughput {
        secs,
        round_trips,
        sent_total,
        received_total,
        pps,
        mbps: pps * wire_len as f64 * 8.0 / 1e6,
    }
}

impl Throughput {
    /// Datagrams sent without an echo, as a percentage of those sent.
    pub fn loss_percent(&self) -> f64 {
        if self.sent_total == 0 {
            return 0.0;
        }
        let lost = self.sent_total.saturating_sub(self.received_total);
        lost as f64 * 100.0 / self.sent_total as f64
    }
}

/// How a scenario ended.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Ok,
    /// Not applicable to this host or build; the reason is reported.
    Skipped(String),
    /// Ran and failed; fails the benchmark run.
    Failed(String),
}

/// One measured (path, device kind, payload size) combination.
#[derive(Clone, Debug, PartialEq)]
pub struct Scenario {
    /// Stable identifier of the packet path, e.g. `sync` or `native-async`.
    pub path: String,
    /// What the path calls, for the table.
    pub label: String,
    /// `tun` or `tap`.
    pub kind: String,
    /// UDP payload bytes per datagram; 0 when the scenario did not run.
    pub payload: usize,
    /// Bytes per packet on the device (IP packet, plus Ethernet for TAP).
    pub wire_len: usize,
    pub latency: Option<Latency>,
    pub throughput: Option<Throughput>,
    pub outcome: Outcome,
}

/// Where and how the run happened.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Meta {
    pub created_utc: String,
    pub os: String,
    pub arch: String,
    /// `default`, `async-io` or `tokio`.
    pub build: String,
    pub cpus: usize,
    pub cpu_model: String,
    pub kernel: String,
    pub runner: String,
    pub git_sha: String,
    pub crate_version: String,
    pub rtts: usize,
    pub secs: f64,
    pub window: u64,
}

/// Escapes `s` as the contents of a JSON string.
pub fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if u32::from(c) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out
}

fn json_f64(value: f64) -> String {
    if value.is_finite() {
        format!("{value:.3}")
    } else {
        "null".to_owned()
    }
}

/// The run as one JSON document.
pub fn to_json(meta: &Meta, scenarios: &[Scenario]) -> String {
    let s = |value: &str| format!("\"{}\"", json_escape(value));
    let mut out = String::new();
    out.push_str("{\n");
    let _ = writeln!(out, "  \"schema\": {},", s(SCHEMA));
    let _ = writeln!(out, "  \"created_utc\": {},", s(&meta.created_utc));
    let _ = writeln!(
        out,
        "  \"host\": {{\"os\": {}, \"arch\": {}, \"cpus\": {}, \"cpu_model\": {}, \"kernel\": {}, \"runner\": {}}},",
        s(&meta.os),
        s(&meta.arch),
        meta.cpus,
        s(&meta.cpu_model),
        s(&meta.kernel),
        s(&meta.runner)
    );
    let _ = writeln!(
        out,
        "  \"build\": {{\"features\": {}, \"tunnel_lattice\": {}, \"git_sha\": {}}},",
        s(&meta.build),
        s(&meta.crate_version),
        s(&meta.git_sha)
    );
    let _ = writeln!(
        out,
        "  \"method\": {{\"rtts\": {}, \"secs\": {}, \"window\": {}, \"latency\": \"one datagram in flight, socket send to echo receive\", \"throughput\": \"windowed echo, round trips per second\"}},",
        meta.rtts,
        json_f64(meta.secs),
        meta.window
    );
    out.push_str("  \"scenarios\": [");
    for (index, scenario) in scenarios.iter().enumerate() {
        out.push_str(if index == 0 { "\n" } else { ",\n" });
        let (outcome, reason) = match &scenario.outcome {
            Outcome::Ok => ("ok", None),
            Outcome::Skipped(reason) => ("skipped", Some(reason)),
            Outcome::Failed(reason) => ("failed", Some(reason)),
        };
        let _ = write!(
            out,
            "    {{\"path\": {}, \"label\": {}, \"kind\": {}, \"payload\": {}, \"wire_len\": {}, \"outcome\": {}, \"reason\": {}",
            s(&scenario.path),
            s(&scenario.label),
            s(&scenario.kind),
            scenario.payload,
            scenario.wire_len,
            s(outcome),
            reason.map_or_else(|| "null".to_owned(), |reason| s(reason))
        );
        match &scenario.latency {
            Some(l) => {
                let _ = write!(
                    out,
                    ", \"latency\": {{\"samples\": {}, \"lost\": {}, \"min_ns\": {}, \"p50_ns\": {}, \"p90_ns\": {}, \"p99_ns\": {}, \"max_ns\": {}, \"mean_ns\": {}}}",
                    l.samples, l.lost, l.min_ns, l.p50_ns, l.p90_ns, l.p99_ns, l.max_ns, l.mean_ns
                );
            }
            None => out.push_str(", \"latency\": null"),
        }
        match &scenario.throughput {
            Some(t) => {
                let _ = write!(
                    out,
                    ", \"throughput\": {{\"secs\": {}, \"round_trips\": {}, \"sent_total\": {}, \"received_total\": {}, \"pps\": {}, \"mbps\": {}, \"loss_percent\": {}}}",
                    json_f64(t.secs),
                    t.round_trips,
                    t.sent_total,
                    t.received_total,
                    json_f64(t.pps),
                    json_f64(t.mbps),
                    json_f64(t.loss_percent())
                );
            }
            None => out.push_str(", \"throughput\": null"),
        }
        out.push('}');
    }
    out.push_str("\n  ]\n}\n");
    out
}

fn micros(ns: u64) -> String {
    format!("{:.1}", ns as f64 / 1000.0)
}

/// Replaces characters that would break a Markdown table cell.
fn cell(text: &str) -> String {
    text.replace('|', "\\|").replace(['\n', '\r'], " ")
}

/// The run as a Markdown section: a heading, one table row per scenario,
/// and a footer saying how and where it was measured.
pub fn to_markdown(meta: &Meta, scenarios: &[Scenario]) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "### Real-device echo benchmark: {} ({}), `{}` build\n",
        meta.os, meta.arch, meta.build
    );
    out.push_str(
        "| Path | Device | Payload | RTT p50 (µs) | RTT p99 (µs) | Echo rate (pkt/s) | Throughput (Mbit/s) | Loss |\n",
    );
    out.push_str("|---|---|---:|---:|---:|---:|---:|---:|\n");
    for scenario in scenarios {
        let label = cell(&scenario.label);
        let kind = scenario.kind.to_uppercase();
        match &scenario.outcome {
            Outcome::Skipped(reason) => {
                let _ = writeln!(
                    out,
                    "| {label} | {kind} | — | skipped: {} | | | | |",
                    cell(reason)
                );
            }
            Outcome::Failed(reason) => {
                let payload = if scenario.payload == 0 {
                    "—".to_owned()
                } else {
                    format!("{} B", scenario.payload)
                };
                let _ = writeln!(
                    out,
                    "| {label} | {kind} | {payload} | **failed**: {} | | | | |",
                    cell(reason)
                );
            }
            Outcome::Ok => {
                let (p50, p99) = scenario.latency.as_ref().map_or_else(
                    || ("—".to_owned(), "—".to_owned()),
                    |l| (micros(l.p50_ns), micros(l.p99_ns)),
                );
                let (pps, mbps, loss) = scenario.throughput.as_ref().map_or_else(
                    || ("—".to_owned(), "—".to_owned(), "—".to_owned()),
                    |t| {
                        (
                            format!("{:.0}", t.pps),
                            format!("{:.1}", t.mbps),
                            format!("{:.2} %", t.loss_percent()),
                        )
                    },
                );
                let _ = writeln!(
                    out,
                    "| {label} | {kind} | {} B | {p50} | {p99} | {pps} | {mbps} | {loss} |",
                    scenario.payload
                );
            }
        }
    }
    let _ = writeln!(
        out,
        "\nRecorded {} on {}; {} CPUs ({}); {}.",
        meta.created_utc,
        if meta.runner.is_empty() {
            "a local host"
        } else {
            &meta.runner
        },
        meta.cpus,
        if meta.cpu_model.is_empty() {
            "unknown model"
        } else {
            &meta.cpu_model
        },
        if meta.kernel.is_empty() {
            "unknown kernel"
        } else {
            &meta.kernel
        }
    );
    let _ = writeln!(
        out,
        "tunnel-lattice {}{}; {} round trips per latency figure, {:.1} s per throughput figure with up to {} datagrams in flight.",
        meta.crate_version,
        if meta.git_sha.is_empty() {
            String::new()
        } else {
            format!(" at {}", &meta.git_sha[..meta.git_sha.len().min(7)])
        },
        meta.rtts,
        meta.secs,
        meta.window
    );
    out.push_str(
        "Each datagram goes from a UDP socket through the host's IP stack into the device, is read and written back by the path under test, and returns through the stack to the socket. RTT includes both kernel traversals. Throughput counts each device packet (IP packet for TUN, Ethernet frame for TAP) once per direction. Shared CI runners are noisy: compare rows within one run, not across runs or hosts.\n",
    );
    out
}

/// Formats seconds since the Unix epoch as an RFC 3339 UTC timestamp.
pub fn utc_rfc3339(epoch_secs: u64) -> String {
    let days = epoch_secs / 86_400;
    let rem = epoch_secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 to a proleptic Gregorian (year, month, day), after
/// Howard Hinnant's `civil_from_days`.
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    (year, month, day)
}
