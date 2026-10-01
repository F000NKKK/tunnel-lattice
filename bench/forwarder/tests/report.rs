//! Report tool tests on fixtures: parsing, aggregation, the paired ratio,
//! and the Markdown table. No privilege, no device, deterministic.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use tunnel_lattice_bench_forwarder::report::{
    self, SCHEMA, collect, markdown, parse_iperf3, parse_samples, parse_variants,
};

fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

fn fixture_json(name: &str) -> Value {
    serde_json::from_str(&fixture(name)).expect("fixture is JSON")
}

/// `iperf3-ok.json` is a real iperf3 3.16 `-J` client output recorded by
/// the benchmark workflow (`tl-sync`, repetition 1), with `intervals` and
/// `end.streams` emptied.
#[test]
fn iperf3_success_fields() {
    let parsed = parse_iperf3(&fixture_json("iperf3-ok.json")).expect("ok fixture");
    assert_eq!(parsed.recv_bps, 2_787_578_560.552_386_8);
    assert_eq!(parsed.sent_bps, 2_790_552_080.059_525_5);
    assert_eq!(parsed.retransmits, 64);
    assert_eq!(parsed.host_cpu, 5.594_368_815_995_75);
    assert_eq!(parsed.remote_cpu, 79.113_969_670_483_34);
}

#[test]
fn iperf3_error_and_missing_fields_fail() {
    let error = parse_iperf3(&fixture_json("iperf3-error.json")).expect_err("error fixture");
    assert!(error.contains("unable to connect to server"), "{error}");

    let mut no_retransmits = fixture_json("iperf3-ok.json");
    no_retransmits["end"]["sum_sent"]
        .as_object_mut()
        .expect("object")
        .remove("retransmits");
    let error = parse_iperf3(&no_retransmits).expect_err("missing field");
    assert!(error.contains("retransmits"), "{error}");

    assert!(parse_iperf3(&json!({})).is_err());
}

#[test]
fn samples_cpu_rss_and_hwm() {
    let forwarder = parse_samples(&fixture("samples.tsv"), 100).expect("fixture");
    // Ticks 0 -> 50 -> 150 -> 300 at 100 ticks/s over 1 s intervals:
    // 50 %, 100 %, 150 %. The cut-short last line is skipped.
    assert_eq!(forwarder.samples, 4);
    assert_eq!(forwarder.cpu_avg, 100.0);
    assert_eq!(forwarder.cpu_max, 150.0);
    assert_eq!(forwarder.ps_cpu_avg, 25.0);
    assert_eq!(forwarder.ps_cpu_max, 40.0);
    assert_eq!(forwarder.rss_avg_kb, 2125.0);
    assert_eq!(forwarder.rss_max_kb, 3000);
    assert_eq!(forwarder.hwm_kb, Some(3100));
}

#[test]
fn samples_reject_unusable_input() {
    assert!(
        parse_samples("1000\t0\t0\t1\t0.0\n", 100).is_err(),
        "one sample"
    );
    assert!(parse_samples("", 100).is_err(), "no samples");
    assert!(
        parse_samples(&fixture("samples.tsv"), 0).is_err(),
        "zero clk_tck"
    );
    let backwards = "2000\t0\t0\t1\t0.0\n1000\t1\t0\t1\t0.0\n";
    assert!(
        parse_samples(backwards, 100).is_err(),
        "time goes backwards"
    );
}

#[test]
fn shipped_registry_is_valid_and_uses_the_facade() {
    let text = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("variants.tsv"))
        .expect("variants.tsv");
    let variants = parse_variants(&text).expect("valid registry");
    assert_eq!(variants.len(), 7);
    for variant in &variants {
        match variant.family.as_str() {
            "tun-rs" => {
                assert_eq!(variant.baseline, None, "{}", variant.id);
                assert_eq!(variant.layer, "tun-rs", "{}", variant.id);
            }
            "tunnel-lattice" => {
                let baseline = variant.baseline.as_deref().expect("has a baseline");
                let base = variants.iter().find(|v| v.id == baseline).expect("listed");
                // Compared on the same build set, so both link the same
                // tun-rs with the same features.
                assert_eq!(base.build_set, variant.build_set, "{}", variant.id);
                assert_eq!(variant.layer, "facade", "{}", variant.id);
            }
            other => panic!("{}: unknown family {other}", variant.id),
        }
        assert!(!variant.offload, "{}", variant.id);
    }
}

