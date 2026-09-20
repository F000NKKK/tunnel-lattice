# Tunnel Lattice project index

Cross-platform Rust library for TUN/TAP tunnel interfaces, designed to
compose with the rest of the Lattice networking stack.

No release has been published yet. `ARCHITECTURE.md`/`ARCHITECTURE.ru.md`
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

## Current status

No release has been published yet. Roadmap/version tracking will move to
the YouTrack project `TL` once it is created; until then, `CONTRIBUTING.md`
and `SUPPORT.md` carry current project-status language.

## Useful commands

```text
cargo fmt --all -- --check
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo doc --workspace --all-features --no-deps
git diff --check
```
