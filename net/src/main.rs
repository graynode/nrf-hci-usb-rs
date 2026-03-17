//! Thingy:53 HCI IPC firmware — network core
//!
//! This binary runs on the nRF5340 **network core** and implements the same
//! role as Zephyr's `hci_ipc` sample: it runs the Nordic SoftDevice Controller
//! (SDC) and bridges HCI packets between the SDC and the application core via
//! a shared-memory IPC protocol.
//!
//! # Packet flow
//!
//! ```
//!  App Core (USB HCI)                     Net Core (this binary)
//!  ─────────────────                      ──────────────────────
//!  ipc_send_to_net(cmd/acl) ──IPC──►  ipc_recv_from_app()
//!                                          │
//!                                          ▼
//!                                      sdc_hci_cmd_*/hci_data_put
//!                                          │
//!                                          ▼  hci_get()
//!  ipc_recv_from_net(evt/acl) ◄─IPC── ipc_send_to_app(evt/acl)
//! ```
//!
//! # IPC shared memory
//!
//! See the `thingy53-ipc` crate for the ring-buffer layout. Both cores use the
//! same physical addresses via the nRF5340 AHB bus.
//!
//! # IPC signalling
//!
//! Interrupt-driven via the nRF5340 IPC peripheral (`embassy_nrf::ipc`):
//! - **Event0 / Channel0** (net → app): triggered here after `ipc_send_to_app`;
//!   the app core's `ipc_recv_task` wakes from `event.wait()`.
//! - **Event1 / Channel1** (app → net): waited here in `ipc_rx_loop`;
//!   the app core triggers after `ipc_send_to_net`.
//!
//! # HCI command dispatch
//!
//! `sdc_hci_cmd_put` was removed from the SDC C library.  Commands must now be
//! dispatched via individual typed `sdc_hci_cmd_*` functions.  Because those
//! functions return the HCI status synchronously (they do **not** queue a
//! Command Complete event), we build the event ourselves and enqueue it on the
//! N→A ring buffer so the app core can forward it to the USB host.

#![no_std]
#![no_main]

use defmt::*;
use embassy_executor::Spawner;
use embassy_nrf::{bind_interrupts, ipc as hw_ipc, peripherals};
use nrf_mpsl::{MultiprotocolServiceLayer, raw as mpsl_raw};
use nrf_sdc::{self as sdc, SoftdeviceController, raw as sdc_raw};
use static_cell::StaticCell;
use thingy53_ipc as ipc;
use {defmt_rtt as _, panic_probe as _};

#[defmt::panic_handler]
fn defmt_panic() -> ! {
    panic_probe::hard_fault();
}

// ── Interrupt bindings ───────────────────────────────────────────────────── //
//
// nRF5340 net-core interrupt names.  MPSL requires EGU0 (low-priority),
// CLOCK_POWER, RADIO, TIMER0, and RTC0.  RNG uses blocking mode, no ISR.
// IPC is added for interrupt-driven ring-buffer signalling from the app core.

bind_interrupts!(struct Irqs {
    EGU0        => nrf_mpsl::LowPrioInterruptHandler;
    CLOCK_POWER => nrf_mpsl::ClockInterruptHandler;
    RADIO       => nrf_mpsl::HighPrioInterruptHandler;
    TIMER0      => nrf_mpsl::HighPrioInterruptHandler;
    RTC0        => nrf_mpsl::HighPrioInterruptHandler;
    IPC         => hw_ipc::InterruptHandler<peripherals::IPC>;
});

