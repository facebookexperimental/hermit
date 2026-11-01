/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Every way Linux ends a robust-futex owner walks its robust list
 * (kernel/futex/core.c, exit_robust_list() and handle_futex_death()), and
 * this guest ends one each way, then reports what the walk did:
 *
 *   thread-exit     a thread's own exit, over a hand-written list: an entry
 *                   it owns, an entry another thread owns (left alone), an
 *                   entry it owns with a waiter (marked, one waiter woken),
 *                   and a pending entry whose word is 0 (its waiter woken,
 *                   the word left 0);
 *   bad-pointer     a thread's own exit, over a list with an entry whose word
 *                   cannot be read: the walk stops there, leaving the entry
 *                   after it and the pending entry alone;
 *   null-entry      a thread's own exit, over a list whose first entry and
 *                   pending entry are both NULL, with a futex offset that
 *                   reaches a word the thread owns: Linux compares the entry
 *                   with the pending entry as pointers, skips the NULL entry
 *                   and leaves the word alone;
 *   pi-thread-exit  a thread's own exit holding two robust
 *                   priority-inheritance mutexes, one with a waiter;
 *   exit-group      exit_group: every thread's list is walked;
 *   fatal-signal    a signal whose default action ends the process;
 *   sigkill-self    a SIGKILL a thread sends its own process;
 *   exec-owner      an exec by the owner: futex_exec_release walks its list;
 *   exec-sibling    an exec that ends an owning sibling (de_thread);
 *   exec-failed     an exec that fails: nothing is walked;
 *   exec-failed-file  an exec that fails while the thread holds a robust
 *                   mutex on a MAP_SHARED memfd page: nothing is walked, so
 *                   the exec returns ENOENT and the thread unlocks the mutex;
 *   exec-alias      an exec whose argument string is the last byte of a lock
 *                   word the exec'ing thread owns: Linux copies the strings
 *                   before it walks the list, so the new image receives the
 *                   byte unmarked.
 *
 * The parent observes each case from another process, through a
 * MAP_SHARED mapping and process-shared mutexes. A waiter that gets
 * EOWNERDEAD makes the mutex consistent and locks it again. Nothing printed
 * depends on a thread ID, so native Linux and every backend print the same
 * lines. With the argument skip-exec-sibling the exec-sibling case is left
 * out, and reported as skipped.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <linux/futex.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdarg.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#if !defined(__GLIBC__)
#error "This guest relies on glibc's robust mutex protocol"
#endif

/* One robust-list node as a hand-written lock lays it out: the link, then
 * the 32-bit lock word. Every hand-written list here uses this offset. */
struct node {
  struct robust_list link;
  _Atomic uint32_t word;
  uint32_t unused;
};

#define FUTEX_OFFSET \
  ((long)offsetof(struct node, word) - (long)offsetof(struct node, link))

struct shared {
  /* thread-exit */
  struct robust_list_head head;
  struct node own, other, contended, pending;
  _Atomic uint32_t other_tid;
  /* The thread that owns a hand-written list. */
  _Atomic uint32_t owner_tid;
  _Atomic int waiters_ready;
  _Atomic int contended_result;
  _Atomic int pending_result;
  /* bad-pointer */
  struct robust_list_head bad_head;
  struct node before, after, bad_pending;
  /* null-entry */
  struct robust_list_head null_head;
  _Atomic uint32_t null_word;
  /* exec-failed-file */
  _Atomic int file_exec_errno;
  _Atomic int file_unlock_result;
  /* glibc robust mutexes, process-shared */
  pthread_mutex_t m1, m2, m3;
  pthread_mutex_t pi1, pi2;
  _Atomic int owners_ready;
  _Atomic int pi_waiter_result;
  _Atomic int pi_uncontended_result;
  _Atomic int stage;
};

static struct shared* state;
static char* fault_pages;
static long page_size;
static const char* self_path;

static void die(const char* what) {
  fprintf(stderr, "robust-routes: %s: %s\n", what, strerror(errno));
  _exit(2);
}

