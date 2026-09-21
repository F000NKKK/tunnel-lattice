# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
