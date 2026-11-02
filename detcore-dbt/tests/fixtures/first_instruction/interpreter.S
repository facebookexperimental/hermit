/* PIC ET_DYN interpreter and later helper. No libc or dynamic relocations.
 * The three original JNE retirements precede the first application syscall.
 * The positive counter must not select sites by these symbols or phase tags.
 */
#include "fixture.h"
.section .text.interpreter,"ax",@progbits
.global interp_start
.type interp_start,@function
interp_start:
  /* Literal first application instruction, before all guest setup. */
  movl $1, FIRST_INSTRUCTION_MARKER
  /* Record entry facts before any stack use, flags change or canary setup.
   * The first store above preserves RSP; this LEA is the actual loaded entry.
   */
  movq %rsp, FIRST_INSTRUCTION_STACK_POINTER
  lea interp_start(%rip), %rax
  movq %rax, FIRST_INSTRUCTION_ENTRY_PC
  movabs $SENTINEL_RBX, %rbx
  movabs $SENTINEL_RBP, %rbp
  movabs $SENTINEL_R12, %r12
  movabs $SENTINEL_R13_INITIAL, %r13
  movabs $SENTINEL_R14, %r14
  mov %rsp, %r15
  pushq $0xad7
  popfq
  mov $3, %ecx
  mov $0, %r8d
.Lprefix_loop:
  lea 1(%r8), %r8
  dec %ecx
  jne .Lprefix_loop
  mov $0x11, %r9d

  mov $1, %eax
  mov $1, %edi
  lea .Lempty_anchor(%rip), %rsi
  mov $0, %edx
  mov $PHASE_PREFIX, %r10d
.type sample_prefix,@function
sample_prefix:
  syscall
.type sample_prefix_after,@function
sample_prefix_after:
  /* This actual mapped helper address survives all later sample syscalls. */
  lea interp_helper(%rip), %r13
  movabs $APP_ENTRY, %rax
  jmp *%rax
.size interp_start, .-interp_start

/* Local symbol only: the executable receives its actual address in R13. */
.type interp_helper,@function
interp_helper:
  mov $5, %ecx
  mov $0, %r8d
.Lhelper_loop:
  lea 1(%r8), %r8
  dec %ecx
  jne .Lhelper_loop
  mov $0x35, %r9d
  ret
.size interp_helper, .-interp_helper

.section .rodata,"a",@progbits
.Lempty_anchor:
  .byte 0
.section .note.GNU-stack,"",@progbits
