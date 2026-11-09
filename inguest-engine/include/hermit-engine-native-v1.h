/* SPDX-License-Identifier: BSD-3-Clause */
#ifndef HERMIT_ENGINE_NATIVE_V1_H
#define HERMIT_ENGINE_NATIVE_V1_H

#include <stddef.h>
#include <stdint.h>
#include <sys/user.h>

/* Separate from the maintained client's sealed ABI16/104-byte callback table.
 * No old client is implicitly compatible with this additional engine ABI.
 * This header declares no initial/successor/private-image entry authority. */
#define HERMIT_ENGINE_NATIVE_ABI_V1 1u
typedef void (*hermit_native_emit_fn)(const uint8_t *, size_t);
typedef void (*hermit_native_idle_fn)(void);
/* Explicit launch facts, separate from the sealed ABI16 callback table.
 * Null+zero means absent; a nonnull zero-length control remains present and
 * invalid. Bytes are borrowed only until the engine copies/decodes them. */
typedef struct {
  uint32_t version, size, mode;
  int32_t panic_on_unsupported;
  uint64_t image;
  int32_t physical_owner, evidence_level;
  /* Genuine application owner and distinct real client-helper task. The
   * helper PID is sampled by its maintained native process-control owner. */
  int32_t background_pid;
  uint32_t reserved;
  hermit_native_emit_fn diagnostic;
  hermit_native_idle_fn idle;
  hermit_native_emit_fn evidence;
  const uint8_t *control;
  size_t control_len;
  const uint8_t *fingerprint;
  size_t fingerprint_len;
  const uint8_t *standalone_json;
  size_t standalone_json_len;
} hermit_native_startup_v1;
_Static_assert(sizeof(hermit_native_startup_v1) == 112, "startup ABI");
typedef int32_t (*hermit_native_current_fn)(uintptr_t);
/* Closed copied/vfork paths have no Tool/RPC/lifecycle identity. This frame
 * forwards only the actual physical callback and its existing launch policy. */
typedef struct {
  uint32_t version, size;
  int32_t physical_pid, physical_tid;
  uint64_t image;
  uintptr_t context;
  hermit_native_current_fn current;
} hermit_native_copied_callback_v1;
_Static_assert(sizeof(hermit_native_copied_callback_v1) == 40, "copied callback ABI");
typedef int64_t (*hermit_native_invoke_fn)(uintptr_t, int64_t, const uint64_t *, uint32_t);
typedef int32_t (*hermit_native_regs_read_fn)(uintptr_t, struct user_regs_struct *);
typedef int32_t (*hermit_native_regs_write_fn)(uintptr_t, const struct user_regs_struct *);
typedef int32_t (*hermit_native_cas_fn)(uintptr_t, uint32_t, uint32_t, uint32_t *);
/* Fixed mechanism records copied by the native owner. They carry observations
 * and one borrowed transaction, never a Rust object or admission authority. */
struct hermit_native_controller_fact {
  int32_t physical_pid, physical_tid;
  uint64_t procfs_device, procfs_inode;
};
struct hermit_native_private_snapshot {
  uint64_t owner[4];
  uint64_t version;
  uint32_t state, count;
  uint64_t ranges[8][2];
};
struct hermit_native_mapping_capture {
  struct hermit_native_private_snapshot expected;
  uint64_t nonce;
  int32_t root_pid, fd;
  unsigned char *request;
  size_t request_capacity, request_length;
  unsigned char *response;
  size_t response_capacity, response_length;
  uintptr_t lease;
  void (*release_lease)(uintptr_t, int32_t);
};
_Static_assert(sizeof(struct hermit_native_controller_fact) == 24, "controller fact ABI");
_Static_assert(sizeof(struct hermit_native_private_snapshot) == 176, "private snapshot ABI");
_Static_assert(sizeof(struct hermit_native_mapping_capture) == 256, "mapping capture ABI");
_Static_assert(offsetof(struct hermit_native_mapping_capture, request) == 192, "mapping request offset");
_Static_assert(offsetof(struct hermit_native_mapping_capture, release_lease) == 248, "mapping release offset");
typedef int32_t (*hermit_native_controller_fn)(uintptr_t, uint32_t, struct hermit_native_controller_fact *);
typedef int32_t (*hermit_native_private_fn)(uintptr_t, const struct hermit_native_private_snapshot *, struct hermit_native_private_snapshot *);
typedef int32_t (*hermit_native_mapping_fn)(uintptr_t, uint32_t, struct hermit_native_mapping_capture *);
typedef int32_t (*hermit_native_memory_read_fn)(uintptr_t, uint8_t *, size_t);
/* Actual backend fault-safe write; the returned byte count may be short. */
typedef size_t (*hermit_native_memory_write_fn)(uintptr_t, const uint8_t *, size_t);

typedef struct {
  uint32_t version;
  uint32_t size;
  hermit_native_current_fn current;
  hermit_native_invoke_fn invoke;
  hermit_native_regs_read_fn read_registers;
  hermit_native_regs_write_fn write_registers;
  hermit_native_cas_fn compare_exchange_u32;
  hermit_native_controller_fn controller_capture;
  hermit_native_private_fn private_memory;
  hermit_native_mapping_fn mapping_capture;
  hermit_native_memory_read_fn read_memory;
  hermit_native_memory_write_fn write_memory;
} hermit_native_mechanisms_v1;

typedef struct {
  int32_t physical_pid, physical_tid, tool_pid, tool_tid;
  uint64_t image;
} hermit_native_identity_v1;

