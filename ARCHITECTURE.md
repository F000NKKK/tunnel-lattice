# Tunnel Lattice Architecture

Tunnel Lattice creates, configures, and transfers packets on TUN/TAP virtual
network interfaces. It does not assign IP addresses to the interfaces it
creates — that is `net-lattice`'s job in the sibling Lattice ecosystem, once
a device exists.

## Crate map

```text
tunnel-lattice-core             errors, IDs — no OS dependency
tunnel-lattice-model             DeviceKind/DeviceConfig/Device/DeviceConfigPatch — no OS dependency
tunnel-lattice-platform          generic provider traits + Capability — depends on core only, never on model
tunnel-lattice-backend-tunrs     tun-rs-backed implementation of the platform traits
tunnel-lattice-async             futures::Stream adapter over a synchronous PacketIo device
tunnel-lattice                   facade: binds platform traits to model types, selects a backend
```

Dependency direction: `core` ← `model`, `core` ← `platform`; `model` and
`platform` never depend on each other directly — `tunnel-lattice-backend-tunrs`
and the `tunnel-lattice` facade are what bind `platform`'s generic associated
types to `model`'s concrete types. This mirrors `net-lattice-platform`'s
"never depends on `net-lattice-model`" rule in the sibling ecosystem, for the
same reason: a backend or the facade decides which concrete types satisfy the
contract, not the contract itself.

## Why this differs from net-lattice's shape

`net-lattice` inspects and mutates OS objects (routes, interfaces, DNS
config) that exist independently of the calling process — a route exists
whether or not any process is watching it, so `net-lattice-platform` splits
read (`RouteProvider`) from write (`RouteMutator`) per domain, and the facade
holds one persistent connection (`Lattice<Backend>`) used across many calls
naming an object by ID.

A TUN/TAP device has no such independent existence: it exists only because
this process created it, and the calling process holds the only handle. So:

- there is no persistent "connection" step before opening a device — opening
  *is* the privileged operation (`DeviceProvider::open`, not
  `Lattice::connect` followed by per-call IDs);
- the returned device handle is itself the target of every subsequent
  operation (`PacketIo`, `DeviceObserver`, `DeviceMutator`), not an ID passed
  back into a shared connection object;
- there is no domain-wide "list every device" read the way `RouteProvider`
  lists every route — a backend that can enumerate *pre-existing persistent*
  devices exposes that as its own inherent API, gated by
  `Capability::PERSISTENT_DEVICES`, rather than as a required trait method.

## Backend replacement plan

`tunnel-lattice-backend-tunrs` wraps the `tun-rs` crate, which is itself
cross-platform (Linux, Windows, macOS, the BSDs, iOS, Android) — unlike
`net-lattice`'s mutually exclusive per-OS backends
(`net-lattice-backend-linux`/`-windows`/`-darwin`, each `target_os`-gated),
Tunnel Lattice starts with one shared backend crate because there is no
platform-specific Rust code to isolate yet: `tun-rs` already did that work
internally.

The workspace is deliberately structured so `tun-rs` can be dropped later
without moving `tunnel-lattice-platform` or the `tunnel-lattice` facade's
public API:

- `tunnel-lattice-platform`'s traits (`DeviceProvider`, `PacketIo`,
  `DeviceObserver`, `DeviceMutator`, `AsyncPacketIo`) name no `tun-rs` type —
  they are generic over associated types, satisfied today by
  `tunnel-lattice-backend-tunrs::TunRsBackend`/`TunRsDevice`;
- the facade selects a backend through a Cargo **feature** (`tun-rs`,
  default-on), not a `target_os` cfg gate. A future hand-written per-OS
  backend (e.g. `tunnel-lattice-backend-linux` built directly on
  `/dev/net/tun` + Netlink, mirroring `net-lattice-backend-linux`'s own
  Netlink usage) would ship as its own crate and its own facade feature,
  selectable *alongside* `tun-rs` rather than only ever replacing it whole —
  a caller could depend on `tunnel-lattice` with `default-features = false,
  features = ["linux-native"]` once that exists;
- when a per-OS backend eventually covers every platform `tun-rs` covers
  today, dropping the `tun-rs` feature (and the dependency itself) becomes a
  backend-crate-only change: nothing in `tunnel-lattice-model` or
  `tunnel-lattice-platform` needs to move.

