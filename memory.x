MEMORY
{
    FLASH : ORIGIN = 0x80000000, LENGTH = 512K
    RAM   : ORIGIN = 0x80080000, LENGTH = 256K
}

/* Shadow stack regions */
_shadow_stack_size = 4K;
_sw_shadow_stack_size = 4K;