/* One line, one write, so no stdio buffer is copied into a child. */
static void say(const char* format, ...) {
  char line[512];
  va_list args;
  va_start(args, format);
  int length = vsnprintf(line, sizeof(line), format, args);
  va_end(args);
  if (length < 0 || (size_t)length >= sizeof(line)) {
    die("format");
  }
  const char* cursor = line;
  while (length > 0) {
    ssize_t written = write(STDOUT_FILENO, cursor, (size_t)length);
    if (written < 0 && errno == EINTR) {
      continue;
    }
    if (written <= 0) {
      die("write");
    }
    cursor += written;
    length -= (int)written;
  }
}

static void wait_until(_Atomic int* flag, int value) {
  for (unsigned i = 0; i < 100000000; ++i) {
    if (atomic_load(flag) >= value) {
      return;
    }
    sched_yield();
  }
  fprintf(stderr, "robust-routes: timed out waiting for %d\n", value);
  _exit(3);
}

/* Until another process or thread blocks on the mutex's word. */
static void wait_for_waiter(pthread_mutex_t* mutex) {
  _Atomic uint32_t* word = (_Atomic uint32_t*)&mutex->__data.__lock;
  for (unsigned i = 0; i < 100000000; ++i) {
    if (atomic_load(word) & FUTEX_WAITERS) {
      return;
    }
    sched_yield();
  }
  fputs("robust-routes: timed out waiting for FUTEX_WAITERS\n", stderr);
  _exit(4);
}

/* Lets threads that announced a wait reach it. */
static void settle(void) {
  struct timespec pause_for = {0, 50 * 1000 * 1000};
  while (nanosleep(&pause_for, &pause_for) != 0 && errno == EINTR) {
  }
}

static uint32_t gettid_u32(void) {
  return (uint32_t)syscall(SYS_gettid);
}

static long futex_wait(_Atomic uint32_t* word, uint32_t expected) {
  struct timespec timeout = {5, 0};
  return syscall(SYS_futex, word, FUTEX_WAIT, expected, &timeout, NULL, 0);
}

/* An errno's name, or "unset" for a result never recorded. */
static const char* errno_name(int result) {
  const char* name = result > 0 ? strerrorname_np(result) : NULL;
  return name != NULL ? name : "unset";
}

static const char* wait_result(int result) {
  return result == 0 ? "woken" : errno_name(result);
}

static const char* lock_result(int result) {
  return result == 0 ? "0" : errno_name(result);
}

/* What happened to a word that held `original`, in terms that name no TID. */
static const char* word_state(uint32_t value, uint32_t original) {
  if (value == original) {
    return "untouched";
  }
  if ((value & FUTEX_TID_MASK) != 0) {
    return "owned-by-another";
  }
  switch (value) {
    case 0:
      return "0";
    case FUTEX_OWNER_DIED:
      return "OWNER_DIED";
    case FUTEX_OWNER_DIED | FUTEX_WAITERS:
      return "OWNER_DIED|WAITERS";
    case FUTEX_WAITERS:
      return "WAITERS";
    default:
      return "unexpected";
  }
}

static const char* child_status(int status, char* buffer, size_t size) {
  if (WIFEXITED(status)) {
    snprintf(buffer, size, "exit %d", WEXITSTATUS(status));
  } else if (WIFSIGNALED(status)) {
    snprintf(buffer, size, "signal %d", WTERMSIG(status));
  } else {
    snprintf(buffer, size, "status %#x", status);
  }
  return buffer;
}

static int reap(pid_t pid) {
  int status;
  while (waitpid(pid, &status, 0) != pid) {
    if (errno != EINTR) {
      die("waitpid");
    }
  }
  return status;
}

