# tunnel-lattice forwarder benchmark

An unpublished benchmark harness that measures how fast `tunnel-lattice`
forwards packets between two TUN devices, compared with raw
[`tun-rs`](https://crates.io/crates/tun-rs) measured on the same machine in
the same run. It is for maintainers and contributors who need throughput
numbers they can trace back to a recorded run. It is not part of any
published crate.

The method follows tun-rs's own
[tun-benchmark2](https://github.com/tun-rs/tun-benchmark2) (accessed
2026-09-30): two TUN devices, the second moved into a network namespace, a
forwarder process copying packets between them, and `iperf3` TCP traffic
from the host to a server inside the namespace, so every packet crosses the
forwarder in both directions. The forwarders here are written independently;
only the method and the command line are shared.

## Requirements

- Linux only. The forwarders compile on Windows and macOS but exit at once
  with code 2.
- `run` needs root: it creates TUN devices, a network namespace and routes.
  `build` and `report` do not, and cargo is never run as root.
- `iperf3`, `iproute2` (`ip`, `ss`), `procps` (`ps`), coreutils, and
  `/dev/net/tun`.

## Usage

From the repository root:

```sh
scripts/bench-forward.sh build
sudo scripts/bench-forward.sh run
scripts/bench-forward.sh report target/bench-forward/runs/<UTC time>
```

- `build [--rustflags FLAGS]` builds every forwarder in three separate cargo
  invocations, one per build set (`sync`, `tokio`, `async-io`), into
  `target/bench-forward/<set>/`. FLAGS default to `-C target-cpu=native`; the
  binaries are meant to run on the machine that built them. It also records
  the git commit, the toolchain and the resolved `tun-rs` and
  `tunnel-lattice` versions in `target/bench-forward/meta.json`.
- `run [--reps 5] [--duration 10] [--variants id,id|all] [--out DIR]
  [--fail-fast]` runs one warm-up run, then every selected variant once per
  repetition, rotating the order each repetition so no variant always runs
  first.
- `report DIR [--allow-incomplete]` writes `DIR/results.json` and
  `DIR/results.md` and prints the Markdown table. It refuses a variant with
  failed runs unless `--allow-incomplete` is given, which marks those rows.

`run` refuses to start if `tun11`, `tun22` or the `ns1` namespace already
exists. It removes the devices, namespace and route it created after every
run and on any exit, including Ctrl-C and SIGTERM. Set `TL_BENCH_IFACE1`,
`TL_BENCH_IFACE2`, `TL_BENCH_IP1`, `TL_BENCH_IP2` and `TL_BENCH_NS` to use
other names or addresses.

The repository's `Forwarder benchmark` GitHub Actions workflow runs the same
three commands on an `ubuntu-24.04` runner when started manually, and
uploads the run directory, `results.json` and `results.md` as an artifact.

## Variants

`variants.tsv` lists every variant; the script and the report tool read
only that file. Each `tunnel-lattice` row is compared with the raw tun-rs
row built in the same build set, so both link the same tun-rs with the same
features.

| Variant | Build set | What it runs | Compared with |
|---|---|---|---|
| `tunrs-sync` | sync | `tun_rs::SyncDevice`, one blocking copy thread per direction, 64 KiB buffer | — |
| `tl-sync` | sync | facade `Handle::recv`/`Handle::send`, one thread per direction, `recv_buffer_len()` buffer | `tunrs-sync` |
| `tunrs-async-tokio` | tokio | `tun_rs::AsyncDevice`, one Tokio task per direction | — |
| `tl-async-tokio` | tokio | facade `Handle::packet_stream` to receive, `Handle::send_async` to send, one task per direction | `tunrs-async-tokio` |
| `tl-async-tokio-shared-pool` | tokio | as above, but both directions receive into one shared `PacketPool` of 256 slots (`packet_stream_with_pool`) | `tunrs-async-tokio` |
| `tunrs-async-asyncio` | async-io | `tun_rs::AsyncDevice`, one `async_io::block_on` thread per direction | — |
| `tl-async-asyncio` | async-io | facade `packet_stream` + `send_async`, one `async_io::block_on` thread per direction | `tunrs-async-asyncio` |
| `tunrs-sync-offload` | sync | `tunrs-sync --offload`: `offload(true)`, `recv_multiple`/`send_multiple` with 128 preallocated buffers and a `GROTable` per thread | — |
| `tl-sync-offload` | sync | `tl-sync --offload`: `with_offload(true)`; `Handle::recv` splits each kernel super-packet, `Handle::send` writes one packet per call | `tunrs-sync-offload` |
| `tunrs-async-tokio-offload` | tokio | `tunrs-async-tokio --offload`: as `tunrs-sync-offload`, one Tokio task per direction | — |
| `tl-async-tokio-offload` | tokio | `tl-async-tokio --offload`: `with_offload(true)`; `packet_stream` to receive, then every packet already ready (up to 128) in one `Handle::send_batch_async` | `tunrs-async-tokio-offload` |
| `tunrs-async-asyncio-offload` | async-io | `tunrs-async-asyncio --offload`, one `async_io::block_on` thread per direction | — |
| `tl-async-asyncio-offload` | async-io | `tl-async-asyncio --offload`, as `tl-async-tokio-offload` on async-io | `tunrs-async-asyncio-offload` |

The `tunnel-lattice` variants use the public facade only. The raw tun-rs
variants assign their addresses themselves, as tun-benchmark2 does. The
script assigns the same addresses for every variant, because
`tunnel-lattice` has no address API.

The offload rows open both devices with Linux TUN segmentation offload, so
the kernel hands the forwarder TCP super-packets of up to 64 KiB and
accepts coalesced ones back. Each `tunnel-lattice` offload row is compared
with tun-rs's own offload path in the same build set; compare its
throughput with the plain row of the same binary for the gain from offload
itself. A forwarder started with `--offload` exits with an error if the
device did not grant offload (`Capability::SEGMENTATION_OFFLOAD` for
`tunnel-lattice`, `tcp_gso()` for tun-rs), so an offload row can never
silently measure the plain path. The sync `tunnel-lattice` row has no
batch send: a blocking loop cannot tell whether another packet is ready
without waiting for it, and `tunnel-lattice` has no batch receive, so that
row measures the receive-side split alone.

Every forwarder accepts the same command line:
`--iface1 <name> --ip1 <ipv4> --iface2 <name> --ip2 <ipv4> [--threads N] [--mtu M] [--offload]`.
It prints `ready <iface1> <iface2>` once both devices are open, and exits
the whole process on any unexpected receive or send error, so a failed copy
loop can never be recorded as a low number.

## What is measured

- **Throughput**: iperf3's receiver rate (`end.sum_received`), median over
  the repetitions.
- **vs tun-rs, same run**: for each repetition where both succeeded, the
  variant's throughput divided by its baseline's, measured in the same
  repetition. The table shows the median ratio with its range. Pairing
  within a repetition cancels drift between repetitions.
- **CPU avg**: the forwarder process's user+system time per second, sampled
  at 1 Hz from `/proc/<pid>/stat` and averaged over the run. 100 % is one
  core; a multi-threaded forwarder can exceed it. `ps %cpu` is also
  recorded in `results.json` for comparison with tun-benchmark2, but it
  averages over the process lifetime, idle setup included, so it is not the
  headline number.
- **RSS max**: the largest resident set sampled (`VmRSS`); `VmHWM` is also
  recorded.
- **Retransmissions**: iperf3's TCP retransmissions (`end.sum_sent`).

## Reading the results

Absolute Gbps from shared or virtual runners are not comparable with
numbers published on other hardware, including tun-rs's own; compare the
same-run ratio. Every number in `results.md` comes from the raw per-run
files next to it (`runs/<variant>/<rep>/iperf3.json`, `samples.tsv`,
`forwarder.log`, `run.json`), and the table footer records the commit,
versions, host and method.

While tun-rs is the only backend, a plain tunnel-lattice row can at best
match tun-rs minus its own overhead. The offload rows show what Linux TUN
segmentation offload adds on top; going further needs native per-OS
backends.

## Development

The harness is a standalone Cargo workspace, not a member of the
repository's root workspace, so the root release tooling never sees it.
Enable exactly one build set per cargo invocation, and pass
`--target-dir target/forwarder` from the repository root: cargo's default
for a standalone workspace is `bench/forwarder/target/`, which git does not
ignore, and `scripts/bench-forward.sh build` would then record the tree as
dirty (`git_dirty: true`, "(uncommitted changes)" in the results footer).

```sh
cargo clippy --manifest-path bench/forwarder/Cargo.toml --target-dir target/forwarder --no-default-features --features tokio --all-targets -- -D warnings
cargo test --manifest-path bench/forwarder/Cargo.toml --target-dir target/forwarder --no-default-features --features sync
```

The tests cover the command line, the report's parsing and aggregation (on
fixtures), the paired ratio and the Markdown output. They need no
privilege. The `bench-forwarder` job of the repository's `CI` workflow runs
these checks on every push and pull request, in all three build sets, with
a `cargo check` for Windows and macOS targets and `shellcheck` on the
script.

Dependency requirements match the root workspace's; update them together.

## License

MPL-2.0, like the rest of the repository.
