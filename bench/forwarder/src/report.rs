//! Results processing: raw run files to `results.json`, and
//! `results.json` to a Markdown table.
//!
//! A run directory written by `scripts/bench-forward.sh run` holds:
//!
//! ```text
//! env.json        host, method and topology (written by the `kv` command)
//! meta.json       git state, toolchain and resolved crate versions of the
//!                 measured binaries (written by `build`)
//! variants.tsv    the variant registry the run used
//! runs/<variant>/<rep>/{iperf3.json,samples.tsv,run.json,forwarder.log}
//! ```
//!
//! [`collect`] turns it into one `results.json` value (schema
//! [`SCHEMA`]); [`markdown`] renders the table from that value alone, so
//! every published number can be traced back to the recorded files.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

use serde_json::{Map, Value, json};

/// The `schema` field of `results.json`. Additive fields do not change it.
pub const SCHEMA: &str = "tunnel-lattice-forwarder-bench/1";

/// Column count of `variants.tsv`.
const VARIANT_COLUMNS: usize = 12;

/// The build sets a variant can belong to.
const BUILD_SETS: [&str; 3] = ["sync", "tokio", "async-io"];

/// One row of the variant registry (`variants.tsv`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variant {
    /// Unique id; also the run directory name.
    pub id: String,
    /// `sync`, `tokio` or `async-io`.
    pub build_set: String,
    /// Binary name in that build set.
    pub binary: String,
    /// Extra forwarder arguments (empty for none).
    pub args: String,
    /// Id of the raw tun-rs variant this one is compared with, if any.
    pub baseline: Option<String>,
    /// Whether the variant uses batch/offload I/O.
    pub offload: bool,
    /// `tun-rs` or `tunnel-lattice`.
    pub family: String,
    /// `sync` or `async`.
    pub mode: String,
    /// `threads`, `tokio` or `async-io`.
    pub runtime: String,
    /// The API layer measured: `tun-rs` or `facade`.
    pub layer: String,
    /// Receive buffer strategy.
    pub buffers: String,
    /// Human-readable configuration name for the table.
    pub label: String,
}

impl Variant {
    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "binary": self.binary,
            "args": self.args,
            "build_set": self.build_set,
            "family": self.family,
            "mode": self.mode,
            "runtime": self.runtime,
            "layer": self.layer,
            "buffers": self.buffers,
            "offload": self.offload,
            "baseline": self.baseline,
            "label": self.label,
        })
    }
}

/// Parses `variants.tsv`: tab-separated, `#` starts a comment line, `-`
/// stands for an empty field.
///
/// # Errors
///
/// A row with the wrong column count, a duplicate id, an unknown build
/// set, an `offload` other than `0`/`1`, or a baseline that is not a
/// listed id or that has a baseline itself.
pub fn parse_variants(text: &str) -> Result<Vec<Variant>, String> {
    let mut variants: Vec<Variant> = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line_no = index + 1;
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() != VARIANT_COLUMNS {
            return Err(format!(
                "variants.tsv:{line_no}: {} columns, expected {VARIANT_COLUMNS}",
                fields.len()
            ));
        }
        let field = |i: usize| match fields[i] {
            "-" => String::new(),
            value => value.to_owned(),
        };
        let id = field(0);
        if id.is_empty() || variants.iter().any(|v| v.id == id) {
            return Err(format!(
                "variants.tsv:{line_no}: empty or duplicate id {id:?}"
            ));
        }
        let build_set = field(1);
        if !BUILD_SETS.contains(&build_set.as_str()) {
            return Err(format!(
                "variants.tsv:{line_no}: unknown build set {build_set:?}"
            ));
        }
        let offload = match fields[5] {
            "0" => false,
            "1" => true,
            other => {
                return Err(format!(
                    "variants.tsv:{line_no}: offload {other:?} is not 0/1"
                ));
            }
        };
        let baseline = Some(field(4)).filter(|b| !b.is_empty());
        variants.push(Variant {
            id,
            build_set,
            binary: field(2),
            args: field(3),
            baseline,
            offload,
            family: field(6),
            mode: field(7),
            runtime: field(8),
            layer: field(9),
            buffers: field(10),
            label: field(11),
        });
    }
    for variant in &variants {
        if let Some(baseline) = &variant.baseline {
            match variants.iter().find(|v| &v.id == baseline) {
                None => return Err(format!("{}: unknown baseline {baseline}", variant.id)),
                Some(b) if b.baseline.is_some() => {
                    return Err(format!(
                        "{}: baseline {baseline} has a baseline itself",
                        variant.id
                    ));
                }
                Some(_) => {}
            }
        }
    }
    Ok(variants)
}

