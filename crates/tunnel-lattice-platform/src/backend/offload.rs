//! Segmentation offload for a Linux TUN queue: the virtio-net codec and the
//! queue-level engine that uses it.
//!
//! A Linux TUN queue opened with `IFF_VNET_HDR` puts a 10-byte
//! `struct virtio_net_hdr` in front of every packet, in both directions.
//! With the kernel's TSO/USO offloads enabled, one read can return a
//! *super-packet* of up to 64 KiB whose header asks the reader to split it
//! into `gso_size`-byte segments, and one write can hand the kernel such a
//! super-packet for it to split.
//!
//! Everything here is safe, portable Rust that compiles and is tested on
//! every host: it does no I/O and calls no operating system function. A
//! backend supplies the native parts, which stay in the backend: the
//! `TUNGETIFF`/`TUNGETVNETHDRSZ`/`TUNSETOFFLOAD` calls that observe a queue's
//! framing, the reads and writes themselves (as closures or an
//! [`AsyncGatherWrite`]), and the table that says what a failed read or write
//! means (an [`OffloadRules`] value).
//!
//! # The codec
//!
//! - [`VirtioNetHdr`] decodes and encodes the header. It is native-endian,
//!   the legacy framing a TUN device uses unless someone set
//!   `TUNSETVNETLE`/`TUNSETVNETBE` on it.
//! - [`SplitCursor`] validates one received frame, then writes it out one
//!   IP packet at a time into a caller buffer. It synthesizes each
//!   segment's IP and TCP/UDP headers with full checksums, or completes the
//!   partial checksum of a non-GSO packet.
//! - [`plan_run`] finds the run of adjacent same-flow packets at the head of
//!   a send batch that the kernel will split back into exactly those
//!   packets, and builds the header bytes to write in front of their
//!   payloads.
//! - [`checksum`], [`sum_words`] and [`fold`] compute the Internet checksum.
//!
//! Nothing in the codec can panic on any input: offsets use checked
//! arithmetic, every slice access goes through `get`, and malformed input
//! becomes a [`DropReason`] (the caller drops the packet and reads again).
//! It never allocates.
//!
//! # Framing follows the device, not the request
//!
//! `IFF_VNET_HDR` is a device-wide flag, and the kernel does not apply a
//! queue's requested flags when it attaches to a multi-queue device that
//! already has queues. So after every Linux TUN open (and every added
//! queue), the backend reads the device's flags and, if the flag is set, its
//! header size, and [`decide_framing`] picks the framing.
//!
//! # The engine
//!
//! - [`OffloadRx`] is the receive staging of one offload-framed queue, and
//!   [`Staging`] its locked state. A `recv` serves the next pending segment
//!   of the last frame read, or reads one frame and serves its first
//!   segment. A `recv_batch` fills position 0 the same way, then drains:
//!   the pending segments go into the next buffers in order, and once none
//!   is pending the next frame is read with a *non-waiting* read and split
//!   on. A segment is sized before it is copied, so one that does not fit
//!   stays pending and ends the batch.
//! - [`send_batch_blocking`] and, with the `async` feature,
//!   [`send_batch_async`] send at most [`MAX_SEGMENTS`] packets per call,
//!   coalescing runs of one flow into single gather writes and following
//!   the `send_batch` prefix contract.
//!
//! The engine never names an operating system, a device kind or an error
//! type; those arrive through [`OffloadRules`].

mod codec;
mod queue;

pub use codec::*;
pub use queue::*;
