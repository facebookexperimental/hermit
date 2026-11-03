/* Ordinary native entry boundary. All addresses are synchronous, same-process
 * borrows; nothing here transports a Rust object or private phase authority. */
#ifndef HERMIT_COMMON_ENGINE_V1_H
#define HERMIT_COMMON_ENGINE_V1_H

#include <stddef.h>
#include <stdint.h>

#if !defined(__linux__) || !defined(__x86_64__)
#error "the common engine requires Linux x86-64"
#endif

/* reverie-inguest/src/guest/context.rs, exact 176-byte repr(C) layout. */
struct engine_register_context {
  uint64_t instruction_pointer, stack_pointer;
  uint64_t r15, r14, r13, r12, r11, r10, r9, r8;
  uint64_t rdi, rsi, rbp, rbx, rdx, rcx, rax, rflags;
  uint64_t saved_extended_state[4];
};

_Static_assert(sizeof(struct engine_register_context) == 176, "context size");
_Static_assert(_Alignof(struct engine_register_context) == 8, "context alignment");
_Static_assert(offsetof(struct engine_register_context, r11) == 48, "r11 offset");
_Static_assert(offsetof(struct engine_register_context, rflags) == 136, "flags offset");

void hermit_engine_preload_initialize_v1(void);
void hermit_engine_loader_initialize_v1(size_t argc, const char *const *argv);
void hermit_engine_saved_syscall_v1(struct engine_register_context *, int64_t number);

/* Negotiated only by a held ordinary loader, before its post_load entry. The
 * callback writes <= capacity bytes and returns their count or negative errno.
 * It transports an opaque authenticated payload; it grants no phase authority. */
typedef long (*engine_take_loader_state_v1)(void *buffer, size_t capacity);
int hermit_engine_install_loader_state_v1(engine_take_loader_state_v1);

/* Naked forward to the existing exact trusted gate; no Rust initialization,
 * allocator or TLS is touched. The caller keeps Linux pointer obligations. */
int64_t hermit_engine_raw_syscall_v1(uint64_t number, uint64_t, uint64_t,
                                   uint64_t, uint64_t, uint64_t, uint64_t);

#endif
