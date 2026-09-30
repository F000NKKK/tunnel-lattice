# tunnel-lattice-platform

Generic provider traits and the `Capability` contract between the domain
model and platform backends. Depends only on `tunnel-lattice-core` — never
on `tunnel-lattice-model`.

## What it provides

- `DeviceProvider`, opening a TUN/TAP device (generic `DeviceConfig`/
  `Device` associated types — this crate never names
  `tunnel_lattice_model` types directly);
- `PacketIo`, synchronous packet transfer on an already-open device handle;
- `DeviceObserver`, reading an open device's current name/MTU/administrative
  state back;
- `DeviceMutator: DeviceObserver`, changing an open device's MTU or
  administrative state, gated by `Capability::DEVICE_MUTATION`;
- `PersistentDevice`, marking an open device to survive process exit,
  gated by `Capability::PERSISTENT_DEVICES`;
- `MultiQueueProvider`, duplicating a hardware-scheduled queue on the same
  device for another thread, gated by `Capability::MULTI_QUEUE` — distinct
  from ordinary multi-threaded use of `PacketIo`, which is always safe on
  every backend since `recv`/`send` take `&self`; see that trait's docs;
- `Capability`, a `bitflags` set of runtime-dependent feature flags
  (`DEVICE_MUTATION`, `PERSISTENT_DEVICES`, `TAP_DEVICES`, `MULTI_QUEUE`,
  `NATIVE_ASYNC`) plus `CapabilityProvider` to report which a connected
  device currently has;
- with the `async` feature: `AsyncPacketIo`, a native non-blocking transfer
  path a backend implements when it has real async I/O rather than a
  blocking syscall — see that trait's docs for why this is preferred over
  `tunnel-lattice-async`'s generic thread-based adapter when available.

## Why this crate exists

This boundary is what lets the initial `tun-rs`-backed implementation
(`tunnel-lattice-backend-tunrs`) be replaced later by hand-written per-OS
TUN/TAP code without moving this crate or the `tunnel-lattice` facade's
public API — see the workspace `ARCHITECTURE.md`, "Backend replacement
plan."

## Usage

This crate is not used directly by application code — see the
`tunnel-lattice` facade. Implement these traits when writing a new backend.

A backend's `PacketIo::recv` and `AsyncPacketIo::recv` must return
`Ok(n)` only after writing all of `buf[..n]`, with `n <= buf.len()`.
A packet that does not fit in `buf` must never be truncated: the backend
discards it, returns `Error::BufferTooSmall`, and keeps the device usable
for the next `recv`. `Device::recv_buffer_len` gives a buffer size that
fits at the device's current MTU.
Callers such as `tunnel-lattice-async`'s buffer pool reuse receive
buffers without clearing them, so reporting bytes that were not written
would expose data from an earlier packet.