No per-OS backend exists yet — this section records the intended shape, not
completed work.

## Error model

Mirrors `net-lattice-core::Error`: one `#[non_exhaustive]` enum
(`PermissionDenied`, `NotFound`, `AlreadyExists`, `Unsupported`,
`InvalidState`, `Disconnected`, `Platform(PlatformErrorCode)`) returned by
every provider trait method instead of a raw OS error type. A backend maps
its native error (`std::io::Error` for `tun-rs`, currently) into this shape
at the boundary; callers never match on a raw `errno`/`DWORD` directly.

## Frozen public API surface

`0.1.0` is published (see `index.md`, `SUPPORT.md`). Nothing in this
workspace is API-frozen; every type, trait, and feature flag described here
may still change in a future `0.x` release (see `versioning.md`'s pre-1.0
policy). See `index.md`, "Current release and roadmap," for the stage this
freeze is scheduled at (`1.0`, unscheduled) and what ships before it.

## Async design

`tunnel-lattice-platform`'s `async` feature adds `AsyncPacketIo`, an
`impl Future`-returning trait a backend implements when it has genuine
non-blocking I/O (an async-registered file descriptor, overlapped I/O, ...).
`tunnel-lattice-backend-tunrs` implements it directly on its
`tun_rs::AsyncDevice` handle (built via `DeviceBuilder::build_async`, not
cloned from a separate sync handle — `tun_rs::SyncDevice::try_clone` only
exists on Linux) and reports `Capability::NATIVE_ASYNC`; its blocking
`PacketIo` impl then blocks on the same async methods rather than keeping a
second handle — but *how* it blocks differs by which of the two mutually
exclusive async backends is active, and the difference matters:
`async-io`'s `AsyncDevice` (built on the runtime-agnostic `async-io`/
`blocking` crates) is blocked on with `futures::executor::block_on`, while
`tokio`'s must instead go through `tokio::runtime::Handle::current().
block_on` — `futures::executor::block_on` never polls Tokio's own I/O
driver, so a Tokio-backed `recv`/`send` would otherwise hang forever waiting
for a readiness notification the driver never delivers. This was found and
fixed by an isolated repro against `tun-rs` directly (confirmed with a real
device under `CAP_NET_ADMIN`: `send()` deadlocked past a 10-second timeout
before the fix, completed immediately after). It also means the `tokio`
feature requires a **multi-threaded** Tokio runtime on the calling thread —
`Handle::block_on` only drives that runtime's I/O reactor on the
`multi_thread` flavor; on `current_thread`, only `Runtime::block_on` (called
on the owned value, not a `Handle`) does, so a `current_thread` runtime
reproduces the same hang. See `tunnel-lattice-backend-tunrs`'s README,
"`tokio` requires a multi-threaded runtime."

`tunnel-lattice-async` provides a fallback for a backend with no native async
path: `from_device` spawns one blocking worker thread per device bridging
`PacketIo::recv` onto a `futures::Stream`. This is also why a *new* backend
never needs its own async story to get one for free: implementing `PacketIo`
alone already makes it usable through `tunnel-lattice-async`, and
`AsyncPacketIo` is purely an additive optimization for a backend with a
genuine native path — the facade's `Handle<D>` and `ConnectedDevice` bound
are identical either way (see `tunnel-lattice`'s rustdoc).

Neither crate depends on Tokio, async-std, or smol directly by default —
`tunnel-lattice`'s `async-io`/`tokio` features (mutually exclusive; enabling
both is a compile error from `tun-rs` itself) are opt-in, so a caller who
enables neither pulls in no async runtime at all. Known limitation: the
thread-based adapter cannot forcibly cancel a worker blocked inside `recv`
with no further packets arriving — see `tunnel-lattice-async`'s rustdoc.

## Platform and privilege notes

Creating a TUN/TAP device generally requires `CAP_NET_ADMIN` on Linux,
Administrator on Windows, or root on macOS/BSD. `Capability` flags describe
implemented surfaces, not proof the current process is authorized — callers
should still handle a permission error from `DeviceProvider::open` even when
a capability is reported as available.