static void init_mutex(pthread_mutex_t* mutex, int protocol) {
  pthread_mutexattr_t attr;
  if (pthread_mutexattr_init(&attr) != 0 ||
      pthread_mutexattr_setpshared(&attr, PTHREAD_PROCESS_SHARED) != 0 ||
      pthread_mutexattr_setrobust(&attr, PTHREAD_MUTEX_ROBUST) != 0 ||
      pthread_mutexattr_setprotocol(&attr, protocol) != 0 ||
      pthread_mutex_init(mutex, &attr) != 0 ||
      pthread_mutexattr_destroy(&attr) != 0) {
    die("mutex init");
  }
}

static void lock_or_die(pthread_mutex_t* mutex) {
  int rc = pthread_mutex_lock(mutex);
  if (rc != 0) {
    errno = rc;
    die("owner lock");
  }
}

/* Locks a mutex whose owner died: EOWNERDEAD is made consistent and
 * unlocked. Returns what the lock returned. */
static int lock_after_owner_death(pthread_mutex_t* mutex) {
  int rc = pthread_mutex_lock(mutex);
  if (rc == EOWNERDEAD) {
    if (pthread_mutex_consistent(mutex) != 0) {
      die("pthread_mutex_consistent");
    }
  }
  if (rc == 0 || rc == EOWNERDEAD) {
    if (pthread_mutex_unlock(mutex) != 0) {
      die("pthread_mutex_unlock");
    }
  }
  return rc;
}

static pid_t fork_or_die(void) {
  pid_t pid = fork();
  if (pid < 0) {
    die("fork");
  }
  return pid;
}

static void* pause_forever(void) {
  for (;;) {
    pause();
  }
  return NULL;
}

/* ---- thread-exit ------------------------------------------------------ */

static void* list_owner(void* unused) {
  (void)unused;
  struct shared* s = state;
  uint32_t me = gettid_u32();
  atomic_store(&s->owner_tid, me);
  s->head.list.next = &s->own.link;
  s->own.link.next = &s->other.link;
  s->other.link.next = &s->contended.link;
  s->contended.link.next = &s->head.list;
  s->head.futex_offset = FUTEX_OFFSET;
  s->head.list_op_pending = &s->pending.link;
  atomic_store(&s->own.word, me);
  atomic_store(&s->other.word, atomic_load(&s->other_tid));
  atomic_store(&s->contended.word, me | FUTEX_WAITERS);
  atomic_store(&s->pending.word, 0);
  if (syscall(SYS_set_robust_list, &s->head, sizeof(s->head)) != 0) {
    die("set_robust_list");
  }
  atomic_store(&s->stage, 1);
  wait_until(&s->waiters_ready, 2);
  settle();
  /* A bare exit: the walk runs in the kernel's exit, after nothing else. */
  syscall(SYS_exit, 0);
  return NULL;
}

static void* contended_waiter(void* unused) {
  (void)unused;
  struct shared* s = state;
  wait_until(&s->stage, 1);
  uint32_t expected = atomic_load(&s->contended.word);
  atomic_fetch_add(&s->waiters_ready, 1);
  long rc = futex_wait(&s->contended.word, expected);
  atomic_store(&s->contended_result, rc == 0 ? 0 : errno);
  return NULL;
}

static void* pending_waiter(void* unused) {
  (void)unused;
  struct shared* s = state;
  wait_until(&s->stage, 1);
  atomic_fetch_add(&s->waiters_ready, 1);
  long rc = futex_wait(&s->pending.word, 0);
  atomic_store(&s->pending_result, rc == 0 ? 0 : errno);
  return NULL;
}