// ── Entry point ──────────────────────────────────────────────────────────── //

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_nrf::init(Default::default());
    info!("Thingy:53 HCI IPC net core starting");

    // ── IPC peripheral configuration ──────────────────────────────────────── //
    //
    // Event0 / Channel0: net → app  (this core triggers, app core waits)
    // Event1 / Channel1: app → net  (this core waits,    app core triggers)
    // Ipc::new enables the IPC interrupt; channel config persists after drop.
    {
        let mut ipc_driver = hw_ipc::Ipc::new(p.IPC, Irqs);
        ipc_driver.event0.configure_trigger([hw_ipc::IpcChannel::Channel0]);
        ipc_driver.event1.configure_wait([hw_ipc::IpcChannel::Channel1]);
    }

    // ── Clock configuration ───────────────────────────────────────────────── //
    //
    // Use the internal RC oscillator for the low-frequency clock.
    // For production consider switching to LF_SRC_XTAL.
    let lfclk_cfg = mpsl_raw::mpsl_clock_lfclk_cfg_t {
        source: mpsl_raw::MPSL_CLOCK_LF_SRC_RC as u8,
        rc_ctiv: mpsl_raw::MPSL_RECOMMENDED_RC_CTIV as u8,
        rc_temp_ctiv: mpsl_raw::MPSL_RECOMMENDED_RC_TEMP_CTIV as u8,
        accuracy_ppm: mpsl_raw::MPSL_WORST_CLOCK_ACCURACY_PPM as u16,
        skip_wait_lfclk_started: mpsl_raw::MPSL_DEFAULT_SKIP_WAIT_LFCLK_STARTED != 0,
    };

    // ── MPSL initialisation ───────────────────────────────────────────────── //
    //
    // nRF53 net-core MPSL peripherals: RTC0, TIMER0, TIMER1, TEMP, PPI 0-2.
    let mpsl_p = nrf_mpsl::Peripherals::new(
        p.RTC0, p.TIMER0, p.TIMER1, p.TEMP,
        p.PPI_CH0, p.PPI_CH1, p.PPI_CH2,
    );

    static MPSL: StaticCell<MultiprotocolServiceLayer> = StaticCell::new();
    let mpsl = MPSL.init(
        MultiprotocolServiceLayer::new::<embassy_nrf::interrupt::typelevel::EGU0, _>(
            mpsl_p, Irqs, lfclk_cfg,
        ).unwrap(),
    );
    spawner.must_spawn(mpsl_task(mpsl));

    // ── SDC initialisation ────────────────────────────────────────────────── //
    //
    // nRF53 net-core SDC PPI channels: PPI_CH3..=PPI_CH12.
    let sdc_p = sdc::Peripherals::new(
        p.PPI_CH3, p.PPI_CH4, p.PPI_CH5, p.PPI_CH6, p.PPI_CH7,
        p.PPI_CH8, p.PPI_CH9, p.PPI_CH10, p.PPI_CH11, p.PPI_CH12,
    );

    // RNG in blocking mode – no interrupt binding needed.
    let mut rng = embassy_nrf::rng::Rng::new_blocking(p.RNG);

    // Memory for SDC — must be large enough for all enabled features.
    // 8 KiB covers ext adv + periodic sync + BIS sink buffers.
    static SDC_MEM: StaticCell<sdc::Mem<8192>> = StaticCell::new();

    let controller = sdc::Builder::new()
        .unwrap()
        .support_adv()
        .support_ext_adv()
        .support_scan()
        .support_ext_scan()
        .support_le_periodic_adv()
        .support_le_periodic_sync()
        .support_bis_sink()     // BLE audio broadcast receive (Auracast / BIS)
        .support_peripheral()
        .support_central()
        // 1 BIG (Broadcast Isochronous Group) with 2 BIS sinks
        .big_count(1).unwrap()
        .bis_sink_count(2).unwrap()
        .iso_buffer_cfg(
            0,    // tx_sdu_buffer_count  (sink only → no TX)
            0,    // tx_sdu_buffer_size
            0,    // tx_pdu_buffer_per_stream_count
            4,    // rx_pdu_buffer_per_stream_count
            2,    // rx_sdu_buffer_count
            251,  // rx_sdu_buffer_size (max LE SDU size)
        ).unwrap()
        .build(sdc_p, &mut rng, mpsl, SDC_MEM.init(sdc::Mem::new()))
        .unwrap();

    info!("SDC initialised; running HCI bridge");

    // Run both HCI bridge loops as joined futures in main.
    // This avoids the 'static lifetime requirement for the controller and rng —
    // both live for the duration of main, which never returns.
    embassy_futures::join::join(
        sdc_event_loop(&controller),
        ipc_rx_loop(&controller),
    ).await;
}

// ── MPSL runner ──────────────────────────────────────────────────────────── //

#[embassy_executor::task]
async fn mpsl_task(mpsl: &'static MultiprotocolServiceLayer<'static>) -> ! {
    mpsl.run().await
}

// ── SDC event loop ──────────────────────────────────────────────────────────── //
//
// Polls the SDC for asynchronous BLE events (advertising reports, connection
// complete, disconnection complete, ACL data …) and forwards them to the app
// core via the IPC ring buffer, then triggers IPC Event0 to wake the app core.
//
// NOTE: command-complete events for *synchronous* commands are NOT returned
// by hci_get; they are generated in dispatch_hci_cmd instead.

