# Native Windows backend

This unpublished crate is an experimental internal implementation. It has no
public API and cannot yet create usable Tunnel Lattice devices. Its first slice
loads the external Wintun DLL and models adapter and session ownership. It also
contains internal nonblocking Wintun packet operations, with receive-buffer
ownership and mock tests; these are not yet a usable device API. TAP, device
configuration, blocking and async waits, facade integration and real-device
validation remain to be implemented.

Wintun and tap-windows6 are external drivers; this project does not ship either.
The existing `tun-rs` backend remains the functional Windows implementation.