static void thread_exit_case(void) {
  struct shared* s = state;
  atomic_store(&s->stage, 0);
  atomic_store(&s->waiters_ready, 0);
  atomic_store(&s->contended_result, -1);
  atomic_store(&s->pending_result, -1);
  pid_t child = fork_or_die();
  if (child == 0) {
    atomic_store(&s->other_tid, gettid_u32());
    pthread_t owner, contended, pending;
    if (pthread_create(&contended, NULL, contended_waiter, NULL) != 0 ||
        pthread_create(&pending, NULL, pending_waiter, NULL) != 0 ||
        pthread_create(&owner, NULL, list_owner, NULL) != 0) {
      die("pthread_create");
    }
    if (pthread_join(owner, NULL) != 0 ||
        pthread_join(contended, NULL) != 0 ||
        pthread_join(pending, NULL) != 0) {
      die("pthread_join");
    }
    _exit(0);
  }
  char status[32];
  child_status(reap(child), status, sizeof(status));
  uint32_t other = atomic_load(&s->other_tid);
  uint32_t owner = atomic_load(&s->owner_tid);
  say("thread-exit: own=%s other=%s contended=%s waiter=%s pending=%s waiter=%s child=%s\n",
      word_state(atomic_load(&s->own.word), owner),
      word_state(atomic_load(&s->other.word), other),
      word_state(atomic_load(&s->contended.word), owner | FUTEX_WAITERS),
      wait_result(atomic_load(&s->contended_result)),
      word_state(atomic_load(&s->pending.word), 0),
      wait_result(atomic_load(&s->pending_result)),
      status);
}

/* ---- bad-pointer ------------------------------------------------------ */

static void* bad_list_owner(void* unused) {
  (void)unused;
  struct shared* s = state;
  uint32_t me = gettid_u32();
  atomic_store(&s->owner_tid, me);
  /* The node's link is the last word of a readable page, and its lock word
   * the first word of the PROT_NONE page after it. */
  struct robust_list* bad =
      (struct robust_list*)(fault_pages + page_size - sizeof(*bad));
  s->bad_head.list.next = &s->before.link;
  s->before.link.next = bad;
  bad->next = &s->after.link;
  s->after.link.next = &s->bad_head.list;
  s->bad_head.futex_offset = FUTEX_OFFSET;
  s->bad_head.list_op_pending = &s->bad_pending.link;
  atomic_store(&s->before.word, me);
  atomic_store(&s->after.word, me);
  atomic_store(&s->bad_pending.word, me);
  if (syscall(SYS_set_robust_list, &s->bad_head, sizeof(s->bad_head)) != 0) {
    die("set_robust_list");
  }
  syscall(SYS_exit, 0);
  return NULL;
}

static void bad_pointer_case(void) {
  struct shared* s = state;
  atomic_store(&s->owner_tid, 0);
  pid_t child = fork_or_die();
  if (child == 0) {
    pthread_t owner;
    if (pthread_create(&owner, NULL, bad_list_owner, NULL) != 0 ||
        pthread_join(owner, NULL) != 0) {
      die("pthread");
    }
    _exit(0);
  }
  char status[32];
  child_status(reap(child), status, sizeof(status));
  uint32_t owner = atomic_load(&s->owner_tid);
  say("bad-pointer: before=%s after=%s pending=%s child=%s\n",
      word_state(atomic_load(&s->before.word), owner),
      word_state(atomic_load(&s->after.word), owner),
      word_state(atomic_load(&s->bad_pending.word), owner),
      status);
}

/* ---- null-entry ------------------------------------------------------- */

/* The list's first entry and its pending entry are both NULL, and the futex
 * offset is the address of a word the thread owns, so NULL + offset names
 * that word. Linux's exit_robust_list compares the entry with the pending
 * entry as pointers (`entry != pending`), so it skips the NULL entry; the
 * fetch of its next pointer then faults and ends the walk, and the word is
 * left alone. */
static void* null_list_owner(void* unused) {
  (void)unused;
  struct shared* s = state;
  uint32_t me = gettid_u32();
  atomic_store(&s->owner_tid, me);
  atomic_store(&s->null_word, me);
  s->null_head.list.next = NULL;
  s->null_head.futex_offset = (long)(uintptr_t)&s->null_word;
  s->null_head.list_op_pending = NULL;
  if (syscall(SYS_set_robust_list, &s->null_head, sizeof(s->null_head)) != 0) {
    die("set_robust_list");
  }
  syscall(SYS_exit, 0);
  return NULL;
}