/// The iperf3 numbers of one run.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Iperf3 {
    /// `end.sum_received.bits_per_second`: the receiver's rate, the
    /// headline throughput.
    pub recv_bps: f64,
    /// `end.sum_sent.bits_per_second`.
    pub sent_bps: f64,
    /// `end.sum_sent.retransmits`.
    pub retransmits: u64,
    /// `end.cpu_utilization_percent.host_total` (the iperf3 client).
    pub host_cpu: f64,
    /// `end.cpu_utilization_percent.remote_total` (the iperf3 server).
    pub remote_cpu: f64,
}

/// Parses `iperf3 -J` client output.
///
/// # Errors
///
/// The text of a top-level `error` field, or a description of the first
/// missing field.
pub fn parse_iperf3(value: &Value) -> Result<Iperf3, String> {
    if let Some(error) = value.get("error") {
        return Err(format!(
            "iperf3: {}",
            error
                .as_str()
                .map_or_else(|| error.to_string(), str::to_owned)
        ));
    }
    let number = |path: &str| {
        value
            .pointer(path)
            .and_then(Value::as_f64)
            .ok_or_else(|| format!("iperf3 output has no numeric {path}"))
    };
    Ok(Iperf3 {
        recv_bps: number("/end/sum_received/bits_per_second")?,
        sent_bps: number("/end/sum_sent/bits_per_second")?,
        retransmits: value
            .pointer("/end/sum_sent/retransmits")
            .and_then(Value::as_u64)
            .ok_or("iperf3 output has no /end/sum_sent/retransmits")?,
        host_cpu: number("/end/cpu_utilization_percent/host_total")?,
        remote_cpu: number("/end/cpu_utilization_percent/remote_total")?,
    })
}

/// Forwarder CPU and memory over one run, from `samples.tsv`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Forwarder {
    /// Mean CPU over the 1 s intervals, in percent of one core (a process
    /// total over all threads, so it can exceed 100).
    pub cpu_avg: f64,
    /// Largest single-interval CPU.
    pub cpu_max: f64,
    /// Mean of `ps -o %cpu` (a lifetime average; kept only for parity with
    /// tun-benchmark2's numbers).
    pub ps_cpu_avg: f64,
    /// Largest `ps -o %cpu` sample.
    pub ps_cpu_max: f64,
    /// Mean resident set size, KiB.
    pub rss_avg_kb: f64,
    /// Largest sampled resident set size, KiB.
    pub rss_max_kb: u64,
    /// `VmHWM` read when sampling stopped, KiB, if recorded.
    pub hwm_kb: Option<u64>,
    /// Number of samples.
    pub samples: usize,
}

