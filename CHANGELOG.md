# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

- **Added packet-path benchmarks and an allocation-count test to
  `tunnel-lattice-async` (development only, no API change):** `cargo bench
  -p tunnel-lattice-async` measures the synchronous caller-buffer receive
  loop and the current `Vec`-per-packet stream paths over in-memory mock
  devices, and `tests/alloc_count.rs` pins today's per-packet heap cost
  (one allocation per packet on the native stream, two on the thread
  bridge) so later buffer changes can be compared against it. `criterion`
  is a dev-dependency only. CI builds the benches on every OS without
  running them.
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
  slot to the pool on drop. Slots are reused without being re-zeroed.
  `PacketStream` still yields `Vec<u8>` items in this change, and the pool
  has no public acquire method. `cargo bench -p tunnel-lattice-async
  --bench pool` measures pool construction, and CI gains a non-blocking
  Miri job for the pool's unsafe code.
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
  such a packet and keep receiving; only `Err(Disconnected)` ends a stream.
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
