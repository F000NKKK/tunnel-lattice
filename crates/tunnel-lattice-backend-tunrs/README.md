<div align="center">

# ⚙️ tunnel-lattice-backend-tunrs

### The `tun-rs`-Backed TUN/TAP Backend for Tunnel Lattice

[![crates.io](https://img.shields.io/crates/v/tunnel-lattice-backend-tunrs.svg?cacheSeconds=86400)](https://crates.io/crates/tunnel-lattice-backend-tunrs)
[![docs.rs](https://img.shields.io/docsrs/tunnel-lattice-backend-tunrs?cacheSeconds=86400)](https://docs.rs/tunnel-lattice-backend-tunrs)
[![License: MPL 2.0](https://img.shields.io/badge/license-MPL--2.0-blue.svg)](https://github.com/F000NKKK/tunnel-lattice/blob/main/LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.93-lightgrey.svg)](https://github.com/F000NKKK/tunnel-lattice)

![Linux](https://img.shields.io/badge/Linux-supported-success)
![Windows](https://img.shields.io/badge/Windows-supported-success)
![macOS](https://img.shields.io/badge/macOS-supported-success)

[Overview](#-overview) • [Platforms](#-supported-platforms) • [Installation](#-installation) • [Errors](#-error-mapping) • [Receiving](#-receiving-packets) • [Opening](#-opening-a-device) • [Windows](#-windows-requires-wintundll)

</div>

---

## 📖 Overview

The cross-platform TUN/TAP backend of
[Tunnel Lattice](https://github.com/F000NKKK/tunnel-lattice), built on
[`tun-rs`](https://github.com/tun-rs/tun-rs). It implements
`tunnel-lattice-platform`'s provider traits and turns every `tun-rs`/OS
error into a typed `tunnel_lattice_core::Error`.

> Not used directly: the
> [`tunnel-lattice`](https://crates.io/crates/tunnel-lattice) facade selects
> this backend by default through its `tun-rs` feature.

### 🎯 What It Adds on Top of `tun-rs`

- **🧭 Typed errors**: every native failure is mapped by platform-specific
  rules to `Disconnected`, `DriverUnavailable`, `AlreadyExists`,
  `BufferTooSmall`, and so on (tables below).
- **🛑 Device removal always ends `recv`**, including the Linux Tokio and
  async macOS TAP cases where `tun-rs`'s own wait never returns.
- **📦 No truncation**: an oversize packet is reported, not cut.
- **🎯 Honest open**: names the OS would not honor exactly are rejected up
  front, and an existing interface is never adopted and then destroyed.

## 🌟 Key Features

- ✅ `TunRsBackend`, a stateless handle whose `DeviceProvider::open` builds a
  device via `tun_rs::DeviceBuilder`, mapping `DeviceKind::Tun`/`Tap` to
  `tun_rs::Layer::L3`/`L2`;
- ✅ `TunRsBackend`'s own `CapabilityProvider`: what the host supports
  before any device is opened (see "Host-level capabilities" below);
- ✅ `TunRsDevice`, the open-device handle implementing `PacketIo`,
  `DeviceObserver`, `DeviceMutator` (MTU, TAP MAC address, and
  administrative state), and
  `CapabilityProvider`. It holds exactly one underlying `tun-rs` handle —
  never both a sync and an async one, since `tun_rs::SyncDevice::try_clone`
  only exists on Linux. With `async-io`, that handle is a
  `tun_rs::AsyncDevice` and `PacketIo` blocks on its async methods via
  `futures::executor::block_on`. With `tokio`, `PacketIo` instead blocks via
  `tokio::runtime::Handle::current().block_on` — required because a
  Tokio-backed `AsyncDevice`'s readiness is only ever delivered by Tokio's
  own I/O driver, which `futures::executor::block_on` never polls (a real
  `send()` call hangs forever otherwise; see "`tokio` requires a
  multi-threaded runtime" below). Either way, this crate also implements
  the native `AsyncPacketIo` directly on the same handle, reporting
  `Capability::NATIVE_ASYNC`. Without either feature, the handle is a
  `tun_rs::SyncDevice` and `PacketIo` calls it directly.
- ✅ Administrative-state read-back (`DeviceObserver::snapshot`'s
  `AdminState`) is exact only on Linux (`tun_rs`'s `is_running`); macOS,
  Windows, and BSD expose only a write-only `enabled(bool)` setter with no
  corresponding getter, so `snapshot()` reports `AdminState::Unknown` there.
- 🐧 `PersistentDevice`/`MultiQueueProvider`, both Linux-only: `TunRsDevice`
  implements them on Linux and does not implement them at all elsewhere
  (verified in `tun-rs`'s source — the underlying `persist`/`multi_queue`/
  `try_clone` methods are `#[cfg(target_os = "linux")]` there too, not
  merely no-ops off Linux). See "Persistent devices and multi-queue" below.
- ✅ A TAP device's MAC address: `DeviceConfig::with_mac` sets it at open
  on every platform, and the backend reads it back, returning
  `Unsupported` and tearing the device down if it was not applied.
  `TunRsDevice` reports `Capability::MAC_MUTATION` on a TAP handle on
  Linux and macOS, where a `DeviceConfigPatch` can change it later; on
  Windows the tap-windows6 driver takes the address only when the adapter
  is created.

## 💻 Supported Platforms

| Platform    | TUN | TAP | Sync | Tokio | async-io | Mechanism |
|-------------|:---:|:---:|:----:|:-----:|:--------:|-----------|
| **Linux**   | ✅  | ✅  | ✅   | ✅    | ✅       | `/dev/net/tun`; persistence and multi-queue |
| **Windows** | ✅  | ⚠️  | ✅   | ✅    | ✅       | TUN via Wintun (`wintun.dll`), TAP via tap-windows6 |
| **macOS**   | ✅  | ✅  | ✅   | ✅    | ✅       | TUN via `utun`, TAP via `feth` pairs and BPF |

✅ tested in CI on real devices. ⚠️ supported, but only the missing-driver
path is tested in CI so far. Administrative-state read-back is exact only
on Linux; elsewhere `snapshot()` reports `AdminState::Unknown`.

## 🧩 Host-level capabilities

`TunRsBackend` implements `CapabilityProvider` itself, answering before any
device is opened (the `tunnel-lattice` facade exposes it as
`Tunnel::capabilities()`):

| Flag                                 | Reported                                                     |
|--------------------------------------|--------------------------------------------------------------|
| `DEVICE_MUTATION`                    | always                                                       |
| `TAP_DEVICES`                        | Linux and macOS always; Windows only if the tap-windows6 driver (hardware id `tap0901`) is installed |
| `PERSISTENT_DEVICES`, `MULTI_QUEUE`  | Linux                                                        |
| `NATIVE_ASYNC`                       | with the `async-io` or `tokio` feature                       |

`MAC_MUTATION` is never in this answer: it belongs to an open TAP handle,
whose `TunRsDevice::capabilities()` reports the backend's answer plus
`MAC_MUTATION` (Linux and macOS) and `TAP_DEVICES`.

On Windows the driver is detected with the same SetupAPI driver lookup
`tun-rs` performs before creating a TAP adapter, stopping before anything is
registered: no adapter is created and no Administrator rights are needed.
It runs on the first call and is cached for the life of the process, so a
driver installed later is noticed only after a restart; a failed lookup
leaves the flag out. The answer is advisory: `open` is never refused because
of it, and still reports a missing driver as `Error::DriverUnavailable`.

```rust
use tunnel_lattice_backend_tunrs::TunRsBackend;
use tunnel_lattice_platform::{Capability, CapabilityProvider};

let host = TunRsBackend::new().capabilities();
if host.contains(Capability::TAP_DEVICES) {
    // a TAP device can be requested on this host
}
```

## 📦 Installation

```toml
[dependencies]
# Synchronous backend
tunnel-lattice-backend-tunrs = "0.4"

# With the native async path on Tokio or async-io (mutually exclusive)
tunnel-lattice-backend-tunrs = { version = "0.4", features = ["tokio"] }
tunnel-lattice-backend-tunrs = { version = "0.4", features = ["async-io"] }
```

## 🧭 Error mapping

Every `tun-rs`/OS `io::Error` is mapped onto `tunnel_lattice_core::Error`
by its portable `io::ErrorKind` first, so callers can match on typed
variants instead of raw platform codes (during `open`, the rules under
"Opening a device" below are applied before this table; during `recv` and
`send`, the rules under "Receiving packets" below are):

| `io::ErrorKind`                               | `Error`            |
|-----------------------------------------------|--------------------|
| `PermissionDenied`                            | `PermissionDenied` |
| `NotFound`                                    | `NotFound`         |
| `AlreadyExists`                               | `AlreadyExists`    |
| `BrokenPipe`, `UnexpectedEof`, `NotConnected` | `Disconnected`     |
| `Unsupported`                                 | `Unsupported`      |
| anything else, with a raw OS code             | `Platform(code)`   |
| anything else, without a raw OS code          | `Platform(PlatformErrorCode::Unknown)` |

`std` derives these kinds from native codes on every platform (Linux/macOS
`EPERM`/`EACCES` and Windows `ERROR_ACCESS_DENIED` all become
`PermissionDenied`), so opening a device without the required privilege
reports `Error::PermissionDenied` rather than a raw errno. The typed
variants do not carry the raw OS code; `Error::Platform` remains the
diagnostic fallback for every kind without a typed counterpart.

`code` is tagged `PlatformErrorCode::Linux`/`Windows`/`Darwin` by the
current target. An error with no raw OS code at all (for example one
`tun-rs` builds itself with `io::Error::other`) becomes
`PlatformErrorCode::Unknown` rather than a fabricated `0` code, and on a
target other than Linux, Windows, or macOS every unmapped error is
`PlatformErrorCode::Unknown`. `PlatformErrorCode` is `#[non_exhaustive]`,
so a `match` on it needs a wildcard arm.

`TunRsBackend` is `#[non_exhaustive]` as well: construct it with
`TunRsBackend::new()` or `TunRsBackend::default()`.

## 📥 Receiving packets

`PacketIo::recv` and `AsyncPacketIo::recv` never truncate a packet. A
packet larger than the caller's buffer is discarded and reported as
`Error::BufferTooSmall`; the device stays usable and the next `recv`
returns the next packet. A buffer of `Device::recv_buffer_len()` bytes (the
MTU for TUN, MTU + 18 for TAP) is large enough at the device's current
MTU, except for a double-tagged (QinQ) TAP frame.

How the oversize case is detected depends on the platform:

| OS / kind          | Mechanism                                                                                   |
|--------------------|---------------------------------------------------------------------------------------------|
| Linux TUN/TAP      | a scatter read into the buffer plus one spare byte; a read that reaches the spare byte did not fit |
| macOS TUN (`utun`) | the same scatter read                                                                       |
| macOS TAP (`feth`) | `tun-rs` reports `io::ErrorKind::InvalidData` without an OS code                            |
| Windows TUN/TAP    | `tun-rs` reports `io::ErrorKind::InvalidInput` without an OS code                           |

On Windows and macOS TAP only an error with no OS code is treated as a
too-small buffer, so a real OS error of the same kind still maps to
`Error::Platform`.

Two conditions are retried inside `recv` instead of being returned:

- `EINTR` (a signal interrupted the read) on Linux and macOS. `send`
  retries it too. An `io::ErrorKind::Interrupted` without the raw `EINTR`
  code is not retried;
- on macOS TAP, the transient empty read that `tun-rs` reports as
  `io::ErrorKind::UnexpectedEof` with the exact message
  `"recv buffer is empty"`. The message is pinned against `tun-rs` 2.8.11;
  a `tun-rs` release that changes it makes that read return
  `Error::Disconnected` instead of being retried.

Every other `UnexpectedEof` still maps to `Error::Disconnected`, including
`tun-rs`'s `"close"` error on a closed macOS device and Windows'
`ERROR_HANDLE_EOF` when the adapter goes away.

### When the device goes away

`recv` and `send` also map the errors that mean the device was deleted or
disabled underneath the handle, before the general table above:

| OS                   | Native error                                                                  | `Error`         |
|----------------------|-------------------------------------------------------------------------------|-----------------|
| macOS                | `ENXIO`: the TAP's BPF descriptor after the peer `feth` it is bound to was destroyed | `Disconnected`  |
| Linux                | `EBADFD`: the device was deleted and its queue detached                       | `Disconnected`  |
| Linux (`recv`)       | `EFAULT`: a blocking read already waiting when the device was deleted         | `Disconnected`  |
| Linux                | `ENXIO` (mapped for symmetry with macOS; the Linux tun driver does not return it on read or write) | `Disconnected` |
| Windows TUN (`send`) | Wintun reports the adapter terminating, which `tun-rs` returns as `io::ErrorKind::WriteZero` without an OS code | `Disconnected` |
| Windows TUN          | `"The interface has been disabled"` (no OS code; exact message pinned against `tun-rs` 2.8.11), after the adapter was disabled, for example by applying `DesiredAdminState::Down` | `InvalidState` |

The disabled Wintun adapter is not `Disconnected`: applying
`DesiredAdminState::Up` starts a new session and the same handle works
again. The rules match raw OS codes only on the platform they belong to
(code 6 is `ENXIO` on Linux and macOS but a different error on Windows;
macOS has no `EBADFD`), and `WriteZero` and `EFAULT` are remapped on one
direction only (`send` and `recv` respectively).

On Linux, a blocking `recv` that is already waiting when the device is
deleted is woken by the kernel with `EFAULT` rather than `EBADFD`; later
calls get `EBADFD`. Both are `Disconnected`, so deleting a Linux device
ends `recv` with `Disconnected` in every feature set. With the `tokio`
feature on Linux, `recv` waits through a private duplicate of the device
descriptor registered with Tokio for readable and error readiness: a
deleted device reports error readiness only, which the readable-only wait
`tun-rs` uses would never see. The duplicate costs one extra file
descriptor per handle (per queue) and closes with the handle.

On macOS, destroying the peer `feth` of a TAP device ends `recv` with
`Disconnected` in every feature set too. Without an async feature, the
blocking BPF read is woken with `ENXIO`. With `async-io` or `tokio`, the
wait needs help: macOS readiness notification for BPF does not report the
interface going away, so `tun-rs`'s own wait would never end. A TAP
`recv` therefore waits for the BPF descriptor itself, on a blocking-pool
thread, in waits of at most 250 ms, and at each timeout checks that the
descriptor is still bound to its interface; once it is not, the next read
returns `ENXIO`, so a pending `recv` ends within about 250 ms. `tun-rs`
still reads every packet. This costs one extra file descriptor per TAP
handle, closed with the handle, and a pipe per pending wait, released as
soon as the wait ends or its `recv` is dropped. Sending and macOS TUN
(`utun`) devices are unchanged.

`tunnel-lattice-async`'s `PacketStream` (and the facade's
`Handle::packet_stream`) ends after any error except `BufferTooSmall`,
so a stream over a deleted or disabled device yields one error and ends;
create a new stream after recovering. The crate's privileged tests delete
the device while a stream or a blocking `recv` is waiting (Linux `ip link
del`, macOS `ifconfig <peer feth> destroy`, Windows adapter disable) and
check that it ends.

## 🚪 Opening a device

### Name and MTU prechecks

`DeviceProvider::open` rejects a requested name or MTU it cannot honor
exactly with `Error::InvalidState`, before any native call (no device is
created or touched):

| OS           | Accepted name                                                                 |
|--------------|-------------------------------------------------------------------------------|
| all          | non-empty, no NUL character                                                   |
| Linux        | at most 15 bytes (`IFNAMSIZ` minus the NUL), no `%`                           |
| macOS TAP    | `feth<N>`, `N` a decimal number from 0 to 32767 with no sign or leading zero |
| macOS TUN    | `utun<N>`, `N` a decimal number below `u32::MAX` with no sign or leading zero, at most 15 bytes |
| Windows      | at most 255 UTF-16 code units (TUN and TAP)                                   |

Two of these rules exist because the name would otherwise not be honored
exactly: on Linux the kernel treats a `%d` in the name as a naming template
(`tl%d` would open as `tl0`), so any name containing `%` is rejected; on
macOS a bare `feth` is `tun-rs`'s own auto-naming request (the kernel would
pick the unit), so a TAP name must carry an explicit unit number. That unit
is capped at 32767, the macOS kernel's highest `feth` unit: larger units
fail natively, and `feth4294967295` is the kernel's own "pick any unit"
wildcard, so it would also open under a different name. Leave `name` unset
to let the OS choose.

An MTU above `u16::MAX` is rejected the same way.

### `open`-specific error mapping

A native failure during `open` is first checked against these rules; only
unmatched errors go through the general table above:

| OS / kind     | Native failure                                                    | `Error`             |
|---------------|-------------------------------------------------------------------|---------------------|
| Windows TUN   | `wintun.dll` could not be loaded, or lacks a required function    | `DriverUnavailable` |
| Windows TAP   | no `tap0901` (tap-windows6) driver installed                      | `DriverUnavailable` |
| Windows TAP   | an adapter with the requested name already exists                 | `AlreadyExists`     |
| Linux         | `ENODEV`/`ENOENT`: the `tun` module or `/dev/net/tun` is missing  | `DriverUnavailable` |
| Linux         | `EINVAL`/`EBUSY` while an interface with the requested name exists | `AlreadyExists`    |
| macOS         | `EBUSY` while an interface with the requested name exists          | `AlreadyExists`    |

`Error::DriverUnavailable` is returned only by `open`; installing or
loading the missing driver makes the same call succeed.

The Windows rules depend on `tun-rs` internals, written against `tun-rs`
2.8.11 (this crate's minimum `tun-rs` version):

- a missing `wintun.dll` is recognized by downcasting the
  `libloading::Error` that `tun-rs` wraps (it carries no OS error code, and
  its message is localized). This crate therefore depends on `libloading`
  on Windows, and that dependency must stay on the same major version as
  `tun-rs`'s own (0.9); the workspace CI fails if two versions appear;
- the TAP driver and existing-adapter cases are recognized by the exact
  messages `tun-rs` produces (`"No driver found"`, and
  `"The network adapter [<name>] already exists."`).

A newer `tun-rs` 2.x is allowed but is not checked by this crate's unit
tests, which only guard this crate's own copies of those strings. A
`tun-rs` release that changes the wintun load failure or the "no driver"
message is caught by two ignored Windows tests (`open(Tun)` without
`wintun.dll` and `open(Tap)` without the tap-windows6 driver must both
report `DriverUnavailable`), which the repository's privileged CI job runs
before installing any driver. The "adapter already exists" message has no
such end-to-end check.

### Existing interface names

`open` never adopts an existing interface and then destroys it on drop:

| OS / kind          | Existing interface with the requested name                         | Result |
|--------------------|--------------------------------------------------------------------|--------|
| Linux TUN/TAP      | other kind, multi-queue mismatch, or a non-multi-queue device that already has a queue attached | `AlreadyExists` |
| Linux TUN/TAP      | a persistent same-kind device with no queue attached               | re-attached; not deleted on drop |
| Linux, `multi_queue = true` | a same-kind multi-queue device, including a live one opened by another process | a queue is attached; not deleted on drop |
| macOS TAP (`feth`) | any                                                                | `AlreadyExists`; the existing `feth` survives |
| macOS TUN (`utun`) | the unit is in use                                                 | `AlreadyExists` |
| Windows TAP        | any adapter with that name                                         | `AlreadyExists`; the adapter is untouched |
| Windows TUN        | a Wintun adapter                                                   | adopted; not deleted on drop |
| Windows TUN        | a non-Wintun adapter                                               | `Platform(Windows(code))` |

On macOS and Windows this relies on disabling `tun-rs`'s `reuse_dev`
default for TAP devices; with it enabled, `tun-rs` would adopt the existing
TAP interface and, on macOS, destroy it when the handle drops. The two
attach cases (Linux, Windows TUN) are documented rather than refused: a
check before opening would race with other processes, and neither case
deletes the interface. On Linux, joining a multi-queue device owned by
another user requires `CAP_NET_ADMIN` in the device's network namespace.
Re-attaching to an existing persistent Linux device with an MTU the kernel
rejects (`EINVAL` from setting the MTU) is reported as `AlreadyExists`,
because the name exists — the same limitation as the other Linux `EINVAL`
case above.

## 🎛️ Feature flags

`async-io` and `tokio` are mutually exclusive — they select `tun-rs`'s own
two async backends (`tun-rs/async_io`, no runtime dependency beyond
`async-io`/`blocking`; `tun-rs/async_tokio`, which pulls in tokio).
Enabling both is a compile error from `tun-rs` itself, not from this crate.
On macOS, either one also makes this crate depend on `blocking` directly,
for the TAP receive wait described in "When the device goes away"; `tun-rs`
already depends on it with both, so no new package enters the build.

### `tokio` requires a multi-threaded runtime

With the `tokio` feature, every call into `TunRsDevice` must happen while a
Tokio runtime is entered on the calling thread: building the
`AsyncDevice` registers it with that runtime. The blocking
`PacketIo::recv`/`send` additionally need that runtime to be
**multi-threaded** (`#[tokio::main]`'s default flavor, or
`Builder::new_multi_thread()` explicitly).
`tokio::runtime::Handle::block_on` only drives that runtime's I/O reactor
on the `multi_thread` flavor, whose worker threads poll it independently of
where `block_on` is called from; on `current_thread`, only
`Runtime::block_on` (called on the owned `Runtime` value, not a `Handle`)
drives it, so on a `current_thread` runtime
(`#[tokio::main(flavor = "current_thread")]`) the first blocking
`recv`/`send` call hangs forever. This was confirmed with an isolated repro
against `tun-rs` directly (not a bug specific to this crate) and is
exercised by this crate's `privileged_tests`.

The blocking `PacketIo::recv`/`send` must also not be called from async
code. With `tokio`, `Handle::current()` panics outside a runtime context
and `Handle::block_on` panics inside an asynchronous context (a task,
`#[tokio::main]`, or `block_on`); with `async-io`,
`futures::executor::block_on` parks the executor thread that polls the
calling task. Inside async code use `AsyncPacketIo` instead — through the
`tunnel-lattice` facade, `Handle::send_async` and `Handle::packet_stream`.
Their futures are polled by the runtime itself rather than blocked on, so
`send_async` also works on a `current_thread` Tokio runtime. Prefer the
`async-io` feature if single-threaded blocking calls are a hard
requirement.

## 🪟 Windows requires `wintun.dll`

`tun_rs::DeviceBuilder::build_sync`/`build_async` for a TUN device (the kind
this crate's `privileged_tests` and the facade's Quick Start both use) loads
`wintun.dll` at runtime on Windows — `tun-rs` does not link or vendor it.
Without it present (or with a `wintun.dll` missing a required function),
`TunRsBackend::open` fails with `Error::DriverUnavailable`. `tun-rs` wraps
the DLL loader's own error rather than a Win32 error code, so there is no
OS code to report; this crate recognizes the wrapped loader error instead
(see "`open`-specific error mapping" above). Releases up to 0.4 reported
this case as `Error::Platform(PlatformErrorCode::Windows(0))`.

Download the matching architecture's `wintun.dll` from
[wintun.net](https://www.wintun.net/) and place it next to your
application's executable, or anywhere on `PATH`. This project's own CI
downloads it at job time (see `.github/workflows/ci.yml`'s `privileged`
job) rather than committing the binary to the repository. TAP mode instead
needs the separate [tap-windows](https://build.openvpn.net/downloads/releases/)
driver installed (without it, `open` for a TAP device also fails with
`Error::DriverUnavailable`) — see `tun-rs`'s own README for details neither
this crate nor `tunnel-lattice` re-derives.

## 🔀 Persistent devices and multi-queue

Both gated by `Capability::PERSISTENT_DEVICES`/`Capability::MULTI_QUEUE`,
Linux-only:

- `Handle::persist` marks an open device to survive process exit
  (`TUNSETPERSIST`). There is no un-persist — `tun-rs` only exposes setting
  the flag.
- `DeviceConfig::with_multi_queue(true)` requests `IFF_MULTI_QUEUE` at open
  time; `Handle::additional_queue` then duplicates a genuinely independent,
  hardware-scheduled queue on the same device (`tun-rs`'s `try_clone`).
  Calling it on a device that wasn't opened with multi-queue requested
  returns `Error::Unsupported` (mapped from `tun-rs`'s own
  `io::ErrorKind::Unsupported`, not a raw platform error code).

Multi-queue is a throughput optimization, not a prerequisite for concurrent
access: `PacketIo::recv`/`send` take `&self` on every platform this crate
supports, so sharing one `Handle` clone across threads and calling them
concurrently is always safe — see `tunnel_lattice::Handle`'s rustdoc and
`ARCHITECTURE.md`'s "Ownership and concurrency contract."

## 🏗️ Why this is one shared crate, not three

`tun-rs` already abstracts Linux/Windows/macOS/BSD TUN/TAP differences
internally, so unlike `net-lattice`'s per-OS backend split there is no
platform-specific Rust code to isolate here yet. See the workspace
`ARCHITECTURE.md`, "Backend replacement plan," for how a future
per-OS backend (bypassing `tun-rs` entirely) would slot in alongside this
crate behind the same `tunnel-lattice-platform` traits.

## 📖 Documentation

- **API reference**: [docs.rs/tunnel-lattice-backend-tunrs](https://docs.rs/tunnel-lattice-backend-tunrs)
- **Facade**: [`tunnel-lattice`](https://crates.io/crates/tunnel-lattice), which selects this backend by default
- **Project**: [github.com/F000NKKK/tunnel-lattice](https://github.com/F000NKKK/tunnel-lattice)

## 📄 License

Licensed under the [Mozilla Public License 2.0](https://github.com/F000NKKK/tunnel-lattice/blob/main/LICENSE).

## 🌟 Acknowledgments

Built on [`tun-rs`](https://github.com/tun-rs/tun-rs) and, on Windows,
[Wintun](https://www.wintun.net/).