/// Parses `samples.tsv`.
///
/// Each data line is `t_ns utime_ticks stime_ticks vmrss_kb ps_pcpu`
/// (tab-separated); a `#hwm_kb <kb>` line records `VmHWM`. Malformed lines
/// (a sample cut short when the process exited) are skipped.
///
/// # Errors
///
/// Fewer than two samples (no interval to compute CPU from), a zero
/// `clk_tck`, or timestamps that do not increase.
pub fn parse_samples(text: &str, clk_tck: u64) -> Result<Forwarder, String> {
    if clk_tck == 0 {
        return Err("clk_tck is zero".into());
    }
    let mut hwm_kb = None;
    let mut rows: Vec<(u64, u64, u64, f64)> = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.first() == Some(&"#hwm_kb") {
            hwm_kb = fields.get(1).and_then(|v| v.parse().ok());
            continue;
        }
        if fields.len() != 5 {
            continue;
        }
        let parsed = (
            fields[0].parse::<u64>(),
            fields[1].parse::<u64>(),
            fields[2].parse::<u64>(),
            fields[3].parse::<u64>(),
            fields[4].trim().parse::<f64>(),
        );
        if let (Ok(t), Ok(utime), Ok(stime), Ok(rss), Ok(ps)) = parsed {
            rows.push((t, utime + stime, rss, ps));
        }
    }
    if rows.len() < 2 {
        return Err(format!("{} usable sample(s); need at least 2", rows.len()));
    }
    let mut cpu = Vec::with_capacity(rows.len() - 1);
    for pair in rows.windows(2) {
        let (t0, ticks0, ..) = pair[0];
        let (t1, ticks1, ..) = pair[1];
        if t1 <= t0 {
            return Err(format!("sample timestamps do not increase ({t0} -> {t1})"));
        }
        let cpu_s = ticks1.saturating_sub(ticks0) as f64 / clk_tck as f64;
        let wall_s = (t1 - t0) as f64 / 1e9;
        cpu.push(cpu_s / wall_s * 100.0);
    }
    let ps: Vec<f64> = rows.iter().map(|r| r.3).collect();
    Ok(Forwarder {
        cpu_avg: mean(&cpu),
        cpu_max: cpu.iter().copied().fold(0.0, f64::max),
        ps_cpu_avg: mean(&ps),
        ps_cpu_max: ps.iter().copied().fold(0.0, f64::max),
        rss_avg_kb: mean(&rows.iter().map(|r| r.2 as f64).collect::<Vec<_>>()),
        rss_max_kb: rows.iter().map(|r| r.2).max().unwrap_or(0),
        hwm_kb,
        samples: rows.len(),
    })
}

fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<f64>() / values.len() as f64
    }
}

/// The median of `values`; the mean of the two middle values for an even
/// count. `None` when empty.
pub fn median(values: &[f64]) -> Option<f64> {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let n = sorted.len();
    match n {
        0 => None,
        _ if n % 2 == 1 => Some(sorted[n / 2]),
        _ => Some((sorted[n / 2 - 1] + sorted[n / 2]) / 2.0),
    }
}

fn min_max(values: &[f64]) -> Option<(f64, f64)> {
    let first = *values.first()?;
    Some(
        values
            .iter()
            .fold((first, first), |(lo, hi), &v| (lo.min(v), hi.max(v))),
    )
}

/// One recorded run, as stored in `results.json`.
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    /// Variant id.
    pub variant: String,
    /// Repetition, from 1.
    pub rep: u64,
    /// Position in the run sequence, as the script recorded it.
    pub order: Option<u64>,
    /// Whether the forwarder, iperf3 and sampling all succeeded.
    pub ok: bool,
    /// Why the run failed.
    pub error: Option<String>,
    /// iperf3 numbers, if parsed.
    pub iperf3: Option<Iperf3>,
    /// Forwarder numbers, if sampled.
    pub forwarder: Option<Forwarder>,
}

impl Run {
    fn to_json(&self) -> Value {
        json!({
            "variant": self.variant,
            "rep": self.rep,
            "order": self.order,
            "ok": self.ok,
            "error": self.error,
            "iperf3": self.iperf3.map(|i| json!({
                "recv_bps": i.recv_bps,
                "sent_bps": i.sent_bps,
                "retransmits": i.retransmits,
                "host_cpu": i.host_cpu,
                "remote_cpu": i.remote_cpu,
            })),
            "forwarder": self.forwarder.map(|f| json!({
                "cpu_avg": f.cpu_avg,
                "cpu_max": f.cpu_max,
                "ps_cpu_avg": f.ps_cpu_avg,
                "ps_cpu_max": f.ps_cpu_max,
                "rss_avg_kb": f.rss_avg_kb,
                "rss_max_kb": f.rss_max_kb,
                "hwm_kb": f.hwm_kb,
                "samples": f.samples,
            })),
        })
    }
}