typedef struct {
  uint32_t version;
  uint32_t size;
  hermit_native_identity_v1 identity;
  uintptr_t context;
  const hermit_native_mechanisms_v1 *mechanisms;
  int32_t parent;
  uint32_t cpuid_interception;
  uint64_t branches;
  /* Zero count selects the concrete engine's maintained actual application
   * stack walk/kernel-vector reader. It never supplies an empty-vector fact. */
  const unsigned long (*auxv)[2];
  size_t auxv_count;
  uint64_t initial_stack;
  const uint8_t (*signal_info)[128];
} hermit_native_callback_v1;

typedef struct {
  /* 0 = same-signal delivery, 1 = Tool answer/suppression, 2 = real native
   * deferred rewritten operation; 3 = original native lifecycle operation
   * retained through its real post owner (maps to native client action0).
   * Entry's negative status is an engine failure, never
   * permission for a raw kernel or old-runtime fallback. */
  int32_t action;
  uint32_t reserved;
  int64_t value;
  int64_t number;
  uint64_t arguments[6];
  uint32_t registers[4];
  int32_t signal;
  uint32_t signal_info_changed;
  uint8_t signal_info[128];
} hermit_native_answer_v1;

_Static_assert(sizeof(hermit_native_identity_v1) == 24, "identity ABI");
_Static_assert(sizeof(hermit_native_mechanisms_v1) == 88, "mechanism ABI");
_Static_assert(sizeof(hermit_native_callback_v1) == 96, "callback ABI");
_Static_assert(sizeof(hermit_native_answer_v1) == 224, "answer ABI");

typedef struct {
  hermit_native_identity_v1 identity;
  uint64_t child_tid_address;
  uint64_t flags;
  int32_t exit_signal;
  uint32_t reserved;
} hermit_native_child_v1;
_Static_assert(sizeof(hermit_native_child_v1) == 48, "child facts ABI");

/* Sampled from the actual birth transaction before the native post body
 * clears it. The real child callback is sampled only after that body and all
 * register writes; these facts never substitute for an affine Rust owner. */
typedef struct {
  int64_t number, result;
  uint64_t flags;
} hermit_native_copy_post_v1;
_Static_assert(sizeof(hermit_native_copy_post_v1) == 24, "copy post facts ABI");

/* Original native returning process-clone facts. These scalar observations
 * never substitute for the real parent's affine Deferred callback. */
typedef struct {
  int64_t number, result;
  uint64_t child_tid_address, flags;
  int32_t tool_child_tid, exit_signal;
} hermit_native_clone_post_v1;
_Static_assert(sizeof(hermit_native_clone_post_v1) == 40, "clone post facts ABI");

/* These are neutral image facts. The real native transport alone renders
 * its ABI16 capability-clear descriptor/ready output; no caller capability
 * or private phase authority is returned by this interface. */
uint64_t hermit_engine_native_image_begin_v1(void);
int32_t hermit_engine_native_ready_v1(uint64_t);
int32_t hermit_engine_native_background_v1(const hermit_native_startup_v1 *);
/* Existing scheduler shutdown + determinism-loss emission only. The native
 * owner still waits for its real background, drains evidence/RPC and exits. */
void hermit_engine_native_process_exit_v1(void);
int32_t hermit_engine_native_initialize_thread_v1(const hermit_native_callback_v1 *, void **);
int32_t hermit_engine_native_thread_created_v1(void *, const hermit_native_callback_v1 *, const hermit_native_child_v1 *);
int32_t hermit_engine_native_process_clone_result_v1(void *, const hermit_native_callback_v1 *, const hermit_native_clone_post_v1 *);
int32_t hermit_engine_native_exec_failed_v1(void *, const hermit_native_callback_v1 *);
/* The native owner frames these actual counters in its unchanged evidence wire. */
int32_t hermit_engine_native_totals_v1(uint64_t *, uint64_t *, uint64_t *, uint64_t *);
/* Only genuine independent-COW child post -> later native initializer. No
 * shared-VM/vfork child, synthetic post or second Tool can use these entries.
 * copied_post consumes/replaces the original locator only on success. */
int32_t hermit_engine_native_copied_post_v1(void *, const hermit_native_callback_v1 *, const hermit_native_copy_post_v1 *, void **);
int32_t hermit_engine_native_initialize_copied_thread_v1(void *, const hermit_native_callback_v1 *);
/* Separate status and copied-policy guest action. No inherited Tool/RPC
 * state is entered; status failure must not be treated as a guest errno. */
int32_t hermit_engine_native_copied_syscall_v1(const hermit_native_copied_callback_v1 *, int64_t, const uint64_t *, int32_t *);

/* Thread locators are created, retained and retired inside the engine. The
 * C owner must never copy/free/dereference one or reuse it in another task.
 * Actual register/FP/SIMD/signal commit, TLS/native context and evidence
 * framing remain the native client's responsibility. */
int32_t hermit_engine_native_syscall_v1(void *, const hermit_native_callback_v1 *, int64_t, const uint64_t (*)[6], uint64_t, hermit_native_answer_v1 *);
int32_t hermit_engine_native_instruction_v1(void *, const hermit_native_callback_v1 *, uint32_t, uint32_t, uint32_t, uint64_t, hermit_native_answer_v1 *);
int32_t hermit_engine_native_signal_v1(void *, const hermit_native_callback_v1 *, int32_t, uint64_t, hermit_native_answer_v1 *);
/* Exactly after actual native completion and its original bookkeeping. This
 * is not called merely because the engine returned action2. */
int32_t hermit_engine_native_completed_v1(void *, const hermit_native_callback_v1 *);
/* Capability-clear native cleanup: SUCCESS is the maintained Tool status;
 * the integer is not an authenticated terminal fact. No terminal receipt is
 * produced. Null means no initialized Tool state; negative status refuses
 * mechanism/owner failure rather than granting a fallback. */
int32_t hermit_engine_native_thread_exit_v1(void *, const hermit_native_callback_v1 *, int32_t);

#endif
