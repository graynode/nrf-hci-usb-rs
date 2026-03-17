# thingy53-hci-usb

A Rust + Embassy firmware for the **Nordic Thingy:53** (nRF5340) that exposes
the Bluetooth LE controller as a standard USB HCI device — equivalent to
Zephyr's `hci_ipc` + `hci_usb` combination.

When plugged into a host PC, the device enumerates as a **Bluetooth adapter**
(USB class `0xE0 / 0x01 / 0x01`). The host's Bluetooth stack drives it exactly
like any USB BT dongle.

---

## Architecture

```
  USB Host  ◄──── USB HCI (class 0xE0) ────►  App Core  ──── IPC ────►  Net Core
  (BlueZ /                                     (Embassy)   shared RAM   (nrf-sdc)
   WinBT /                                      embassy-               MPSL + SDC
   macOS)                                        usb                  → BT Radio
```

Two Embassy binaries are required — one per nRF5340 core:

| Crate          | Core        | Role                                      |
|----------------|-------------|-------------------------------------------|
| `thingy53-app` | App core 0  | USB HCI device + IPC bridge to net core   |
| `thingy53-net` | Net core 1  | BLE controller (MPSL + SDC) + IPC bridge  |

### IPC protocol

A 32 KB region at the top of app-core RAM (`0x20038000–0x2003FFFF`) is shared
between the two cores via the nRF5340's AHB bus.  Two ring buffers (net→app
and app→net) each hold length-prefixed HCI packets (2-byte LE length + data).
The nRF5340 IPC peripheral provides edge-triggered interrupt signalling between
cores.

Packet framing follows UART HCI (`H4`) conventions:
- `0x01` prefix → HCI Command
- `0x02` prefix → HCI ACL Data
- `0x04` prefix → HCI Event

---

## Build environment setup

### 1 — Install Rust

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

The `rust-toolchain.toml` in this repo pins the exact compiler version and
adds the two embedded targets automatically the first time you run `cargo`.
No manual `rustup target add` is needed.

### 2 — Install Cargo tools

```sh
# probe-rs: flash + RTT log viewer
cargo install probe-rs-tools

# cargo-binutils: rust-objcopy, rust-size (used by `make`)
cargo install cargo-binutils

# flip-link: stack-overflow protection linker wrapper (used by app core)
cargo install flip-link
```

### 3 — Install C cross-compiler

The Nordic SoftDevice Controller is a pre-compiled C library. `nrf-sdc-sys`
links it via `build.rs` and needs an ARM cross-compiler in `PATH`:

```sh
# macOS (Homebrew)
brew install arm-none-eabi-gcc

# Ubuntu / Debian
sudo apt install gcc-arm-none-eabi

# Fedora / RHEL
sudo dnf install arm-none-eabi-gcc arm-none-eabi-newlib
```

### 4 — Install nrfjprog (Nordic command-line tools)

`nrfjprog` is used by `make flash` to erase and program each core separately.

```sh
# macOS (Homebrew)
brew install nrf-command-line-tools

# Linux / Windows
# Download the installer from:
# https://www.nordicsemi.com/Products/Development-tools/nRF-Command-Line-Tools
```

After installation, verify the J-Link USB driver is installed — the Nordic
installer bundles it on all platforms.

### 5 — Verify

```sh
cargo --version          # e.g. cargo 1.94.0
probe-rs --version       # e.g. probe-rs 0.31.0
rust-objcopy --version   # part of cargo-binutils
arm-none-eabi-gcc --version
nrfjprog --version
```

All five commands should succeed before attempting a build.

---

## Building

```sh
# Build the app core from the workspace root (default member/target)
cargo build --release -p thingy53-app

# Build the net core from the workspace root
cargo build --release -p thingy53-net --target thumbv8m.main-none-eabi

# Equivalent shortcut aliases from the workspace root
cargo build-app --release
cargo build-net --release
```

`cargo build` at the workspace root builds the app core by default. The app and
net binaries cannot be built together in a single Cargo invocation because they
use different targets and incompatible Nordic feature sets.

---

## Flashing

Flash the **network core first** (probe-rs routes by ELF load address), then the
**application core**. The simplest way is via `make`:

```sh
make flash
```

Or manually:

```sh
# Flash network core (Bluetooth controller) — ELF load address 0x01000000
probe-rs download --chip nRF5340_xxAA \
    target/thumbv8m.main-none-eabi/release/thingy53-net

# Flash application core (USB HCI) and reset
probe-rs run --chip nRF5340_xxAA \
    target/thumbv8m.main-none-eabihf/release/thingy53-app
```

---

## Viewing logs (RTT)

```sh
# Terminal 1 – App core logs
probe-rs attach --chip nRF5340_xxAA --core-index 0

# Terminal 2 – Net core logs
probe-rs attach --chip nRF5340_xxAA --core-index 1
```

Or use `defmt-print` via `cargo run` (default runner targets app core 0).

---

## Using the device on Linux (BlueZ)

After flashing, plug the Thingy:53 into USB.  It should appear as:

```
Bus 001 Device 005: ID 1915:521f Nordic Semiconductor ASA Thingy:53 HCI USB
```

BlueZ should auto-detect it.  Verify with:

```sh
hciconfig        # should show hci0 or hciX
hciconfig hciX up
bluetoothctl scan on
```

---

## Crate versions

| Crate            | Version |
|------------------|---------|
| `embassy-nrf`    | 0.9     |
| `embassy-executor`| 0.9    |
| `embassy-usb`    | 0.5     |
| `embassy-sync`   | 0.7     |
| `nrf-mpsl`       | 0.3     |
| `nrf-sdc`        | 0.4     |
| `bt-hci`         | 0.8     |
| `defmt`          | 1.x     |

---

## Customisation

### Crystal oscillator

If your Thingy:53 has a 32.768 kHz crystal (most do), change the MPSL clock
source in `net/src/main.rs`:

```rust
let lfclk_cfg = mpsl_raw::mpsl_clock_lfclk_cfg_t {
    source: mpsl_raw::MPSL_CLOCK_LF_SRC_XTAL as u8,  // ← change RC → XTAL
    rc_ctiv: 0,
    rc_temp_ctiv: 0,
    accuracy_ppm: mpsl_raw::MPSL_CLOCK_LF_ACCURACY_20_PPM as u16,
};
```

### BLE roles

Add or remove `.support_peripheral()`, `.support_central()`, `.support_scan()`
calls in `net/src/main.rs` `sdc::Builder` chain to match your requirements.
Each role increases the SDC memory footprint.

### USB VID/PID

Change the constants in `app/src/main.rs`:

```rust
let mut config = embassy_usb::Config::new(
    0x1915, // Vendor ID  (Nordic Semiconductor — only for development)
    0x521F, // Product ID (choose your own for production)
);
```

---

## Known limitations

- **ISO / SCO** data packets are not yet forwarded over USB (only ACL + events).
- The **USB PID `0x521F`** is for development only. Obtain your own VID/PID
  for production hardware.

---

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