/// Reads one run directory (`runs/<variant>/<rep>`).
///
/// A run is `ok` only if the script recorded it as ok, the iperf3 output
/// parses without an `error`, and the samples give at least one CPU
/// interval. A missing directory is a failed run, not an error.
pub fn read_run(dir: &Path, variant: &str, rep: u64, clk_tck: u64) -> Run {
    let mut run = Run {
        variant: variant.to_owned(),
        rep,
        order: None,
        ok: false,
        error: None,
        iperf3: None,
        forwarder: None,
    };
    let mut errors = Vec::new();
    match read_json(&dir.join("run.json")) {
        Ok(status) => {
            run.order = status.get("order").and_then(Value::as_u64);
            if status.get("ok").and_then(Value::as_bool) != Some(true) {
                errors.push(
                    status
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("run marked failed")
                        .to_owned(),
                );
            }
        }
        Err(error) => errors.push(error),
    }
    match read_json(&dir.join("iperf3.json")).and_then(|v| parse_iperf3(&v)) {
        Ok(iperf3) => run.iperf3 = Some(iperf3),
        Err(error) => errors.push(error),
    }
    match std::fs::read_to_string(dir.join("samples.tsv")) {
        Ok(text) => match parse_samples(&text, clk_tck) {
            Ok(forwarder) => run.forwarder = Some(forwarder),
            Err(error) => errors.push(format!("samples.tsv: {error}")),
        },
        Err(error) => errors.push(format!("samples.tsv: {error}")),
    }
    errors.dedup();
    run.ok = errors.is_empty();
    run.error = (!errors.is_empty()).then(|| errors.join("; "));
    run
}

fn read_json(path: &Path) -> Result<Value, String> {
    let text =
        std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    serde_json::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))
}

/// Per-variant aggregate over the repetitions.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    /// Variant id.
    pub variant: String,
    /// Successful repetitions.
    pub n_ok: usize,
    /// Repetitions attempted.
    pub reps: usize,
    /// Median, minimum and maximum receiver throughput, Gbit/s.
    pub gbps: Option<(f64, f64, f64)>,
    /// Median retransmissions.
    pub retrans_median: Option<f64>,
    /// Median of the per-run mean forwarder CPU, percent of one core.
    pub cpu_avg_median: Option<f64>,
    /// Median of the per-run largest RSS, MB (10^6 bytes).
    pub rss_max_median_mb: Option<f64>,
    /// Median, minimum and maximum of the paired per-repetition throughput
    /// ratio to the baseline; `None` for a baseline or without any pair.
    pub ratio: Option<(f64, f64, f64)>,
    /// Number of repetitions where both this variant and its baseline
    /// succeeded.
    pub n_ratio: usize,
}

impl Summary {
    fn to_json(&self) -> Value {
        json!({
            "variant": self.variant,
            "n_ok": self.n_ok,
            "reps": self.reps,
            "gbps_median": self.gbps.map(|g| g.0),
            "gbps_min": self.gbps.map(|g| g.1),
            "gbps_max": self.gbps.map(|g| g.2),
            "retrans_median": self.retrans_median,
            "cpu_avg_median": self.cpu_avg_median,
            "rss_max_median_mb": self.rss_max_median_mb,
            "ratio_median": self.ratio.map(|r| r.0),
            "ratio_min": self.ratio.map(|r| r.1),
            "ratio_max": self.ratio.map(|r| r.2),
            "n_ratio": self.n_ratio,
        })
    }
}

