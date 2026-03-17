/* nRF5340 Network Core memory layout
 *
 * Flash: 256 KB @ 0x01000000
 * RAM:    64 KB @ 0x21000000
 *
 * The network core also has read/write access to the application core's RAM.
 * The IPC shared-memory region (app-core RAM 0x20038000–0x2003FFFF) is
 * accessed by the net core using that physical address directly — no special
 * mapping is needed on Cortex-M33 with the nRF5340's AHB interconnect.
 */
MEMORY
{
    FLASH : ORIGIN = 0x01000000, LENGTH = 256K
    RAM   : ORIGIN = 0x21000000, LENGTH = 64K
}
