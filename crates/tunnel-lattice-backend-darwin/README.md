# Native Darwin backend

This unpublished crate is under development and has no public API. An
internal, TUN-only provider can open an automatically named macOS `utun`
socket, but its native path has not yet been validated on macOS. Requested
names and MTU at open are unsupported. Snapshot and post-open MTU/admin
mutation are implemented but unverified on a real device; TAP, async I/O,
facade integration and real-device validation remain future work. Use the
existing `tun-rs` backend for functioning macOS support.
