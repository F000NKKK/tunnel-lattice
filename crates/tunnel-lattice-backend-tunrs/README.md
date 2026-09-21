# tunnel-lattice-backend-tunrs

`tun-rs`-backed cross-platform TUN/TAP backend for Tunnel Lattice, implementing
`tunnel-lattice-platform`'s provider traits.

## What it provides

- `TunRsBackend`, a stateless handle whose `DeviceProvider::open` builds a
  device via `tun_rs::DeviceBuilder`, mapping `DeviceKind::Tun`/`Tap` to
  `tun_rs::Layer::L3`/`L2`;
- `TunRsDevice`, the open-device handle implementing `PacketIo`,
  `DeviceObserver`, `DeviceMutator` (MTU and administrative state), and
  `CapabilityProvider`. It holds exactly one underlying `tun-rs` handle —
  never both a sync and an async one, since `tun_rs::SyncDevice::try_clone`
  only exists on Linux. With this crate's `async-io`/`tokio` feature, that
  handle is a `tun_rs::AsyncDevice` (built via `DeviceBuilder::build_async`)
  and `PacketIo` blocks on its async methods
  (`futures::executor::block_on`); this crate then also implements the
  native `AsyncPacketIo` directly on the same handle, reporting
  `Capability::NATIVE_ASYNC`. Without either feature, the handle is a
  `tun_rs::SyncDevice` and `PacketIo` calls it directly.
- Administrative-state read-back (`DeviceObserver::snapshot`'s
  `AdminState`) is exact only on Linux (`tun_rs`'s `is_running`); macOS,
  Windows, and BSD expose only a write-only `enabled(bool)` setter with no
  corresponding getter, so `snapshot()` reports `AdminState::Unknown` there.

## Feature flags

`async-io` and `tokio` are mutually exclusive — they select `tun-rs`'s own
two async backends (`tun-rs/async_io`, no runtime dependency beyond
`async-io`/`blocking`; `tun-rs/async_tokio`, which pulls in tokio).
Enabling both is a compile error from `tun-rs` itself, not from this crate.

## Why this is one shared crate, not three

`tun-rs` already abstracts Linux/Windows/macOS/BSD TUN/TAP differences
internally, so unlike `net-lattice`'s per-OS backend split there is no
platform-specific Rust code to isolate here yet. See the workspace
`ARCHITECTURE.md`, "Backend replacement plan," for how a future
per-OS backend (bypassing `tun-rs` entirely) would slot in alongside this
crate behind the same `tunnel-lattice-platform` traits.

## Usage

Not used directly — see the `tunnel-lattice` facade, which selects this
backend by default.
