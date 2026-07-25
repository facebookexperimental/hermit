#ifndef ITERATIONS
#define ITERATIONS 100000
#endif

.global _start
.text
_start:
    mov $ITERATIONS, %r12
1:
    mov $39, %eax
    syscall
    dec %r12
    jnz 1b

    mov $231, %eax
    xor %edi, %edi
    syscall
