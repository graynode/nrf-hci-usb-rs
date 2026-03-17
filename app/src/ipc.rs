//! App-core IPC shim.
//!
//! Re-exports the shared ring-buffer API from `thingy53-ipc` and wraps the
//! send functions to trigger the net core's IPC interrupt (Event1 / Channel1)
//! after each successful ring-buffer write.
//!
//! # IPC wiring (configured in `main` via `embassy_nrf::ipc::Ipc::new`)
//!
//! | Event  | Channel  | Direction  | Role on this core |
//! |--------|----------|------------|-------------------|
//! | Event0 | Channel0 | net → app  | `wait()` in `ipc_recv_task` |
//! | Event1 | Channel1 | app → net  | `trigger()` after ring-buffer write |

use embassy_nrf::{ipc as hw_ipc, peripherals};

pub use thingy53_ipc::ipc_init;
pub use thingy53_ipc::ipc_recv_from_net;

/// Write a framed HCI packet into the app→net ring buffer and wake the net
/// core by triggering IPC Event1.
///
/// Returns `false` if the ring buffer is full (caller should drop or retry).
pub fn ipc_send_to_net(packet: &[u8]) -> bool {
    let ok = thingy53_ipc::ipc_send_to_net(packet);
    if ok {
        // Safety: Event1 is exclusively used for triggering (never awaited) on
        // the app core, so creating a transient Event handle here is safe.
        unsafe { hw_ipc::Event::steal::<peripherals::IPC>(hw_ipc::EventNumber::Event1) }.trigger();
    }
    ok
}
