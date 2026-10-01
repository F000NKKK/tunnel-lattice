# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

- **iperf3 forwarder benchmark (unpublished, Linux).** `bench/forwarder/`
  forwards packets between two TUN devices (one in a network namespace)
  while `iperf3` measures TCP throughput across them, the method of tun-rs's
  tun-benchmark2. Each run measures raw `tun-rs` baselines (sync, Tokio,
  async-io) and the `tunnel-lattice` facade (`Handle::recv`/`send`;
  `packet_stream` or a shared `PacketPool` with `send_async`) in the same
  repetitions, and reports throughput, the paired per-repetition ratio to
  the baseline, forwarder CPU and RSS, and TCP retransmissions as
  `results.json` plus a Markdown table. `scripts/bench-forward.sh`
  builds unprivileged, runs as root with cleanup of the devices, namespace
  and route on every exit path, and reports; a manual `Forwarder benchmark`
  GitHub Actions workflow runs it and uploads the results. A standalone
  Cargo workspace, so no published crate or root-workspace build changes.

- **Windows TAP is now tested on real devices in CI.** The privileged CI
  job stages the tap-windows6 9.27.0 driver package (SHA-256 and catalog
  signature checked, `pnputil /add-driver`, no adapter created in advance)
  after the missing-driver tests and runs Windows TAP tests in every
  feature set: the driver lookup and `TAP_DEVICES`, open and snapshot (a
  MAC is reported, `MAC_MUTATION` is not), a MAC set at creation that
  reads back while a later MAC patch returns `Unsupported`, MTU changes
  read back, sending a 42-byte frame, an oversize receive reported as
  `BufferTooSmall`, and `AlreadyExists` for an existing adapter name. The
  job also fails if a TAP adapter is left behind. The platform tables now
  mark Windows TAP as tested.
  - `TunRsDevice::apply` no longer runs the Windows driver lookup: whether
    a handle can change its MAC is decided from its kind and the OS. The
    lookup itself now also ignores a driver whose version is 0, as `tun-rs`
    does.
  - Documentation only: `TunRsBackend`'s "Opening a device" now describes
    the MAC precheck and read-back on `open`, and `DeviceConfig::mac` notes
    that on Linux opening an existing persistent TAP device with a MAC
    changes that device's MAC.

