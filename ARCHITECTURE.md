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
tunnel-lattice-backend-linux     native Linux backend (/dev/net/tun + sync rtnetlink) — in development, no public API yet; empty off Linux
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

Rules that name model types but are the same for every backend live in
`tunnel-lattice-model` behind the non-default `backend` feature
(`tunnel_lattice_model::backend`), so each backend keeps only its native
calls and trait impls: the per-OS interface-name rules, the checks `open`
runs before any native call, the segmentation-offload request rule, the
administrative-state flag and status rules, and the `apply` contract
(precondition order, step order, compensation). It is a support API for
backend authors with no stability promise; the `tunnel-lattice` facade never
enables it and never re-exports it, and it performs no I/O.

The rules that name no model type live the same way in
`tunnel-lattice-platform` behind its own non-default `backend` feature
(`tunnel_lattice_platform::backend`): the step a `recv`/`send` loop takes
after one native attempt, the `recv_batch` drain (generic over the error type
of the read, with the classifier passed in as a closure), and the raw error
codes, including the Linux `errno` table that maps a code and the operation
that produced it to an outcome. It is safe Rust with no `libc` dependency
(the Linux constants are checked against `libc` in the crate's own tests),
carries the same no-stability-promise status, and is likewise never enabled or
re-exported by the facade.

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
- any backend, including one written outside this workspace or a test
  double, is injected with `Tunnel::new(backend)`; the `tun-rs` feature
  only adds `Tunnel::connect()` as shorthand for
  `Tunnel::new(TunRsBackend::new())`. `Tunnel::capabilities()` reports what
  the host supports before any device is opened (for example whether a TAP
  driver was found), but `open` stays authoritative. `Handle::id()` and
  `Handle::kind()` make no native call: the identity is captured at open
  (`DeviceObserver::id`) and need not equal the interface's current OS
  index, which is why interop with `net-lattice` resolves the interface by
  name;
- when a per-OS backend eventually covers every platform `tun-rs` covers
  today, dropping the `tun-rs` feature (and the dependency itself) becomes a
  backend-crate-only change: nothing in `tunnel-lattice-model` or
  `tunnel-lattice-platform` needs to move.

The first per-OS backend, `tunnel-lattice-backend-linux`, is in development:
the crate exists (Linux-only, empty on other targets) and holds its raw-errno
error mapping, its synchronous rtnetlink control plane, and a synchronous
device core that implements the provider traits internally: opening TUN and
TAP devices through `TUNSETIFF` with `IFF_NO_PI` (non-blocking from
creation, the interface index captured as the device id, the device node
never created), packet I/O with `readv` plus a one-byte sentinel for
oversize detection and write-first sends, snapshot and best-effort-reverted
apply over rtnetlink, persistence, and multi-queue, with the same Linux
capability set as `tunnel-lattice-backend-tunrs`. It still exports no
public API, has no async I/O and no segmentation offload, and no facade
feature selects it. Until it does, this section records the intended shape,
not completed work.

## Error model

Mirrors `net-lattice-core::Error`: one `#[non_exhaustive]` enum
(`PermissionDenied`, `NotFound`, `AlreadyExists`, `Unsupported`,
`InvalidState`, `Disconnected`, `DriverUnavailable`, `BufferTooSmall`,
`Platform(PlatformErrorCode)`) returned by
every provider trait method instead of a raw OS error type. A backend maps
its native error (`std::io::Error` for `tun-rs`, currently) into this shape
at the boundary; callers never match on a raw `errno`/`DWORD` directly.
`tunnel-lattice-backend-tunrs` maps by portable `io::ErrorKind` first
(`PermissionDenied`, `NotFound`, `AlreadyExists`, `Unsupported`, and
`BrokenPipe`/`UnexpectedEof`/`NotConnected` → `Disconnected`); only kinds
without a typed counterpart fall back to `Platform(PlatformErrorCode)`.
`PlatformErrorCode` is itself `#[non_exhaustive]` (`Linux(i32)`,
`Windows(u32)`, `Darwin(i32)`, `Unknown`): a native failure that carried no
OS error code, or one on a target with no platform tag, is reported as
`Platform(PlatformErrorCode::Unknown)` — never a fabricated `0` code, which
would read as "success" on every platform.

`DeviceProvider::open` adds two layers in front of that mapping. First,
prechecks reject a requested name (per-OS format, documented on
`DeviceConfig::name`) or MTU the platform cannot honor with `InvalidState`,
before any native call. Second, an open-only classifier turns native
failures that only have a specific meaning during creation into typed
variants: a missing driver or user-mode runtime (`wintun.dll`, the
tap-windows6 driver, the Linux `tun` module) becomes `DriverUnavailable`,
which no other operation returns; an existing interface with the requested
name becomes `AlreadyExists` (on Linux and macOS only when an interface with
that name actually exists, because the same errno also covers unrelated
failures). Everything else falls through to the general mapping. `open`
never adopts an existing interface and then destroys it on drop: the
`tun-rs` backend turns off `tun-rs`'s TAP "reuse existing device" default on
macOS/Windows, and the two remaining attach cases — Linux persistent or
multi-queue devices, and existing Wintun adapters on Windows — are
documented rather than refused, since neither deletes the interface and a
check before opening would race with other processes.

`recv` adds its own layer too. It never truncates a packet silently and
never reports more bytes than the buffer holds: a packet that does not fit
is discarded and `recv` returns `BufferTooSmall`, and the device stays
usable. Natively this differs per OS — Linux TUN/TAP and macOS `utun`
truncate silently, so the `tun-rs` backend reads into the caller's buffer
plus a one-byte sentinel and treats a length past the buffer as "did not
fit"; macOS TAP (`feth`) and Windows (Wintun, tap-windows6) reject the
packet with an error that the backend maps to `BufferTooSmall`.
`Device::recv_buffer_len()` (the MTU for TUN, MTU + 18 for TAP: the
Ethernet header and one 802.1Q tag) gives a buffer that fits any packet at
the snapshot's MTU, except a double-tagged (QinQ) TAP frame. `recv`
and `send` also retry two transient conditions internally instead of
reporting them: `EINTR` on Linux and macOS, and a macOS TAP read that
produced no complete frame (which `tun-rs` reports as an end-of-file with
a specific message). Every other end-of-file stays `Disconnected`.
On a Linux TUN queue that uses segmentation offload (see "Segmentation
offload" below), `recv` also drops and re-reads two more conditions
internally: a raw `EINVAL` from the read (the kernel refused to frame a
packet and has already freed it) and a received frame that fails
validation. Neither is reported, so neither adds an error variant or ends
a stream. On such a queue `BufferTooSmall` drops one segment rather than
one packet.

`recv` and `send` also recognize a device that was deleted or disabled
underneath the handle. `Disconnected` means the device channel is gone for
good: raw `ENXIO` (macOS, the TAP's BPF descriptor after the peer `feth`
it is bound to was destroyed; also mapped on Linux for symmetry), Linux
`EBADFD` (the tun file was detached because the device was deleted),
Linux `EFAULT` on `recv` only (a blocking read already waiting when the
device was deleted), and Wintun's send-side "adapter terminating" signal
on Windows. A Wintun adapter that was disabled (for example by applying
`DesiredAdminState::Down`) reports `InvalidState` instead, because applying
`Up` recovers the same handle, and so does Linux `send` on an
administratively down TUN/TAP device: the tun driver returns raw `EIO`
from a write only for that case. On a Windows TAP handle, applying
`Down` disconnects the adapter's media, and tap-windows then fails every
`send` and `recv` made while the media is disconnected at once with raw
`ERROR_OPERATION_ABORTED` (995); that code is `InvalidState` on a TAP
handle only (the rule takes the device kind as well as the OS), since
applying `Up` recovers the same handle. A read already pending when
`Down` is applied is not aborted: it keeps waiting, as on a down Linux
device, through `Down` and a later `Up`, and only disabling the adapter
ends it.
A healthy TAP adapter also reports 995 for a read cancelled because the
thread that started it exited (an async `recv` starts its read on the
polling thread, so a dropped `recv` whose thread then exits leaves one
behind). On `recv`, the first 995 in a call is therefore retried once with
a fresh read if the adapter's operational status reads up; the status is
read at most once per call. A single call that meets two cancelled reads
reports `InvalidState` once on a healthy adapter. `send` is not retried,
because `tun-rs` discards a cancelled pending write.
`InvalidState` from `recv`/`send` means "the device exists but is not
passing packets because it is down or disabled"; when `apply(Down)` caused
it, `apply(Up)` on the same handle recovers it. A down Linux device makes
`recv` wait rather than fail, and a down macOS device still accepts
`send`. A Windows TAP adapter disabled outside the crate
(`Disable-NetAdapter`) fails with the same code and so also reports
`InvalidState`, although only re-enabling it outside the crate recovers
it. These rules match raw OS codes only on the OS they belong to, and a
raw-coded error is never matched through `kind()` (which `std` decodes
with the running host's code table); only the code-less Wintun errors are
matched by kind and message. Every rule is therefore unit-tested on every
host.

Both `PacketStream` variants yield `BufferTooSmall` and keep receiving.
Every other error is yielded once and then ends the stream — `Disconnected`
and recoverable errors alike. A device whose `recv` fails immediately and
repeatedly (a deleted Linux device, a destroyed macOS `feth`, a disabled
Wintun adapter) would otherwise turn the stream into a busy loop of error
items and, on the thread bridge, keep its worker thread spinning; a
back-off or an error-count cap would need a timer (the crate is
runtime-agnostic) or an arbitrary limit. The caller creates a new stream
with `Handle::packet_stream` after recovering. Transient conditions never
reach the stream because the backend retries them, which is why the
`EINTR` retry and the Windows TAP cancelled-read retry are required:
without them a signal, or a read cancelled by an exited thread, would end
a healthy stream. `BufferTooSmall` stays the only error that does not end
a stream, on every OS.

`DeviceMutator::apply` has one contract for every backend. It checks
every precondition before any native call:

- a patch for another device, an MTU above `u16::MAX`, or a MAC address
  for a TUN device returns `InvalidState`;
- then a setting the backend cannot apply, such as a MAC address on a
  handle without `Capability::MAC_MUTATION`, returns `Unsupported`.

It then applies the MTU, then the MAC address, then the administrative
state, which goes last so that no later step can fail after it and it never
needs reverting. If a step fails, it reverts the earlier steps in
reverse order on a best-effort basis and returns the failed step's own
error. To make that revert possible it reads the previous MTU and MAC
first, but only when a later step exists. There is no
`PartiallyApplied` error: after any `Err`, `snapshot()` is authoritative.
`InvalidState` therefore always means that a precondition was rejected and
nothing changed, the same rule `net-lattice` follows.

A TAP device has a MAC address; a TUN device has none. `Device::mac`
reports it for TAP (`None` for TUN), `DeviceConfig::with_mac` requests one
at open, and `DeviceConfigPatch::with_mac` changes it later. `MacAddress`
is pure data shaped like `net-lattice-model`'s own, so the two convert
through `[u8; 6]`. A MAC requested for a TUN device is refused with
`InvalidState` before anything is created. After opening, the backend
reads the address back; if the platform did not apply it, `open` returns
`Unsupported` and the new device is torn down. A later change needs
`Capability::MAC_MUTATION`, which the `tun-rs` backend reports on a TAP
handle on Linux and macOS. On Windows the tap-windows6 driver takes the
address from the adapter's registry settings when the adapter is created,
so it can be requested at open but not changed afterwards.

## Frozen public API surface

`0.6.0` is published (see `index.md`, `SUPPORT.md`). Nothing in this
workspace is API-frozen; every type, trait, and feature flag described here
may still change in a future `0.x` release: until `1.0.0`, any new `0.x`
minor release may change the public API, including in breaking ways, and
every such change is listed in `CHANGELOG.md`. See `index.md`, "Current
release and roadmap," for the stage this
freeze is scheduled at (`1.0`, unscheduled) and what ships before it.

## Ownership and concurrency contract

`tunnel_lattice::Handle<D>` wraps its backend device in an `Arc<D>` and is
itself `Clone` (a cheap `Arc::clone`, not a device duplication) — this is a
deliberate, explicit contract, not an implementation detail of
`packet_stream`'s internal sharing:

- **Multiple readers/writers sharing one `Handle` clone are always safe, on
  every backend.** `PacketIo::recv`/`send` take `&self`; this is verified
  against `tun-rs`'s own signatures on Linux, macOS, and Windows (all three
  declare `recv`/`send` as `fn(&self, ...)`), and Windows's Wintun sessions
  are documented thread-safe for concurrent `WintunReceivePacket`/
  `WintunSendPacket` calls specifically so this holds without any special
  handling on our side. No capability check or feature gates this — sharing
  a `Handle` clone across threads and calling `recv`/`send` concurrently is
  the baseline, portable multiplexing story. The privileged CI job checks
  it on a real TUN device on Linux, macOS, and Windows in all three
  feature sets: the device gets an address, one clone blocks in `recv`
  while another clone writes ICMP echo requests from the peer address,
  and the host's echo reply, routed back into the same device, must reach
  the waiting `recv` within 60 seconds (with `async-io`/`tokio` also with
  a `packet_stream` task on one clone and `send_async` on another). On
  Windows the test adds a firewall rule letting echo requests to the
  device's address in, and removes it afterwards.
- **`Handle::additional_queue`** (gated by `D: MultiQueueProvider`, see
  below) is a *different, stronger* thing: an independent `Handle` over a
  second OS-scheduled queue, not a second reference to the same one. The
  two `Handle`s returned this way share no `Arc`; dropping one has no
  effect on the other.
- **Drop closes the device once every clone (`Handle` or `PacketStream`) of
  its `Arc` is gone**, via `D`'s own `Drop` impl (`tun_rs::SyncDevice`/
  `AsyncDevice` close their file descriptor there). There is no explicit
  "close" method — letting every reference drop is the only way, and a
  `PacketStream` created from a `Handle` holds its own `Arc` clone
  independent of the `Handle` it came from, so either can outlive the
  other.

See `tunnel_lattice::Handle`'s own rustdoc for the full contract; this
section exists so the answer to "who owns the device, can I share it,
what happens on drop" is recorded before `1.0`, not left implicit in code
someone has to read to find out.

## Persistent devices and multi-queue

`Capability::PERSISTENT_DEVICES` (`tunnel_lattice_platform::PersistentDevice`)
and `Capability::MULTI_QUEUE` (`tunnel_lattice_platform::MultiQueueProvider`)
are both Linux-only in `tunnel-lattice-backend-tunrs` — verified directly in
`tun-rs`'s source, not assumed: `DeviceImpl::persist`, `DeviceBuilder::
multi_queue`, and `SyncDevice`/`AsyncDevice::try_clone` are all
`#[cfg(target_os = "linux")]` in `tun-rs` itself, with no equivalent on
macOS/Windows at all (not merely "ignored" — the methods don't exist there).
Both platform traits are still declared unconditionally in
`tunnel-lattice-platform`, since the contract itself is generic; only the
`tunnel-lattice-backend-tunrs` implementation is `#[cfg(target_os =
"linux")]`-gated.

- **Persistence** (`Handle::persist`, `Handle::unpersist`) marks an open
  device to survive its last handle closing (including process exit), and
  clears that again so the device is destroyed once its last handle, in
  any process, closes. Both are the kernel's `TUNSETPERSIST` ioctl, which
  takes its argument by value: non-zero sets `IFF_PERSIST`, zero clears it.
  `persist` goes through `tun-rs`; `tun-rs` has no way to clear the flag,
  so `unpersist` issues `TUNSETPERSIST(0)` on the handle's descriptor
  itself, in one documented `unsafe` call, passing the zero as
  an integer, never a pointer (a pointer to zero is non-zero and would set
  the flag). The kernel performs no capability check on this ioctl:
  holding a handle attached to the device is enough, and getting one
  normally required `CAP_NET_ADMIN` or ownership of the device. Both calls
  work from any queue. Re-attaching to an existing persistent device by
  name needs no extra API: it's ordinary Linux `TUNSETIFF`-by-name kernel
  behavior — open with the same `DeviceConfig::name`, kind, and
  `multi_queue`. The kernel attaches to any existing device of the same
  type and multi-queue setting (including, with multi-queue, a live device
  another process has open), refuses a type or multi-queue mismatch with
  `EINVAL` and a non-multi-queue device that already has a queue with
  `EBUSY` (both reported as `Error::AlreadyExists`), and `open` cannot
  tell whether it attached or created. A dedicated CI job checks the whole
  cycle across two processes: one creates and persists a device and exits,
  a second re-attaches by name, unpersists, and closes it, and the device
  is gone.
- **Multi-queue** (`DeviceConfig::with_multi_queue`, `Handle::
  additional_queue`) requests `IFF_MULTI_QUEUE` at open time and, once
  granted, duplicates a genuinely independent, hardware-scheduled queue on
  the same device (`tun-rs`'s `try_clone`) — distinct from the "naive"
  multiplexing described above, which needs no multi-queue request at all
  and works on every platform. Calling `additional_queue` on a device that
  wasn't opened with `with_multi_queue(true)` returns
  `Error::Unsupported` (mapped from `tun-rs`'s own
  `io::ErrorKind::Unsupported` — a portable signal distinct from a raw OS
  error code, unlike the `Error::Platform` cases elsewhere in this crate).
  A queue attached to an existing multi-queue device, and every
  `additional_queue`, takes the device's packet framing, which the backend
  reads back after the open (see the next section).

## Segmentation offload (Linux TUN)

`DeviceConfig::offload` (`with_offload`, off by default) requests kernel
segmentation offload. Like `multi_queue`, it is a request, not a
guarantee: the `tun-rs` backend honours it only for Linux TUN devices and
ignores it without an error on Windows, macOS, and for TAP (the split
path parses IP only, not Ethernet). The answer is
`Capability::SEGMENTATION_OFFLOAD`, reported on a handle only and never by
the host-level `Tunnel::capabilities()`. Offload is fixed at open;
`DeviceConfigPatch` has no field for it and `apply` never touches it.

The kernel side is a 10-byte virtio-net header in front of every packet
on the queue (`IFF_VNET_HDR`) plus a device-wide offload mask
(`TUNSETOFFLOAD`) that lets it deliver one TCP or UDP super-packet of up
to 64 KiB in place of many packets, and accept one on write. The backend
parses and builds that header itself, in a private module that does no
I/O, uses checked arithmetic only, and compiles and is unit-tested on
every OS; it uses only `tun-rs`'s public builder option and vectored
reads and writes, none of its hidden offload helpers. Packet framing as a
caller sees it does not change:

- **Receive.** `recv` (sync and async) still returns exactly one IP
  packet with no header, so `PacketStream`, `PacketPool`, and "one item is
  one packet" are untouched. Each offload queue owns a staging buffer
  (header + 65 536 bytes + a one-byte sentinel, about 64 KiB, allocated
  only for an offload queue) and a split cursor behind a mutex. A `recv`
  serves the next pending segment straight into the caller's buffer
  (headers patched, payload copied once, checksums completed); only when
  none is pending does it read the next super-packet. The async paths
  never hold the lock across an await, and nothing awaits between a
  successful read and the return, so a dropped `recv` future never loses
  a read super-packet: unreturned segments wait for the next call. The
  receive split is bounded only by the frame length, never by a segment
  count, so a small-MSS TCP super-packet is never dropped. `recv_batch`
  serves the pending segments into consecutive buffers and reads further
  super-packets without waiting (see "Async design"), so one call can
  return a whole super-packet, or several. A segment too long for its
  buffer after the first is not dropped there: it ends the batch and
  stays pending. A frame the split rejects, or a raw `EINVAL`, met while
  draining is dropped and the drain goes on.
- **Send.** `send` writes the packet behind an all-zero header.
  `send_batch` coalesces runs of adjacent same-flow packets (TCP with
  contiguous sequence numbers and otherwise identical headers, or UDP with
  equal-sized datagrams when the kernel accepts UDP segmentation offload)
  into one gather write of `[header, patched copy of the first packet's
  headers, payload slices...]`, with no allocation and no payload copy. A
  call handles at most 128 packets (the kernel's per-super-packet segment
  limit) and returns a short `Ok` beyond that. A run's write refused with
  `EINVAL` sent nothing and is resent packet by packet, so a kernel that
  lacks a given offload degrades to per-packet sends.

Framing follows the device, not the request. `IFF_VNET_HDR` and the
header size are device-wide, and a queue attached to a multi-queue device
that already has queues does not get its own flags applied, so after
every open and `additional_queue` the backend reads the real framing back
(`TUNGETIFF`, `TUNGETVNETHDRSZ`): header flag set with size 10 gives an
offload queue (even without a request), any other size fails `open` with
`Error::Unsupported`, and a clear flag gives a plain queue (even with a
request), in which case the backend clears the offload mask the request
set (`TUNSETOFFLOAD(0)`), since a device without the header must not
produce super-packets for anyone. Up to 0.5, a plain queue
attached to an offload multi-queue device handed each packet to the
caller with the 10-byte header in front of it.

The offload mask is device-wide and has a side effect on other queues and
processes: an offload open sets it (checksum, TSO for IPv4 and IPv6, and
UDP segmentation where the kernel has it), and `tun-rs` clears it on every
Linux open without offload, so a plain open of a shared multi-queue
offload device turns super-packets off for all of its queues. They stay
framed and correct, and simply receive single packets. Nothing is
compensated on drop: the mask is not restored, matching `tun-rs`. The
header is decoded in host byte order; a big-endian host whose device was
switched to little-endian headers by someone else is unsupported and not
detected. There is no compile-time or minimum-kernel gate: what the
kernel negotiates at open is authoritative. Offload needs no privilege
beyond opening the device.

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

On Linux with `tokio`, `recv` does not wait through `tun-rs` at all: it
reads through a private duplicate of the device descriptor registered with
Tokio for readable *and* error readiness. A deleted Linux device reports
error readiness only, which a readable-only wait (what `tun-rs` uses) never
sees, so a pending `recv` would otherwise hang instead of ending with
`Disconnected`. `send` stays on the `tun-rs` handle.

On macOS with `async-io` or `tokio`, a TAP (`feth`) `recv` keeps `tun-rs`
doing the read but replaces its readiness wait. macOS readiness
notification for BPF only reports buffered data, never the interface
going away, so `tun-rs`'s unbounded wait on the BPF descriptor would never
end after the peer `feth` is destroyed. The backend instead waits on a
duplicate of that descriptor on the `blocking` thread pool (the pool
`tun-rs` itself uses there, so it needs no reactor and no runtime timer),
in waits of at most 250 ms, and at each timeout asks the kernel which
interface the descriptor is bound to; once that fails, the next read
returns `ENXIO`, which maps to `Disconnected`. A per-wait pipe lets a
dropped `recv` end its thread's wait at once, and no packet is read inside
the wait, so dropping a `recv` never loses one. The cost is one extra
descriptor per TAP handle and a pipe per pending wait; `send` and `utun`
devices are unchanged.

`tunnel-lattice-async` provides two ways to build the `futures::Stream`
`Handle::packet_stream` returns: `from_async_device`, wrapping a backend's
`AsyncPacketIo` directly with no worker thread at all (built on
`futures::stream::unfold` over repeated `recv().await` calls), and
`from_device`, the original thread-based bridge over blocking `PacketIo`
for a backend with no native async path. `Handle::packet_stream` picks
between them at runtime by checking `Capability::NATIVE_ASYNC` — this is
also why a *new* backend never needs its own async story to get a stream at
all: implementing `PacketIo` alone already makes it usable through
`from_device`, and `AsyncPacketIo` is an additive optimization that
`packet_stream` prefers automatically once implemented, not something a
caller has to opt into by name.

This distinction is not cosmetic. `from_async_device`'s stream is
*genuinely* cancellable: dropping it drops the boxed, currently-polled
`AsyncPacketIo::recv` future, which is ordinary Rust future-drop semantics
— nothing is left running. `from_device`'s worker thread has no such
guarantee: dropping the stream can only set a flag the thread checks
*between* `recv` calls, so a worker parked inside a blocking `recv` with no
further packets arriving keeps running until the device itself unblocks it
(this remains `from_device`'s documented limitation for whatever backend
actually needs it — a hypothetical one with no native async path at all;
every backend `tunnel-lattice` ships as of this crate implements
`AsyncPacketIo` whenever `packet_stream` is reachable in the first place, so
`from_device` is not on the path any shipped build actually takes).
`Handle::packet_stream`'s bound was tightened accordingly to require
`AsyncPacketIo` (previously only `PacketIo`), a pre-1.0 change that was
additive in effect for every shipped backend.

The single-packet send side is one method, `Handle::send_async`, under the
same features (its batch counterpart is described below). It returns the
device's own `AsyncPacketIo::send` future unchanged (declared `impl Future<Output = Result<usize>> + Send`, not an
`async fn`, so `Send` is part of the contract), with no capability check,
no fallback, and no error mapping: with the tun-rs backend the blocking
`send` blocks on the very same future, so both have identical results and
errors. It exists because the blocking `send` cannot be used from async
code (with `tokio` it panics inside a task and hangs on `current_thread`;
with `async-io` it parks the executor thread), and `Handle` does not
expose its device. A backend without `Capability::NATIVE_ASYNC` must still
implement `AsyncPacketIo` itself, and its send future may block the
polling thread. There is deliberately no `Sink`: `AsyncPacketIo::send` is
not poll-based, so a `Sink` would box a future and own a copy (or a pool
slot) per packet, which the packet-buffer design below exists to avoid; one
can be layered on `send_async` later. Dropping the future is always safe
and never sends part of a packet. Per OS with tun-rs: on Linux and macOS
(`utun` and `feth`) a send dropped before completion sent nothing, because
the write happens only in the poll that completes it; on Windows tun-rs
copies the packet and writes on its blocking pool, so a dropped send may or
may not have gone out. The portable contract, also stated on
`AsyncPacketIo::send` as a backend obligation, is "the whole packet at most
once; unknown after drop."

Batch send is additive and works the same way. `PacketIo::send_batch` and
`AsyncPacketIo::send_batch` are provided trait methods (no required method
was added, and `PacketIo` stays dyn-compatible) whose default sends one
packet at a time; `Handle::send_batch` and, under the async features,
`Handle::send_batch_async` pass straight through. The contract is a prefix
with no deferred errors: `Ok(n)` means `packets[..n]` were each sent whole
and in order and `packets[n..]` were not touched, and `n` may be short, so
callers loop; an empty slice gives `Ok(0)`; `Err(e)` means nothing was
sent and `e` belongs to `packets[0]`, classified as `send` would classify
it; a failure after `k > 0` packets becomes `Ok(k)` and the next call,
starting at `packets[k]`, sees the error. Dropping a `send_batch` future
extends the single-packet rule: an unknown prefix was sent, each packet
whole and at most once, never a packet after one that was not sent. Only a
Linux TUN offload queue overrides the default (see "Segmentation offload"
above); every other backend, OS, and queue sends packet by packet. `recv_batch`
mirrors it on receive: it waits for the first packet only, returns a
prefix, and reports an error after `k` packets on the next call. Its
default receives exactly one packet, which the `tun-rs` backend keeps on
Windows and macOS. On Linux (TUN and TAP, every feature set) it overrides
it with a drain: the first packet is received exactly as `recv` receives
it, and each further position is one read that never waits (`preadv2`
with `RWF_NOWAIT` in the blocking build, a non-blocking read on the
already registered descriptor in the async builds, with `tokio` a
`try_io` on the same readiness guard), so the descriptor's `O_NONBLOCK`
flag is never toggled under another clone of the handle. The drain ends
at the capacity, at an empty queue (`EAGAIN`) or a read that cannot avoid
waiting (`EOPNOTSUPP`), or at any other error, which is left for the next
call's own read to report; `EINTR` repeats the read. A plain packet that
turns out too long for its buffer after the first has already left the
queue, so the batch ends before it and the next `recv` or `recv_batch` on
that handle returns `BufferTooSmall` first, once (a flag per queue). On an
offload queue each position takes the next staged segment (see
"Segmentation offload" above), and a segment too long for its buffer
after the first stays pending instead. Nothing awaits after the first
packet, so a dropped async `recv_batch` has received nothing. The drain
itself is written against read closures, with no I/O of its own, so a
native backend can reuse it. `PacketStream` and `PacketPool` still
receive through the one-packet `recv`.
A downstream type that implements both `PacketIo` and `AsyncPacketIo` and
calls `send_batch` with both traits in scope must name the trait, as it
already must for `recv` and `send`.

### Packet buffers

`PacketStream` yields `Result<PacketBuf>`. Each stream receives into a
`PacketPool`: one zeroed slab allocated once and carved into equally sized
slots (`buf_len + 1` rounded up to 64 bytes, so slots never share a cache
line), with one atomic ownership flag per slot. A receive claims one free
slot, passes exactly `buf_len` bytes of it to the device's `recv`, and
turns the result into a `PacketBuf`: a counted reference to the pool plus a
32-bit offset and length, 16 bytes on 64-bit targets. Dropping the
`PacketBuf` frees the slot with a plain store to its flag. A steady stream
therefore allocates nothing, copies nothing, and takes no lock per packet
at this layer, on both paths. Each stream claims slots through its own
acquirer, which pays for pool references in batches; a stream that owns the
only handle of its pool (always the case for `from_async_device` and
`from_device`) claims without even a compare-and-swap, while streams that
share a pool through `*_with_pool` claim with one. Before this design the
native path allocated a zeroed buffer per packet and the thread bridge
copied every packet into a new `Vec` plus an unbounded-channel node.

- **One item is one packet.** Every `Ok` item is exactly what `recv`
  returned: one IP packet (TUN) or one Ethernet frame (TAP), never a
  fragment, an aggregate, or a buffer with a prefix header, and never
  longer than the pool's `buf_len`. A length past the slot is reported as
  `BufferTooSmall` by one shared mapping on both paths, which also decides
  nothing else; one private predicate decides which errors end a stream.
- **Back-pressure instead of growth.** A stream receives only while its
  pool has a free slot, so the memory it owns is bounded by the pool (plus,
  on the thread bridge, a `sync_channel` with one entry per slot, created
  once). Holding every item stalls the stream instead of growing a queue;
  exhaustion is a wait, never an error, and needs no timer. Streams built
  with `*_with_pool` may share one pool and then share its slots, with no
  fairness between them.
- **Waking.** A native stream that finds no free slot registers its waker
  under the pool mutex, one entry per pending receive, then flags itself in
  the pool's reference count and checks the slots once more. A release
  takes the locked slow path, and wakes it, only when a waiter has flagged
  itself; otherwise releasing is lock-free. A thread-bridge worker instead
  parks on a condvar, which a release notifies only when a worker is
  actually parked. No waker is cloned, woken, or dropped while a lock is
  held, and every wake that runs outside the consumer's own `poll_next` (in
  a release, on the worker thread, or in a destructor) is wrapped in
  `catch_unwind`, so a panicking waker can neither abort the process during
  unwinding nor end another stream.
- **Thread-bridge shutdown.** Dropping the stream sets the worker's stop
  flag under the pool mutex and wakes every parked worker, then drops the
  channel receiver (which discards and releases queued items). A worker
  waiting for a slot or for room in the channel exits promptly; one inside
  a blocking `recv` still exits only when that call returns. Whatever way
  the worker ends, including a panic in `recv`, it disconnects the channel
  and wakes the consumer, so the stream ends with `None`.
- **Unsafe code.** All of it, with every invariant it depends on, lives in
  the pool module: the slab allocation, the pointer arithmetic, the slices
  over one slot or one view, and the pool's own reference count (a release
  that has to wake a waiter keeps its reference until the wake is done, so
  the pool can never be freed under it). Every function there that creates or
  reshapes a view checks that it stays inside its slot, so code outside
  that module cannot break soundness. Its tests also run under Miri in CI.