/// Aggregates `runs` per variant.
///
/// The ratio is paired: for each repetition in which both the variant and
/// its baseline succeeded, `recv_bps(variant) / recv_bps(baseline)`; the
/// summary holds the median, minimum and maximum of those ratios. Pairing
/// within a repetition cancels drift between repetitions.
pub fn summarize(variants: &[Variant], runs: &[Run], reps: usize) -> Vec<Summary> {
    let ok_runs = |id: &str| -> HashMap<u64, &Run> {
        runs.iter()
            .filter(|r| r.variant == id && r.ok)
            .map(|r| (r.rep, r))
            .collect()
    };
    variants
        .iter()
        .map(|variant| {
            let mine = ok_runs(&variant.id);
            let gbps: Vec<f64> = mine
                .values()
                .filter_map(|r| r.iperf3.map(|i| i.recv_bps / 1e9))
                .collect();
            let retrans: Vec<f64> = mine
                .values()
                .filter_map(|r| r.iperf3.map(|i| i.retransmits as f64))
                .collect();
            let cpu: Vec<f64> = mine
                .values()
                .filter_map(|r| r.forwarder.map(|f| f.cpu_avg))
                .collect();
            let rss: Vec<f64> = mine
                .values()
                .filter_map(|r| r.forwarder.map(|f| f.rss_max_kb as f64 * 1024.0 / 1e6))
                .collect();
            let mut ratios = Vec::new();
            if let Some(baseline) = &variant.baseline {
                let base = ok_runs(baseline);
                for (rep, run) in &mine {
                    let pair = (run.iperf3, base.get(rep).and_then(|b| b.iperf3));
                    if let (Some(own), Some(base)) = pair
                        && base.recv_bps > 0.0
                    {
                        ratios.push(own.recv_bps / base.recv_bps);
                    }
                }
            }
            Summary {
                variant: variant.id.clone(),
                n_ok: mine.len(),
                reps,
                gbps: median(&gbps)
                    .zip(min_max(&gbps))
                    .map(|(m, (lo, hi))| (m, lo, hi)),
                retrans_median: median(&retrans),
                cpu_avg_median: median(&cpu),
                rss_max_median_mb: median(&rss),
                ratio: median(&ratios)
                    .zip(min_max(&ratios))
                    .map(|(m, (lo, hi))| (m, lo, hi)),
                n_ratio: ratios.len(),
            }
        })
        .collect()
}

/// Builds the `results.json` value for a run directory.
///
/// # Errors
///
/// A missing or malformed `env.json`, `meta.json` or `variants.tsv`, or a
/// selected variant id the registry does not list.
pub fn collect(dir: &Path) -> Result<Value, String> {
    let env = read_json(&dir.join("env.json"))?;
    let meta = read_json(&dir.join("meta.json"))?;
    let registry = std::fs::read_to_string(dir.join("variants.tsv"))
        .map_err(|error| format!("variants.tsv: {error}"))?;
    let registry = parse_variants(&registry)?;

    let env_u64 = |key: &str| {
        env.get(key)
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("env.json has no numeric {key:?}"))
    };
    let env_str = |key: &str| {
        env.get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    };
    let reps = env_u64("reps")?;
    let clk_tck = env_u64("clk_tck")?;

    let selected: Vec<Variant> = match env.get("variants").and_then(Value::as_str) {
        None | Some("" | "all") => registry,
        Some(list) => list
            .split(',')
            .map(|id| {
                registry
                    .iter()
                    .find(|v| v.id == id)
                    .cloned()
                    .ok_or_else(|| format!("selected variant {id:?} is not in variants.tsv"))
            })
            .collect::<Result<_, _>>()?,
    };

    let mut runs = Vec::new();
    for variant in &selected {
        for rep in 1..=reps {
            let run_dir = dir.join("runs").join(&variant.id).join(rep.to_string());
            runs.push(read_run(&run_dir, &variant.id, rep, clk_tck));
        }
    }
    let summary = summarize(
        &selected,
        &runs,
        usize::try_from(reps).unwrap_or(usize::MAX),
    );

    Ok(json!({
        "schema": SCHEMA,
        "created_utc": env_str("created_utc"),
        "git": {
            "sha": meta.get("git_sha").and_then(Value::as_str).unwrap_or(""),
            "dirty": meta.get("git_dirty").and_then(Value::as_bool),
        },
        "host": {
            "kernel": env_str("kernel"),
            "cpu_model": env_str("cpu_model"),
            "nproc": env.get("nproc").and_then(Value::as_u64),
            "os": env_str("os"),
            "ci": {
                "github_actions": env.get("github_actions").and_then(Value::as_bool).unwrap_or(false),
                "runner_image": env_str("runner_image"),
            },
        },
        "toolchain": {
            "rustc": meta.get("rustc").cloned().unwrap_or(Value::Null),
            "rustflags": meta.get("rustflags").cloned().unwrap_or(Value::Null),
        },
        "versions": {
            "tun-rs": meta.get("tun_rs").cloned().unwrap_or(Value::Null),
            "tunnel-lattice": meta.get("tunnel_lattice").cloned().unwrap_or(Value::Null),
            "iperf3": env_str("iperf3"),
        },
        "method": {
            "duration_s": env.get("duration_s").and_then(Value::as_u64),
            "reps": reps,
            "warmup_runs": env.get("warmup_runs").and_then(Value::as_u64).unwrap_or(0),
            "order": "rotated",
            "throughput": "iperf3 end.sum_received",
            "cpu": "/proc utime+stime 1 Hz",
        },
        "topology": {
            "iface1": env_str("iface1"),
            "ip1": format!("{}/24", env_str("ip1")),
            "iface2": env_str("iface2"),
            "ip2": format!("{}/24", env_str("ip2")),
            "netns": env_str("netns"),
            "mtu": env.get("mtu").and_then(Value::as_u64),
        },
        "variants": selected.iter().map(Variant::to_json).collect::<Vec<_>>(),
        "runs": runs.iter().map(Run::to_json).collect::<Vec<_>>(),
        "summary": summary.iter().map(Summary::to_json).collect::<Vec<_>>(),
    }))
}