async fn sdc_event_loop(sdc: &SoftdeviceController<'_>) {
    let mut buf = [0u8; sdc_raw::HCI_MSG_BUFFER_MAX_SIZE as usize];
    loop {
        match sdc.hci_get(&mut buf).await {
            Ok(kind) => {
                let indicator: u8 = match kind {
                    bt_hci::PacketKind::Event   => 0x04,
                    bt_hci::PacketKind::AclData => 0x02,
                    _ => continue, // ISO not forwarded
                };

                let pkt_len = hci_packet_len(indicator, &buf);
                debug!("SDC→IPC: ind=0x{:02X} len={}", indicator, pkt_len);

                let mut ipc_buf = [0u8; 1 + sdc_raw::HCI_MSG_BUFFER_MAX_SIZE as usize];
                ipc_buf[0] = indicator;
                ipc_buf[1..1 + pkt_len].copy_from_slice(&buf[..pkt_len]);

                notify_app(&ipc_buf[..1 + pkt_len]);
            }
            Err(e) => error!("SDC hci_get error: {:?}", e),
        }
    }
}

// ── IPC RX loop ──────────────────────────────────────────────────────────── //
//
// Waits on IPC Event1 (triggered by the app core after writing to the A→N
// ring) then drains the ring buffer and dispatches each packet to the SDC.

async fn ipc_rx_loop(sdc: &SoftdeviceController<'_>) {
    // Safety: Event1 is exclusively awaited here; no other code steals it.
    let mut event =
        unsafe { hw_ipc::Event::steal::<peripherals::IPC>(hw_ipc::EventNumber::Event1) };
    let mut buf = [0u8; 1 + sdc_raw::HCI_MSG_BUFFER_MAX_SIZE as usize];
    loop {
        event.wait().await;
        while let Some(n) = ipc::ipc_recv_from_app(&mut buf) {
            if n == 0 {
                continue;
            }
            let indicator = buf[0];
            let payload   = &buf[1..n];
            debug!("IPC→SDC: ind=0x{:02X} len={}", indicator, payload.len());

            match indicator {
                0x01 => {
                    if !dispatch_hci_cmd(payload) {
                        warn!("Unhandled HCI command opcode 0x{:04X}",
                              u16::from_le_bytes([payload[0], payload[1]]));
                    }
                }
                0x02 => {
                    if let Err(e) = sdc.hci_data_put(payload) {
                        error!("hci_data_put failed: {:?}", e);
                    }
                }
                other => warn!("IPC RX: unknown indicator 0x{:02X}", other),
            }
        }
    }
}

// ── HCI command dispatcher ────────────────────────────────────────────────── //
//
// Routes raw HCI command bytes (opcode[2] + param_len[1] + params[...]) to the
// appropriate sdc_hci_cmd_* C function.  Because the C functions return the
// HCI status synchronously we build the Command Complete (or Command Status for
// asynchronous commands) event and enqueue it on the N→A ring buffer.
//
// The C structs produced by bindgen are layout-compatible with the raw HCI
// parameter bytes (both are packed little-endian), so we can safely cast the
// byte pointer.  Alignment is guaranteed to be 1 byte for all the generated
// structs.

