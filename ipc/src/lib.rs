//! Shared IPC ring-buffer transport between the nRF5340 application core and
//! network core.
//!
//! Two ring-buffers live in a 32 KB shared-memory region at the top of
//! application-core RAM (accessible by both cores over the AHB bus):
//!
//! ```text
//! 0x20038000  ┌─────────────────────────────────┐
//!             │  net→app ring buffer  (14 KB)   │
//! 0x20039800  │  app→net ring buffer  (14 KB)   │
//! 0x2003FFFF  └─────────────────────────────────┘
//! ```
//!
//! Each ring buffer layout:
//! ```text
//! offset 0  │ write_idx : u32   (producer writes)
//! offset 4  │ read_idx  : u32   (consumer writes)
//! offset 8  │ data[N]           (ring buffer payload)
//! ```
//!
//! Packet framing: `[length : u16 LE][payload bytes…]`
//! The first payload byte is the HCI indicator (0x01 cmd / 0x02 acl / 0x04 event).
//!
//! # IPC Peripheral Signalling
//!
//! Interrupt signalling uses the nRF5340 IPC peripheral via the
//! `embassy_nrf::ipc` driver, configured in each binary:
//!
//! | Direction  | IPC Event | IPC Channel | Who triggers | Who waits |
//! |------------|-----------|-------------|--------------|-----------|
//! | net → app  | Event0    | Channel0    | net core     | app core  |
//! | app → net  | Event1    | Channel1    | app core     | net core  |
//!
//! This crate only provides the ring-buffer read/write primitives.
//! Each binary is responsible for peripheral setup (`embassy_nrf::ipc::Ipc::new`),
//! triggering (`Event::trigger()`), and interrupt-driven waiting (`Event::wait()`).

#![no_std]

use core::sync::atomic::{fence, AtomicU32, Ordering};

// ── Shared-memory layout ──────────────────────────────────────────────────── //

pub const IPC_SHMEM_BASE: usize = 0x2003_8000;

const N2A_BASE: usize = IPC_SHMEM_BASE;             // Net → App ring buffer
const N2A_BUF_SIZE: usize = 14 * 1024 - 8;

const A2N_BASE: usize = IPC_SHMEM_BASE + 14 * 1024; // App → Net ring buffer
const A2N_BUF_SIZE: usize = 14 * 1024 - 8;

// ── Ring-buffer primitives ─────────────────────────────────────────────────── //

#[inline(always)]
fn rb_write_idx(base: usize) -> &'static AtomicU32 {
    unsafe { &*(base as *const AtomicU32) }
}

#[inline(always)]
fn rb_read_idx(base: usize) -> &'static AtomicU32 {
    unsafe { &*((base + 4) as *const AtomicU32) }
}

#[inline(always)]
fn rb_data(base: usize) -> *mut u8 {
    (base + 8) as *mut u8
}

fn rb_write(base: usize, buf_size: usize, payload: &[u8]) -> bool {
    let total = 2 + payload.len();
    if total > buf_size {
        defmt::warn!("IPC: packet too large ({} > {})", total, buf_size);
        return false;
    }

    let wi = rb_write_idx(base);
    let ri = rb_read_idx(base);
    let w = wi.load(Ordering::Relaxed) as usize;
    let r = ri.load(Ordering::Acquire) as usize;

    let used = w.wrapping_sub(r);
    let free = buf_size.wrapping_sub(used);
    if free < total {
        return false; // buffer full
    }

    let data = rb_data(base);
    let len = (payload.len() as u16).to_le_bytes();
    unsafe {
        *data.add(w % buf_size) = len[0];
        *data.add((w + 1) % buf_size) = len[1];
        for (i, &b) in payload.iter().enumerate() {
            *data.add((w + 2 + i) % buf_size) = b;
        }
    }

    fence(Ordering::Release);
    wi.store((w + total) as u32, Ordering::Release);
    true
}

fn rb_read(base: usize, buf_size: usize, out: &mut [u8]) -> Option<usize> {
    let wi = rb_write_idx(base);
    let ri = rb_read_idx(base);
    let w = wi.load(Ordering::Acquire) as usize;
    let r = ri.load(Ordering::Relaxed) as usize;

    if w == r {
        return None;
    }

    let data = rb_data(base);
    let lo = unsafe { *data.add(r % buf_size) };
    let hi = unsafe { *data.add((r + 1) % buf_size) };
    let pkt_len = u16::from_le_bytes([lo, hi]) as usize;

    if pkt_len > out.len() {
        defmt::error!("IPC: output buffer too small ({} > {})", pkt_len, out.len());
        ri.store((r + 2 + pkt_len) as u32, Ordering::Release);
        return None;
    }

    for i in 0..pkt_len {
        out[i] = unsafe { *data.add((r + 2 + i) % buf_size) };
    }

    fence(Ordering::Release);
    ri.store((r + 2 + pkt_len) as u32, Ordering::Release);
    Some(pkt_len)
}

// ── Public API ─────────────────────────────────────────────────────────────── //

/// Zero the ring buffer headers so the net core sees clean state on startup.
///
/// Call from the **app core** before releasing the network core.
pub fn ipc_init() {
    rb_write_idx(N2A_BASE).store(0, Ordering::Relaxed);
    rb_read_idx(N2A_BASE).store(0, Ordering::Relaxed);
    rb_write_idx(A2N_BASE).store(0, Ordering::Relaxed);
    rb_read_idx(A2N_BASE).store(0, Ordering::Relaxed);
    fence(Ordering::SeqCst);
    defmt::trace!("IPC shared memory initialised");
}

/// Write a framed HCI packet into the **app→net** ring buffer.
///
/// Returns `false` if the buffer is full. After a successful write the caller
/// must trigger the net core via `Event::trigger()` on IPC Event1 so the net
/// core's `ipc_rx_loop` wakes from `event.wait()`.
pub fn ipc_send_to_net(packet: &[u8]) -> bool {
    rb_write(A2N_BASE, A2N_BUF_SIZE, packet)
}

/// Read the next HCI packet from the **net→app** ring buffer into `out`.
///
/// Returns the number of bytes written (≥ 1, first byte is HCI indicator),
/// or `None` if the buffer is empty. Called from the **app core** receive task
/// after waking from `event.wait()` on IPC Event0.
pub fn ipc_recv_from_net(out: &mut [u8]) -> Option<usize> {
    rb_read(N2A_BASE, N2A_BUF_SIZE, out)
}

/// Write a framed HCI packet into the **net→app** ring buffer.
///
/// Returns `false` if the buffer is full. After a successful write the caller
/// must trigger the app core via `Event::trigger()` on IPC Event0 so the app
/// core's `ipc_recv_task` wakes from `event.wait()`.
pub fn ipc_send_to_app(packet: &[u8]) -> bool {
    rb_write(N2A_BASE, N2A_BUF_SIZE, packet)
}

/// Read the next HCI packet from the **app→net** ring buffer into `out`.
///
/// Returns the number of bytes written (≥ 1, first byte is HCI indicator),
/// or `None` if the buffer is empty. Called from the **net core** receive loop
/// after waking from `event.wait()` on IPC Event1.
pub fn ipc_recv_from_app(out: &mut [u8]) -> Option<usize> {
    rb_read(A2N_BASE, A2N_BUF_SIZE, out)
}