/// Extracts the resolved `tun-rs` and `tunnel-lattice` versions from
/// `cargo metadata --format-version 1` output.
///
/// # Errors
///
/// Either package is missing from the metadata.
pub fn resolved_versions(metadata: &Value) -> Result<(String, String), String> {
    let version = |name: &str| {
        metadata
            .get("packages")
            .and_then(Value::as_array)
            .and_then(|packages| {
                packages
                    .iter()
                    .find(|p| p.get("name").and_then(Value::as_str) == Some(name))
            })
            .and_then(|p| p.get("version").and_then(Value::as_str))
            .map(str::to_owned)
            .ok_or_else(|| format!("cargo metadata has no package {name}"))
    };
    Ok((version("tun-rs")?, version("tunnel-lattice")?))
}

/// Builds a JSON object from `key=value` (string) and `key:=value` (JSON
/// literal) arguments, as the run script records its state.
///
/// # Errors
///
/// An argument without `=`, an empty key, or an invalid JSON literal.
pub fn kv_object<I, S>(pairs: I) -> Result<Value, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut object = Map::new();
    for pair in pairs {
        let pair = pair.as_ref();
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| format!("{pair:?}: expected key=value or key:=json"))?;
        let (key, value) = match key.strip_suffix(':') {
            Some(key) => (
                key,
                serde_json::from_str(value).map_err(|error| format!("{pair:?}: {error}"))?,
            ),
            None => (key, Value::String(value.to_owned())),
        };
        if key.is_empty() {
            return Err(format!("{pair:?}: empty key"));
        }
        object.insert(key.to_owned(), value);
    }
    Ok(Value::Object(object))
}

