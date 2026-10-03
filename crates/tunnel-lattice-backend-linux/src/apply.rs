//! The `DeviceMutator::apply` steps on a real link.
//!
//! The contract itself (preconditions, step order, compensation) is the
//! shared `tunnel_lattice_model::backend::apply_patch`; this module only
//! supplies the native steps it drives:
//!
//! - each read is one `RTM_GETLINK` by interface index;
//! - each change is one `RTM_NEWLINK` by interface index.
//!
//! After any `Err`, `snapshot` is authoritative.

use tunnel_lattice_core::Result;
use tunnel_lattice_model::MacAddress;
use tunnel_lattice_model::backend::ApplySteps;

use crate::control::{LinkChange, LinkQuery, RouteSocket};
use crate::errno;

/// The steps on a real link: one `RTM_GETLINK` per read and one
/// `RTM_NEWLINK` per change, by interface index.
pub(crate) struct LinkSteps<'a> {
    /// The device's control socket, locked for the whole `apply`.
    pub(crate) socket: &'a mut RouteSocket,
    /// The device's interface index.
    pub(crate) index: u32,
}

impl ApplySteps for LinkSteps<'_> {
    /// An MTU that does not fit `u16` cannot be restored through the shared
    /// contract; it is reported as a malformed reply, so `apply` changes
    /// nothing. The kernel's `tun` driver never reports one.
    fn mtu(&mut self) -> Result<u16> {
        let mtu = self.socket.get_link(LinkQuery::Index(self.index))?.mtu;
        u16::try_from(mtu).map_err(|_| errno::malformed())
    }

    fn set_mtu(&mut self, mtu: u16) -> Result<()> {
        self.socket
            .set_link(self.index, LinkChange::Mtu(u32::from(mtu)))
    }

    /// A link with no hardware address cannot have it restored; that is
    /// reported as a malformed reply, so `apply` changes nothing.
    fn mac(&mut self) -> Result<MacAddress> {
        self.socket
            .get_link(LinkQuery::Index(self.index))?
            .mac
            .map(MacAddress::new)
            .ok_or_else(errno::malformed)
    }

    fn set_mac(&mut self, mac: MacAddress) -> Result<()> {
        self.socket
            .set_link(self.index, LinkChange::Mac(mac.octets()))
    }

    fn set_up(&mut self, up: bool) -> Result<()> {
        self.socket.set_link(self.index, LinkChange::AdminUp(up))
    }
}