static void null_entry_case(void) {
  struct shared* s = state;
  atomic_store(&s->owner_tid, 0);
  pid_t child = fork_or_die();
  if (child == 0) {
    pthread_t owner;
    if (pthread_create(&owner, NULL, null_list_owner, NULL) != 0 ||
        pthread_join(owner, NULL) != 0) {
      die("pthread");
    }
    _exit(0);
  }
  char status[32];
  child_status(reap(child), status, sizeof(status));
  say("null-entry: word=%s child=%s\n",
      word_state(atomic_load(&s->null_word), atomic_load(&s->owner_tid)),
      status);
}

/* ---- pi-thread-exit --------------------------------------------------- */

static void* pi_owner(void* unused) {
  (void)unused;
  struct shared* s = state;
  lock_or_die(&s->pi1);
  lock_or_die(&s->pi2);
  atomic_store(&s->stage, 9);
  wait_for_waiter(&s->pi1);
  syscall(SYS_exit, 0);
  return NULL;
}

static void* pi_waiter(void* unused) {
  (void)unused;
  struct shared* s = state;
  wait_until(&s->stage, 9);
  atomic_store(&s->pi_waiter_result, lock_after_owner_death(&s->pi1));
  return NULL;
}

static void pi_thread_exit_case(void) {
  struct shared* s = state;
  init_mutex(&s->pi1, PTHREAD_PRIO_INHERIT);
  init_mutex(&s->pi2, PTHREAD_PRIO_INHERIT);
  atomic_store(&s->stage, 0);
  atomic_store(&s->pi_waiter_result, -1);
  atomic_store(&s->pi_uncontended_result, -1);
  pid_t child = fork_or_die();
  if (child == 0) {
    pthread_t owner, waiter;
    if (pthread_create(&owner, NULL, pi_owner, NULL) != 0 ||
        pthread_create(&waiter, NULL, pi_waiter, NULL) != 0 ||
        pthread_join(owner, NULL) != 0 || pthread_join(waiter, NULL) != 0) {
      die("pthread");
    }
    atomic_store(&s->pi_uncontended_result, lock_after_owner_death(&s->pi2));
    _exit(0);
  }
  char status[32];
  child_status(reap(child), status, sizeof(status));
  say("pi-thread-exit: waiter=%s uncontended=%s child=%s\n",
      lock_result(atomic_load(&s->pi_waiter_result)),
      lock_result(atomic_load(&s->pi_uncontended_result)),
      status);
}

/* ---- the process deaths ----------------------------------------------- */

static void* first_owner(void* unused) {
  (void)unused;
  lock_or_die(&state->m1);
  atomic_fetch_add(&state->owners_ready, 1);
  return pause_forever();
}

static void* second_owner(void* unused) {
  (void)unused;
  lock_or_die(&state->m2);
  atomic_fetch_add(&state->owners_ready, 1);
  return pause_forever();
}

enum death { EXIT_GROUP, FATAL_SIGNAL, SIGKILL_SELF };

/* Two threads own m1 and m2, the process's first thread owns m3; once the
 * parent waits on m1, the process dies all at once. */
