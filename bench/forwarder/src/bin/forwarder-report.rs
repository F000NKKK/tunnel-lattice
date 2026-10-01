//! Results tool for `scripts/bench-forward.sh`.
//!
//! ```text
//! forwarder-report collect <run-dir>                    writes <run-dir>/results.json
//! forwarder-report markdown <results.json> [--allow-incomplete]
//! forwarder-report kv --out <file> key=value|key:=json ...
//! forwarder-report meta --out <file> --cargo-metadata <file> key=value|key:=json ...
//! forwarder-report iperf3-check <iperf3.json>           exit 1 with the error if it failed
//! ```
//!
//! `kv` writes a JSON object from `key=value` (string) and `key:=json`
//! (literal) pairs; `meta` does the same and adds the `tun_rs` and
//! `tunnel_lattice` versions resolved in `cargo metadata` output.
//!
//! Needs no privilege and no feature; it does every JSON step so the
//! script needs no `jq`.

use std::path::Path;
use std::process::ExitCode;

use serde_json::{Value, json};
use tunnel_lattice_bench_forwarder::report;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("forwarder-report: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let (command, rest) = args.split_first().ok_or(USAGE)?;
    match command.as_str() {
        "collect" => {
            let [dir] = rest else {
                return Err(USAGE.into());
            };
            let dir = Path::new(dir);
            let results = report::collect(dir)?;
            write_json(&dir.join("results.json"), &results)
        }
        "markdown" => {
            let (path, allow_incomplete) = match rest {
                [path] => (path, false),
                [path, flag] if flag == "--allow-incomplete" => (path, true),
                _ => return Err(USAGE.into()),
            };
            let results = read_json(Path::new(path))?;
            print!("{}", report::markdown(&results, allow_incomplete)?);
            Ok(())
        }
        "kv" => {
            let [flag, out, pairs @ ..] = rest else {
                return Err(USAGE.into());
            };
            if flag != "--out" {
                return Err(USAGE.into());
            }
            write_json(Path::new(out), &report::kv_object(pairs)?)
        }
        "meta" => {
            let [out_flag, out, metadata_flag, metadata, pairs @ ..] = rest else {
                return Err(USAGE.into());
            };
            if out_flag != "--out" || metadata_flag != "--cargo-metadata" {
                return Err(USAGE.into());
            }
            let (tun_rs, tunnel_lattice) =
                report::resolved_versions(&read_json(Path::new(metadata))?)?;
            let mut meta = report::kv_object(pairs)?;
            meta["tun_rs"] = json!(tun_rs);
            meta["tunnel_lattice"] = json!(tunnel_lattice);
            write_json(Path::new(out), &meta)
        }
        "iperf3-check" => {
            let [path] = rest else {
                return Err(USAGE.into());
            };
            report::parse_iperf3(&read_json(Path::new(path))?).map(drop)
        }
        _ => Err(USAGE.into()),
    }
}

const USAGE: &str = "usage: forwarder-report collect <run-dir> | markdown <results.json> \
                     [--allow-incomplete] | kv --out <file> key=value|key:=json ... | meta --out \
                     <file> --cargo-metadata <file> key=value|key:=json ... | iperf3-check \
                     <iperf3.json>";

fn read_json(path: &Path) -> Result<Value, String> {
    let text =
        std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    serde_json::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))
}

fn write_json(path: &Path, value: &Value) -> Result<(), String> {
    let mut text = serde_json::to_string_pretty(value).map_err(|error| error.to_string())?;
    text.push('\n');
    std::fs::write(path, text).map_err(|error| format!("{}: {error}", path.display()))
}
