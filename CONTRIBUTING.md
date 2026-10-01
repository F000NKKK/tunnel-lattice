# Contributing to Tunnel Lattice

Thank you for your interest in contributing to Tunnel Lattice.

## Project Status

Tunnel Lattice has published releases (see [CHANGELOG.md](CHANGELOG.md)) but
is still pre-1.0 — nothing in the public API is frozen. The most valuable
contributions right now are:

- Feedback on the crate architecture and API shape (see
  [ARCHITECTURE.md](ARCHITECTURE.md))
- Real-device reports for `tunnel-lattice-backend-tunrs` on Windows and
  macOS beyond what the privileged CI jobs cover (see that crate's README)
- Documentation and tooling improvements

Please check open issues and discussions before starting significant work, to
avoid duplicated effort.

## Getting Started

1. Fork the repository and clone your fork.
2. Create a topic branch for your change.
3. Make your changes, following the conventions described below.
4. Open a pull request against `main` using the provided pull request template.

## Development Conventions

Tunnel Lattice follows standard Rust ecosystem conventions:

- Code must be formatted with `rustfmt`.
- Code must be free of `clippy` warnings.
- Public APIs must be documented.
- Changes must include appropriate tests.
- Every affected crate must retain a standalone crate-local README, and
  English/Russian project documentation must remain synchronized.
- Privileged network tests must be isolated, opt-in, and restore changed state.
- Changes to the packet receive path should report before/after numbers
  from `cargo bench -p tunnel-lattice-async` and keep
  `crates/tunnel-lattice-async/tests/alloc_count.rs` passing.
- Changes to `crates/tunnel-lattice-async/src/pool.rs`, which holds all of
  that crate's `unsafe` code, should pass
  `cargo +nightly miri test -p tunnel-lattice-async --lib pool`. CI runs
  this as a non-blocking job.
- Commit messages should be clear and descriptive.

## Throughput Benchmarks

`bench/forwarder/` holds an unpublished iperf3 forwarder benchmark (Linux
only): raw `tun-rs` baselines and `tunnel-lattice` forwarding between two
TUN devices, measured in the same run. It is a standalone Cargo workspace,
not part of the root workspace, so check it with
`--manifest-path bench/forwarder/Cargo.toml --target-dir target/forwarder`
and one build set (`sync`, `tokio` or `async-io`) per invocation. The
`--target-dir` keeps build output under the ignored root `target/`; without
it cargo writes `bench/forwarder/target/`, which git does not ignore, and
the benchmark then records the tree as dirty. Running it needs root and
`iperf3`:

```sh
scripts/bench-forward.sh build
sudo scripts/bench-forward.sh run
scripts/bench-forward.sh report target/bench-forward/runs/<UTC time>
```

CI's `bench-forwarder` job (in `.github/workflows/ci.yml`) runs on every
push and pull request and keeps the harness formatted, building, linted
(`-D warnings`, each build set, plus a Windows and macOS `cargo check`)
and tested, and runs `bash -n` and `shellcheck` on
`scripts/bench-forward.sh`; it never runs the benchmark. The
`Forwarder benchmark` workflow (`.github/workflows/bench-forward.yml`) is
manual only: it runs the same commands as above when started and uploads
the results as an artifact. Throughput numbers in the
documentation must be copied from such a recorded `results.md`, with its
footer, never typed in by hand. See `bench/forwarder/README.md` for the
method and the variants.

### Real-device latency benchmark

The `device` bench of the `tunnel-lattice` crate
(`crates/tunnel-lattice/benches/device/`) opens a real TUN device, and a
TAP device where the host supports one (Linux `tap`, macOS `feth`, Windows
`tap0901`), on Linux, macOS and Windows. It echoes UDP datagrams back
through the device and measures round-trip latency (p50/p99) and the echo
rate of each packet path: synchronous `recv`/`send`, the native async
`packet_stream` + `send_async` (with the `tokio` or `async-io` feature) and
the thread bridge. It needs root or Administrator and does nothing unless
`TUNNEL_LATTICE_PRIVILEGED_BENCH=1` is set, so a plain `cargo bench` or
`cargo test` never touches the host's interfaces. Build unprivileged, then
run only the built binary with privileges:

```sh
cargo bench -p tunnel-lattice --bench device --no-run   # prints the executable
sudo env TUNNEL_LATTICE_PRIVILEGED_BENCH=1 \
  TUNNEL_LATTICE_BENCH_OUT=target/bench-device \
  target/release/deps/device-<hash> --bench
```

Add `--no-default-features --features tun-rs,tokio` (or `tun-rs,async-io`)
to measure the async paths. `TUNNEL_LATTICE_BENCH_RTTS`, `_SECS`, `_WINDOW`
and `_KINDS` (`tun,tap`) adjust the run; the module documentation in
`benches/device/main.rs` describes the method. Each device is torn down
before the next case starts, and the bench exits non-zero if any case
failed. The `Device benchmark` workflow runs it manually on all three
platforms in each feature set and uploads the JSON and Markdown results;
as with the forwarder, published numbers must come from such a recorded
run.

## Reporting Issues

Please use the issue templates under `.github/ISSUE_TEMPLATE/` when filing
bug reports or feature requests. Include as much context as possible.

## Security Issues

Do not report security vulnerabilities through public GitHub issues. See
[SECURITY.md](SECURITY.md) for the responsible disclosure process.

## Code of Conduct

By participating in this project, you agree to abide by the
[Code of Conduct](CODE_OF_CONDUCT.md).

## License

By contributing to Tunnel Lattice, you agree that your contributions will be licensed
under the [Mozilla Public License 2.0](LICENSE).