static void group_death_case(const char* name, enum death death) {
  struct shared* s = state;
  init_mutex(&s->m1, PTHREAD_PRIO_NONE);
  init_mutex(&s->m2, PTHREAD_PRIO_NONE);
  init_mutex(&s->m3, PTHREAD_PRIO_NONE);
  atomic_store(&s->owners_ready, 0);
  atomic_store(&s->stage, 0);
  pid_t child = fork_or_die();
  if (child == 0) {
    pthread_t first, second;
    if (pthread_create(&first, NULL, first_owner, NULL) != 0 ||
        pthread_create(&second, NULL, second_owner, NULL) != 0) {
      die("pthread_create");
    }
    lock_or_die(&s->m3);
    wait_until(&s->owners_ready, 2);
    atomic_store(&s->stage, 3);
    wait_for_waiter(&s->m1);
    switch (death) {
      case EXIT_GROUP:
        syscall(SYS_exit_group, 0);
        break;
      case FATAL_SIGNAL:
        raise(SIGTERM);
        break;
      case SIGKILL_SELF:
        kill(getpid(), SIGKILL);
        break;
    }
    _exit(5);
  }
  wait_until(&s->stage, 3);
  int m1 = pthread_mutex_lock(&s->m1);
  int consistent = -1;
  int relock = -1;
  if (m1 == EOWNERDEAD) {
    consistent = pthread_mutex_consistent(&s->m1);
    if (pthread_mutex_unlock(&s->m1) != 0) {
      die("unlock m1");
    }
    relock = pthread_mutex_lock(&s->m1);
    if (relock == 0 && pthread_mutex_unlock(&s->m1) != 0) {
      die("unlock m1 again");
    }
  } else if (m1 == 0) {
    pthread_mutex_unlock(&s->m1);
  }
  int m2 = lock_after_owner_death(&s->m2);
  int m3 = lock_after_owner_death(&s->m3);
  char status[32];
  child_status(reap(child), status, sizeof(status));
  say("%s: m1=%s consistent=%s relock=%s m2=%s m3=%s child=%s\n",
      name,
      lock_result(m1),
      consistent == -1 ? "-" : lock_result(consistent),
      relock == -1 ? "-" : lock_result(relock),
      lock_result(m2),
      lock_result(m3),
      status);
}

/* ---- the execs ---------------------------------------------------------- */

enum exec_kind { EXEC_OWNER, EXEC_SIBLING, EXEC_FAILED };

static void exec_case(const char* name, enum exec_kind kind) {
  struct shared* s = state;
  init_mutex(&s->m1, PTHREAD_PRIO_NONE);
  atomic_store(&s->owners_ready, 0);
  atomic_store(&s->stage, 0);
  pid_t child = fork_or_die();
  if (child == 0) {
    if (kind == EXEC_SIBLING) {
      pthread_t owner;
      if (pthread_create(&owner, NULL, first_owner, NULL) != 0) {
        die("pthread_create");
      }
      wait_until(&s->owners_ready, 1);
    } else {
      lock_or_die(&s->m1);
    }
    atomic_store(&s->stage, 6);
    wait_for_waiter(&s->m1);
    if (kind == EXEC_FAILED) {
      execl("/nonexistent/robust-list-owner-death-routes", "missing", NULL);
      settle();
      _exit(pthread_mutex_unlock(&s->m1) == 0 ? 0 : 6);
    }
    execl(self_path, self_path, "after-exec", NULL);
    _exit(7);
  }
  wait_until(&s->stage, 6);
  int m1 = lock_after_owner_death(&s->m1);
  char status[32];
  child_status(reap(child), status, sizeof(status));
  say("%s: m1=%s child=%s\n", name, lock_result(m1), status);
}

/* The process's only thread holds a robust mutex whose word is on a
 * MAP_SHARED mapping of a memfd, and calls an exec that fails. Linux walks
 * the robust list only past the exec's point of no return, so a failed exec
 * walks nothing: it returns ENOENT, and the mutex is still the thread's to
 * unlock. */
