//! Native error rules for the shared segmentation-offload engine.
//!
//! The engine is not attached to live queues yet. Its blocking read and
//! write callbacks must wait on EAGAIN themselves; the drain callback is
//! nonblocking. Keeping this adapter independent of a runtime lets both
//! synchronous and asynchronous paths use the same framing and rules.

use tunnel_lattice_platform::backend::offload::OffloadRules;
use tunnel_lattice_platform::backend::{DrainStep, Step};

use crate::errno::{self, Class, Op};

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "used when offload queues are enabled")
)]
pub(crate) struct LinuxOffloadRules;

impl OffloadRules<i32> for LinuxOffloadRules {
    fn read_error(&self, err: i32) -> Step {
        match errno::classify(err, Op::OffloadRecv) {
            Class::Retry | Class::DropAndReread => Step::Retry,
            Class::Fail(error) => Step::Done(Err(error)),
            _ => Step::Done(Err(errno::platform(err))),
        }
    }

    fn drain_error(&self, err: &i32) -> DrainStep {
        match errno::classify(*err, Op::OffloadRecv) {
            Class::Retry | Class::DropAndReread => DrainStep::Repeat,
            _ => DrainStep::End,
        }
    }

    fn write_error(&self, err: i32) -> Step {
        match errno::classify(err, Op::Send) {
            Class::Retry => Step::Retry,
            Class::Fail(error) => Step::Done(Err(error)),
            _ => Step::Done(Err(errno::platform(err))),
        }
    }

    fn run_refused(&self, err: &i32) -> bool {
        matches!(
            errno::classify(*err, Op::OffloadSend),
            Class::ResendPerPacket
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tunnel_lattice_core::Error;
    use tunnel_lattice_platform::backend::offload::{OffloadRx, VNET_HDR_NONE};

    #[test]
    fn native_rules_distinguish_retry_drain_and_refused_run() {
        let rules = LinuxOffloadRules;
        assert!(matches!(rules.read_error(libc::EINTR), Step::Retry));
        assert!(matches!(rules.read_error(libc::EINVAL), Step::Retry));
        assert!(matches!(
            rules.read_error(libc::EBADFD),
            Step::Done(Err(Error::Disconnected))
        ));
        assert_eq!(rules.drain_error(&libc::EAGAIN), DrainStep::End);
        assert_eq!(rules.drain_error(&libc::EINVAL), DrainStep::Repeat);
        assert!(matches!(
            rules.write_error(libc::EIO),
            Step::Done(Err(Error::InvalidState))
        ));
        assert!(rules.run_refused(&libc::EINVAL));
        assert!(!rules.run_refused(&libc::EAGAIN));
    }

    #[test]
    fn shared_receiver_retries_a_rejected_frame_then_decodes_the_next() {
        let rx = OffloadRx::new();
        let mut calls = 0;
        let mut out = [0; 8];
        let n = rx
            .recv_blocking(
                &mut out,
                |buf| {
                    calls += 1;
                    if calls == 1 {
                        return Err(libc::EINVAL);
                    }
                    buf[..VNET_HDR_NONE.len()].copy_from_slice(&VNET_HDR_NONE);
                    buf[VNET_HDR_NONE.len()..VNET_HDR_NONE.len() + 4].copy_from_slice(b"data");
                    Ok(VNET_HDR_NONE.len() + 4)
                },
                &LinuxOffloadRules,
            )
            .unwrap();
        assert_eq!(&out[..n], b"data");
        assert_eq!(calls, 2);
    }
}
