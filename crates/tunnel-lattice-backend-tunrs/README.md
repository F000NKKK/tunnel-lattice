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
  only exists on Linux. With `async-io`, that handle is a
  `tun_rs::AsyncDevice` and `PacketIo` blocks on its async methods via
  `futures::executor::block_on`. With `tokio`, `PacketIo` instead blocks via
  `tokio::runtime::Handle::current().block_on` — required because a
  Tokio-backed `AsyncDevice`'s readiness is only ever delivered by Tokio's
  own I/O driver, which `futures::executor::block_on` never polls (a real
  `send()` call hangs forever otherwise; see the "Runtime requirement"
  caveat below). Either way, this crate also implements the native
  `AsyncPacketIo` directly on the same handle, reporting
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

### `tokio` requires a multi-threaded runtime

With the `tokio` feature, every call into `TunRsDevice` — including plain
`PacketIo::recv`/`send`, not only the async API — must happen while a
**multi-threaded** Tokio runtime is entered on the calling thread
(`#[tokio::main]`'s default flavor, or `Builder::new_multi_thread()`
explicitly). `tokio::runtime::Handle::block_on` only drives that runtime's
I/O reactor on the `multi_thread` flavor, whose worker threads poll it
independently of where `block_on` is called from; on `current_thread`, only
`Runtime::block_on` (called on the owned `Runtime` value, not a `Handle`)
drives it, so a `current_thread` runtime
(`#[tokio::main(flavor = "current_thread")]`) hangs the first `recv`/`send`
call forever. This was confirmed with an isolated repro against `tun-rs`
directly (not a bug specific to this crate) and is exercised by this crate's
`privileged_tests`. Prefer the `async-io` feature instead if a
single-threaded runtime is a hard requirement.

## Windows requires `wintun.dll`

`tun_rs::DeviceBuilder::build_sync`/`build_async` for a TUN device (the kind
this crate's `privileged_tests` and the facade's Quick Start both use) loads
`wintun.dll` at runtime on Windows — `tun-rs` does not link or vendor it.
Without it present, `TunRsBackend::open` fails with a generic
`Error::Platform(PlatformErrorCode::Windows(0))`: `raw_os_error()` returns
`None` because "DLL not found" isn't a Win32 error at all, so this crate's
`io_error` mapping falls back to `0` — confirmed on a real CI run before the
fix below, not a hypothetical failure mode.

Download the matching architecture's `wintun.dll` from
[wintun.net](https://www.wintun.net/) and place it next to your
application's executable, or anywhere on `PATH`. This project's own CI
downloads it at job time (see `.github/workflows/ci.yml`'s `privileged`
job) rather than committing the binary to the repository. TAP mode instead
needs the separate [tap-windows](https://build.openvpn.net/downloads/releases/)
driver installed — see `tun-rs`'s own README for details neither this crate
nor `tunnel-lattice` re-derives.

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