static void exec_failed_file_case(void) {
  struct shared* s = state;
  atomic_store(&s->file_exec_errno, -1);
  atomic_store(&s->file_unlock_result, -1);
  pid_t child = fork_or_die();
  if (child == 0) {
    int fd = (int)syscall(SYS_memfd_create, "robust-routes", 0);
    if (fd < 0 || ftruncate(fd, page_size) != 0) {
      die("memfd");
    }
    pthread_mutex_t* mutex = mmap(
        NULL, (size_t)page_size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (mutex == MAP_FAILED) {
      die("mmap memfd");
    }
    init_mutex(mutex, PTHREAD_PRIO_NONE);
    lock_or_die(mutex);
    execl("/nonexistent/robust-list-owner-death-routes", "missing", NULL);
    atomic_store(&s->file_exec_errno, errno);
    atomic_store(&s->file_unlock_result, pthread_mutex_unlock(mutex));
    _exit(0);
  }
  char status[32];
  child_status(reap(child), status, sizeof(status));
  say("exec-failed-file: errno=%s unlock=%s child=%s\n",
      errno_name(atomic_load(&s->file_exec_errno)),
      lock_result(atomic_load(&s->file_unlock_result)),
      status);
}

/* The process's only thread owns a hand-written robust-list entry whose lock
 * word holds its TID, and passes the exec, as its argument, the string that
 * starts at the word's last byte: that byte, then the zero byte after the
 * word. Linux copies the argument strings (do_execveat_common) before the
 * exec walks the list (exec_mm_release, past the point of no return), so the
 * new image receives the byte as it was, the TID's highest byte, which is 0
 * for any TID below 2^24: an empty string. Marking the word first would make
 * that byte FUTEX_OWNER_DIED's 0x40. The new image exits with the length of
 * the string it received. */
static void exec_alias_case(void) {
  pid_t child = fork_or_die();
  if (child == 0) {
    static struct robust_list_head head;
    static struct node owned;
    head.list.next = &owned.link;
    owned.link.next = &head.list;
    head.futex_offset = FUTEX_OFFSET;
    head.list_op_pending = NULL;
    atomic_store(&owned.word, gettid_u32());
    owned.unused = 0;
    if (syscall(SYS_set_robust_list, &head, sizeof(head)) != 0) {
      die("set_robust_list");
    }
    char* alias = (char*)&owned.word + sizeof(uint32_t) - 1;
    execl(self_path, self_path, "argument-length", alias, NULL);
    _exit(7);
  }
  int raw = reap(child);
  char status[32];
  child_status(raw, status, sizeof(status));
  say("exec-alias: argument-length=%d child=%s\n",
      WIFEXITED(raw) ? WEXITSTATUS(raw) : -1,
      status);
}

int main(int argc, char** argv) {
  if (argc == 2 && strcmp(argv[1], "after-exec") == 0) {
    return 0;
  }
  /* The image exec-alias execs: exit with the argument's length. */
  if (argc == 3 && strcmp(argv[1], "argument-length") == 0) {
    return (int)strlen(argv[2]);
  }
  /* skip-exec-sibling leaves out the one case that needs an exec by a
   * process with another live thread, for a run on a host that cannot make
   * one yet (Hermit's KVM backend:
   * https://github.com/rrnewton/hermit/issues/4904). */
  bool skip_exec_sibling = argc == 2 && strcmp(argv[1], "skip-exec-sibling") == 0;
  if (argc != 1 && !skip_exec_sibling) {
    fprintf(stderr, "usage: %s [skip-exec-sibling]\n", argv[0]);
    return 64;
  }
  self_path = argv[0];
  page_size = sysconf(_SC_PAGESIZE);
  state = mmap(
      NULL,
      sizeof(*state),
      PROT_READ | PROT_WRITE,
      MAP_SHARED | MAP_ANONYMOUS,
      -1,
      0);
  if (state == MAP_FAILED) {
    die("mmap shared");
  }
  fault_pages = mmap(
      NULL,
      2 * (size_t)page_size,
      PROT_READ | PROT_WRITE,
      MAP_PRIVATE | MAP_ANONYMOUS,
      -1,
      0);
  if (fault_pages == MAP_FAILED ||
      mprotect(fault_pages + page_size, (size_t)page_size, PROT_NONE) != 0) {
    die("fault pages");
  }

  thread_exit_case();
  bad_pointer_case();
  null_entry_case();
  pi_thread_exit_case();
  group_death_case("exit-group", EXIT_GROUP);
  group_death_case("fatal-signal", FATAL_SIGNAL);
  group_death_case("sigkill-self", SIGKILL_SELF);
  exec_case("exec-owner", EXEC_OWNER);
  if (skip_exec_sibling) {
    say("exec-sibling: skipped\n");
  } else {
    exec_case("exec-sibling", EXEC_SIBLING);
  }
  exec_case("exec-failed", EXEC_FAILED);
  exec_failed_file_case();
  exec_alias_case();
  say("robust-list-owner-death-routes: done\n");
  return 0;
}