#[test]
fn registry_errors() {
    let row = |cols: &[&str]| cols.join("\t");
    let base = row(&[
        "b", "sync", "bin", "-", "-", "0", "tun-rs", "sync", "threads", "tun-rs", "x", "B",
    ]);
    let cases = [
        (row(&["b", "sync"]), "columns"),
        (format!("{base}\n{base}"), "duplicate"),
        (
            base.replace("\tsync\tbin", "\tgso\tbin"),
            "unknown build set",
        ),
        (base.replace("\t0\t", "\t2\t"), "offload"),
        (base.replace("\t-\t-\t", "\t-\tnope\t"), "unknown baseline"),
        (
            format!(
                "{base}\n{}\n{}",
                base.replacen("b\t", "c\t", 1)
                    .replace("\t-\t-\t", "\t-\tb\t"),
                base.replacen("b\t", "d\t", 1)
                    .replace("\t-\t-\t", "\t-\tc\t")
            ),
            "has a baseline itself",
        ),
    ];
    for (text, expected) in cases {
        let error = parse_variants(&text).expect_err(&text);
        assert!(error.contains(expected), "{text:?}: {error}");
    }
    assert_eq!(
        parse_variants(&format!("# comment\n\n{base}\n")).map(|v| v.len()),
        Ok(1)
    );
}

/// A scratch run directory under cargo's per-test temp dir.
fn run_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create run dir");
    dir
}

fn write_json(path: &Path, value: &Value) {
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(path, serde_json::to_string(value).expect("serialize")).expect("write");
}

/// Writes one repetition: `gbps = None` records a failed run (iperf3
/// error, `ok = false`).
fn write_rep(dir: &Path, variant: &str, rep: u64, gbps: Option<f64>) {
    let rep_dir = dir.join("runs").join(variant).join(rep.to_string());
    let iperf3 = match gbps {
        Some(gbps) => {
            let mut value = fixture_json("iperf3-ok.json");
            value["end"]["sum_received"]["bits_per_second"] = json!(gbps * 1e9);
            value
        }
        None => fixture_json("iperf3-error.json"),
    };
    write_json(&rep_dir.join("iperf3.json"), &iperf3);
    let status = match gbps {
        Some(_) => json!({"ok": true, "error": null, "order": rep}),
        None => json!({"ok": false, "error": "iperf3 exited with 1", "order": rep}),
    };
    write_json(&rep_dir.join("run.json"), &status);
    fs::write(rep_dir.join("samples.tsv"), fixture("samples.tsv")).expect("samples");
}

fn write_run_dir(dir: &Path, reps: u64, base: &[Option<f64>], tl: &[Option<f64>]) {
    let variants = [
        "# test registry",
        "base\tsync\ttunrs-sync\t-\t-\t0\ttun-rs\tsync\tthreads\ttun-rs\tstack-64k\ttun-rs sync",
        "tl\tsync\ttl-sync\t-\tbase\t0\ttunnel-lattice\tsync\tthreads\tfacade\trecv-buffer-len\ttunnel-lattice sync",
    ]
    .join("\n");
    fs::write(dir.join("variants.tsv"), variants).expect("variants");
    let env = report::kv_object([
        "created_utc=2026-10-01T12:00:00Z".to_owned(),
        "kernel=6.11.0-1018-azure".to_owned(),
        "cpu_model=Test CPU".to_owned(),
        "nproc:=4".to_owned(),
        "os=Ubuntu 24.04".to_owned(),
        "github_actions:=true".to_owned(),
        "runner_image=ubuntu24".to_owned(),
        "iperf3=3.16".to_owned(),
        "clk_tck:=100".to_owned(),
        "duration_s:=10".to_owned(),
        format!("reps:={reps}"),
        "warmup_runs:=1".to_owned(),
        "variants=all".to_owned(),
        "iface1=tun11".to_owned(),
        "ip1=10.0.1.1".to_owned(),
        "iface2=tun22".to_owned(),
        "ip2=10.0.2.1".to_owned(),
        "netns=ns1".to_owned(),
        "mtu:=1500".to_owned(),
    ])
    .expect("env");
    write_json(&dir.join("env.json"), &env);
    write_json(
        &dir.join("meta.json"),
        &json!({
            "rustc": "rustc 1.93.0",
            "rustflags": "-C target-cpu=native",
            "tun_rs": "2.8.11",
            "tunnel_lattice": "0.4.0",
            "git_sha": "0123456789abcdef",
            "git_dirty": false,
        }),
    );
    for (i, (b, t)) in base.iter().zip(tl).enumerate() {
        let rep = i as u64 + 1;
        write_rep(dir, "base", rep, *b);
        write_rep(dir, "tl", rep, *t);
    }
}

fn summary_of<'a>(results: &'a Value, id: &str) -> &'a Value {
    results["summary"]
        .as_array()
        .expect("summary")
        .iter()
        .find(|s| s["variant"] == id)
        .expect("variant summary")
}

