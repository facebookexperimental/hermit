/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * Licensed under the BSD-style license in the LICENSE file. */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

/* Foreign waiters contend for one child of their process.
 *
 * Linux lets any thread of a process wait for a child that another thread
 * created, unless the wait passes __WNOTHREAD. In every contended case the
 * leader holds its child alive on a pipe until each contending thread has
 * written one byte saying that its next system call is the wait. The leader
 * then yields a fixed number of times and writes the release byte. The
 * yields are scheduling opportunities, not proof: the Rust harness proves
 * from both retained INFO logs that every contender was parked in the
 * scheduler's child-wait pool before that release byte was written, and it
 * fails otherwise.
 *
 * Case 0 is an uncontended control reap; it measures the CPU one child adds.
 * Case 1 has two exact consumers, A and B, and a foreign thread N that owns
 * a live child D. N's __WNOTHREAD waits for the leader's child C fail with
 * ECHILD at once; N's blocking __WNOTHREAD wait parks on D, must not be
 * woken by C's exit, and reaps only D.
 * Case 2 has an observer (waitid WEXITED|WNOWAIT) and an exact consumer.
 * Case 3 has two any-child consumers and two held children; whichever
 * consumer loses the first child must park again and reap the second.
 *
 * The argument selects the consumers' call: "waitid" or "wait4". wait4 has
 * no WNOWAIT, so the observer always uses waitid. Every expectation below is
 * also what native Linux returns; which contender wins is left to the
 * scheduler, and the harness requires the same winner in both verify runs.
 *
 * Every thread keeps SIGCHLD blocked; threads and children inherit the mask
 * from main. No wait here depends on the signal, and Linux reports the same
 * results with it blocked. Serial KVM currently fails the whole run when a
 * thread exits while a sibling's SIGCHLD delivery is deferred behind other
 * runnable work; that defect predates child-wait support and reproduces
 * without any wait call, so this fixture keeps it out of scope. With the
 * signal blocked, each exited child leaves SIGCHLD pending for the process,
 * and the KVM backend refuses to create a thread while a process-directed
 * signal is pending. So main creates all seven contenders before its first
 * fork; each one blocks on its own start pipe until its case begins. */
enum { WAITID = 1, WAIT4 = 2 };
enum { EXACT = 1, ANY = 2, OBSERVE = 3, FOREIGN = 4 };
enum { YIELDS = 16, UNWRITTEN = 0x5a5a5a5a };
static int family;
static unsigned assertions, calls;
static pid_t leader;
static __thread const char *role = "main";
static int ready_pipe[2] = {-1, -1}, finish_pipe[2] = {-1, -1};

static void require(int condition, const char *what) {
  __atomic_add_fetch(&assertions, 1, __ATOMIC_SEQ_CST);
  if (!condition) {
    fprintf(stderr, "wait contenders role=%s: %s errno=%d\n", role, what, errno);
    /* Stop every thread at once; never strand a pipe or child waiter. */
    syscall(SYS_exit_group, 90);
    __builtin_unreachable();
  }
}

static long thread_id(void) { return syscall(SYS_gettid); }
static void flush(void) { require(fflush(stdout) == 0, "flush complete row"); }
static const char *family_name(void) { return family == WAITID ? "waitid" : "wait4"; }

struct child { pid_t pid; int status; int release; };

/* The caller creates the release pipe. The leader keeps every write end open
 * until it exits, so each release write has its own descriptor number in
 * the INFO log. */
static struct child spawn(unsigned case_number, const char *name, int status,
                          int release[2]) {
  flush();
  pid_t pid = fork();
  require(pid >= 0, "fork");
  if (!pid) {
    char byte = 0;
    close(release[1]);
    if (read(release[0], &byte, 1) != 1 || byte != 'r') _exit(91);
    for (unsigned i = 0; i < 64; ++i) syscall(SYS_getpid);
    _exit(status);
  }
  require(close(release[0]) == 0, "close release read end");
  printf("{\"type\":\"spawn\",\"case\":%u,\"name\":\"%s\",\"actor\":\"%s\","
         "\"tid\":%ld,\"child\":%d,\"status\":%d,\"release_fd\":%d}\n",
         case_number, name, role, thread_id(), pid, status, release[1]);
  flush();
  return (struct child){pid, status, release[1]};
}

