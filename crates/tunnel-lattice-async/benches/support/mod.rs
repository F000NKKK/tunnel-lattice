//! Shared mock devices and the frozen `Vec`-per-packet baseline for the
//! packet-path benchmarks.

pub mod baseline;

use std::future::{Future, ready};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};

use tunnel_lattice_async::{Error, Result};
use tunnel_lattice_platform::{AsyncPacketIo, PacketIo};

/// Number of packets each measured iteration receives.
pub const N: usize = 10_000;

/// `(payload, buf_len)` pairs: small, mid, full-MTU TUN, full-MTU TAP
/// (1500 + 14 Ethernet header in a 1518 buffer), and jumbo.
pub const PAIRS: [(usize, usize); 5] = [
    (64, 1500),
    (576, 1500),
    (1500, 1500),
    (1514, 1518),
    (9018, 9018),
];

/// A finite in-memory device: `count` packets of `payload` bytes, then
/// `Error::Disconnected` forever. Each `recv` writes the payload into the
/// caller's buffer, standing in for the kernel's copy-out.
///
/// With [`FiniteDevice::gated`], the first `recv` blocks until
/// [`FiniteDevice::open_gate`] is called, so a thread-bridge worker cannot
/// produce packets before the timed routine starts.
pub struct FiniteDevice {
    remaining: AtomicUsize,
    payload: usize,
    gate: Option<Gate>,
}

struct Gate {
    open: Mutex<bool>,
    cv: Condvar,
    arrived: AtomicBool,
    passed: AtomicBool,
}

impl FiniteDevice {
    /// An ungated device.
    pub fn new(count: usize, payload: usize) -> Self {
        Self {
            remaining: AtomicUsize::new(count),
            payload,
            gate: None,
        }
    }

    /// A device whose first `recv` waits for [`FiniteDevice::open_gate`].
    pub fn gated(count: usize, payload: usize) -> Self {
        Self {
            gate: Some(Gate {
                open: Mutex::new(false),
                cv: Condvar::new(),
                arrived: AtomicBool::new(false),
                passed: AtomicBool::new(false),
            }),
            ..Self::new(count, payload)
        }
    }

    /// Releases a `recv` blocked on the gate. No-op on an ungated device.
    pub fn open_gate(&self) {
        if let Some(gate) = &self.gate {
            *gate.open.lock().unwrap() = true;
            gate.cv.notify_all();
        }
    }

    /// Waits until a `recv` is blocked on the gate, so a worker thread's
    /// start-up happens before the timed routine, not inside it. Returns
    /// at once on an ungated device.
    pub fn wait_arrived(&self) {
        if let Some(gate) = &self.gate {
            while !gate.arrived.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
        }
    }

    fn wait_gate(&self) {
        let Some(gate) = &self.gate else { return };
        if gate.passed.load(Ordering::Acquire) {
            return;
        }
        let mut open = gate.open.lock().unwrap();
        gate.arrived.store(true, Ordering::Release);
        while !*open {
            open = gate.cv.wait(open).unwrap();
        }
        gate.passed.store(true, Ordering::Release);
    }

    fn recv_now(&self, buf: &mut [u8]) -> Result<usize> {
        self.wait_gate();
        let took = self
            .remaining
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
            .is_ok();
        if !took {
            return Err(Error::Disconnected);
        }
        let n = self.payload.min(buf.len());
        buf[..n].fill(0xA5);
        Ok(n)
    }
}

impl PacketIo for FiniteDevice {
    fn recv(&self, buf: &mut [u8]) -> Result<usize> {
        self.recv_now(buf)
    }

    fn send(&self, buf: &[u8]) -> Result<usize> {
        Ok(buf.len())
    }
}

impl AsyncPacketIo for FiniteDevice {
    fn recv(&self, buf: &mut [u8]) -> impl Future<Output = Result<usize>> + Send {
        ready(self.recv_now(buf))
    }

    fn send(&self, buf: &[u8]) -> impl Future<Output = Result<usize>> + Send {
        ready(Ok(buf.len()))
    }
}
