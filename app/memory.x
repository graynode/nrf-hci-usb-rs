/* nRF5340 Application Core memory layout
 *
 * Total Flash: 1MB  @ 0x00000000
 * Total RAM:   256KB @ 0x20000000
 *
 * The last 32KB of app-core RAM (0x20038000–0x2003FFFF) is reserved as IPC
 * shared memory, accessible by both the application and network cores via the
 * nRF5340 AHB bus.  This region is excluded from the linker RAM region so that
 * the Rust runtime does not zero-initialise or otherwise overwrite it.
 *
 * IPC ring buffers are accessed via raw pointers at runtime (see ipc.rs) and
 * must both be zero-initialised by the application core on first boot before
 * the network core is started.
 */
MEMORY
{
    FLASH : ORIGIN = 0x00000000, LENGTH = 1024K
    RAM   : ORIGIN = 0x20000000, LENGTH = 224K   /* 256K total − 32K IPC */
}
