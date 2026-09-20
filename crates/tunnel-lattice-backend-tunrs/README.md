# tunnel-lattice-backend-tunrs

`tun-rs`-backed cross-platform TUN/TAP backend for Tunnel Lattice, implementing
`tunnel-lattice-platform`'s provider traits.

## What it provides

- `TunRsBackend`, a stateless handle whose `DeviceProvider::open` builds a
  device via `tun_rs::DeviceBuilder`, mapping `DeviceKind::Tun`/`Tap` to
  `tun_rs::Layer::L3`/`L2`;
- `TunRsDevice`, the open-device handle implementing `PacketIo` (blocking
  `recv`/`send` via `tun_rs::SyncDevice`), `DeviceObserver`, `DeviceMutator`
  (MTU and administrative state), `CapabilityProvider`, and — with the
  `async` feature — `AsyncPacketIo` via a cloned `tun_rs::AsyncDevice`
  handle on the same underlying device.

## Why this is one shared crate, not three

`tun-rs` already abstracts Linux/Windows/macOS/BSD TUN/TAP differences
internally, so unlike `net-lattice`'s per-OS backend split there is no
platform-specific Rust code to isolate here yet. See the workspace
`ARCHITECTURE.md`, "Backend replacement plan," for how a future
per-OS backend (bypassing `tun-rs` entirely) would slot in alongside this
crate behind the same `tunnel-lattice-platform` traits.

## Unverified against upstream

This crate's `tun-rs` usage was written against the API surface visible on
docs.rs for `tun-rs` 2.8.11 (`DeviceBuilder`, `SyncDevice`, `AsyncDevice`,
`Layer`), not against a local build with the real dependency compiled and
tested. Before publishing, run the workspace's ordinary `cargo build`/
`cargo test` against the pinned `tun-rs` version and correct any signature
drift.

## Usage

Not used directly — see the `tunnel-lattice` facade, which selects this
backend by default.