fn dispatch_hci_cmd(payload: &[u8]) -> bool {
    if payload.len() < 2 {
        return false;
    }
    let opcode = u16::from_le_bytes([payload[0], payload[1]]);
    // HCI command params start after opcode(2) + param_len(1).
    let params = if payload.len() > 3 { &payload[3..] } else { &[] };

    // Helper: send Command Complete event with status only.
    macro_rules! cc {
        ($status:expr) => {
            send_cc_event(opcode, $status, &[])
        };
    }

    // Command with params only (no return data).
    macro_rules! cmd_p {
        ($fn:ident, $t:ident) => {{
            let sz = core::mem::size_of::<sdc_raw::$t>();
            let status = if params.len() >= sz {
                unsafe { sdc_raw::$fn(params.as_ptr() as *const sdc_raw::$t) }
            } else {
                0x12 // Invalid HCI Command Parameters
            };
            cc!(status);
            true
        }};
    }

    // Command with no params (no return data).
    macro_rules! cmd_n {
        ($fn:ident) => {{
            let status = unsafe { sdc_raw::$fn() };
            cc!(status);
            true
        }};
    }

    // Command with return data only (no params).
    macro_rules! cmd_r {
        ($fn:ident, $rt:ident) => {{
            let mut ret = unsafe { core::mem::zeroed::<sdc_raw::$rt>() };
            let status = unsafe { sdc_raw::$fn(&mut ret) };
            let bytes = unsafe {
                core::slice::from_raw_parts(
                    &ret as *const _ as *const u8,
                    core::mem::size_of::<sdc_raw::$rt>(),
                )
            };
            send_cc_event(opcode, status, bytes);
            true
        }};
    }

    // Command with params AND return data.
    macro_rules! cmd_pr {
        ($fn:ident, $pt:ident, $rt:ident) => {{
            let sz = core::mem::size_of::<sdc_raw::$pt>();
            if params.len() >= sz {
                let mut ret = unsafe { core::mem::zeroed::<sdc_raw::$rt>() };
                let status = unsafe {
                    sdc_raw::$fn(
                        params.as_ptr() as *const sdc_raw::$pt,
                        &mut ret,
                    )
                };
                let bytes = unsafe {
                    core::slice::from_raw_parts(
                        &ret as *const _ as *const u8,
                        core::mem::size_of::<sdc_raw::$rt>(),
                    )
                };
                send_cc_event(opcode, status, bytes);
            } else {
                cc!(0x12u8);
            }
            true
        }};
    }

    // Asynchronous command: send Command Status event immediately; the actual
    // result event arrives later via hci_get.
    macro_rules! cmd_async_p {
        ($fn:ident, $t:ident) => {{
            let sz = core::mem::size_of::<sdc_raw::$t>();
            let status = if params.len() >= sz {
                unsafe { sdc_raw::$fn(params.as_ptr() as *const sdc_raw::$t) }
            } else {
                0x12
            };
            send_cs_event(opcode, status);
            true
        }};
    }

    macro_rules! cmd_async_n {
        ($fn:ident) => {{
            let status = unsafe { sdc_raw::$fn() };
            send_cs_event(opcode, status);
            true
        }};
    }

    match opcode {
        // ── Controller & Baseband (OGF 0x03) ──────────────────────────────── //
        0x0C01 => cmd_p!(sdc_hci_cmd_cb_set_event_mask,
                         sdc_hci_cmd_cb_set_event_mask_t),
        0x0C03 => cmd_n!(sdc_hci_cmd_cb_reset),
        0x0C31 => cmd_p!(sdc_hci_cmd_cb_set_controller_to_host_flow_control,
                         sdc_hci_cmd_cb_set_controller_to_host_flow_control_t),
        0x0C33 => cmd_p!(sdc_hci_cmd_cb_host_buffer_size,
                         sdc_hci_cmd_cb_host_buffer_size_t),
        0x0C35 => cmd_p!(sdc_hci_cmd_cb_host_number_of_completed_packets,
                         sdc_hci_cmd_cb_host_number_of_completed_packets_t),
        0x0C63 => cmd_p!(sdc_hci_cmd_cb_set_event_mask_page_2,
                         sdc_hci_cmd_cb_set_event_mask_page_2_t),

        // ── Link Control (OGF 0x01) ───────────────────────────────────────── //
        0x0406 => cmd_async_p!(sdc_hci_cmd_lc_disconnect,
                               sdc_hci_cmd_lc_disconnect_t),
        0x041D => cmd_async_p!(sdc_hci_cmd_lc_read_remote_version_information,
                               sdc_hci_cmd_lc_read_remote_version_information_t),

        // ── Informational Parameters (OGF 0x04) ──────────────────────────── //
        0x1001 => cmd_r!(sdc_hci_cmd_ip_read_local_version_information,
                         sdc_hci_cmd_ip_read_local_version_information_return_t),
        0x1002 => cmd_r!(sdc_hci_cmd_ip_read_local_supported_commands,
                         sdc_hci_cmd_ip_read_local_supported_commands_return_t),
        0x1003 => cmd_r!(sdc_hci_cmd_ip_read_local_supported_features,
                         sdc_hci_cmd_ip_read_local_supported_features_return_t),
        0x1009 => cmd_r!(sdc_hci_cmd_ip_read_bd_addr,
                         sdc_hci_cmd_ip_read_bd_addr_return_t),

        // ── LE Controller Commands (OGF 0x08) ────────────────────────────── //
        0x2001 => cmd_p!(sdc_hci_cmd_le_set_event_mask,
                         sdc_hci_cmd_le_set_event_mask_t),
        0x2002 => cmd_r!(sdc_hci_cmd_le_read_buffer_size,
                         sdc_hci_cmd_le_read_buffer_size_return_t),
        0x2003 => cmd_r!(sdc_hci_cmd_le_read_local_supported_features,
                         sdc_hci_cmd_le_read_local_supported_features_return_t),
        0x2005 => cmd_p!(sdc_hci_cmd_le_set_random_address,
                         sdc_hci_cmd_le_set_random_address_t),
        0x2006 => cmd_p!(sdc_hci_cmd_le_set_adv_params,
                         sdc_hci_cmd_le_set_adv_params_t),
        0x2007 => cmd_r!(sdc_hci_cmd_le_read_adv_physical_channel_tx_power,
                         sdc_hci_cmd_le_read_adv_physical_channel_tx_power_return_t),
        0x2008 => cmd_p!(sdc_hci_cmd_le_set_adv_data,
                         sdc_hci_cmd_le_set_adv_data_t),
        0x2009 => cmd_p!(sdc_hci_cmd_le_set_scan_response_data,
                         sdc_hci_cmd_le_set_scan_response_data_t),
        0x200A => cmd_p!(sdc_hci_cmd_le_set_adv_enable,
                         sdc_hci_cmd_le_set_adv_enable_t),
        0x200B => cmd_p!(sdc_hci_cmd_le_set_scan_params,
                         sdc_hci_cmd_le_set_scan_params_t),
        0x200C => cmd_p!(sdc_hci_cmd_le_set_scan_enable,
                         sdc_hci_cmd_le_set_scan_enable_t),
        0x200D => cmd_async_p!(sdc_hci_cmd_le_create_conn,
                               sdc_hci_cmd_le_create_conn_t),
        0x200E => cmd_async_n!(sdc_hci_cmd_le_create_conn_cancel),
        0x200F => cmd_r!(sdc_hci_cmd_le_read_filter_accept_list_size,
                         sdc_hci_cmd_le_read_filter_accept_list_size_return_t),
        0x2010 => cmd_n!(sdc_hci_cmd_le_clear_filter_accept_list),
        0x2011 => cmd_p!(sdc_hci_cmd_le_add_device_to_filter_accept_list,
                         sdc_hci_cmd_le_add_device_to_filter_accept_list_t),
        0x2012 => cmd_p!(sdc_hci_cmd_le_remove_device_from_filter_accept_list,
                         sdc_hci_cmd_le_remove_device_from_filter_accept_list_t),
        0x2013 => cmd_async_p!(sdc_hci_cmd_le_conn_update,
                               sdc_hci_cmd_le_conn_update_t),
        0x2014 => cmd_p!(sdc_hci_cmd_le_set_host_channel_classification,
                         sdc_hci_cmd_le_set_host_channel_classification_t),
        0x2015 => cmd_pr!(sdc_hci_cmd_le_read_channel_map,
                          sdc_hci_cmd_le_read_channel_map_t,
                          sdc_hci_cmd_le_read_channel_map_return_t),
        0x2016 => cmd_async_p!(sdc_hci_cmd_le_read_remote_features,
                               sdc_hci_cmd_le_read_remote_features_t),
        0x2017 => cmd_pr!(sdc_hci_cmd_le_encrypt,
                          sdc_hci_cmd_le_encrypt_t,
                          sdc_hci_cmd_le_encrypt_return_t),
        0x2018 => cmd_r!(sdc_hci_cmd_le_rand,
                         sdc_hci_cmd_le_rand_return_t),
        0x2019 => cmd_async_p!(sdc_hci_cmd_le_enable_encryption,
                               sdc_hci_cmd_le_enable_encryption_t),
        0x201A => cmd_pr!(sdc_hci_cmd_le_long_term_key_request_reply,
                          sdc_hci_cmd_le_long_term_key_request_reply_t,
                          sdc_hci_cmd_le_long_term_key_request_reply_return_t),
        0x201B => cmd_pr!(sdc_hci_cmd_le_long_term_key_request_negative_reply,
                          sdc_hci_cmd_le_long_term_key_request_negative_reply_t,
                          sdc_hci_cmd_le_long_term_key_request_negative_reply_return_t),
        0x201C => cmd_r!(sdc_hci_cmd_le_read_supported_states,
                         sdc_hci_cmd_le_read_supported_states_return_t),
        0x2022 => cmd_pr!(sdc_hci_cmd_le_set_data_length,
                          sdc_hci_cmd_le_set_data_length_t,
                          sdc_hci_cmd_le_set_data_length_return_t),
        0x2023 => cmd_r!(sdc_hci_cmd_le_read_suggested_default_data_length,
                         sdc_hci_cmd_le_read_suggested_default_data_length_return_t),
        0x2024 => cmd_p!(sdc_hci_cmd_le_write_suggested_default_data_length,
                         sdc_hci_cmd_le_write_suggested_default_data_length_t),
        0x2027 => cmd_p!(sdc_hci_cmd_le_add_device_to_resolving_list,
                         sdc_hci_cmd_le_add_device_to_resolving_list_t),
        0x2028 => cmd_p!(sdc_hci_cmd_le_remove_device_from_resolving_list,
                         sdc_hci_cmd_le_remove_device_from_resolving_list_t),
        0x2029 => cmd_n!(sdc_hci_cmd_le_clear_resolving_list),
        0x202A => cmd_r!(sdc_hci_cmd_le_read_resolving_list_size,
                         sdc_hci_cmd_le_read_resolving_list_size_return_t),
        0x202C => cmd_p!(sdc_hci_cmd_le_set_privacy_mode,
                         sdc_hci_cmd_le_set_privacy_mode_t),
        0x202D => cmd_p!(sdc_hci_cmd_le_set_address_resolution_enable,
                         sdc_hci_cmd_le_set_address_resolution_enable_t),
        0x202E => cmd_p!(sdc_hci_cmd_le_set_resolvable_private_address_timeout,
                         sdc_hci_cmd_le_set_resolvable_private_address_timeout_t),
        0x202F => cmd_r!(sdc_hci_cmd_le_read_max_data_length,
                         sdc_hci_cmd_le_read_max_data_length_return_t),
        0x2030 => cmd_pr!(sdc_hci_cmd_le_read_phy,
                          sdc_hci_cmd_le_read_phy_t,
                          sdc_hci_cmd_le_read_phy_return_t),
        0x2031 => cmd_p!(sdc_hci_cmd_le_set_default_phy,
                         sdc_hci_cmd_le_set_default_phy_t),
        0x2032 => cmd_async_p!(sdc_hci_cmd_le_set_phy,
                               sdc_hci_cmd_le_set_phy_t),

        // ── LE 5.0 Extended Advertising (OGF 0x08) ───────────────────────── //
        0x2035 => cmd_p!(sdc_hci_cmd_le_set_adv_set_random_address,
                         sdc_hci_cmd_le_set_adv_set_random_address_t),
        0x2036 => cmd_pr!(sdc_hci_cmd_le_set_ext_adv_params,
                          sdc_hci_cmd_le_set_ext_adv_params_t,
                          sdc_hci_cmd_le_set_ext_adv_params_return_t),
        0x2037 => cmd_p!(sdc_hci_cmd_le_set_ext_adv_data,
                         sdc_hci_cmd_le_set_ext_adv_data_t),
        0x2039 => cmd_p!(sdc_hci_cmd_le_set_ext_adv_enable,
                         sdc_hci_cmd_le_set_ext_adv_enable_t),
        0x203A => cmd_r!(sdc_hci_cmd_le_read_max_adv_data_length,
                         sdc_hci_cmd_le_read_max_adv_data_length_return_t),
        0x203B => cmd_r!(sdc_hci_cmd_le_read_number_of_supported_adv_sets,
                         sdc_hci_cmd_le_read_number_of_supported_adv_sets_return_t),
        0x203C => cmd_p!(sdc_hci_cmd_le_remove_adv_set,
                         sdc_hci_cmd_le_remove_adv_set_t),
        0x203D => cmd_n!(sdc_hci_cmd_le_clear_adv_sets),

        // ── LE 5.0 Extended Scanning (OGF 0x08) ──────────────────────────── //
        0x2041 => cmd_p!(sdc_hci_cmd_le_set_ext_scan_params,
                         sdc_hci_cmd_le_set_ext_scan_params_t),
        0x2042 => cmd_p!(sdc_hci_cmd_le_set_ext_scan_enable,
                         sdc_hci_cmd_le_set_ext_scan_enable_t),
        0x2043 => cmd_async_p!(sdc_hci_cmd_le_ext_create_conn,
                               sdc_hci_cmd_le_ext_create_conn_t),

        // ── LE 5.0 Periodic Advertising (OGF 0x08) ───────────────────────── //
        0x2044 => cmd_async_p!(sdc_hci_cmd_le_periodic_adv_create_sync,
                               sdc_hci_cmd_le_periodic_adv_create_sync_t),
        0x2045 => cmd_async_n!(sdc_hci_cmd_le_periodic_adv_create_sync_cancel),
        0x2046 => cmd_p!(sdc_hci_cmd_le_periodic_adv_terminate_sync,
                         sdc_hci_cmd_le_periodic_adv_terminate_sync_t),
        0x2047 => cmd_p!(sdc_hci_cmd_le_add_device_to_periodic_adv_list,
                         sdc_hci_cmd_le_add_device_to_periodic_adv_list_t),
        0x2048 => cmd_p!(sdc_hci_cmd_le_remove_device_from_periodic_adv_list,
                         sdc_hci_cmd_le_remove_device_from_periodic_adv_list_t),
        0x2049 => cmd_n!(sdc_hci_cmd_le_clear_periodic_adv_list),
        0x204A => cmd_r!(sdc_hci_cmd_le_read_periodic_adv_list_size,
                         sdc_hci_cmd_le_read_periodic_adv_list_size_return_t),
        0x204B => cmd_r!(sdc_hci_cmd_le_read_transmit_power,
                         sdc_hci_cmd_le_read_transmit_power_return_t),

        // ── LE 5.2 Host Feature (OGF 0x08) ───────────────────────────────── //
        0x2059 => cmd_p!(sdc_hci_cmd_le_set_host_feature,
                         sdc_hci_cmd_le_set_host_feature_t),

        // ── LE 5.2 Isochronous Channels — CIS (OGF 0x08) ─────────────────── //
        0x2062 => cmd_pr!(sdc_hci_cmd_le_set_cig_params,
                          sdc_hci_cmd_le_set_cig_params_t,
                          sdc_hci_cmd_le_set_cig_params_return_t),
        0x2063 => cmd_pr!(sdc_hci_cmd_le_set_cig_params_test,
                          sdc_hci_cmd_le_set_cig_params_test_t,
                          sdc_hci_cmd_le_set_cig_params_test_return_t),
        0x2064 => cmd_async_p!(sdc_hci_cmd_le_create_cis,
                               sdc_hci_cmd_le_create_cis_t),
        0x2065 => cmd_pr!(sdc_hci_cmd_le_remove_cig,
                          sdc_hci_cmd_le_remove_cig_t,
                          sdc_hci_cmd_le_remove_cig_return_t),
        0x2066 => cmd_p!(sdc_hci_cmd_le_accept_cis_request,
                         sdc_hci_cmd_le_accept_cis_request_t),
        0x2067 => cmd_pr!(sdc_hci_cmd_le_reject_cis_request,
                          sdc_hci_cmd_le_reject_cis_request_t,
                          sdc_hci_cmd_le_reject_cis_request_return_t),

        // ── LE 5.2 Isochronous Channels — BIS (OGF 0x08) ─────────────────── //
        0x2068 => cmd_async_p!(sdc_hci_cmd_le_create_big,
                               sdc_hci_cmd_le_create_big_t),
        0x2069 => cmd_async_p!(sdc_hci_cmd_le_create_big_test,
                               sdc_hci_cmd_le_create_big_test_t),
        0x206A => cmd_async_p!(sdc_hci_cmd_le_terminate_big,
                               sdc_hci_cmd_le_terminate_big_t),
        0x206B => cmd_async_p!(sdc_hci_cmd_le_big_create_sync,
                               sdc_hci_cmd_le_big_create_sync_t),
        0x206C => cmd_pr!(sdc_hci_cmd_le_big_terminate_sync,
                          sdc_hci_cmd_le_big_terminate_sync_t,
                          sdc_hci_cmd_le_big_terminate_sync_return_t),

        // ── LE 5.2 ISO Data Path & Test (OGF 0x08) ───────────────────────── //
        0x206E => cmd_pr!(sdc_hci_cmd_le_setup_iso_data_path,
                          sdc_hci_cmd_le_setup_iso_data_path_t,
                          sdc_hci_cmd_le_setup_iso_data_path_return_t),
        0x206F => cmd_pr!(sdc_hci_cmd_le_remove_iso_data_path,
                          sdc_hci_cmd_le_remove_iso_data_path_t,
                          sdc_hci_cmd_le_remove_iso_data_path_return_t),
        0x2070 => cmd_pr!(sdc_hci_cmd_le_iso_transmit_test,
                          sdc_hci_cmd_le_iso_transmit_test_t,
                          sdc_hci_cmd_le_iso_transmit_test_return_t),
        0x2071 => cmd_pr!(sdc_hci_cmd_le_iso_receive_test,
                          sdc_hci_cmd_le_iso_receive_test_t,
                          sdc_hci_cmd_le_iso_receive_test_return_t),
        0x2072 => cmd_pr!(sdc_hci_cmd_le_iso_read_test_counters,
                          sdc_hci_cmd_le_iso_read_test_counters_t,
                          sdc_hci_cmd_le_iso_read_test_counters_return_t),
        0x2073 => cmd_pr!(sdc_hci_cmd_le_iso_test_end,
                          sdc_hci_cmd_le_iso_test_end_t,
                          sdc_hci_cmd_le_iso_test_end_return_t),
        0x2075 => cmd_pr!(sdc_hci_cmd_le_read_iso_tx_sync,
                          sdc_hci_cmd_le_read_iso_tx_sync_t,
                          sdc_hci_cmd_le_read_iso_tx_sync_return_t),
        0x2076 => cmd_pr!(sdc_hci_cmd_le_read_iso_link_quality,
                          sdc_hci_cmd_le_read_iso_link_quality_t,
                          sdc_hci_cmd_le_read_iso_link_quality_return_t),

        _ => false,
    }
}