/// Renders the results table and its footer from a `results.json` value.
///
/// Each baseline row comes first, followed by the variants compared with
/// it. With `allow_incomplete`, a variant with fewer successful runs than
/// repetitions is marked instead of refused.
///
/// # Errors
///
/// The value is not a `results.json` of schema [`SCHEMA`], or a variant
/// is incomplete and `allow_incomplete` is false.
pub fn markdown(results: &Value, allow_incomplete: bool) -> Result<String, String> {
    if results.get("schema").and_then(Value::as_str) != Some(SCHEMA) {
        return Err(format!("not a {SCHEMA} results file"));
    }
    let empty = Vec::new();
    let variants = results
        .get("variants")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let summary = results
        .get("summary")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let str_of = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).unwrap_or("").to_owned();
    let summary_of = |id: &str| {
        summary
            .iter()
            .find(|s| s.get("variant").and_then(Value::as_str) == Some(id))
    };

    let incomplete: Vec<String> = summary
        .iter()
        .filter(|s| {
            s.get("n_ok").and_then(Value::as_u64).unwrap_or(0)
                < s.get("reps").and_then(Value::as_u64).unwrap_or(0)
        })
        .map(|s| str_of(s, "variant"))
        .collect();
    if !incomplete.is_empty() && !allow_incomplete {
        return Err(format!(
            "incomplete variants (fewer successful runs than repetitions): {}; \
             pass --allow-incomplete to render them marked",
            incomplete.join(", ")
        ));
    }

    // Baselines first, each followed by the variants compared with it,
    // then any variant whose baseline was not run.
    let ids: Vec<String> = variants.iter().map(|v| str_of(v, "id")).collect();
    let baseline_of = |v: &Value| v.get("baseline").and_then(Value::as_str).map(str::to_owned);
    let mut ordered: Vec<&Value> = Vec::new();
    for base in variants.iter().filter(|v| baseline_of(v).is_none()) {
        ordered.push(base);
        let base_id = str_of(base, "id");
        ordered.extend(
            variants
                .iter()
                .filter(|v| baseline_of(v).as_deref() == Some(&base_id)),
        );
    }
    ordered.extend(
        variants
            .iter()
            .filter(|v| baseline_of(v).is_some_and(|b| !ids.contains(&b))),
    );

    let mut out = String::new();
    out.push_str(
        "| Configuration | Throughput (median Gbps) | vs tun-rs, same run | CPU avg | RSS max | Retransmissions |\n",
    );
    out.push_str("|---|---:|---:|---:|---:|---:|\n");
    let num = |s: &Value, key: &str| s.get(key).and_then(Value::as_f64);
    for variant in ordered {
        let id = str_of(variant, "id");
        let Some(s) = summary_of(&id) else { continue };
        let mut label = str_of(variant, "label");
        if incomplete.contains(&id) {
            let _ = write!(
                label,
                " (incomplete: {}/{} runs)",
                s.get("n_ok").and_then(Value::as_u64).unwrap_or(0),
                s.get("reps").and_then(Value::as_u64).unwrap_or(0)
            );
        }
        let na = || "n/a".to_owned();
        let gbps = num(s, "gbps_median").map_or_else(na, |g| format!("{g:.2}"));
        let ratio = match baseline_of(variant) {
            None => "—".to_owned(),
            Some(base) if !ids.contains(&base) => "n/a (baseline not run)".to_owned(),
            Some(_) => match (
                num(s, "ratio_median"),
                num(s, "ratio_min"),
                num(s, "ratio_max"),
            ) {
                (Some(m), Some(lo), Some(hi)) => {
                    format!("{:.1} % ({:.1}–{:.1})", m * 100.0, lo * 100.0, hi * 100.0)
                }
                _ => na(),
            },
        };
        let cpu = num(s, "cpu_avg_median").map_or_else(na, |c| format!("{c:.0} %"));
        let rss = num(s, "rss_max_median_mb").map_or_else(na, |m| format!("{m:.1} MB"));
        let retrans = num(s, "retrans_median").map_or_else(na, |r| {
            if r.fract() == 0.0 {
                format!("{r:.0}")
            } else {
                format!("{r:.1}")
            }
        });
        let _ = writeln!(
            out,
            "| {label} | {gbps} | {ratio} | {cpu} | {rss} | {retrans} |"
        );
    }

    out.push('\n');
    out.push_str(&footer(results));
    Ok(out)
}

