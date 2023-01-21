/* STM32G473 */


MEMORY
{
  FLASH  : ORIGIN = 0x08000000, LENGTH = 512K
  RAM (xrw)         : ORIGIN = 0x20000000, LENGTH = 124K
  CCM (xrw)         : ORIGIN = 0x2001F000, LENGTH = 4K
}



/* STM32H743ZI2 */

/*
MEMORY
{
  FLASH  : ORIGIN = 0x08000000, LENGTH = 2M
  RAM    : ORIGIN = 0x24000000, LENGTH = 512K
}
*/


/* STM32H723 */

/*
MEMORY
{
  FLASH  : ORIGIN = 0x08000000, LENGTH = 1M
  RAM    : ORIGIN = 0x24000000, LENGTH = 320K
}
*/