static void release_child(const struct child *child) {
  require(write(child->release, "r", 1) == 1, "release held child");
}

/* One raw wait, printed as one row. waitid reports the event in siginfo;
 * wait4 reports the PID as its result and the status through a word that
 * starts as UNWRITTEN. Rusage is NULL: native child CPU time varies. */
struct outcome { int waitid; long rc; int error; pid_t pid; int status, code, signo, raw; };

static struct outcome do_waitid(unsigned case_number, const char *name, int which,
                                pid_t id, int options) {
  siginfo_t info;
  memset(&info, 0, sizeof(info));
  errno = 0;
  long rc = syscall(SYS_waitid, which, id, &info, options, NULL);
  struct outcome got = {1, rc, errno, info.si_pid, info.si_status, info.si_code,
                        info.si_signo, 0};
  __atomic_add_fetch(&calls, 1, __ATOMIC_SEQ_CST);
  printf("{\"type\":\"wait\",\"case\":%u,\"name\":\"%s\",\"actor\":\"%s\","
         "\"tid\":%ld,\"call\":\"waitid\",\"which\":%d,\"id\":%d,\"options\":%d,"
         "\"rc\":%ld,\"errno\":%d,\"si_signo\":%d,\"si_code\":%d,\"si_pid\":%d,"
         "\"si_status\":%d}\n", case_number, name, role, thread_id(), which, id,
         options, rc, got.error, got.signo, got.code, got.pid, got.status);
  flush();
  return got;
}

static struct outcome do_wait4(unsigned case_number, const char *name, pid_t pid,
                               int options) {
  int raw = UNWRITTEN;
  errno = 0;
  long rc = syscall(SYS_wait4, pid, &raw, options, NULL);
  struct outcome got = {0, rc, errno, rc > 0 ? (pid_t)rc : 0,
                        WIFEXITED(raw) ? WEXITSTATUS(raw) : -1, 0, 0, raw};
  __atomic_add_fetch(&calls, 1, __ATOMIC_SEQ_CST);
  printf("{\"type\":\"wait\",\"case\":%u,\"name\":\"%s\",\"actor\":\"%s\","
         "\"tid\":%ld,\"call\":\"wait4\",\"pid\":%d,\"options\":%d,\"rc\":%ld,"
         "\"errno\":%d,\"raw_status\":%d}\n", case_number, name, role, thread_id(),
         pid, options, rc, got.error, raw);
  flush();
  return got;
}

/* The family's wait for an exact child (pid > 0) or any child (pid == -1). */
static struct outcome family_wait(unsigned case_number, const char *name, pid_t pid,
                                  int options) {
  if (family == WAIT4) return do_wait4(case_number, name, pid, options);
  int waitid_options = WEXITED | options;
  return pid > 0 ? do_waitid(case_number, name, P_PID, pid, waitid_options)
                 : do_waitid(case_number, name, P_ALL, 0, waitid_options);
}

static int reaped(struct outcome got, const struct child *child) {
  if (got.waitid)
    return got.rc == 0 && got.error == 0 && got.signo == SIGCHLD &&
           got.code == CLD_EXITED && got.pid == child->pid && got.status == child->status;
  return got.rc == child->pid && got.error == 0 && got.raw == child->status << 8;
}

static int echild(struct outcome got) {
  if (got.waitid)
    return got.rc == -1 && got.error == ECHILD && got.signo == 0 && got.pid == 0 &&
           got.code == 0 && got.status == 0;
  return got.rc == -1 && got.error == ECHILD && got.raw == UNWRITTEN;
}

static int live(struct outcome got) {
  if (got.waitid)
    return got.rc == 0 && got.error == 0 && got.signo == 0 && got.pid == 0;
  return got.rc == 0 && got.error == 0 && got.raw == UNWRITTEN;
}

