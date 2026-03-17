//! USB Bluetooth HCI class implementation.
//!
//! # Endpoint layout
//!
//! | EP   | Type      | Dir | Use |
//! |------|-----------|-----|-----|
//! | EP0  | Control   | B   | HCI Commands (host→ctrl) via class control request |
//! | EP1  | Interrupt | IN  | HCI Events  (ctrl→host) |
//! | EP2  | Bulk      | IN  | HCI ACL data (ctrl→host) |
//! | EP2  | Bulk      | OUT | HCI ACL data (host→ctrl) |

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_usb::control::{OutResponse, Recipient, Request, RequestType};
use embassy_usb::driver::{Direction, Driver, EndpointAddress, EndpointError, EndpointIn};
use embassy_usb::{Builder, Handler};

use defmt::*;

const BT_CLASS: u8 = 0xE0;
const BT_SUBCLASS: u8 = 0x01;
const BT_PROTOCOL: u8 = 0x01;

/// Maximum HCI command payload (opcode 2B + param_len 1B + params up to 255B).
pub const MAX_CMD_LEN: usize = 258;
/// Maximum HCI ACL payload for BLE (header 4B + PDU up to 1020B, rounded to 1024).
pub const MAX_ACL_LEN: usize = 1024;
/// Maximum payload across all packet types (used for the net→host channel).
pub const MAX_HCI_PAYLOAD: usize = MAX_ACL_LEN; // 1024 > 257

const CHANNEL_DEPTH: usize = 4;

// ── Shared packet type ────────────────────────────────────────────────────────

/// Owned HCI packet passed through channels between tasks.
pub struct HciPacket {
    /// HCI indicator byte: 0x01=Command, 0x02=ACL, 0x04=Event.
    pub indicator: u8,
    pub payload: [u8; MAX_HCI_PAYLOAD],
    pub len: usize, // valid bytes in payload
}

/// Channel carrying HCI packets from the net core to the USB host.
pub static NET_TO_HOST: Channel<CriticalSectionRawMutex, HciPacket, CHANNEL_DEPTH> =
    Channel::new();

// ── HCI Command channel (Control EP → bridge) ─────────────────────────────────

pub struct HciCmdBuf {
    pub data: [u8; MAX_CMD_LEN],
    pub len: usize,
}

/// HCI commands received on the USB control endpoint.
pub static HCI_CMD_CHANNEL: Channel<CriticalSectionRawMutex, HciCmdBuf, CHANNEL_DEPTH> =
    Channel::new();

// ── Control-EP handler ────────────────────────────────────────────────────────

pub struct State {
    inner: HciHandler,
}

impl State {
    pub const fn new() -> Self {
        Self { inner: HciHandler }
    }
}

struct HciHandler;

impl Handler for HciHandler {
    fn configured(&mut self, configured: bool) {
        debug!("USB HCI configured: {}", configured);
    }

    fn control_out(&mut self, req: Request, data: &[u8]) -> Option<OutResponse> {
        // The Bluetooth HCI USB transport spec (Vol 4, Part B §2.2.1) sends HCI
        // commands as class requests to the *device* (bmRequestType=0x20).
        // Some hosts use interface recipient (0x21); accept both for compatibility.
        if req.request_type == RequestType::Class
            && req.request == 0x00
            && matches!(req.recipient, Recipient::Device | Recipient::Interface)
        {
            let len = data.len().min(MAX_CMD_LEN);
            let mut cmd = HciCmdBuf {
                data: [0u8; MAX_CMD_LEN],
                len,
            };
            cmd.data[..len].copy_from_slice(&data[..len]);
            debug!("HCI CMD {} bytes from USB", len);
            if HCI_CMD_CHANNEL.try_send(cmd).is_err() {
                warn!("HCI CMD channel full – dropping command");
            }
            Some(OutResponse::Accepted)
        } else {
            None
        }
    }
}

// ── Class ─────────────────────────────────────────────────────────────────────

pub struct BluetoothHciClass<'d, D: Driver<'d>> {
    event_ep: D::EndpointIn,
    acl_in_ep: D::EndpointIn,
    acl_out_ep: D::EndpointOut,
}

impl<'d, D: Driver<'d>> BluetoothHciClass<'d, D> {
    pub fn new(builder: &mut Builder<'d, D>, state: &'d mut State) -> Self {
        let (event_ep, acl_in_ep, acl_out_ep) = {
            let mut func = builder.function(BT_CLASS, BT_SUBCLASS, BT_PROTOCOL);
            let mut iface = func.interface();
            let mut alt = iface.alt_setting(BT_CLASS, BT_SUBCLASS, BT_PROTOCOL, None);
            // Explicit endpoint addresses to match Bumble's hardcoded expectations:
            //   0x81 = Interrupt IN  (HCI Events)
            //   0x82 = Bulk IN       (HCI ACL data to host)
            //   0x02 = Bulk OUT      (HCI ACL data from host)
            // IN and OUT allocators are independent; without explicit addresses the
            // OUT endpoint would land at 0x01 (EP1 OUT), not 0x02.
            let event_ep  = alt.endpoint_interrupt_in(Some(EndpointAddress::from_parts(1, Direction::In)), 64, 1);
            let acl_in_ep = alt.endpoint_bulk_in(Some(EndpointAddress::from_parts(2, Direction::In)), 64);
            let acl_out_ep = alt.endpoint_bulk_out(Some(EndpointAddress::from_parts(2, Direction::Out)), 64);
            (event_ep, acl_in_ep, acl_out_ep)
            // alt, iface, func dropped here → builder borrow released
        };

        builder.handler(&mut state.inner);

        Self { event_ep, acl_in_ep, acl_out_ep }
    }

    /// Consume the class and yield the three raw endpoints.
    ///
    /// Each endpoint can then be moved into its own dedicated task, solving
    /// the multiple-mutable-borrow problem without unsafe code.
    pub fn split(self) -> (D::EndpointIn, D::EndpointIn, D::EndpointOut) {
        (self.event_ep, self.acl_in_ep, self.acl_out_ep)
    }
}

// ── Endpoint helpers used by tasks ────────────────────────────────────────────

/// Write `data` to an IN endpoint, chunking at 64 bytes and appending a ZLP
/// when the final chunk is a full packet (so the host knows the transfer ended).
pub async fn write_ep<E: EndpointIn>(ep: &mut E, data: &[u8]) -> Result<(), EndpointError> {
    for chunk in data.chunks(64) {
        ep.write(chunk).await?;
    }
    if data.len() % 64 == 0 {
        ep.write(&[]).await?; // ZLP
    }
    Ok(())
}
