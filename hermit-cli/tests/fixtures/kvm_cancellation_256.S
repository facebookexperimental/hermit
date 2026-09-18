/* Freestanding Detcore cancellation regression. Fixed protocol, no timing sleeps. */
.global _start, worker_timestamp, worker_timestamp_next, worker_completed
.section .text
_start:
    mov $435, %eax                 /* clone3 */
    lea clone_args(%rip), %rdi
    mov $88, %esi
    syscall
    test %rax, %rax
    js setup_failed
    jz worker
parent_wait:
    cmpl $0, ready(%rip)
    jne parent_exit
    mov $202, %eax                 /* FUTEX_WAIT | PRIVATE */
    lea ready(%rip), %rdi
    mov $128, %esi
    xor %edx, %edx
    xor %r10d, %r10d
    xor %r8d, %r8d
    xor %r9d, %r9d
    syscall
    test %rax, %rax
    jz parent_wait
    cmp $-11, %rax                 /* EAGAIN */
    je parent_wait
    cmp $-4, %rax                  /* EINTR */
    je parent_wait
    jmp setup_failed
parent_exit:
    mov $17, %edi
    jmp exit_group
worker:
    movl $1, ready(%rip)
    mov $202, %eax                 /* FUTEX_WAKE | PRIVATE */
    lea ready(%rip), %rdi
    mov $129, %esi
    mov $1, %edx
    xor %r10d, %r10d
    xor %r8d, %r8d
    xor %r9d, %r9d
    syscall
    test %rax, %rax
    js setup_failed
    mov $256, %r12d
worker_timestamp:
    rdtsc
worker_timestamp_next:
    dec %r12d
    jnz worker_timestamp
worker_completed:
    movl $1, completed(%rip)
    mov $95, %edi                  /* Worker queues 95; leader Exit(17) wins and cancels it. */
    jmp exit_group
setup_failed:
    mov $97, %edi
exit_group:
    mov $231, %eax
    syscall
    ud2
.section .data
.balign 8
clone_args:
    .quad 0x50f00                 /* VM | FS | FILES | SIGHAND | THREAD | SYSVSEM */
    .quad 0, 0, 0, 0
    .quad worker_stack
    .quad 16384
    .quad 0, 0, 0, 0
.section .bss
.balign 4
ready: .skip 4
completed: .skip 4
.balign 16
worker_stack: .skip 16384
.section .note.GNU-stack,"",@progbits
