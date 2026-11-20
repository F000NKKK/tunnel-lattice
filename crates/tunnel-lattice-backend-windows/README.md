# Native Windows backend

This unpublished crate is an experimental internal implementation. It has no
public API and cannot yet create usable Tunnel Lattice devices. Its first slice
loads the external Wintun DLL and models adapter and session ownership; packet
I/O, TAP, device configuration, async I/O and facade integration remain to be
implemented and tested on real Windows devices.

Wintun and tap-windows6 are external drivers; this project does not ship either.
The existing `tun-rs` backend remains the functional Windows implementation.
