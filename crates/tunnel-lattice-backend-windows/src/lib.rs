//! Experimental native Windows backend. No device API is available yet.

#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(any(windows, test))]
#[allow(dead_code)] // Internal code is not exposed before backend integration.
mod contract;

#[cfg(any(windows, test))]
#[allow(dead_code)] // Internal code is not exposed before backend integration.
mod lifecycle;

#[cfg(windows)]
#[allow(dead_code)] // Internal code is not exposed before backend integration.
mod wintun;
