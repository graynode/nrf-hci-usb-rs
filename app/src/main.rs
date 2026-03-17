//! Thingy:53 USB HCI firmware — application core
//!
//! Runs on the nRF5340 **application core**. Presents the Thingy:53 to the
//! USB host as a Bluetooth HCI adapter (USB class 0xE0) and bridges packets
//! to/from the network core via shared-memory ring buffers.
//!
//! # Task architecture
//!
//! Each USB endpoint lives in its own task; tasks exchange owned packets
//! through static `embassy_sync::channel::Channel`s — no shared mutable
//! references needed.
//!
//! ```
//!  [USB host]
//!     │  Control EP (HCI commands)  ──►  HCI_CMD_CHANNEL  ──►  [cmd_task]  ──► IPC
//!     │  Bulk OUT   (HCI ACL)       ──►  [usb_read_task]         ──────────────► IPC
//!     │  Interrupt IN (HCI events)  ◄──  NET_TO_HOST channel  ◄──  [ipc_recv_task]
//!     │  Bulk IN    (HCI ACL)       ◄──  NET_TO_HOST channel  ◄──  [ipc_recv_task]
//!                                         ▲
//!                                   [usb_write_task]
//! ```
//!
//! IPC is interrupt-driven: the net core triggers IPC Event0 (Channel0) after
//! writing to the N→A ring buffer; `ipc_recv_task` wakes via `event.wait()`.
//! The app core triggers IPC Event1 (Channel1) via `ipc::ipc_send_to_net()`
//! after writing to the A→N ring buffer.

#![no_std]
#![no_main]

use defmt::*;
use embassy_executor::Spawner;
use embassy_nrf::reset::release_network_core;
use embassy_nrf::usb::vbus_detect::HardwareVbusDetect;
use embassy_nrf::{bind_interrupts, ipc as hw_ipc, peripherals, usb};
use embassy_time::Timer;
use embassy_usb::driver::{Endpoint, EndpointOut}; // for wait_enabled() and read()
use embassy_usb::Builder;
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

#[defmt::panic_handler]
fn defmt_panic() -> ! {
    panic_probe::hard_fault();
}

mod ipc;
mod led;
mod usb_hci;

use led::{LedColor, LED_STATE};
use usb_hci::{HciPacket, HCI_CMD_CHANNEL, NET_TO_HOST};

// ── Interrupt bindings ───────────────────────────────────────────────────── //

bind_interrupts!(struct Irqs {
    USBD => usb::InterruptHandler<peripherals::USBD>;
    USBREGULATOR => usb::vbus_detect::InterruptHandler;
    IPC => hw_ipc::InterruptHandler<peripherals::IPC>;
});

// ── Concrete endpoint type aliases ───────────────────────────────────────── //

type UsbDriver = usb::Driver<'static, HardwareVbusDetect>;
type EpIn = <UsbDriver as embassy_usb::driver::Driver<'static>>::EndpointIn;
type EpOut = <UsbDriver as embassy_usb::driver::Driver<'static>>::EndpointOut;

// ── Static storage ───────────────────────────────────────────────────────── //

static HCI_USB_STATE: StaticCell<usb_hci::State> = StaticCell::new();

// ── Entry point ──────────────────────────────────────────────────────────── //

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_nrf::init(Default::default());
    info!("Thingy:53 HCI-USB app core starting");

    // ── RGB LED ──────────────────────────────────────────────────────────── //
    // Yellow = booting / waiting for USB
    spawner.spawn(led::led_task(p.PWM0, p.P1_08, p.P1_06, p.P1_07)).unwrap();
    LED_STATE.signal(LedColor::Yellow);

    // Configure SPU: mark the IPC shared-memory region as non-secure so the
    // network core (which accesses app-core SRAM as a non-secure AHB master)
    // can read and write it without triggering a BusFault.
    //
    // App-core SRAM: 512 KB at 0x2000_0000, SPU divides it into 64 regions of
    // 8 KB each.  IPC_SHMEM_BASE = 0x2003_8000 → region 28.
    // 28 KB of shared buffers (2 × 14 KB) spans regions 28–31.
    {
        let spu = embassy_nrf::pac::SPU_S;
        for i in 28..=31usize {
            spu.ramregion(i).perm().write(|w| {
                w.set_read(true);
                w.set_write(true);
                w.set_secattr(false); // non-secure → net core may access
            });
        }
        info!("SPU: IPC shared memory regions 28-31 set non-secure");
    }

    ipc::ipc_init();

    // Configure the IPC peripheral for interrupt-driven transport:
    //   Event0 / Channel0: net → app  (this core waits, net core triggers)
    //   Event1 / Channel1: app → net  (this core triggers, net core waits)
    // Ipc::new enables the IPC interrupt; the channel config persists after drop.
    {
        let mut ipc_driver = hw_ipc::Ipc::new(p.IPC, Irqs);
        ipc_driver.event0.configure_wait([hw_ipc::IpcChannel::Channel0]);
        ipc_driver.event1.configure_trigger([hw_ipc::IpcChannel::Channel1]);
    }

    release_network_core();

    let driver = usb::Driver::new(p.USBD, Irqs, HardwareVbusDetect::new(Irqs));

    let mut config = embassy_usb::Config::new(0x1915, 0x521F);
    config.manufacturer = Some("Nordic Semiconductor");
    config.product = Some("Thingy:53 HCI USB");
    config.serial_number = Some("THINGY53HCI");
    config.device_class = 0xE0;
    config.device_sub_class = 0x01;
    config.device_protocol = 0x01;
    config.composite_with_iads = false;
    config.max_power = 500;

    static CONFIG_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static BOS_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static MSOS_DESC: StaticCell<[u8; 128]> = StaticCell::new();
    static CONTROL_BUF: StaticCell<[u8; 128]> = StaticCell::new();

    let state = HCI_USB_STATE.init(usb_hci::State::new());
    let mut builder = Builder::new(
        driver,
        config,
        &mut CONFIG_DESC.init([0; 256])[..],
        &mut BOS_DESC.init([0; 256])[..],
        &mut MSOS_DESC.init([0; 128])[..],
        &mut CONTROL_BUF.init([0; 128])[..],
    );

    let (event_ep, acl_in_ep, acl_out_ep) =
        usb_hci::BluetoothHciClass::new(&mut builder, state).split();
    let usb_device = builder.build();

    spawner.spawn(usb_task(usb_device)).unwrap();
    spawner.spawn(usb_write_task(event_ep, acl_in_ep)).unwrap();
    spawner.spawn(usb_read_task(acl_out_ep)).unwrap();
    spawner.spawn(cmd_task()).unwrap();
    spawner.spawn(ipc_recv_task()).unwrap();

    loop {
        Timer::after_secs(60).await;
    }
}

