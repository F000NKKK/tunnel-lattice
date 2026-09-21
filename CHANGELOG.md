# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

- Fixed `tunnel-lattice-backend-tunrs` failing to build on macOS/Windows CI:
  `tun_rs::SyncDevice::try_clone`/`is_running` only exist on Linux.
  `TunRsDevice` now holds exactly one handle (never clones one), and
  `snapshot()` reports `AdminState::Unknown` off Linux instead of assuming
  `is_running` exists everywhere.
- Split `tunnel-lattice`'s async support into two mutually exclusive
  features, `async-io` and `tokio`, matching `tun-rs`'s own two async
  backends (enabling both is a compile error). The previous single `async`
  feature silently meant "pull in tokio" regardless of what a caller
  wanted, defeating the point of offering a lightweight option.
- `tunnel-lattice`'s facade gained `ConnectedDevice`, a named bound for
  "an open device handle usable through `Tunnel`/`Handle`", replacing a
  four-trait bound list duplicated across both types.

## [0.1.0]

- Repository bootstrap: workflow, policies, and packaging scaffolding.
- Initial crate architecture: `tunnel-lattice-core`, `-model`, `-platform`,
  `-backend-tunrs` (implemented on the `tun-rs` crate), `-async`, and the
  `tunnel-lattice` facade. See `ARCHITECTURE.md`, "Backend replacement
  plan," for why the initial backend is one `tun-rs`-backed crate rather
  than net-lattice's per-OS split, and how a future native backend would
  slot in alongside it.
- `tunnel-lattice`'s optional async feature adds a `futures::Stream` of
  received packets, using a backend's native async path
  (`Capability::NATIVE_ASYNC`) when available and falling back to
  `tunnel-lattice-async`'s thread-based adapter otherwise. No async runtime
  dependency is imposed unless a caller opts in.
- `scripts/release.sh`/`scripts/gh_release.sh`, ported from `net-lattice`:
  per-crate version bump/publish with dependency-ordered cascades, and
  idempotent git tag/GitHub release backfill from crates.io state.