- **Async send on the facade: `Handle::send_async`.** With the `async-io`
  or `tokio` feature, `Handle::send_async(&buf)` returns the device's own
  `AsyncPacketIo::send` future, declared `Send`, so an async forwarder can
  be written against the facade alone (`packet_stream` to receive,
  `send_async` to send; a received `PacketBuf` is sent without a copy).
  Previously the only facade send was the blocking `Handle::send`, which
  with `tokio` panics inside a task ("Cannot start a runtime from within a
  runtime") and hangs on a `current_thread` runtime, and with `async-io`
  parks the executor thread. `send_async` works on a `current_thread`
  Tokio runtime too. Additive: no existing signature or behaviour changes.
  - Same results and errors as `Handle::send` in the same build; no
    capability check, fallback, or mapping is added. A backend that does
    not report `NATIVE_ASYNC` may return a future that blocks the polling
    thread.
  - Dropping the future never sends part of a packet. With the `tun-rs`
    backend a send dropped before completion sent nothing on Linux and
    macOS, and may or may not have been sent on Windows. The portable
    contract, "the whole packet at most once; unknown after drop", is now
    also documented as a backend obligation on `AsyncPacketIo::send`.
  - Documentation only: `Handle::send`/`recv` and `TunRsDevice` now state
    that with `tokio` the blocking calls panic outside a runtime context or
    inside async code, and that with `async-io` they block the executor
    thread.
  - CI's privileged job now also runs the facade's ignored tests (async
    feature sets, Linux, macOS, and Windows), which open a TUN device and
    send with `send_async` from a task on a multi-threaded Tokio runtime,
    on a `current_thread` Tokio runtime, and under a foreign executor with
    `async-io`.

- **Host-level capabilities from the `tun-rs` backend, with an honest
  `TAP_DEVICES` on Windows.** `TunRsBackend` now implements
  `CapabilityProvider`, so `Tunnel::connect().capabilities()` reports what
  the host supports before any device is opened: `DEVICE_MUTATION`,
  `TAP_DEVICES` on Linux and macOS, `PERSISTENT_DEVICES` and `MULTI_QUEUE`
  on Linux, and `NATIVE_ASYNC` with an async feature. It never includes
  `MAC_MUTATION`, which stays a per-handle flag of an open TAP device.
  - On Windows, `TAP_DEVICES` is reported only if the tap-windows6 driver
    (hardware id `tap0901`) is installed. Previously it was reported
    unconditionally, on every device handle. The driver is detected on the
    first call with the same SetupAPI driver lookup `tun-rs` performs
    before creating a TAP adapter, stopping before anything is registered:
    no adapter is created and no elevation is needed. The answer is cached
    for the life of the process.
  - An open device's `capabilities()` is now the backend's host-level
    answer plus its per-handle flags (`MAC_MUTATION` on a TAP handle on
    Linux and macOS; `TAP_DEVICES` on any TAP handle).
  - `open` stays authoritative: it is never refused because of this answer,
    and a missing driver is still reported by `open` as
    `Error::DriverUnavailable`.
  - `tunnel-lattice-backend-tunrs` gains a Windows-only `windows-sys`
    dependency, already in the dependency graph through `tun-rs`.

- **TAP MAC address.** Added `MacAddress`, a six-octet value shaped like
  `net-lattice-model`'s own so the two convert through `[u8; 6]`; the
  facade re-exports it.
  - `Device::mac` reports a TAP device's MAC address (`None` for TUN).
  - `DeviceConfig::with_mac` requests one at open. A MAC for a TUN device
    returns `InvalidState` before anything is created. The
    `tunnel-lattice-backend-tunrs` backend reads the address back after
    opening and returns `Unsupported`, tearing the new device down, if the
    platform did not apply it.
  - `DeviceConfigPatch::new_mac` and `DeviceConfigPatch::with_mac` change
    it later through `DeviceMutator::apply`, which now applies the MTU,
    then the MAC address, then the administrative state, and reverts the
    earlier steps in reverse order if a later one fails. A MAC in a patch
    for a TUN device returns `InvalidState`.
  - Added `Capability::MAC_MUTATION`. `tunnel-lattice-backend-tunrs`
    reports it on a TAP handle on Linux and macOS. On Windows the
    tap-windows6 driver takes the address only when the adapter is
    created, so a patch with a MAC returns `Unsupported` there.

- **Breaking (behavioral): `DeviceMutator::apply` rejects a patch for
  another device and reverts a partly applied patch.** The contract is now
  written on the trait and binds every backend.
  - All preconditions are checked before any native call, and nothing
    changes when one fails. A patch whose device identifier differs from
    the handle's `DeviceObserver::id()` returns `InvalidState`; previously
    it was applied to whichever device the handle wrapped. An MTU above
    `u16::MAX` returns `InvalidState`, then a setting the backend cannot
    apply returns `Unsupported`.
  - The MTU is applied before the administrative state. When a patch sets
    both and changing the administrative state fails,
    `tunnel-lattice-backend-tunrs` restores the previous MTU on a
    best-effort basis and returns the original error. Previously the new
    MTU stayed in place. A failed revert is not reported: after any `Err`,
    `snapshot()` is authoritative.

- **Breaking: `DeviceObserver` gains a required `id()` method, and the
  facade can wrap any backend.** `id()` returns the device identity
  captured when the device was opened, without a native call; the record
  returned by `snapshot()` must carry the same identity. A backend
  implemented outside this workspace must add the method.
  - `tunnel-lattice-backend-tunrs` reads the interface index once at
    `open` and keeps it. `snapshot().id` no longer re-reads the index, so
    it stays stable if Windows re-indexes the adapter after a disable and
    re-enable. `open` now fails with `Error::Platform` if the OS reports
    index 0, and the new device is torn down. The identity is not
    guaranteed to equal the interface's current OS index; resolve an
    interface by name to use it with `net-lattice`.
  - Added `Tunnel::new(backend)` to wrap any backend, including one
    written outside this workspace or a test double. `Tunnel::connect()` is
    now shorthand for `Tunnel::new(TunRsBackend::new())`.
  - Added `Tunnel::capabilities()`, available when the backend implements
    `CapabilityProvider`. It reports what the host supports before any
    device is opened; `open` is never refused because of it.
  - Added `Handle::id()` and `Handle::kind()`, both without a native call.
    There is deliberately no `Handle::name()`: an interface can be renamed
    outside this process, so read the current name with `snapshot()`.
  - The facade now re-exports `PlatformErrorCode`, `PacketIo`,
    `DeviceProvider`, `DeviceObserver`, `DeviceMutator`, `TunRsBackend` and
    `TunRsDevice` (the last two with the `tun-rs` feature), and
    `AsyncPacketIo` and `PacketStream` with an async feature, so a custom
    backend needs no direct dependency on the inner crates.

- **Breaking: `PacketStream` yields `Result<PacketBuf>` instead of
  `Result<Vec<u8>>`, and the stream constructors become fallible.** Each
  packet is now received straight into a slot of a `PacketPool` and handed
  out as a `PacketBuf` view (16 bytes on 64-bit targets) that dereferences
  to `[u8]` and returns its slot when dropped, so a steady stream makes no
  heap allocation and no copy per packet in `tunnel-lattice-async`, on both
  paths. Previously the native path allocated a zeroed `buf_len`-byte
  buffer per packet, and the thread bridge copied every packet into a new
  `Vec` and an unbounded-channel node. Code that keeps the bytes past the
  item's lifetime must call `to_vec()`. Each `Ok` item is exactly one
  packet (one IP packet for TUN, one Ethernet frame for TAP), at most the
  pool's `buf_len` long.
  - `tunnel_lattice_async::from_async_device` and `from_device`, and
    `tunnel_lattice::Handle::packet_stream`, now return
    `Result<PacketStream>`. They build the pool with
    `PacketPool::with_buf_len(buf_len)` and return `Err(Error::InvalidState)`
    only if that rejects `buf_len` (zero, or too large for a slot), before
    allocating anything and without touching the device. A zero `buf_len`
    previously produced a stream that reported every packet as
    `BufferTooSmall`. The stream itself never yields `InvalidState` for its
    own reasons. A failed slab allocation aborts, as for a `Vec`, and the
    thread bridge still panics if the OS cannot spawn its worker thread.
  - **Back-pressure:** a stream receives only while its pool has a free
    slot. A consumer that holds every item now pauses its stream (it
    returns `Pending` and stops calling `recv`) until it drops one. The
    thread bridge's queue is bounded by the pool's slot count instead of
    unbounded, and a worker waiting for a slot or for room in the queue now
    exits promptly when the stream is dropped.
  - A thread-bridge worker that panics (for example inside the device's
    `recv`) ends the stream with `None`. A panicking waker called from
    another thread never ends a stream and never aborts the process.
- **Added `from_async_device_with_pool` and `from_device_with_pool` to
  `tunnel-lattice-async`, and `Handle::packet_stream_with_pool` plus
  re-exports of `PacketBuf` and `PacketPool` to `tunnel-lattice` (async
  features; additive):** the same streams over a `PacketPool` the caller
  built, for example to choose the slot count or to share one pool between
  several streams (which then share its slots, with no fairness between
  them). They cannot fail.
- **Breaking (behavioral): `PacketStream` now ends after any error except
  `Error::BufferTooSmall`.** Both `tunnel-lattice-async` variants
  (`from_async_device` and the `from_device` thread bridge), and so
  `Handle::packet_stream`, yield the error once and then end. Previously
  only `Err(Disconnected)` ended a stream and every other error was yielded
  while the stream kept polling, so a device whose `recv` failed
  immediately and repeatedly (a deleted Linux device, a destroyed macOS TAP
  interface, a disabled Wintun adapter) produced an endless stream of error
  items, and the thread bridge's worker kept queueing them. The worker now
  stops calling `recv` and exits right after forwarding the error. Callers
  that relied on a stream surviving an error must create a new stream
  (`Handle::packet_stream` again) after recovering,
  for example after applying `DesiredAdminState::Up` to a disabled
  interface. Polling an ended native stream again now returns `None`
  instead of panicking.
- **`tunnel-lattice-backend-tunrs` maps device deletion and disabling on
  `recv`/`send` (behavioral, no signature change):** raw `ENXIO` on macOS
  (the TAP's BPF descriptor after the peer `feth` it is bound to was
  destroyed) and
  Linux, and raw `EBADFD` on Linux (the device was deleted), are now
  `Error::Disconnected` instead of `Error::Platform(...)`. On Windows, a
  Wintun `send` after the adapter started terminating (`tun-rs` reports
  `WriteZero`) is now `Error::Disconnected`, matching `recv`, instead of
  `Error::Platform(Unknown)`; and the code-less `"The interface has been
  disabled"` error after the Wintun adapter was disabled is now
  `Error::InvalidState` (recoverable by applying `DesiredAdminState::Up`)
  instead of `Error::Platform(Unknown)`. On Linux `recv` only, raw `EFAULT`
  (the kernel's error for a blocking read already waiting when the device
  is deleted) is also `Error::Disconnected` instead of
  `Error::Platform(Linux(14))`, so deleting a Linux device ends `recv` with
  `Disconnected` in every feature set; `EFAULT` on `send` and on macOS keeps
  the generic mapping.
- **Fixed `tunnel-lattice-backend-tunrs` hanging on a deleted Linux device
  under the `tokio` feature:** a pending `recv` (and so a `PacketStream`)
  over a TUN/TAP device deleted with `ip link del` was never woken, because
  the deleted device reports error readiness only and `tun-rs` waits for
  readable readiness alone. On Linux with `tokio`, `recv` now waits through
  a private duplicate of the device descriptor registered for both, and
  ends with `Error::Disconnected`. Each handle (and each additional queue)
  holds one extra file descriptor; `send` is unchanged. The crate's
  `tokio` requirement is raised from `1` to `1.49`, the floor `tun-rs`
  2.8.11 already imposes, so resolved versions do not change.
- **Fixed `tunnel-lattice-backend-tunrs` hanging on a destroyed macOS TAP
  device under `async-io`/`tokio`:** a pending `recv` (and so a
  `PacketStream`) over a TAP device whose peer `feth` was destroyed was
  never woken, because macOS readiness notification for BPF does not
  report the interface going away and `tun-rs` waits on the BPF descriptor
  without a timeout. On macOS with either async feature, a TAP `recv` now
  waits for the descriptor itself on the `blocking` thread pool, in waits
  of at most 250 ms, checking at each timeout that the descriptor is still
  bound to its interface, and ends with `Error::Disconnected` within about
  250 ms of the destroy, as in the sync build. `tun-rs` still reads every
  packet. Each TAP handle holds one extra file descriptor, and each pending
  wait a pipe; `send` and macOS TUN (`utun`) devices are unchanged. The
  crate now depends on `blocking` directly on macOS in async builds;
  `tun-rs` already depends on it there, so no new package enters the build.
- **Added packet-path benchmarks and an allocation-count test to
  `tunnel-lattice-async` (development only, no API change):** `cargo bench
  -p tunnel-lattice-async` measures the synchronous caller-buffer receive
  loop, both pooled `PacketStream` paths (also with the consumer holding
  packets, and over a mixed-size packet load), and a frozen copy of the
  earlier `Vec`-per-packet streams, over in-memory mock devices. `--bench
  pool` measures pool construction and first use, the pool's own
  per-packet overhead, and contention on a shared pool.
  `tests/alloc_count.rs` pins zero heap allocations per packet on both
  paths (including a native stream whose every receive waits for a free
  slot) and bounds the memory a stream owns. `criterion` is a
  dev-dependency only. CI builds the benches on every OS without running
  them.
- **Added `PacketPool` and `PacketBuf` to `tunnel-lattice-async`
  (additive):** `PacketPool::new(slots, buf_len)` and
  `PacketPool::with_buf_len(buf_len)` build a fixed-capacity pool of
  receive slots carved from one zeroed slab that is allocated once
  (`with_buf_len` picks up to 128 slots within a 4 MiB budget). An invalid
  size (zero slots, a zero `buf_len`, or a slab over `u32::MAX` bytes, or
  over `isize::MAX` on 32-bit targets) is rejected with
  `Error::InvalidState` before anything is allocated. A failed
  slab allocation aborts the process, as it does for a `Vec`. `PacketBuf` is
  a 16-byte (on 64-bit targets) view of one packet inside a slot. It
  dereferences to `[u8]`, supports `advance`/`truncate`, and returns its
  slot to the pool on drop. Slots are reused without being re-zeroed (see
  `SECURITY.md`). Only the streams hand out `PacketBuf`s; the pool has no
  public acquire method. CI gains a non-blocking Miri job for the pool's
  unsafe code.
  The receive and release paths are lock-free in the common case: each
  slot has an atomic ownership flag, the pool keeps its own reference count
  paid in batches, and the pool lock is taken only while a stream waits
  for a free slot.
- **Documented the `recv` contract on `PacketIo` and `AsyncPacketIo`
  (rustdoc only, no signature change):** on `Ok(n)` an implementation must
  have written `buf[..n]`, and `n <= buf.len()`.
- **Breaking: added `Error::BufferTooSmall`** (with
  `Error::is_buffer_too_small`, Display "receive buffer too small for the
  packet"). `PacketIo::recv` and `AsyncPacketIo::recv` must never truncate
  a packet: one that does not fit in the caller's buffer is discarded, the
  call returns `Error::BufferTooSmall`, and the device stays usable. The
  contract no longer mentions `Error::InvalidState` for this case.
  `tunnel-lattice-backend-tunrs` previously truncated the packet silently
  on Linux TUN/TAP and macOS TUN, and reported `Error::Platform(Unknown)`
  on macOS TAP and Windows. It now detects the case with a scatter read
  into the buffer plus one spare byte on Linux and macOS, and maps `tun-rs`'s
  code-less `InvalidData` (macOS TAP) and `InvalidInput` (Windows) receive
  errors. `tunnel-lattice-async`'s streams yield `Err(BufferTooSmall)` for
  such a packet and keep receiving (it is the only error that does not end
  a stream; see the stream end-of-life entry above).
  A device that reports more bytes than the buffer holds is also reported
  as `BufferTooSmall` by the streams instead of being sliced.
- **Added `Device::recv_buffer_len` to `tunnel-lattice-model`
  (additive):** a receive buffer size that fits one packet at the
  snapshot's MTU — the MTU for TUN, MTU + 18 for TAP (Ethernet header plus
  one 802.1Q tag; a double-tagged frame does not fit). The
  facade's examples now size buffers with it.
- **Renamed the `mtu` parameter of `Handle::packet_stream`,
  `tunnel_lattice_async::from_async_device`, and `from_device` to
  `buf_len` (source-compatible):** it has always been the per-packet
  buffer size, not the device MTU, and must be `recv_buffer_len()` for a
  TAP device.
- **`tunnel-lattice-backend-tunrs` retries two transient conditions
  inside `recv` (behavioral, no signature change):** a read interrupted by
  a signal (`EINTR`) on Linux and macOS, which `send` now also retries, and
  the macOS TAP empty read that `tun-rs` 2.8.11 reports as `UnexpectedEof`
  with the message `"recv buffer is empty"`. Both were previously returned
  to the caller (as `Platform(...)` and `Error::Disconnected`).
- **Changed `tunnel-lattice-backend-tunrs`'s error mapping (behavioral,
  no signature change):** `io::Error`s are now mapped by `io::ErrorKind`
  onto the typed `Error` variants that previously existed but were never
  produced — `PermissionDenied` → `Error::PermissionDenied`, `NotFound` →
  `Error::NotFound`, `AlreadyExists` → `Error::AlreadyExists`, and
  `BrokenPipe`/`UnexpectedEof`/`NotConnected` → `Error::Disconnected`
  (`Unsupported` → `Error::Unsupported` is unchanged). Every other kind
  still becomes `Error::Platform(code)`. Callers that matched on
  `Error::Platform` with a specific errno/Win32 code for these cases (for
  example `EPERM` when opening a device without `CAP_NET_ADMIN`) now
  receive the typed variant instead, without the raw code. (A missing
  tap-windows driver, which `tun-rs` reports as `io::ErrorKind::NotFound`,
  is the exception: `open` reports it as `Error::DriverUnavailable`, below.
  The macOS TAP transient empty read, also an `UnexpectedEof`, is retried
  inside `recv` instead of being reported as `Disconnected`, above.)