fn footer(results: &Value) -> String {
    let text = |path: &str| {
        results
            .pointer(path)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    };
    let number = |path: &str| results.pointer(path).and_then(Value::as_u64);
    let or_unknown = |s: String| {
        if s.is_empty() {
            "unknown".to_owned()
        } else {
            s
        }
    };

    let date = or_unknown(text("/created_utc"));
    let mut host = String::new();
    if results
        .pointer("/host/ci/github_actions")
        .and_then(Value::as_bool)
        == Some(true)
    {
        let image = text("/host/ci/runner_image");
        let _ = write!(host, "GitHub Actions {} runner, ", or_unknown(image));
    }
    let _ = write!(
        host,
        "{}, {} CPUs, Linux {}",
        or_unknown(text("/host/cpu_model")),
        number("/host/nproc").map_or_else(|| "?".to_owned(), |n| n.to_string()),
        or_unknown(text("/host/kernel")),
    );
    let sha = text("/git/sha");
    let short = sha.get(..7).unwrap_or(&sha).to_owned();
    let dirty = if results.pointer("/git/dirty").and_then(Value::as_bool) == Some(true) {
        " (uncommitted changes)"
    } else {
        ""
    };
    let reps = number("/method/reps").unwrap_or(0);
    let duration = number("/method/duration_s").unwrap_or(0);
    let warmup = number("/method/warmup_runs").unwrap_or(0);

    let mut out = String::new();
    let _ = writeln!(out, "Recorded {date} on {host}.");
    let _ = writeln!(
        out,
        "Code {}{dirty}; tun-rs {}, tunnel-lattice {}, iperf3 {}; {}, RUSTFLAGS `{}`.",
        or_unknown(short),
        or_unknown(text("/versions/tun-rs")),
        or_unknown(text("/versions/tunnel-lattice")),
        or_unknown(text("/versions/iperf3")),
        or_unknown(text("/toolchain/rustc")),
        text("/toolchain/rustflags"),
    );
    let _ = writeln!(
        out,
        "Method: two TUN devices (one moved into a network namespace) joined by the forwarder; \
         `iperf3 -t {duration}` TCP from the host to a server in the namespace. Each row is the \
         median of {reps} runs (order rotated every repetition, {warmup} warm-up run(s) \
         discarded). Throughput is iperf3's receiver rate. \"vs tun-rs, same run\" is the median \
         (min–max) of the per-repetition ratio to the tun-rs baseline above it, measured in the \
         same repetition. CPU is the forwarder process's user+system time per second sampled at \
         1 Hz (100 % = one core); RSS is the largest resident set sampled."
    );
    let _ = writeln!(
        out,
        "Reproduce: `scripts/bench-forward.sh build && sudo scripts/bench-forward.sh run && \
         scripts/bench-forward.sh report <dir>`."
    );
    let _ = writeln!(
        out,
        "Absolute Gbps from shared or virtual runners are not comparable with numbers published \
         on other hardware; compare the same-run ratio."
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_of_odd_even_and_empty() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), Some(2.5));
        assert_eq!(median(&[]), None);
    }

    #[test]
    fn kv_object_types_values() {
        let value = kv_object(["a=1", "b:=1", "c:=true", "d=x=y", "e:=\"s\""]).expect("valid");
        assert_eq!(
            value,
            json!({"a": "1", "b": 1, "c": true, "d": "x=y", "e": "s"})
        );
        assert!(kv_object(["novalue"]).is_err());
        assert!(kv_object(["=x"]).is_err());
        assert!(kv_object(["a:=not json"]).is_err());
    }

    #[test]
    fn resolved_versions_reads_cargo_metadata() {
        let metadata = json!({"packages": [
            {"name": "tun-rs", "version": "2.8.11"},
            {"name": "tunnel-lattice", "version": "0.4.0"},
        ]});
        assert_eq!(
            resolved_versions(&metadata),
            Ok(("2.8.11".to_owned(), "0.4.0".to_owned()))
        );
        assert!(resolved_versions(&json!({"packages": []})).is_err());
    }
}
