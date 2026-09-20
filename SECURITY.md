# Security Policy

## Supported Versions

Tunnel Lattice has not published a release yet. There is no supported version line;
security fixes will target the latest development state on `main` until a
first release ships.

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
or MTU/administrative-state mutation.

Out of scope: `tun-rs` itself (report upstream), and any code path only
reachable with attacker-controlled `Cargo.toml`/build configuration.

No release has been published yet — see `SUPPORT.md` for current status.
This file will be updated once a first release defines a supported version
line.