- **Slots are not re-zeroed** between packets. `PacketBuf` exposes only the
  bytes `recv` reported; a backend that reports bytes it did not write could
  expose an earlier packet's bytes (see `SECURITY.md`).

Linux TUN segmentation offload keeps one packet per item: its receive
split happens behind `recv`, before a slot is filled, and claims one slot
per segment like any other receive. A batched receive (several packets
per call or per item) is not part of the design.

Neither crate depends on Tokio, async-std, or smol directly by default —
`tunnel-lattice`'s `async-io`/`tokio` features (mutually exclusive; enabling
both is a compile error from `tun-rs` itself) are opt-in, so a caller who
enables neither pulls in no async runtime at all.

## Platform and privilege notes

Creating a TUN/TAP device generally requires `CAP_NET_ADMIN` on Linux,
Administrator on Windows, or root on macOS/BSD. `Capability` flags describe
implemented surfaces, not proof the current process is authorized — callers
should still handle a permission error from `DeviceProvider::open` even when
a capability is reported as available.

Capabilities are answered at two levels. The backend itself
(`TunRsBackend`, surfaced as `Tunnel::capabilities()`) reports what the
host supports before any device is opened; an open device's handle reports
that same answer plus flags that only exist per handle (`MAC_MUTATION` on a
TAP handle on Linux and macOS, `SEGMENTATION_OFFLOAD` on a Linux TUN
handle whose queue was verified to use offload framing).
`CapabilityProvider::capabilities()` stays infallible: a capability the
backend cannot confirm is simply absent.

