/* STM32G473 */


MEMORY
{
  FLASH  : ORIGIN = 0x08000000, LENGTH = 512K
  RAM (xrw)         : ORIGIN = 0x20000000, LENGTH = 124K
  CCM (xrw)         : ORIGIN = 0x2001F000, LENGTH = 4K
}
