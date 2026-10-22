/* Companion: RX text plus RW/NX marker BSS, explicit PT_INTERP, fixed entry.
 * Do not move app_entry without changing and rechecking fixture.h APP_ENTRY.
 */
OUTPUT_FORMAT(elf64-x86-64)
OUTPUT_ARCH(i386:x86-64)
ENTRY(app_entry)
PHDRS
{
  phdr PT_PHDR PHDRS;
  interpreter PT_INTERP;
  property 0x6474e553 FLAGS(4); /* PT_GNU_PROPERTY, separate from PT_INTERP. */
  text PT_LOAD FILEHDR PHDRS FLAGS(5);
  marker PT_LOAD FLAGS(6);
  stack PT_GNU_STACK FLAGS(6);
}
SECTIONS
{
  . = 0x400000 + SIZEOF_HEADERS;
  .interp : { KEEP(*(.interp)) } :text :interpreter
  ASSERT(SIZEOF(.interp) > 1, "PT_INTERP pathname is required")
  /* Preserve the compiler's ISA property without extending the path segment. */
  .note.gnu.property : { KEEP(*(.note.gnu.property)) } :text :property
  ASSERT(. < 0x401000, "PT_INTERP exceeds the fixed header page")
  . = 0x401000;
  .text : { KEEP(*(.text.entry)) *(.text .text.*) } :text
  .rodata : { *(.rodata .rodata.*) } :text
  .data : { *(.data .data.*) } :text
  ASSERT(app_entry == 0x401000, "executable entry moved")
  ASSERT(SIZEOF(.data) == 0, "unexpected writable data")
  ASSERT(. <= 0x402000, "executable text overlaps marker page")
  . = 0x402000;
  .bss (NOLOAD) :
  {
    KEEP(*(.bss.first_instruction_marker))
    *(.bss .bss.*) *(COMMON)
  } :marker
  ASSERT(ADDR(.bss) == 0x402000, "marker segment moved")
  ASSERT(first_instruction_marker == 0x402000, "marker symbol moved")
  ASSERT(first_instruction_stack_pointer == 0x402008, "entry stack-pointer symbol moved")
  ASSERT(first_instruction_entry_pc == 0x402010, "entry PC symbol moved")
  ASSERT(SIZEOF(.bss) == 24, "marker and entry facts must occupy exactly 24 BSS bytes")
  /DISCARD/ : { *(.comment) *(.note.GNU-stack) *(.eh_frame .eh_frame.*) }
}
