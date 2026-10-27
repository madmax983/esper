/* The memory map the emulated images link against.
 *
 * `qemu-system-arm -machine mps2-an385` (Cortex-M3) and `mps2-an386`
 * (Cortex-M4) both put 4 MiB of code memory at 0x00000000 and 4 MiB of
 * SRAM at 0x20000000. The image is linked into 512 KiB of each: roughly
 * the ESP32-S3's SRAM, the part Esper ships on, so an image that fits
 * here fits the RAM the real target has — and an image that grows past
 * it fails to link rather than quietly using an emulator's generosity.
 */
MEMORY
{
  FLASH : ORIGIN = 0x00000000, LENGTH = 512K
  RAM   : ORIGIN = 0x20000000, LENGTH = 512K
}
