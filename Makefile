.PHONY: all build build-app build-net flash logs-app logs-net debug-server size clean help

APP_TARGET  := thumbv8m.main-none-eabihf
NET_TARGET  := thumbv8m.main-none-eabi
CHIP        := nRF5340_xxAA

APP_ELF      := target/$(APP_TARGET)/release/thingy53-app
NET_ELF      := target/$(NET_TARGET)/release/thingy53-net
APP_HEX      := target/thingy53-app.hex
NET_HEX      := target/thingy53-net.hex
NET_UICR_HEX := net/uicr-approtect.hex

all: build

build: build-net build-app

build-app:
	cargo build --release -p thingy53-app --target $(APP_TARGET)

build-net:
	cargo build --release -p thingy53-net --target $(NET_TARGET)

$(NET_HEX): $(NET_ELF)
	rust-objcopy -O ihex $(NET_ELF) $(NET_HEX)

$(APP_HEX): $(APP_ELF)
	rust-objcopy -O ihex $(APP_ELF) $(APP_HEX)

flash: build $(NET_HEX) $(APP_HEX)
	nrfjprog --recover --coprocessor CP_NETWORK
	nrfjprog --program $(NET_HEX) --coprocessor CP_NETWORK --sectorerase --verify
	# Write UICR.APPROTECT=0x50FA50FA so probe-rs can attach after reset.
	# --recover erases UICR (leaving 0xFFFFFFFF=protected); we restore the
	# disable magic here since --sectorerase above does not touch UICR.
	nrfjprog --program $(NET_UICR_HEX) --coprocessor CP_NETWORK
	nrfjprog --program $(APP_HEX) --sectorerase --verify
	nrfjprog --reset

# probe-rs attach reads RTT directly via the debug probe and decodes defmt.
# Works now that the app core calls release_network_core() so the net core
# AHB-AP has DeviceEn=1 and probe-rs can connect to both cores.

logs-app:
	probe-rs attach --chip $(CHIP) $(APP_ELF)

logs-net:
	probe-rs attach --chip $(CHIP) $(NET_ELF)

# Starts a probe-rs DAP server for Zed debugger integration.
# Keep this running in a separate terminal while debugging from Zed.
# Zed connects to port 50000; select "Debug: App Core" or "Debug: Net Core"
# from the debug panel (see .zed/debug.json).
debug-server:
	probe-rs dap-server --port 50000

size: build
	@echo "=== Net core (flash: 128K, RAM: 64K) ==="
	@rust-size -A $(NET_ELF) | awk '\
	  /\.text|\.rodata|\.vector_table|\.gnu.sgstubs/ { flash += $$2 } \
	  /\.data|\.bss|\.uninit/                        { ram   += $$2 } \
	  END { printf "  Flash: %d bytes (%.1f%%)\n  RAM:   %d bytes (%.1f%%)\n", \
	        flash, flash/131072*100, ram, ram/65536*100 }'
	@echo ""
	@echo "=== App core (flash: 1024K, RAM: 224K) ==="
	@rust-size -A $(APP_ELF) | awk '\
	  /\.text|\.rodata|\.vector_table|\.gnu.sgstubs/ { flash += $$2 } \
	  /\.data|\.bss|\.uninit/                        { ram   += $$2 } \
	  END { printf "  Flash: %d bytes (%.1f%%)\n  RAM:   %d bytes (%.1f%%)\n", \
	        flash, flash/1048576*100, ram, ram/229376*100 }'
	@echo ""
	@echo "=== Full section breakdown ==="
	@rust-size -A $(NET_ELF) $(APP_ELF)

clean:
	cargo clean
	rm -f $(APP_HEX) $(NET_HEX)

help:
	@echo "Usage: make [target]"
	@echo ""
	@echo "Targets:"
	@echo "  all           Build both cores (default)"
	@echo "  build         Build both cores"
	@echo "  build-app     Build the app core ($(APP_TARGET))"
	@echo "  build-net     Build the network core ($(NET_TARGET))"
	@echo "  flash         Build, convert to hex, and flash both cores via nrfjprog"
	@echo "  logs-app      Stream RTT logs from the app core via probe-rs"
	@echo "  logs-net      Stream RTT logs from the network core via probe-rs"
	@echo "  debug-server  Start a probe-rs DAP server on port 50000 for Zed debugger"
	@echo "  size          Build and report flash/RAM usage for both cores"
	@echo "  clean         Remove build artifacts and generated hex files"
	@echo "  help          Show this help message"
