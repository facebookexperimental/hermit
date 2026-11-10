/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * chosen_guest_ids: prints every process and thread ID the guest can observe
 * through the kernel, by each way it learns one, so that two runs of a guest
 * without a PID namespace (`hermit run --no-namespace`) can be compared ID for
 * ID.
 *
 * It creates, in this order: two threads (pthread_create, whose clone the C
 * library makes with CLONE_PARENT_SETTID), a process by fork, a process by
 * vfork, a process by a raw clone with CLONE_PARENT_SETTID and
 * CLONE_CHILD_SETTID, and a process by a raw clone3. For each it prints the ID
 * the creating call returned, the ID the new task reads for itself (getpid or
 * gettid), the words the kernel wrote for it (the settid words), and the ID
 * the wait for it returned (waitpid, waitid's si_pid). It prints the main
 * thread's own IDs and its /proc/self link first.
 *
 * With the argument `collide`, before the fork the guest makes a process of
 * its own with clone3's set_tid naming the ID three after its first (the ID
 * Hermit would choose next), which it needs CAP_CHECKPOINT_RESTORE or
 * CAP_SYS_ADMIN for, and keeps it alive across the fork, so that Hermit's
 * automatic allocator must skip that accepted guest choice.
 * `collide-external REQUEST ACK` instead asks a native supervisor outside
 * Hermit to hold that ID and acknowledges it before the fork, exercising a
 * genuine external collision. `external-holder REQUEST ACK COMMAND...` runs
 * that supervisor and bounds/reaps its children.
 *
 * With one of these arguments it makes one call instead and prints its
 * outcome:
 * - `sighand`: a raw clone with CLONE_SIGHAND but not CLONE_VM, which Linux
 *   refuses with EINVAL;
 * - `newpid`: a clone3 with CLONE_NEWPID and no set_tid; the child prints its
 *   own ID, 1 in its namespace;
 * - `newpid-fork`, `newpid-set-tid`: `newpid`, whose child then forks, or
 *   makes a clone3 with CLONE_NEWPID and set_tid [1], from inside its new
 *   namespace;
 * - `own-tid N`: a clone3 with CLONE_NEWPID and its own set_tid of N entries,
 *   [1] (the new namespace's ID only) or [1, first + 100] (also the ID in the
 *   guest's own namespace); the child checks it is 1 in its namespace;
 * - `unknown-flag`: `own-tid 1` with an unknown flag bit too (EINVAL);
 * - `edge-tid`: `own-tid 1` with the array in the last four bytes of a page
 *   that an unmapped page follows;
 * - `kernel-args`, `kernel-args-top`: a clone3 whose struct clone_args
 *   pointer is in the kernel half, (void*)-4096 or (void*)-2 (EFAULT);
 * - `wronly-args`, `wronly-vfork`: a clone3 (with CLONE_VFORK for the
 *   second) whose struct clone_args is on a page mapped writable but not
 *   readable, which the kernel reads all the same; `wronly-tid` the same for
 *   `own-tid 1`'s array; `wronly-short` the first with a size of 32, which
 *   every kernel refuses (EINVAL);
 * - `leader-gone CALL`: CALL, by a second thread once the first has exited
 *   (`pthread_exit`), so that the thread group leader is a zombie;
 * - `vfork-detached`: a raw clone with CLONE_VFORK, CLONE_PIDFD and
 *   CLONE_DETACHED, which every kernel refuses (EINVAL); `untraced-detached`
 *   the same with CLONE_UNTRACED instead of CLONE_VFORK;
 * - `nested`: `own-tid 1` after unshare(CLONE_NEWPID), so that the array is
 *   two levels short of the guest's own namespace;
 * - `no-caps`: `own-tid 1` with CLONE_NEWUSER too, after the guest gives up
 *   every capability: Linux lets it make the user namespace and choose the
 *   ID inside it;
 * - `bad-tid`: a clone3 whose set_tid points at an unmapped address (EFAULT);
 * - `bad-size`: a clone3 with set_tid but set_tid_size 0 (EINVAL);
 * - `seccomp`: a fork after the guest installs a seccomp filter of its own
 *   (one that allows everything);
 * - `signal65`: a raw clone with exit signal 65, accepted by some older
 *   kernels and refused with EINVAL by newer ones; `vfork65` the same with
 *   CLONE_VFORK, whose parent waits
 *   for its child's exit;
 * - `sp1`: a raw clone with new stack pointer 1, which clone3 cannot express
 *   (its child would fault at once).
 *
 * Every line is the ID's offset from the first process's, so a run whose IDs
 * are chosen from a base prints the same bytes whatever the base, with the
 * offsets in creation order, and a run on host IDs shows whatever IDs other
 * host tasks left. Exits 0 when every ID agreed with the others for the same
 * task, 1 otherwise.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/capability.h>
#include <linux/filter.h>
#include <linux/sched.h>
#include <linux/seccomp.h>
#include <pthread.h>
#include <poll.h>
#include <sched.h>
#include <sys/mman.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/stat.h>
#include <sys/socket.h>
#include <sys/vfs.h>
#include <sys/wait.h>
#include <unistd.h>

static pid_t first;
static int mismatches;

static void report(const char* what, long id, long expected) {
  printf("%s %+ld\n", what, id - first);
  if (id != expected) {
    printf("  MISMATCH: %s is %ld, expected %ld\n", what, id, expected);
    mismatches++;
  }
}

/* An ID whose value only the numbering decides: printed, not checked. */
static void show(const char* what, long id) {
  printf("%s %+ld\n", what, id - first);
}

static void* thread_main(void* arg) {
  *(pid_t*)arg = (pid_t)syscall(SYS_gettid);
  return NULL;
}

static pid_t wait_for(pid_t child, const char* what) {
  int status = 0;
  pid_t waited = waitpid(child, &status, 0);
  if (waited < 0) {
    printf("%s: waitpid failed: %s\n", what, strerror(errno));
    exit(1);
  }
  return waited;
}

/* A process with ID `id`, made by clone3's set_tid, that waits to be
 * killed. */
static pid_t hold(pid_t id) {
  struct clone_args args;
  memset(&args, 0, sizeof args);
  args.exit_signal = SIGCHLD;
  args.set_tid = (uint64_t)(uintptr_t)&id;
  args.set_tid_size = 1;
  long holder = syscall(SYS_clone3, &args, sizeof args);
  if (holder == 0) {
    for (;;) {
      pause();
    }
  }
  if (holder < 0) {
    printf("clone3 set_tid %d failed: %s\n", id, strerror(errno));
    exit(1);
  }
  return (pid_t)holder;
}

/* Native supervisor, outside Hermit: the guest requests the ID only after
 * startup admission checked the initial window. Acknowledgement proves the
 * external holder exists before the guest attempts its next fork. */
static int external_holder(int argc, char** argv) {
  if (argc < 5) return 99;
  const char* request_path = argv[2];
  const char* ack_path = argv[3];
  int request = -1, ack = -1, result = 99;
  pid_t hermit = -1, holder = -1;
  if (mkfifo(request_path, 0600) != 0 || mkfifo(ack_path, 0600) != 0) goto done;
  request = open(request_path, O_RDWR | O_NONBLOCK | O_CLOEXEC);
  ack = open(ack_path, O_RDWR | O_CLOEXEC);
  if (request < 0 || ack < 0) goto done;
  hermit = fork();
  if (hermit < 0) goto done;
  if (hermit == 0) {
    execv(argv[4], argv + 4);
    _exit(99);
  }
  // At most 150 polls of 100 ms; the enclosing test runner owns its unchanged
  // CPU/wall bounds. Every failure kills and reaps both direct children.
  for (int turn = 0; turn < 150; turn++) {
    int status;
    pid_t ended = waitpid(hermit, &status, WNOHANG);
    if (ended == hermit) {
      hermit = -1;
      result = WIFEXITED(status) ? WEXITSTATUS(status) : 99;
      goto done;
    }
    if (ended < 0) goto done;
    if (holder > 0) {
      if (poll(NULL, 0, 100) < 0 && errno != EINTR) goto done;
      continue;
    }
    struct pollfd ready = {.fd = request, .events = POLLIN};
    int polled = poll(&ready, 1, 100);
    if (polled < 0 && errno == EINTR) continue;
    if (polled < 0) goto done;
    if (polled == 0) continue;
    if (!(ready.revents & POLLIN)) goto done;
    pid_t id;
    if (read(request, &id, sizeof id) != (ssize_t)sizeof id || id <= 0) goto done;
    struct clone_args args = {0};
    args.exit_signal = SIGCHLD;
    args.set_tid = (uint64_t)(uintptr_t)&id;
    args.set_tid_size = 1;
    holder = syscall(SYS_clone3, &args, sizeof args);
    if (holder == 0) {
      for (;;) pause();
    }
    if (holder != id) goto done;
    if (write(ack, &id, sizeof id) != (ssize_t)sizeof id) goto done;
  }
done:
  if (hermit > 0) {
    kill(hermit, SIGKILL);
    while (waitpid(hermit, NULL, 0) < 0 && errno == EINTR) {}
  }
  if (holder > 0) {
    int status;
    if (kill(holder, SIGKILL) != 0) result = 99;
    pid_t reaped;
    do { reaped = waitpid(holder, &status, 0); } while (reaped < 0 && errno == EINTR);
    if (reaped != holder || !WIFSIGNALED(status) || WTERMSIG(status) != SIGKILL) result = 99;
  }
  if (request >= 0) close(request);
  if (ack >= 0) close(ack);
  unlink(request_path);
  unlink(ack_path);
  if (result == 99) fprintf(stderr, "external holder setup or cleanup failed\n");
  return result;
}

static int open_external_holder(const char* request_path, const char* ack_path,
                                int* request, int* ack) {
  *request = open(request_path, O_WRONLY);
  *ack = open(ack_path, O_RDONLY);
  if (*request < 0 || *ack < 0) {
    if (*request >= 0) close(*request);
    if (*ack >= 0) close(*ack);
    return 1;
  }
  return 0;
}

static int request_external_holder(int request, int ack) {
  pid_t id = first + 3, answer = 0;
  int failed = write(request, &id, sizeof id) != (ssize_t)sizeof id ||
      read(ack, &answer, sizeof answer) != (ssize_t)sizeof answer || answer != id;
  close(request);
  close(ack);
  if (failed) return 1;
  show("external holder", answer);
  return 0;
}

/* Original mutable clone3 input deliberately remains original. A retained
 * birth cannot certify its lifecycle fields for a later reserved-input call.
 * Keep first-call success/errno independent of that later policy decision. */
static int reap_lifecycle_holder(pid_t holder) {
  if (kill(holder, SIGKILL) != 0) return 1;
  int status = 0;
  if (waitpid(holder, &status, 0) != holder ||
      !WIFSIGNALED(status) || WTERMSIG(status) != SIGKILL) return 1;
  show("explicit reaped", holder);
  return 0;
}

static int guest_chosen_lifecycle(int argc, char** argv) {
  if (argc != 4) return 1;
  int live = strcmp(argv[2], "live") == 0;
  int no_child = strcmp(argv[2], "no-child") == 0;
  if (!live && !no_child && strcmp(argv[2], "reaped") != 0) return 1;
  int offset = atoi(argv[3]);
  if (offset <= 0) return 1;
  pid_t chosen = first + offset;
  struct clone_args args;
  memset(&args, 0, sizeof args);
  args.exit_signal = SIGCHLD;
  args.set_tid = (uint64_t)(uintptr_t)&chosen;
  args.set_tid_size = 1;
  /* A stable Linux contradiction, still issued as the original GuestChosen
   * clone3 (not the legacy-clone preflight or an added-ID rewrite). */
  if (no_child) args.flags = CLONE_SIGHAND;
  errno = 0;
  long original = syscall(SYS_clone3, &args, sizeof args);
  int error = errno;
  if (original == 0) {
    if (no_child || getpid() != chosen) _exit(99);
    for (;;) pause();
  }
  if (no_child) {
    if (original != -1 || error != EINVAL) return 1;
    printf("original clone3: %s\n", strerror(error));
  } else {
    if (original != chosen) return 1;
    show("explicit accepted", original);
    if (!live && reap_lifecycle_holder((pid_t)original) != 0) return 1;
  }
  pid_t later = fork();
  if (later < 0) return 1;
  if (later == 0) _exit(23);
  int status = 0;
  if (waitpid(later, &status, 0) != later ||
      !WIFEXITED(status) || WEXITSTATUS(status) != 23) return 1;
  if (no_child) show("later returned", later);
  printf("later child reaped\n");
  if (live && reap_lifecycle_holder((pid_t)original) != 0) return 1;
  return 0;
}

/* One call, as the arguments `sighand`, `own-tid N`, `bad-tid` and
 * `bad-size` ask, and its outcome. */
static int one_call(int argc, char** argv) {
  const char* what = argv[1];
  if (strcmp(what, "guest-chosen-lifecycle") == 0) {
    return guest_chosen_lifecycle(argc, argv);
  }
  if (strcmp(what, "explicit-then-fork") == 0) {
    if (argc != 3) return 1;
    pid_t chosen = first + atoi(argv[2]);
    pid_t holder = hold(chosen);
    if (kill(holder, SIGKILL) != 0) return 1;
    int status = 0;
    if (waitpid(holder, &status, 0) != holder ||
        !WIFSIGNALED(status) || WTERMSIG(status) != SIGKILL) return 1;
    show("explicit reaped", holder);
    pid_t child = fork();
    if (child < 0) return 1;
    if (child == 0) _exit(23);
    if (waitpid(child, &status, 0) != child ||
        !WIFEXITED(status) || WEXITSTATUS(status) != 23) return 1;
    show("automatic reaped", child);
    printf("distinct IDs: %d\n", child != chosen);
    return child == chosen;
  }
  if (strcmp(what, "parent-signal") == 0) {
    if (argc != 3) return 1;
    int signal = atoi(argv[2]);
    pid_t probe = fork();
    if (probe < 0) return 1;
    if (probe == 0) {
      errno = 0;
      long child = syscall(SYS_clone, (unsigned long)(CLONE_PARENT | signal),
                           0L, 0L, 0L, 0L);
      int error = errno;
      if (child == 0) _exit(23);
      printf("parent signal %d: created=%d errno=%d\n", signal, child > 0, error);
      _exit(child < 0 && error != EINVAL ? 1 : 0);
    }
    int probes = 0, clones = 0, status = 0;
    pid_t reaped;
    while ((reaped = waitpid(-1, &status, __WALL)) > 0) {
      if (reaped == probe && WIFEXITED(status) && WEXITSTATUS(status) == 0) probes++;
      else if (WIFEXITED(status) && WEXITSTATUS(status) == 23) clones++;
      else return 1;
    }
    if (errno != ECHILD || probes != 1 || clones > 1) return 1;
    printf("parent signal %d: reaped probe=%d clone=%d\n", signal, probes, clones);
    pid_t after = fork();
    if (after < 0) return 1;
    if (after == 0) _exit(0);
    if (waitpid(after, &status, 0) != after ||
        !WIFEXITED(status) || WEXITSTATUS(status) != 0) return 1;
    show("after signal returned", after);
    return 0;
  }
  if (strcmp(what, "vfork-detached") == 0 ||
      strcmp(what, "untraced-detached") == 0) {
#ifndef CLONE_DETACHED
#define CLONE_DETACHED 0x00400000
#endif
    int pidfd = -1;
    unsigned long extra =
        strcmp(what, "vfork-detached") == 0 ? CLONE_VFORK : CLONE_UNTRACED;
    long child =
        syscall(SYS_clone, extra | CLONE_PIDFD | CLONE_DETACHED | SIGCHLD, 0,
                &pidfd, 0, 0);
    if (child == 0) {
      syscall(SYS_exit, 0);
    }
    if (child > 0) {
      waitpid((pid_t)child, NULL, __WALL);
    }
    printf("clone %s: %s\n", what, child < 0 ? strerror(errno) : "created");
    return 0;
  }
  if (strcmp(what, "kernel-args") == 0 ||
      strcmp(what, "kernel-args-top") == 0) {
    long pointer = strcmp(what, "kernel-args") == 0 ? -4096L : -2L;
    long child = syscall(SYS_clone3, (void*)pointer, 88);
    printf("clone3 %s: %s\n", what, child < 0 ? strerror(errno) : "created");
    return 0;
  }
  if (strcmp(what, "sighand") == 0) {
    long child = syscall(SYS_clone, CLONE_SIGHAND | SIGCHLD, 0, 0, 0, 0);
    if (child == 0) {
      _exit(0);
    }
    if (child > 0) {
      wait_for((pid_t)child, "clone(CLONE_SIGHAND)");
    }
    printf("clone(CLONE_SIGHAND): %s\n",
           child < 0 ? strerror(errno) : "created");
    return 0;
  }
  if (strcmp(what, "seccomp") == 0) {
    struct sock_filter allow = BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW);
    struct sock_fprog program = {1, &allow};
    if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 ||
        syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0, &program) != 0) {
      printf("seccomp: %s\n", strerror(errno));
      return 1;
    }
    pid_t child = fork();
    if (child == 0) {
      _exit(0);
    }
    wait_for(child, "fork");
    printf("fork after seccomp: child %+ld\n", (long)child - first);
    return 0;
  }
  if (strcmp(what, "signal65") == 0 || strcmp(what, "vfork65") == 0 ||
      strcmp(what, "sp1") == 0) {
    int sp1 = strcmp(what, "sp1") == 0;
    unsigned long flags = sp1 ? SIGCHLD : 65;
    if (strcmp(what, "vfork65") == 0) {
      flags |= CLONE_VFORK;
    }
    long child = syscall(SYS_clone, flags, sp1 ? 1 : 0, 0, 0, 0);
    if (child == 0) {
      syscall(SYS_exit, 0);
    }
    int error = errno;
    if (child > 0) {
      int status;
      if (waitpid((pid_t)child, &status, __WALL) != child) return 1;
      if (!sp1 && (!WIFEXITED(status) || WEXITSTATUS(status) != 0)) return 1;
    }
    printf("clone %s: %s\n", what, child < 0 ? strerror(error) : "created");
    return 0;
  }
  pid_t tids[2] = {1, first + 100};
  struct clone_args args;
  memset(&args, 0, sizeof args);
  args.exit_signal = SIGCHLD;
  args.set_tid = (uint64_t)(uintptr_t)tids;
  struct clone_args* argp = &args;
  int nested_call = 0;
  if (strncmp(what, "newpid", 6) == 0) {
    args.flags = CLONE_NEWPID;
    args.set_tid = 0;
    nested_call = strcmp(what, "newpid-fork") == 0   ? 1
                  : strcmp(what, "newpid-set-tid") == 0 ? 2
                                                        : 0;
  } else if (strcmp(what, "own-tid") == 0 && argc > 2) {
    args.flags = CLONE_NEWPID;
    args.set_tid_size = strtoul(argv[2], NULL, 10);
  } else if (strcmp(what, "unknown-flag") == 0) {
    args.flags = CLONE_NEWPID | (1ULL << 40);
    args.set_tid_size = 1;
  } else if (strcmp(what, "nested") == 0) {
    if (unshare(CLONE_NEWPID) != 0) {
      printf("unshare: %s\n", strerror(errno));
      return 1;
    }
    args.flags = CLONE_NEWPID;
    args.set_tid_size = 1;
  } else if (strcmp(what, "no-caps") == 0) {
    struct __user_cap_header_struct header = {_LINUX_CAPABILITY_VERSION_3, 0};
    struct __user_cap_data_struct none[2];
    memset(none, 0, sizeof none);
    if (syscall(SYS_capset, &header, none) != 0) {
      printf("capset: %s\n", strerror(errno));
      return 1;
    }
    args.flags = CLONE_NEWUSER | CLONE_NEWPID;
    args.set_tid_size = 1;
  } else if (strcmp(what, "bad-tid") == 0) {
    args.set_tid = 8;
    args.set_tid_size = 1;
  } else if (strcmp(what, "bad-size") == 0) {
    args.set_tid_size = 0;
  } else if (strcmp(what, "edge-tid") == 0) {
    long page = sysconf(_SC_PAGESIZE);
    char* pages = mmap(NULL, 2 * page, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (pages == MAP_FAILED || munmap(pages + page, page) != 0) {
      printf("mmap: %s\n", strerror(errno));
      return 1;
    }
    pid_t* edge = (pid_t*)(pages + page - sizeof(pid_t));
    *edge = 1;
    args.flags = CLONE_NEWPID;
    args.set_tid = (uint64_t)(uintptr_t)edge;
    args.set_tid_size = 1;
  } else if (strcmp(what, "wronly-tid") == 0) {
    pid_t* wronly = mmap(NULL, sysconf(_SC_PAGESIZE), PROT_WRITE,
                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (wronly == MAP_FAILED) {
      printf("mmap: %s\n", strerror(errno));
      return 1;
    }
    *wronly = 1;
    args.flags = CLONE_NEWPID;
    args.set_tid = (uint64_t)(uintptr_t)wronly;
    args.set_tid_size = 1;
  } else if (strcmp(what, "wronly-args") == 0 ||
             strcmp(what, "wronly-vfork") == 0 ||
             strcmp(what, "wronly-short") == 0) {
    args.set_tid = 0;
    if (strcmp(what, "wronly-vfork") == 0) {
      args.flags = CLONE_VFORK;
    }
    argp = mmap(NULL, sysconf(_SC_PAGESIZE), PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (argp == MAP_FAILED) {
      printf("mmap: %s\n", strerror(errno));
      return 1;
    }
    *argp = args;
  } else {
    printf("unknown call %s\n", what);
    return 2;
  }
  size_t size = strcmp(what, "wronly-short") == 0 ? 32 : sizeof args;
  long child = syscall(SYS_clone3, argp, size);
  if (child == 0) {
    if (args.set_tid == 0 && args.flags == CLONE_NEWPID) {
      printf("newpid child: own ID %ld\n", (long)getpid());
    }
    if (nested_call != 0) {
      pid_t inner_tids[1] = {1};
      struct clone_args inner;
      memset(&inner, 0, sizeof inner);
      inner.exit_signal = SIGCHLD;
      if (nested_call == 2) {
        inner.flags = CLONE_NEWPID;
        inner.set_tid = (uint64_t)(uintptr_t)inner_tids;
        inner.set_tid_size = 1;
      }
      long grandchild = syscall(SYS_clone3, &inner, sizeof inner);
      if (grandchild == 0) {
        _exit(0);
      }
      if (grandchild > 0) {
        waitpid((pid_t)grandchild, NULL, 0);
      }
      printf("newpid child's clone3: %s\n",
             grandchild < 0 ? strerror(errno) : "created");
    }
    _exit(getpid() == 1 ? 0 : 1);
  }
  if (child < 0) {
    printf("clone3 %s: %s\n", what, strerror(errno));
    return 0;
  }
  int status = 0;
  waitpid((pid_t)child, &status, 0);
  printf("clone3 %s: child %+ld, exit %d\n", what, child - first,
         WIFEXITED(status) ? WEXITSTATUS(status) : -1);
  return 0;
}

struct deferred_call {
  int argc;
  char** argv;
};

/* Waits until the thread group leader has exited, then makes the call and
 * ends the process with its status. */
static void* call_after_the_leader(void* arg) {
  struct deferred_call* call = arg;
  char path[64];
  snprintf(path, sizeof path, "/proc/self/task/%d/stat", first);
  for (;;) {
    char stat[512] = {0};
    FILE* file = fopen(path, "r");
    if (file == NULL) {
      break;
    }
    size_t got = fread(stat, 1, sizeof stat - 1, file);
    fclose(file);
    char* state = strrchr(stat, ')');
    if (got == 0 || state == NULL || state[1] == '\0' || state[2] == 'Z' ||
        state[2] == 'X') {
      break;
    }
    sched_yield();
  }
  exit(one_call(call->argc, call->argv));
}

/* A public foreground-write neighbor of the reserved-input C/W guard.
 * Open the capability before sharing; never write through it. A child's
 * F_SETFD is visible in the parent only with a genuinely shared fd table.
 * The socket stays nonblocking and its one-byte write must finish, not fall
 * back to background I/O or silently skip the later Chosen clone. */
static void cw_require(int condition, const char *step) {
  if (!condition) {
    dprintf(2, "cw-neighbor failed: %s (errno %d)\n", step, errno);
    _exit(91);
  }
}

struct cw_message {
  pid_t observed;
  int cloexec;
};

static void cw_read_exact(int fd, void *buffer, size_t length) {
  size_t received = 0;
  while (received != length) {
    ssize_t n = read(fd, (char *)buffer + received, length - received);
    if (n < 0 && (errno == EINTR || errno == EAGAIN || errno == EWOULDBLOCK)) {
      sched_yield();
      continue;
    }
    cw_require(n > 0, "socket read");
    received += (size_t)n;
  }
}

static int cw_neighbor(int argc, char **argv) {
  if (argc != 4) return 64;
  int capability = strcmp(argv[2], "proc") == 0;
  int shared = strcmp(argv[3], "shared") == 0;
  if ((!capability && strcmp(argv[2], "ordinary") != 0) ||
      (!shared && strcmp(argv[3], "copied") != 0)) return 64;
  int fd = open(capability ? "/proc/thread-self/mem" : "/dev/null", O_RDWR);
  cw_require(fd >= 0, "open capability before sharing");
  cw_require((fcntl(fd, F_GETFL) & O_ACCMODE) == O_RDWR, "writable access");
  cw_require(fcntl(fd, F_GETFD) == 0, "initial descriptor flags");
  struct stat st;
  struct statfs fs;
  cw_require(fstat(fd, &st) == 0 && fstatfs(fd, &fs) == 0, "object facts");
  if (capability) {
    cw_require(S_ISREG(st.st_mode) && (unsigned long)fs.f_type == 0x9fa0,
               "actual proc regular object");
    volatile unsigned char sentinel = 0x53;
    unsigned char observed = 0;
    cw_require(pread(fd, &observed, 1, (off_t)(uintptr_t)&sentinel) == 1 &&
                   observed == sentinel,
               "proc mem retains this address space");
  } else {
    cw_require(S_ISCHR(st.st_mode) && (unsigned long)fs.f_type != 0x9fa0,
               "ordinary descriptor is not proc memory");
  }
  int pair[2];
  cw_require(socketpair(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK, 0, pair) == 0,
             "fresh nonblocking socketpair");
  cw_require((fcntl(pair[0], F_GETFL) & O_NONBLOCK) != 0 &&
                 (fcntl(pair[1], F_GETFL) & O_NONBLOCK) != 0,
             "guest-visible nonblocking endpoints");
  unsigned long flags = (shared ? CLONE_FILES : 0) | SIGCHLD;
  long child = syscall(SYS_clone, flags, 0, 0, 0, 0);
  cw_require(child >= 0, "first register-only clone");
  if (child == 0) {
    cw_require(fcntl(fd, F_SETFD, FD_CLOEXEC) == 0, "child changes fd flags");
    struct cw_message message = {(pid_t)syscall(SYS_getpid), fcntl(fd, F_GETFD)};
    cw_require(message.observed > 0 && message.cloexec == FD_CLOEXEC,
               "child identity and fd flags");
    cw_require(write(pair[1], &message, sizeof message) == (ssize_t)sizeof message,
               "first child acknowledgement write");
    char token = 0;
    cw_read_exact(pair[1], &token, 1);
    cw_require(token == 'Z', "exact parent byte");
    syscall(SYS_exit, 0);
    __builtin_unreachable();
  }
  struct cw_message message;
  cw_read_exact(pair[0], &message, sizeof message);
  cw_require(message.observed == child && message.cloexec == FD_CLOEXEC,
             "actual child identity");
  cw_require(fcntl(fd, F_GETFD) == (shared ? FD_CLOEXEC : 0),
             "actual shared versus copied descriptor table");
  /* The child cannot exit normally before consuming this byte. It has not
   * changed the OFD or socket membership; the empty stream has room. */
  cw_require(write(pair[0], "Z", 1) == 1, "parent synchronous socket write");
  int status = -1;
  pid_t waited = waitpid((pid_t)child, &status, 0);
  cw_require(waited == child && WIFEXITED(status) && WEXITSTATUS(status) == 0,
             "first child status");
  long later = syscall(SYS_clone, (unsigned long)SIGCHLD, 0, 0, 0, 0);
  cw_require(later >= 0, "later register-only clone");
  if (later == 0) {
    struct cw_message second = {(pid_t)syscall(SYS_getpid), fcntl(fd, F_GETFD)};
    cw_require(write(pair[1], &second, sizeof second) == (ssize_t)sizeof second,
               "later child identity write");
    syscall(SYS_exit, 0);
    __builtin_unreachable();
  }
  struct cw_message second;
  cw_read_exact(pair[0], &second, sizeof second);
  status = -1;
  pid_t later_waited = waitpid((pid_t)later, &status, 0);
  cw_require(second.observed == later && later != child && later != first &&
                 child != first && later_waited == later && WIFEXITED(status) &&
                 WEXITSTATUS(status) == 0 &&
                 second.cloexec == (shared ? FD_CLOEXEC : 0),
             "later child identity, inherited table, and wait result");
  printf("first process %d\n", first);
  printf("cw capability %s\n", capability ? "proc" : "ordinary");
  printf("cw table %s\n", shared ? "shared" : "copied");
  show("cw first returned", child);
  report("cw first observed", message.observed, child);
  report("cw first waited", waited, child);
  puts("cw socket Z");
  show("cw later returned", later);
  report("cw later observed", second.observed, later);
  report("cw later waited", later_waited, later);
  cw_require(close(fd) == 0 && close(pair[0]) == 0 && close(pair[1]) == 0,
             "close retained objects");
  return mismatches == 0 ? 0 : 1;
}

int main(int argc, char** argv) {
  if (argc > 1 && strcmp(argv[1], "external-holder") == 0) {
    return external_holder(argc, argv);
  }
  int collide = argc > 1 && strcmp(argv[1], "collide") == 0;
  int external_late = argc == 4 && strcmp(argv[1], "collide-external-late-open") == 0;
  int external = argc == 4 &&
      (strcmp(argv[1], "collide-external") == 0 || external_late);
  setvbuf(stdout, NULL, _IONBF, 0);
  first = getpid();
  if (argc > 1 && strcmp(argv[1], "cw-neighbor") == 0) {
    return cw_neighbor(argc, argv);
  }
  if (argc > 2 && strcmp(argv[1], "leader-gone") == 0) {
    static struct deferred_call call;
    call.argc = argc - 1;
    call.argv = argv + 1;
    pthread_t thread;
    if (pthread_create(&thread, NULL, call_after_the_leader, &call) != 0) {
      printf("pthread_create failed\n");
      return 1;
    }
    pthread_exit(NULL);
  }
  if (argc > 1 && !collide && !external) {
    return one_call(argc, argv);
  }
  printf("first process %d\n", first);
  report("main gettid", syscall(SYS_gettid), first);
  char link[32] = {0};
  if (readlink("/proc/self", link, sizeof link - 1) < 0) {
    printf("readlink /proc/self failed: %s\n", strerror(errno));
    return 1;
  }
  report("/proc/self", strtol(link, NULL, 10), first);

  // Acquire the handshake endpoints while this table is proved private. The
  // supervisor creates no holder until the request below, after both threads.
  // The late-open mode retains the shared-table proof-unavailable control.
  int request = -1, ack = -1;
  if (external && !external_late &&
      open_external_holder(argv[2], argv[3], &request, &ack) != 0) return 1;

  for (int i = 0; i < 2; i++) {
    pthread_t thread;
    pid_t seen = 0;
    if (pthread_create(&thread, NULL, thread_main, &seen) != 0) {
      printf("pthread_create failed\n");
      return 1;
    }
    pthread_join(thread, NULL);
    /* The C library keeps the new thread's ID where CLONE_PARENT_SETTID
     * wrote it; the thread's own gettid is the kernel's answer. */
    show("thread gettid", seen);
  }

  pid_t holder = collide ? hold(first + 3) : 0;
  if (external_late &&
      open_external_holder(argv[2], argv[3], &request, &ack) != 0) return 1;
  if (external && request_external_holder(request, ack) != 0) return 1;
  pid_t forked = fork();
  if (forked == 0) {
    _exit(0);
  }
  if (holder > 0) {
    show("holder", holder);
    kill(holder, SIGKILL);
    wait_for(holder, "holder");
  }
  show("fork returned", forked);
  report("fork waitpid", wait_for(forked, "fork"), forked);

  pid_t vforked = vfork();
  if (vforked == 0) {
    _exit(0);
  }
  show("vfork returned", vforked);
  report("vfork waitpid", wait_for(vforked, "vfork"), vforked);

  /* A raw clone, which the kernel answers through both settid words. */
  static pid_t parent_word, child_word;
  long cloned = syscall(
      SYS_clone,
      CLONE_PARENT_SETTID | CLONE_CHILD_SETTID | SIGCHLD,
      0,
      &parent_word,
#if defined(__aarch64__)
      0,
      &child_word
#else
      &child_word,
      0
#endif
  );
  if (cloned == 0) {
    _exit(child_word == getpid() ? 0 : 4);
  }
  if (cloned < 0) {
    printf("clone failed: %s\n", strerror(errno));
    return 1;
  }
  show("clone returned", cloned);
  report("clone parent_tid word", parent_word, cloned);
  siginfo_t info;
  memset(&info, 0, sizeof info);
  if (waitid(P_PID, (id_t)cloned, &info, WEXITED) != 0) {
    printf("waitid failed: %s\n", strerror(errno));
    return 1;
  }
  report("clone waitid si_pid", info.si_pid, cloned);
  if (info.si_status != 0) {
    printf("  MISMATCH: the clone child's child_tid word was not its pid\n");
    mismatches++;
  }

  /* A raw clone3 of the guest's own. */
  struct clone_args args;
  memset(&args, 0, sizeof args);
  args.exit_signal = SIGCHLD;
  long cloned3 = syscall(SYS_clone3, &args, sizeof args);
  if (cloned3 == 0) {
    _exit(0);
  }
  if (cloned3 < 0) {
    printf("clone3 failed: %s\n", strerror(errno));
    return 1;
  }
  show("clone3 returned", cloned3);
  report("clone3 waitpid", wait_for((pid_t)cloned3, "clone3"), cloned3);

  printf("%d mismatches\n", mismatches);
  return mismatches == 0 ? 0 : 1;
}
