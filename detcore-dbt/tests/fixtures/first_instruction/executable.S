/* Freestanding ET_EXEC, linked at the checked APP_ENTRY.
 * APP_INTERPRETER_PATH is one quoted absolute pathname supplied at compile time.
 * Four zero-byte writes are the only sampling syscalls. The controller, not
 * hidden guest branches, checks their return values and the state oracle.
 */
#include "fixture.h"
#ifndef APP_INTERPRETER_PATH
#error "Define APP_INTERPRETER_PATH as a quoted absolute interpreter pathname"
#endif
.section .interp,"a",@progbits
  .asciz APP_INTERPRETER_PATH

.section .text.entry,"ax",@progbits
.global app_entry
.type app_entry,@function
app_entry:
  mov $2, %ecx
  mov $0, %r8d
.Lmain_loop:
  lea 1(%r8), %r8
  dec %ecx
  jne .Lmain_loop
  mov $0x22, %r9d

  mov $1, %eax
  mov $1, %edi
  lea .Lempty_anchor(%rip), %rsi
  mov $0, %edx
  mov $PHASE_MAIN, %r10d
.type sample_main,@function
sample_main:
  syscall
.type sample_main_after,@function
sample_main_after:
  call *%r13

  mov $1, %eax
  mov $1, %edi
  lea .Lempty_anchor(%rip), %rsi
  mov $0, %edx
  mov $PHASE_HELPER, %r10d
.type sample_helper,@function
sample_helper:
  syscall
.type sample_helper_after,@function
sample_helper_after:
  /* This interval executes JMP, CALL and RET, but no conditional transfer. */
  jmp .Lzero_entry
.Lzero_leaf:
  mov $0x40, %r9d
  ret
.Lzero_entry:
  call .Lzero_leaf
  mov $0, %r8d
  mov $0, %ecx
  mov $1, %eax
  mov $1, %edi
  lea .Lempty_anchor(%rip), %rsi
  mov $0, %edx
  mov $PHASE_ZERO, %r10d
.type sample_zero,@function
sample_zero:
  syscall
.type sample_zero_after,@function
sample_zero_after:
  mov $60, %eax
  mov $0, %edi
  mov $PHASE_EXIT, %r10d
.type terminal_exit,@function
terminal_exit:
  syscall
  ud2
.size app_entry, .-app_entry

.section .rodata,"a",@progbits
.Lempty_anchor:
  .byte 0
.section .note.GNU-stack,"",@progbits

/* Separate RW/NX main-image BSS; fresh images start with all 24 bytes zero. */
.section .bss.first_instruction_marker,"aw",@nobits
.balign 4
.global first_instruction_marker
.type first_instruction_marker,@object
first_instruction_marker:
  .zero 4
.size first_instruction_marker, .-first_instruction_marker

/* Explicit padding keeps the two 64-bit entry facts naturally aligned. */
  .zero 4
.global first_instruction_stack_pointer
.type first_instruction_stack_pointer,@object
first_instruction_stack_pointer:
  .zero 8
.size first_instruction_stack_pointer, .-first_instruction_stack_pointer
.global first_instruction_entry_pc
.type first_instruction_entry_pc,@object
first_instruction_entry_pc:
  .zero 8
.size first_instruction_entry_pc, .-first_instruction_entry_pc
