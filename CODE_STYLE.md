# Code style and architecture

This describes patterns in the current implementation; `ARCHITECTURE.md`
defines the detailed contracts. The API is not frozen before 1.0.

## Boundaries

- `core` contains errors and IDs; `model` contains device data; `platform`
  contains generic provider traits and capability flags without depending on
  `model`. The facade binds the contracts and exposes `Tunnel` and `Handle`.
- The current default backend wraps `tun-rs`. A separate Linux backend is in
  progress and is not interchangeable through the public facade yet.
- Packet streaming and pooling live in the async crate. Runtime choices are
  feature-gated; `tokio` and `async-io` are mutually exclusive.
- Device lifecycle and packet I/O belong here; interface addressing and
  routes belong to net-lattice or the host application.

## Rust conventions

- Prefer typed configuration, errors and capability checks to OS-specific
  assumptions. State ownership, cancellation, buffer and concurrency contracts
  explicitly in rustdoc; never silently truncate packets.
- Inject provider implementations for isolated tests. Exercise real device
  lifecycle and failure paths in privileged tests on each supported OS.
- Keep backend and unsafe details behind the provider boundary. Run formatting,
  clippy, tests and rustdoc using the supported feature combinations in CI.

Capabilities do not grant OS privileges or install required drivers.