- **Breaking: `PlatformErrorCode`, `DesiredAdminState`, and `TunRsBackend`
  are now `#[non_exhaustive]`.** A `match` on `PlatformErrorCode` or
  `DesiredAdminState` outside its defining crate needs a wildcard arm, and
  `TunRsBackend` can no longer be built with the `TunRsBackend` literal
  outside its crate — use `TunRsBackend::new()` or `TunRsBackend::default()`
  (the `tunnel-lattice` facade's `connect` already does). This lets later
  releases add platform tags, admin states, and backend configuration
  without another breaking change.
- **Breaking: added `PlatformErrorCode::Unknown`** (a unit variant, so the
  enum stays `Copy + Eq`) for a native failure that carried no OS error
  code or occurred on a target with no platform tag.
  `tunnel-lattice-backend-tunrs` now reports a code-less error that has no
  typed counterpart as `Error::Platform(PlatformErrorCode::Unknown)`
  instead of a fabricated `Linux(0)`/`Windows(0)`/`Darwin(0)`, and on
  targets other than Linux, Windows, and macOS reports every unmapped error
  as `Platform(PlatformErrorCode::Unknown)` instead of `Error::Unsupported`.
  The typed `io::ErrorKind` mappings above are unchanged and still take
  precedence.
- **Breaking: added `Error::DriverUnavailable`** (with
  `Error::is_driver_unavailable`, Display "required driver or runtime is
  unavailable"), returned only by `DeviceProvider::open` when the OS driver
  or user-mode runtime needed for the device is missing.
  `tunnel-lattice-backend-tunrs` returns it for a `wintun.dll` that cannot
  be loaded or lacks a required function (previously
  `Platform(Windows(0))`), a missing tap-windows `tap0901` driver
  (previously `Platform(Windows(0))` in 0.4, `NotFound` above), and a Linux
  `ENODEV`/`ENOENT` when the `tun` module or `/dev/net/tun` is missing
  (previously `Platform(Linux(19))`/`NotFound`). The backend now depends on
  `libloading` 0.9 on Windows (already in the dependency graph via
  `tun-rs`) to recognize the wintun load failure.
- **Breaking: `open` rejects unusable names with `Error::InvalidState`
  before any native call.** Empty names and names containing NUL
  everywhere; on Linux names over 15 bytes or containing `%`; on macOS TAP
  names that are not a canonical `feth<N>` (an explicit unit number from 0
  to 32767, the kernel's highest `feth` unit), and TUN names that are not a
  canonical `utun<N>`; on Windows names over 255 UTF-16 units. These
  previously failed inside `tun-rs` as `Platform(...)` errors — or silently
  opened a device under a different name: a non-canonical `utun` name such
  as `utun07`, a bare `feth` or `feth4294967295` (the kernel's wildcard
  unit; both let the kernel pick the unit), and a
  Linux name with `%d` (which the kernel expands as a template, so `tl%d`
  opened as `tl0`). `DeviceConfig::name`'s docs list the accepted formats.
- **Raised the workspace's `tun-rs` requirement from `2` to `2.8.11`**, the
  release the backend's `open` error classifier and name prechecks were
  verified against. It is a minimum, not an exact pin; the privileged
  Windows CI job now runs two missing-driver tests (`open(Tun)` without
  `wintun.dll`, `open(Tap)` without the tap-windows6 driver) before any
  driver is installed, to detect a newer `tun-rs` that changes those
  errors.
- **Breaking: `open` on an existing name reports `Error::AlreadyExists`
  where it previously adopted the device or returned a platform error.**
  On macOS and Windows, `tunnel-lattice-backend-tunrs` now disables
  `tun-rs`'s `reuse_dev` default for TAP devices, so an existing TAP name
  fails with `AlreadyExists` and the existing interface is left alone
  (previously it was adopted and, on macOS, destroyed when the handle was
  dropped). The code-less Windows "adapter already exists" error, Linux
  `EINVAL`/`EBUSY` (kind or multi-queue mismatch, or a non-multi-queue
  device that already has a queue), and macOS `EBUSY` (a `utun` unit in use)
  map to `AlreadyExists` when an interface with the requested name exists.
  Re-attaching to a Linux persistent or multi-queue device and adopting an
  existing Wintun adapter on Windows still work, and are now documented on
  `DeviceConfig::name`/`multi_queue`; neither deletes the device on drop.
- **Raised every external dependency requirement to its latest release**,
  matching the versions used by `net-lattice` and `dns-lattice` where they
  share a dependency: `tokio` 1.53.1 (from `1` in the workspace and `1.49`
  in `tunnel-lattice-backend-tunrs`), `futures` 0.3.34, `bitflags` 2.13.2,
  `libc` 0.2.189, `blocking` 1.7.0, `libloading` 0.9.0, and the dev-only
  `criterion` 0.8.2. `tun-rs` 2.8.11 is already the latest. No public API
  change; the MSRV stays 1.93.

## [0.4.0]

- **Fixed `Handle::packet_stream`'s cancellation:** it previously always
  used `tunnel-lattice-async`'s thread-based bridge, regardless of whether
  the backend also implemented `AsyncPacketIo`/reported
  `Capability::NATIVE_ASYNC` — meaning dropping the stream could only set a
  flag a worker thread checks *between* `recv` calls, never join it, so a
  worker parked in a blocking `recv` with no further packets kept running
  until the device itself unblocked it. Now `packet_stream` dispatches to a
  new `tunnel_lattice_async::from_async_device` when `Capability::
  NATIVE_ASYNC` is set: no worker thread at all, built directly from
  `AsyncPacketIo::recv` via `futures::stream::unfold`, so dropping the
  stream drops the in-flight future — genuine, immediate cancellation, the
  same as dropping any other future. `Handle::packet_stream`'s bound
  tightened from `PacketIo` to `PacketIo + AsyncPacketIo +
  CapabilityProvider` accordingly (every backend `tunnel-lattice` ships
  already satisfies this whenever the method is reachable at all). Full
  rationale and alternatives considered recorded as an ADR. Investigated
  `tun-rs`'s own `InterruptEvent`/`recv_intr` mechanism first; found it's
  only public on `SyncDevice`, not `AsyncDevice`, so it could not have
  fixed this specific problem regardless.

## [0.3.0]

- Added `tunnel_lattice_platform::PersistentDevice` (`Handle::persist`) and
  `MultiQueueProvider` (`Handle::additional_queue`), both gated by their
  matching `Capability` flag and implemented only on Linux in
  `tunnel-lattice-backend-tunrs` — verified against `tun-rs`'s source that
  the underlying `persist`/`multi_queue`/`try_clone` calls genuinely don't
  exist on macOS/Windows (not merely no-ops there). `DeviceConfig` gained
  `multi_queue`/`with_multi_queue` to request `IFF_MULTI_QUEUE` at open
  time; `additional_queue` on a device that didn't request it returns
  `Error::Unsupported`, mapped from `tun-rs`'s own
  `io::ErrorKind::Unsupported` rather than a meaningless `Error::Platform`
  code (`io_error` now checks for this portable error kind generally, not
  only for this one call site).
- `tunnel_lattice::Handle<D>` is now `Clone` (a cheap `Arc::clone`), making
  explicit a sharing model that was previously only used internally by
  `packet_stream`: `PacketIo::recv`/`send` take `&self` on every platform
  this crate ships, so multiple threads sharing one `Handle` clone can
  already call them concurrently without any capability check — verified
  against `tun-rs`'s own `recv`/`send` signatures on Linux, macOS, and
  Windows, and against Wintun's documented thread-safety for concurrent
  session calls. Documented as an explicit ownership/concurrency contract
  (who owns the device, what Drop does, how this differs from
  `additional_queue`) in `ARCHITECTURE.md` and `Handle`'s own rustdoc,
  before `1.0` rather than left implicit.

## [0.2.0]

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
- **Fixed a real deadlock in `tunnel-lattice-backend-tunrs`'s `tokio`
  feature:** `PacketIo::recv`/`send` blocked on a Tokio-backed `AsyncDevice`
  via `futures::executor::block_on`, which never polls Tokio's own I/O
  driver — a plain `send()` call hung forever (confirmed against a real
  device under `CAP_NET_ADMIN`, deadlocked past a 10-second timeout). Now
  blocks via `tokio::runtime::Handle::current().block_on` instead, which
  requires (and now documents) that a caller using the `tokio` feature run
  with a **multi-threaded** Tokio runtime — `current_thread` reproduces the
  same hang, since only `Runtime::block_on` (not `Handle::block_on`) drives
  that flavor's I/O reactor. Added `#[ignore]`d privileged tests
  (`tunnel-lattice-backend-tunrs::privileged_tests`) that open, mutate, and
  tear down a real device under each feature set, plus a `privileged` CI job
  that runs them with `sudo`/Administrator on all three target platforms —
  this bug was only found by actually running the new tests against a real
  device, not by reading `tun-rs`'s source.
- Documented (`tunnel-lattice-backend-tunrs`'s and `tunnel-lattice`'s
  READMEs) that `wintun.dll` must ship alongside a Windows application using
  this crate's default TUN backend — `tun-rs` loads it at runtime and does
  not vendor it, and its absence surfaces as an unhelpful
  `Error::Platform(PlatformErrorCode::Windows(0))` with no OS error code
  rather than a named "DLL not found" error. Found via the `privileged` CI
  job's real Windows runs, all three of which failed this way before the
  job was updated to download `wintun.dll` (pinned to the 0.14.1 build) and
  add it to `PATH`.

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