// ── HCI event helpers ─────────────────────────────────────────────────────── //

/// Send an HCI Command Complete event over the N→A IPC ring buffer.
///
/// Format: `[0x04][0x0E][param_len][0x01][op_lo][op_hi][status][return_data...]`
fn send_cc_event(opcode: u16, status: u8, return_data: &[u8]) {
    // Maximum return data: 64 bytes (for Read Local Supported Commands).
    let mut buf = [0u8; 7 + 64];
    let ret_len = return_data.len().min(64);
    let param_len = (4 + ret_len) as u8;
    buf[0] = 0x04; // HCI Event indicator
    buf[1] = 0x0E; // HCI_EV_CMD_COMPLETE
    buf[2] = param_len;
    buf[3] = 0x01; // num_hci_cmd_packets
    buf[4] = (opcode & 0xFF) as u8;
    buf[5] = (opcode >> 8) as u8;
    buf[6] = status;
    buf[7..7 + ret_len].copy_from_slice(&return_data[..ret_len]);
    notify_app(&buf[..7 + ret_len]);
}

/// Send an HCI Command Status event (for asynchronous commands).
///
/// Format: `[0x04][0x0F][0x04][status][0x01][op_lo][op_hi]`
fn send_cs_event(opcode: u16, status: u8) {
    let buf = [
        0x04u8,               // HCI Event indicator
        0x0F,                 // HCI_EV_CMD_STATUS
        0x04,                 // param_len
        status,
        0x01,                 // num_hci_cmd_packets
        (opcode & 0xFF) as u8,
        (opcode >> 8) as u8,
    ];
    notify_app(&buf);
}

// ── IPC send helper ───────────────────────────────────────────────────────── //
//
// Write a packet into the N→A ring buffer via the shared `thingy53-ipc` crate,
// then trigger IPC Event0 to wake the app core's `ipc_recv_task`.

fn notify_app(pkt: &[u8]) {
    if ipc::ipc_send_to_app(pkt) {
        // Safety: Event0 is only ever triggered (never awaited) on the net core.
        unsafe { hw_ipc::Event::steal::<peripherals::IPC>(hw_ipc::EventNumber::Event0) }.trigger();
    } else {
        warn!("IPC N→A ring full – packet dropped");
    }
}

// ── HCI packet length helper ──────────────────────────────────────────────── //

fn hci_packet_len(indicator: u8, buf: &[u8]) -> usize {
    match indicator {
        0x04 if buf.len() >= 2 => 2 + buf[1] as usize,
        0x02 if buf.len() >= 4 => 4 + u16::from_le_bytes([buf[2], buf[3]]) as usize,
        _ => buf.len(),
    }
}
