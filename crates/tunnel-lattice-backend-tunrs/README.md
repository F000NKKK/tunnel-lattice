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
- `PersistentDevice`/`MultiQueueProvider`, both Linux-only: `TunRsDevice`
  implements them on Linux and does not implement them at all elsewhere
  (verified in `tun-rs`'s source — the underlying `persist`/`multi_queue`/
  `try_clone` methods are `#[cfg(target_os = "linux")]` there too, not
  merely no-ops off Linux). See "Persistent devices and multi-queue" below.

## Error mapping

Every `tun-rs`/OS `io::Error` is mapped onto `tunnel_lattice_core::Error`
by its portable `io::ErrorKind` first, so callers can match on typed
variants instead of raw platform codes (during `open`, the rules under
"Opening a device" below are applied before this table):

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

## Opening a device

### Name and MTU prechecks

`DeviceProvider::open` rejects a requested name or MTU it cannot honor
exactly with `Error::InvalidState`, before any native call (no device is
created or touched):

| OS           | Accepted name                                                                 |
|--------------|-------------------------------------------------------------------------------|
| all          | non-empty, no NUL character                                                   |
| Linux        | at most 15 bytes (`IFNAMSIZ` minus the NUL), no `%`                           |
| macOS TAP    | `feth<N>`, `N` a decimal number that fits in a `u32` with no sign or leading zero, at most 15 bytes |
| macOS TUN    | `utun<N>`, `N` a decimal number below `u32::MAX` with no sign or leading zero, at most 15 bytes |
| Windows      | at most 255 UTF-16 code units (TUN and TAP)                                   |

Two of these rules exist because the name would otherwise not be honored
exactly: on Linux the kernel treats a `%d` in the name as a naming template
(`tl%d` would open as `tl0`), so any name containing `%` is rejected; on
macOS a bare `feth` is `tun-rs`'s own auto-naming request (the kernel would
pick the unit), so a TAP name must carry an explicit unit number. Leave
`name` unset to let the OS choose.

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

## Persistent devices and multi-queue

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