struct cpu { int64_t user, system; };
static struct cpu children_cpu(unsigned case_number, const char *name) {
  struct rusage value;
  require(getrusage(RUSAGE_CHILDREN, &value) == 0, "children CPU query");
  struct cpu result = {value.ru_utime.tv_sec * INT64_C(1000000) + value.ru_utime.tv_usec,
                       value.ru_stime.tv_sec * INT64_C(1000000) + value.ru_stime.tv_usec};
  printf("{\"type\":\"cpu\",\"case\":%u,\"name\":\"%s\",\"actor\":\"%s\","
         "\"user_us\":%ld,\"system_us\":%ld}\n", case_number, name, role,
         (long)result.user, (long)result.system);
  flush();
  return result;
}
static void equal_cpu(struct cpu a, struct cpu b) {
  require(a.user == b.user && a.system == b.system, "children CPU unchanged");
}
static void added_cpu(struct cpu a, struct cpu b) {
  require(b.user >= a.user && b.system >= a.system &&
          (b.user > a.user || b.system > a.system), "one reap adds child CPU");
}

struct waiter {
  const char *name;
  unsigned case_number;
  int kind;
  const struct child *target; /* EXACT, OBSERVE: the child; FOREIGN: the leader's */
  int release[2];             /* FOREIGN: holds its own child D */
  int handoff[2];             /* FOREIGN: carries D's PID as pipe data */
  int go[2];                  /* Main writes one byte when the case begins. */
  pid_t tid;                  /* Read only after pthread_join. */
  struct outcome got;         /* Read only after pthread_join. */
};

/* The contender's next system call after this byte is its wait. */
static void announce(void) {
  require(write(ready_pipe[1], role, 1) == 1, "announce the wait");
}

static void *waiter_main(void *opaque) {
  struct waiter *w = opaque;
  role = w->name;
  w->tid = (pid_t)thread_id();
  require(getpid() == leader && w->tid != leader, "same-process sibling thread");
  sigset_t mask;
  require(pthread_sigmask(SIG_BLOCK, NULL, &mask) == 0 && sigismember(&mask, SIGCHLD) == 1,
          "SIGCHLD stays blocked in every thread");
  printf("{\"type\":\"thread\",\"case\":%u,\"name\":\"%s\",\"tid\":%d,\"kind\":%d}\n",
         w->case_number, w->name, w->tid, w->kind);
  flush();
  char go = 0;
  require(read(w->go[0], &go, 1) == 1 && go == 'g', "case start");
  /* The start byte orders main's write of the target child before this read. */
  unsigned n = w->case_number;
  pid_t target = w->target ? w->target->pid : 0;
  if (w->kind == FOREIGN) {
    struct child own = spawn(n, "D", 62, w->release);
    require(write(w->handoff[1], &own.pid, sizeof(own.pid)) == (ssize_t)sizeof(own.pid),
            "send own child PID");
    /* The nearby case: the leader's live child is not this thread's. */
    require(echild(family_wait(n, "deny-nohang", target, WNOHANG | __WNOTHREAD)),
            "foreign __WNOTHREAD WNOHANG is refused");
    require(echild(family_wait(n, "deny-blocking", target, __WNOTHREAD)),
            "foreign __WNOTHREAD blocking wait is refused at once");
    require(live(family_wait(n, "own-live", -1, WNOHANG | __WNOTHREAD)),
            "own live child is waitable");
    announce();
    w->got = family_wait(n, "own-consume", -1, __WNOTHREAD);
    require(reaped(w->got, &own), "reaps only its own child");
    require(echild(family_wait(n, "own-echild", -1, WNOHANG | __WNOTHREAD)),
            "own child consumed once");
  } else if (w->kind == OBSERVE) {
    announce();
    w->got = do_waitid(n, "observe", P_PID, target, WEXITED | WNOWAIT);
  } else {
    announce();
    w->got = family_wait(n, "contend", w->kind == EXACT ? target : -1, 0);
    if (w->kind == ANY)
      require(write(finish_pipe[1], role, 1) == 1, "report finished wait");
  }
  return NULL;
}

static void start(pthread_t *thread, struct waiter *w) {
  require(pipe(w->go) == 0, "start pipe");
  flush();
  require(pthread_create(thread, NULL, waiter_main, w) == 0, "pthread_create");
}

static void begin(struct waiter *w) {
  require(write(w->go[1], "g", 1) == 1, "begin the waiter's case");
}