| Flag | Level | `tun-rs` backend |
|---|---|---|
| `DEVICE_MUTATION` | host and handle | always |
| `PERSISTENT_DEVICES` | host and handle | Linux |
| `TAP_DEVICES` | host and handle | per OS, below |
| `MULTI_QUEUE` | host and handle | Linux |
| `NATIVE_ASYNC` | host and handle | with `async-io` or `tokio` |
| `MAC_MUTATION` | handle only | TAP on Linux and macOS |
| `SEGMENTATION_OFFLOAD` | handle only | Linux TUN queue with verified offload framing |

`TAP_DEVICES` is per OS:

| OS | How `TAP_DEVICES` is determined |
|---|---|
| Linux | always reported (the `tun` driver serves TUN and TAP alike) |
| macOS | always reported (`feth` is built into the kernel) |
| Windows | reported only if the tap-windows6 driver (hardware id `tap0901`) is installed |

The Windows answer comes from a SetupAPI driver lookup that follows the one
`tun-rs` performs before creating a TAP adapter (the same hardware-id and
driver-version checks, without selecting the driver) and stops before
anything is registered: it creates no adapter, needs no elevation, runs
once per process on the first call, and is cached (a driver installed
later is noticed after a restart). Either answer is advisory: `open` is
never refused because of it, and a missing driver is still reported by
`open` as `Error::DriverUnavailable`.