// ── USB device runner ─────────────────────────────────────────────────────── //

#[embassy_executor::task]
async fn usb_task(mut usb: embassy_usb::UsbDevice<'static, UsbDriver>) -> ! {
    usb.run().await;
}

// ── USB writer: NET_TO_HOST channel → Interrupt IN / Bulk IN ─────────────── //
//
// Owns both IN endpoints. Waits for the USB device to be configured, then
// drains the channel and writes each packet to the appropriate endpoint.
// On disconnect, loops back to wait_enabled().

#[embassy_executor::task]
async fn usb_write_task(mut event_ep: EpIn, mut acl_in_ep: EpIn) -> ! {
    loop {
        event_ep.wait_enabled().await;
        info!("USB write task: connected");
        LED_STATE.signal(LedColor::Blue);

        loop {
            let pkt = NET_TO_HOST.receive().await;
            let result = match pkt.indicator {
                0x04 => {
                    debug!("EVT {} bytes → USB", pkt.len);
                    usb_hci::write_ep(&mut event_ep, &pkt.payload[..pkt.len]).await
                }
                0x02 => {
                    debug!("ACL {} bytes → USB", pkt.len);
                    usb_hci::write_ep(&mut acl_in_ep, &pkt.payload[..pkt.len]).await
                }
                other => {
                    warn!("usb_write: unknown indicator 0x{:02X}", other);
                    continue;
                }
            };
            if result.is_err() {
                warn!("USB write error — waiting for reconnect");
                LED_STATE.signal(LedColor::Yellow);
                break;
            }
        }
    }
}

// ── USB reader: Bulk OUT → IPC ────────────────────────────────────────────── //
//
// Owns the OUT endpoint. Reads HCI ACL data sent by the host and forwards it
// to the net core via the IPC ring buffer.

#[embassy_executor::task]
async fn usb_read_task(mut acl_out_ep: EpOut) -> ! {
    let mut buf = [0u8; usb_hci::MAX_ACL_LEN];
    loop {
        acl_out_ep.wait_enabled().await;

        loop {
            match acl_out_ep.read(&mut buf).await {
                Ok(n) => {
                    debug!("USB ACL OUT {} bytes → net", n);
                    let mut pkt = [0u8; 1 + usb_hci::MAX_ACL_LEN];
                    pkt[0] = 0x02;
                    pkt[1..1 + n].copy_from_slice(&buf[..n]);
                    ipc::ipc_send_to_net(&pkt[..1 + n]);
                }
                Err(_) => {
                    warn!("USB ACL OUT disabled — waiting for reconnect");
                    break;
                }
            }
        }
    }
}

// ── Command forwarder: HCI_CMD_CHANNEL → IPC ──────────────────────────────── //

#[embassy_executor::task]
async fn cmd_task() -> ! {
    loop {
        let cmd = HCI_CMD_CHANNEL.receive().await;
        debug!("HCI CMD {} bytes → net", cmd.len);
        let mut pkt = [0u8; 1 + usb_hci::MAX_CMD_LEN];
        pkt[0] = 0x01;
        pkt[1..1 + cmd.len].copy_from_slice(&cmd.data[..cmd.len]);
        ipc::ipc_send_to_net(&pkt[..1 + cmd.len]);
    }
}

// ── IPC receive task: IPC interrupt → NET_TO_HOST channel ────────────────── //
//
// Sleeps until the net core writes a packet and triggers IPC Event0. On wakeup
// drains the entire N→A ring buffer — one `event.wait()` may correspond to
// multiple packets if the net core wrote several before this task ran.
// The channel provides back-pressure: if the USB write task is slow we block
// here rather than dropping packets.

#[embassy_executor::task]
async fn ipc_recv_task() -> ! {
    // Safety: Event0 is exclusively awaited here; no other code steals it.
    let mut event =
        unsafe { hw_ipc::Event::steal::<peripherals::IPC>(hw_ipc::EventNumber::Event0) };
    let mut buf = [0u8; 1 + usb_hci::MAX_HCI_PAYLOAD];
    loop {
        event.wait().await;
        while let Some(n) = ipc::ipc_recv_from_net(&mut buf) {
            if n > 0 {
                let indicator = buf[0];
                let payload_len = (n - 1).min(usb_hci::MAX_HCI_PAYLOAD);
                let mut pkt = HciPacket {
                    indicator,
                    payload: [0u8; usb_hci::MAX_HCI_PAYLOAD],
                    len: payload_len,
                };
                pkt.payload[..payload_len].copy_from_slice(&buf[1..1 + payload_len]);
                NET_TO_HOST.send(pkt).await;
            }
        }
    }
}