static void join(pthread_t thread) {
  void *value = (void *)1;
  require(pthread_join(thread, &value) == 0 && value == NULL, "pthread_join");
}

/* Read one announcement per contender, then offer the scheduler a fixed
 * number of turns. Whether they parked is checked from the log, not here. */
static void await_announced(unsigned count) {
  for (unsigned i = 0; i < count; ++i) {
    char byte = 0;
    require(read(ready_pipe[0], &byte, 1) == 1, "announcement");
  }
  for (unsigned i = 0; i < YIELDS; ++i) require(sched_yield() == 0, "yield");
}

static void case_row(unsigned case_number, const char *name) {
  printf("{\"type\":\"case\",\"case\":%u,\"name\":\"%s\",\"passed\":true}\n",
         case_number, name);
  flush();
}

int main(int argc, char **argv) {
  require(argc == 2, "one family argument");
  family = strcmp(argv[1], "waitid") == 0 ? WAITID : strcmp(argv[1], "wait4") == 0 ? WAIT4 : 0;
  require(family != 0, "family is waitid or wait4");
  require(setvbuf(stdout, NULL, _IOLBF, 0) == 0, "line buffered observations");
  sigset_t sigchld;
  require(sigemptyset(&sigchld) == 0 && sigaddset(&sigchld, SIGCHLD) == 0, "SIGCHLD set");
  require(sigprocmask(SIG_BLOCK, &sigchld, NULL) == 0, "block SIGCHLD before any thread");
  leader = (pid_t)thread_id();
  require(leader == getpid(), "main is the thread-group leader");
  require(pipe(ready_pipe) == 0 && pipe(finish_pipe) == 0, "handoff pipes");
  int pipes[6][2];
  for (unsigned i = 0; i < 6; ++i) require(pipe(pipes[i]) == 0, "release pipe");

  /* Every contender exists before the first child; see the header. */
  struct child k = {0}, c = {0}, d = {0}, e = {0}, f = {0}, g = {0};
  struct waiter n = {.name = "N", .case_number = 1, .kind = FOREIGN, .target = &c,
                     .release = {pipes[2][0], pipes[2][1]}};
  require(pipe(n.handoff) == 0, "foreign handoff pipe");
  struct waiter a = {.name = "A", .case_number = 1, .kind = EXACT, .target = &c};
  struct waiter b = {.name = "B", .case_number = 1, .kind = EXACT, .target = &c};
  struct waiter o = {.name = "O", .case_number = 2, .kind = OBSERVE, .target = &e};
  struct waiter q = {.name = "Q", .case_number = 2, .kind = EXACT, .target = &e};
  struct waiter x = {.name = "X", .case_number = 3, .kind = ANY};
  struct waiter y = {.name = "Y", .case_number = 3, .kind = ANY};
  pthread_t tn, ta, tb, to, tq, tx, ty;
  start(&tn, &n);
  start(&ta, &a);
  start(&tb, &b);
  start(&to, &o);
  start(&tq, &q);
  start(&tx, &x);
  start(&ty, &y);

  /* Case 0: uncontended control. */
  k = spawn(0, "K", 60, pipes[0]);
  struct cpu before = children_cpu(0, "before-release");
  release_child(&k);
  require(reaped(family_wait(0, "control-consume", k.pid, 0), &k), "control reap");
  struct cpu after = children_cpu(0, "after-reap");
  added_cpu(before, after);
  require(echild(family_wait(0, "control-echild", k.pid, WNOHANG)), "control consumed once");
  equal_cpu(after, children_cpu(0, "after-echild"));
  case_row(0, "control");

  /* Case 1: two exact consumers and a parked foreign __WNOTHREAD waiter. */
  c = spawn(1, "C", 61, pipes[1]);
  begin(&n);
  d = (struct child){0, 62, pipes[2][1]};
  require(read(n.handoff[0], &d.pid, sizeof(d.pid)) == (ssize_t)sizeof(d.pid) && d.pid > 0,
          "receive foreign child PID");
  begin(&a);
  begin(&b);
  await_announced(3);
  before = children_cpu(1, "before-release");
  release_child(&c);
  join(ta);
  join(tb);
  int a_won = reaped(a.got, &c), b_won = reaped(b.got, &c);
  require(a_won + b_won == 1, "exactly one consumer reaps C");
  require(echild(a_won ? b.got : a.got), "the other consumer gets ECHILD");
  printf("{\"type\":\"outcome\",\"case\":1,\"winner\":\"%s\",\"winner_tid\":%d,"
         "\"loser\":\"%s\",\"loser_tid\":%d}\n", a_won ? "A" : "B",
         a_won ? a.tid : b.tid, a_won ? "B" : "A", a_won ? b.tid : a.tid);
  flush();
  after = children_cpu(1, "after-contest");
  added_cpu(before, after);
  require(echild(family_wait(1, "later-echild", c.pid, WNOHANG)), "C consumed once");
  equal_cpu(after, children_cpu(1, "after-echild"));
  release_child(&d);
  join(tn);
  struct cpu own = children_cpu(1, "after-foreign-reap");
  added_cpu(after, own);
  require(echild(family_wait(1, "foreign-echild", d.pid, WNOHANG)), "D consumed once");
  equal_cpu(own, children_cpu(1, "after-foreign-echild"));
  case_row(1, "two-consumers-and-foreign-nothread");

  /* Case 2: an observer and a consumer. */
  e = spawn(2, "E", 63, pipes[3]);
  begin(&o);
  begin(&q);
  await_announced(2);
  before = children_cpu(2, "before-release");
  release_child(&e);
  join(to);
  join(tq);
  require(reaped(q.got, &e), "the only consumer reaps E");
  int observed = reaped(o.got, &e);
  require(observed || echild(o.got), "observer sees E or finds it consumed");
  printf("{\"type\":\"outcome\",\"case\":2,\"observer\":\"%s\",\"observer_tid\":%d,"
         "\"consumer_tid\":%d}\n", observed ? "observed" : "echild", o.tid, q.tid);
  flush();
  after = children_cpu(2, "after-pair");
  added_cpu(before, after);
  require(echild(family_wait(2, "later-echild", e.pid, WNOHANG)), "E consumed once");
  equal_cpu(after, children_cpu(2, "after-echild"));
  case_row(2, "observer-and-consumer");

  /* Case 3: two any-child consumers; the loser parks again for G. */
  f = spawn(3, "F", 64, pipes[4]);
  g = spawn(3, "G", 65, pipes[5]);
  begin(&x);
  begin(&y);
  await_announced(2);
  before = children_cpu(3, "before-release");
  release_child(&f);
  char first = 0, second = 0;
  require(read(finish_pipe[0], &first, 1) == 1, "first finished wait");
  after = children_cpu(3, "after-first");
  added_cpu(before, after);
  for (unsigned i = 0; i < YIELDS; ++i) require(sched_yield() == 0, "yield");
  equal_cpu(after, children_cpu(3, "before-second-release"));
  release_child(&g);
  require(read(finish_pipe[0], &second, 1) == 1, "second finished wait");
  join(tx);
  join(ty);
  require((first == 'X' && second == 'Y') || (first == 'Y' && second == 'X'),
          "each consumer finished once");
  struct waiter *winner = first == 'X' ? &x : &y, *next = first == 'X' ? &y : &x;
  require(reaped(winner->got, &f), "first finisher reaps F");
  require(reaped(next->got, &g), "the loser reaps the next child G");
  printf("{\"type\":\"outcome\",\"case\":3,\"winner\":\"%s\",\"winner_tid\":%d,"
         "\"next\":\"%s\",\"next_tid\":%d}\n", winner->name, winner->tid,
         next->name, next->tid);
  flush();
  struct cpu last = children_cpu(3, "after-second");
  added_cpu(after, last);
  require(echild(family_wait(3, "later-echild", -1, WNOHANG)), "no child remains");
  equal_cpu(last, children_cpu(3, "after-echild"));
  case_row(3, "any-consumers-next-child");

  printf("{\"type\":\"summary\",\"family\":\"%s\",\"cases\":4,\"children\":6,"
         "\"threads\":7,\"calls\":%u,\"assertions\":%u,\"leader\":%d,\"passed\":true}\n",
         family_name(), calls, assertions, leader);
  flush();
  return 0;
}
