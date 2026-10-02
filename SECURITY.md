# Security Policy

## Supported Versions

Only the latest published `0.x` release is supported. Until `1.0.0`, any
new `0.x` minor release may change the public API, including in breaking
ways, and every such change is listed in `CHANGELOG.md`; security fixes
target the latest release and `main`, not older `0.x` versions.

| Version | Supported |
| ------- | --------- |
| 0.6.x   | ✅ |
| < 0.6.0 | ❌ |

## Reporting a Vulnerability

If you discover a security vulnerability in Tunnel Lattice, please **do not** open a
public GitHub issue.

Instead, report it privately using
[GitHub's private vulnerability reporting](https://github.com/F000NKKK/tunnel-lattice/security/advisories/new)
feature for this repository.

Please include as much of the following information as possible:

- A description of the vulnerability and its potential impact
- Steps to reproduce the issue
- Affected versions or commits, if known
- Any suggested mitigations

We will make a best effort to acknowledge reports promptly and to keep you
informed as the issue is investigated and resolved.

## Scope

In scope: `tunnel-lattice-backend-tunrs`'s handling of untrusted packet data
read from an open device, and any privilege-boundary bug in device creation
or MTU/administrative-state mutation. On a Linux TUN queue with
segmentation offload this includes the backend's own parsing of the
kernel's virtio-net header and its splitting and coalescing of
super-packets: a panic, an out-of-bounds read, or a segment built from
bytes outside the received frame there is a reportable bug.

Changing a device's offload mask is device-wide and affects every queue
and process using that device (see the backend README), but it needs the
same privilege as attaching to the device in the first place, so it is not
a privilege-boundary issue by itself.

Out of scope: `tun-rs` itself (report upstream), and any code path only
reachable with attacker-controlled `Cargo.toml`/build configuration.

## Packet buffer reuse

`tunnel-lattice-async`'s `PacketStream` receives packets into the slots of a
`PacketPool` and reuses those slots without re-zeroing them. A `PacketBuf`
only exposes the bytes the device's `recv` reported, so this is safe as long
as the backend honors `recv`'s contract that every reported byte was
written. A backend that reports more bytes than it wrote could expose stale
bytes from an earlier packet, possibly one received by another stream
sharing the same pool. That is an information leak, not undefined
behaviour, and `tunnel-lattice-backend-tunrs` conforms. If streams for
different trust domains must not see each other's data, give each stream
its own pool (the default for `packet_stream` and the `buf_len`
constructors). A backend that violates this contract is in scope for a
report.