#[test]
fn collect_pairs_ratios_per_repetition() {
    let dir = run_dir("paired");
    // Paired ratios 1.1, 0.7, 0.9: median 0.9. The unpaired ratio of
    // medians (14 / 20 = 0.7) would differ, so this checks the pairing.
    // Rep 4: the baseline failed, so that rep has no ratio.
    write_run_dir(
        &dir,
        4,
        &[Some(10.0), Some(20.0), Some(30.0), None],
        &[Some(11.0), Some(14.0), Some(27.0), Some(5.0)],
    );
    let results = collect(&dir).expect("collect");
    assert_eq!(results["schema"], SCHEMA);
    assert_eq!(results["versions"]["tun-rs"], "2.8.11");
    assert_eq!(results["topology"]["ip2"], "10.0.2.1/24");
    assert_eq!(results["runs"].as_array().map(Vec::len), Some(8));

    let base = summary_of(&results, "base");
    assert_eq!(base["n_ok"], 3);
    assert_eq!(base["gbps_median"], 20.0);
    assert_eq!(base["ratio_median"], Value::Null);

    let tl = summary_of(&results, "tl");
    assert_eq!(tl["n_ok"], 4);
    assert_eq!(tl["n_ratio"], 3);
    assert_eq!(tl["gbps_median"], 12.5);
    assert_eq!(tl["gbps_min"], 5.0);
    assert_eq!(tl["gbps_max"], 27.0);
    let close = |v: &Value, expected: f64| (v.as_f64().expect("number") - expected).abs() < 1e-12;
    assert!(close(&tl["ratio_median"], 0.9), "{}", tl["ratio_median"]);
    assert!(close(&tl["ratio_min"], 0.7), "{}", tl["ratio_min"]);
    assert!(close(&tl["ratio_max"], 1.1), "{}", tl["ratio_max"]);
    assert_eq!(tl["retrans_median"], 64.0);
    assert_eq!(tl["cpu_avg_median"], 100.0);

    let failed = results["runs"]
        .as_array()
        .expect("runs")
        .iter()
        .find(|r| r["variant"] == "base" && r["rep"] == 4)
        .expect("rep 4");
    assert_eq!(failed["ok"], false);
    let error = failed["error"].as_str().expect("error text");
    assert!(error.contains("iperf3 exited with 1"), "{error}");
    assert!(error.contains("unable to connect"), "{error}");

    // An incomplete variant is refused unless explicitly allowed, then
    // marked.
    let refused = markdown(&results, false).expect_err("incomplete");
    assert!(refused.contains("base"), "{refused}");
    let table = markdown(&results, true).expect("allowed");
    assert!(
        table.contains("| tun-rs sync (incomplete: 3/4 runs) |"),
        "{table}"
    );
}

#[test]
fn collect_treats_a_missing_run_directory_as_failed() {
    let dir = run_dir("missing");
    write_run_dir(&dir, 2, &[Some(10.0)], &[Some(9.0)]);
    let results = collect(&dir).expect("collect");
    assert_eq!(summary_of(&results, "base")["n_ok"], 1);
    let run = results["runs"]
        .as_array()
        .expect("runs")
        .iter()
        .find(|r| r["variant"] == "tl" && r["rep"] == 2)
        .expect("rep 2");
    assert_eq!(run["ok"], false);
}

#[test]
fn collect_rejects_an_unknown_selected_variant() {
    let dir = run_dir("unknown-selected");
    write_run_dir(&dir, 1, &[Some(10.0)], &[Some(9.0)]);
    let mut env: Value =
        serde_json::from_str(&fs::read_to_string(dir.join("env.json")).expect("env"))
            .expect("json");
    env["variants"] = json!("base,nope");
    write_json(&dir.join("env.json"), &env);
    let error = collect(&dir).expect_err("unknown variant");
    assert!(error.contains("nope"), "{error}");
}

#[test]
fn markdown_table_and_footer() {
    let dir = run_dir("complete");
    write_run_dir(
        &dir,
        3,
        &[Some(10.0), Some(20.0), Some(30.0)],
        &[Some(11.0), Some(14.0), Some(27.0)],
    );
    let results = collect(&dir).expect("collect");
    let table = markdown(&results, false).expect("complete");
    let lines: Vec<&str> = table.lines().collect();
    assert_eq!(
        lines[0],
        "| Configuration | Throughput (median Gbps) | vs tun-rs, same run | CPU avg | RSS max | Retransmissions |"
    );
    assert_eq!(
        lines[2],
        "| tun-rs sync | 20.00 | — | 100 % | 3.1 MB | 64 |"
    );
    assert_eq!(
        lines[3],
        "| tunnel-lattice sync | 14.00 | 90.0 % (70.0–110.0) | 100 % | 3.1 MB | 64 |"
    );
    assert!(table.contains("Recorded 2026-10-01T12:00:00Z on GitHub Actions ubuntu24 runner, Test CPU, 4 CPUs, Linux 6.11.0-1018-azure."));
    assert!(table.contains("Code 0123456; tun-rs 2.8.11, tunnel-lattice 0.4.0, iperf3 3.16"));
    assert!(table.contains("median of 3 runs"));
    assert!(table.contains("scripts/bench-forward.sh build && sudo scripts/bench-forward.sh run"));
    assert!(table.contains("compare the same-run ratio"));
    assert!(!table.contains("TL-"), "no tracker IDs in generated output");
}

#[test]
fn markdown_rejects_other_files() {
    assert!(markdown(&json!({"schema": "something-else"}), true).is_err());
    assert!(markdown(&json!([]), true).is_err());
}
