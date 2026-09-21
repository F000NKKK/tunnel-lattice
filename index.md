# Tunnel Lattice project index

Cross-platform Rust library for TUN/TAP tunnel interfaces, designed to
compose with the rest of the Lattice networking stack.

`0.1.0` is published (see `SUPPORT.md`). `ARCHITECTURE.md`/`ARCHITECTURE.ru.md`
describe the current crate design, but nothing in the workspace is
API-frozen — do not assume any stability guarantee from this file.

## Workspace map

```text
tunnel-lattice/
├── crates/
│   ├── tunnel-lattice-core/           errors, IDs — no OS dependency
│   ├── tunnel-lattice-model/          DeviceKind/DeviceConfig/Device — no OS dependency
│   ├── tunnel-lattice-platform/       generic provider traits + Capability
│   ├── tunnel-lattice-backend-tunrs/  tun-rs-backed implementation
│   ├── tunnel-lattice-async/          futures::Stream adapter over PacketIo
│   └── tunnel-lattice/                facade
├── .github/workflows/          CI
├── .claude/                    Reusable Claude Code rules, roles, and templates
├── .codex/                     Reusable Codex rules, roles, and templates
├── scripts/                    Release automation
├── README.md                   English user documentation
├── README.ru.md                Russian user documentation
├── ARCHITECTURE.md             Crate design and rationale
├── ARCHITECTURE.ru.md          Russian architecture documentation
├── CHANGELOG.md                Release history
├── SECURITY.md                 Vulnerability reporting policy
├── SUPPORT.md                  Support and project status
├── CONTRIBUTING.md             Contribution workflow
├── AGENTS.md                   Repository agent entry point (Codex)
├── CLAUDE.md                   Repository agent entry point (Claude Code)
└── index.md                    This project map
```

## Lattice ecosystem

Tunnel Lattice is one crate in the Lattice networking ecosystem:

```text
net-lattice      OS networking inspection/configuration (routes, DNS, interfaces)
tunnel-lattice   TUN/TAP tunnel interfaces
dns-lattice      Programmable DNS control plane
flow-lattice     Policy compiler: rules -> platform-neutral network plans
sdk-lattice      Application-facing SDK composing the crates above
```

## Current release and roadmap

Published stage baseline: `tunnel-lattice 0.1.0` (see `SECURITY.md`'s
supported-version table). Read the current workspace version from
`crates/tunnel-lattice/Cargo.toml`; do not duplicate a patch version here.
Roadmap/version tracking lives in the YouTrack project `TL`
(`@.claude/rules/youtrack.md`) — Sprint entries map 1:1 to the stages below.

- **0.1 (done, published):** repository bootstrap; initial crate
  architecture (`-core`, `-model`, `-platform`, `-backend-tunrs`, `-async`,
  facade); `tun-rs`-backed sync `PacketIo` with MTU/administrative-state
  mutation; optional `async-io`/`tokio` features (mutually exclusive) adding
  `Handle::packet_stream`, using a backend's `AsyncPacketIo` when it reports
  `Capability::NATIVE_ASYNC` and falling back to `tunnel-lattice-async`'s
  thread-based adapter otherwise; `scripts/release.sh`/`gh_release.sh`
  release automation ported from `net-lattice`.
- **0.2 (in progress):** privileged per-platform CI. Added `#[ignore]`d
  `privileged_tests` to `tunnel-lattice-backend-tunrs` (open/mutate/tear
  down a real device) and a `privileged` CI job running them with `sudo`/
  Administrator on Linux, Windows, and macOS across all three feature sets
  — mirroring `net-lattice`'s privileged-job pattern, per `.claude/rules/
  ci.md`'s "test platforms separately" rule. Verified locally on Linux under
  real `CAP_NET_ADMIN` (a rootless container, not just compiling): this
  found and fixed a genuine deadlock in the `tokio` feature (see
  `CHANGELOG.md`'s `[Unreleased]` entry) that reading `tun-rs`'s source
  alone would never have surfaced. Remaining: confirm the Windows/macOS legs
  actually pass once this lands on GitHub Actions — the local run only
  covers Linux.
- **0.3 (proposed):** `Capability::PERSISTENT_DEVICES` and
  `Capability::MULTI_QUEUE`. Both bits are already defined in
  `tunnel-lattice-platform::Capability`, but no backend sets either one yet
  — `tun-rs` supports attaching to a persistent Linux TUN by name and
  opening multi-queue devices, so this is `tunnel-lattice-backend-tunrs`
  work, not a new contract.
- **0.4 (proposed):** cancellable `PacketStream` shutdown. `tunnel-lattice-
  async::PacketStream::drop` currently cannot unblock a worker thread parked
  inside a blocking `recv` with no further packets arriving — a documented
  bootstrap-stage limitation (see the type's rustdoc and `ARCHITECTURE.md`,
  "Async design"), not a hypothetical one. Needs either a cancellable `recv`
  variant on `PacketIo` or a documented per-backend unblocking mechanism.
- **Unscheduled:** a hand-written per-OS TUN/TAP backend (`tunnel-lattice-
  backend-linux`/`-windows`/`-darwin`, mirroring `net-lattice`'s split) to
  eventually let `tun-rs` be dropped, per the original design goal recorded
  in `ARCHITECTURE.md`, "Backend replacement plan." Deliberately not before
  0.1's Cargo-feature-based backend selection has actually had a second
  backend implemented against it — until then this stays a stated intent,
  not a scheduled Sprint.
- **1.0 (unscheduled):** compatibility audit and API freeze, mirroring
  `net-lattice`'s 0.21 stage (`Error`/enum `#[non_exhaustive]` review, full
  rustdoc coverage, a "Frozen 1.0 Public API Surface" inventory). Not
  started — `ARCHITECTURE.md`'s "Frozen public API surface" section
  explicitly states nothing is frozen yet.

## Useful commands

```text
cargo fmt --all -- --check
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo doc --workspace --all-features --no-deps
git diff --check
```
