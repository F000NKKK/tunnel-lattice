//! Experimental native macOS tunnel backend. No device API is available yet.

#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(any(test, target_os = "macos"))]
#[allow(dead_code)] // TAP packet I/O is not wired up yet.
mod bpf_frame;

#[cfg(target_os = "macos")]
#[allow(dead_code)] // TAP lifecycle is not wired up to the provider yet.
mod feth;

#[cfg(any(test, target_os = "macos"))]
#[allow(dead_code)] // The native backend has not yet been exposed.
mod provider;

#[cfg(any(test, target_os = "macos"))]
#[allow(dead_code)] // The native backend has not yet been exposed.
mod observe;

#[cfg(any(test, target_os = "macos"))]
#[allow(dead_code)] // The native backend has not yet been exposed.
mod utun_frame;

#[cfg(target_os = "macos")]
#[allow(dead_code)] // The native backend has not yet been exposed.
mod utun;
