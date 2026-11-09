// A timed sleep or futex wait that a signal ends in the kernel returns
// ERESTART_RESTARTBLOCK. With no handler to run, Linux restarts it through
// restart_syscall until the end time it kept (hrtimer_nanosleep_restart,
// futex_wait_restart); with a handler it returns EINTR. Under a tracer a
// default-ignored signal is still queued, so it ends the wait in the kernel.
//
// "nanosleep", "clock_nanosleep", "abs" and "futex": a child sends 40 SIGWINCH,
// which is ignored by default, 20 ms apart while the parent waits for 100 ms
// (nanosleep, a relative CLOCK_MONOTONIC clock_nanosleep, an absolute one, and
// a FUTEX_WAIT no one wakes). The wait must not end before the third signal
// and must end long before the storm does.
// "alrm": a 30 ms ITIMER_REAL with a SIGALRM handler interrupts each of the
// three relative waits: each returns EINTR, and nanosleep and clock_nanosleep
// report the time left.
#define _GNU_SOURCE
#include <errno.h>
#include <linux/futex.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/select.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t alarms;
static int word;

static void on_alarm(int sig) {
  (void)sig;
  alarms++;
}

static const char *outcome(long r) {
  if (r == 0) {
    return "0";
  }
  if (errno == EINTR) {
    return "EINTR";
  }
  if (errno == ETIMEDOUT) {
    return "ETIMEDOUT";
  }
  return strerror(errno);
}

// One wait of 100 ms; `rem` receives the time left where the call reports it.
static long wait_once(const char *mode, struct timespec *rem) {
  struct timespec ts = {0, 100 * 1000 * 1000};
  if (strcmp(mode, "nanosleep") == 0) {
    return syscall(SYS_nanosleep, &ts, rem);
  }
  if (strcmp(mode, "clock_nanosleep") == 0) {
    return syscall(SYS_clock_nanosleep, CLOCK_MONOTONIC, 0, &ts, rem);
  }
  if (strcmp(mode, "abs") == 0) {
    struct timespec end;
    clock_gettime(CLOCK_MONOTONIC, &end);
    end.tv_nsec += ts.tv_nsec;
    if (end.tv_nsec >= 1000 * 1000 * 1000) {
      end.tv_sec++;
      end.tv_nsec -= 1000 * 1000 * 1000;
    }
    return syscall(SYS_clock_nanosleep, CLOCK_MONOTONIC, TIMER_ABSTIME, &end,
                   NULL);
  }
  return syscall(SYS_futex, &word, FUTEX_WAIT_PRIVATE, 0, &ts, NULL, 0);
}

static int reap(void) {
  int children = 0;
  int status;
  while (wait(&status) > 0) {
    if (WIFEXITED(status) && WEXITSTATUS(status) == 0) {
      children++;
    }
  }
  return children;
}

static int alarm_mode(void) {
  struct sigaction sa;
  memset(&sa, 0, sizeof sa);
  sa.sa_handler = on_alarm;
  sigaction(SIGALRM, &sa, NULL);
  const char *modes[] = {"nanosleep", "clock_nanosleep", "futex"};
  printf("alrm:");
  for (int i = 0; i < 3; i++) {
    struct itimerval once = {{0, 0}, {0, 30 * 1000}};
    struct timespec rem = {-1, -1};
    setitimer(ITIMER_REAL, &once, NULL);
    long r = wait_once(modes[i], &rem);
    printf(" %s %s", modes[i], outcome(r));
    if (i < 2) {
      long long left = rem.tv_sec * 1000000000LL + rem.tv_nsec;
      printf(" (time left within 0-100 ms: %s)",
             left > 0 && left < 100 * 1000 * 1000 ? "yes" : "no");
    }
    printf(i < 2 ? "," : ";");
  }
  printf(" %d alarms\n", (int)alarms);
  return 0;
}

int main(int argc, char **argv) {
  const char *mode = argc > 1 ? argv[1] : "nanosleep";
  if (strcmp(mode, "alrm") == 0) {
    return alarm_mode();
  }
  int done[2];
  if (pipe(done) != 0) {
    perror("pipe");
    return 2;
  }
  // The number of signals the child has sent, shared with the parent.
  volatile int *sent = mmap(NULL, sizeof *sent, PROT_READ | PROT_WRITE,
                            MAP_SHARED | MAP_ANONYMOUS, -1, 0);
  if (sent == MAP_FAILED) {
    perror("mmap");
    return 2;
  }
  pid_t parent = getpid();
  if (fork() == 0) {
    struct timespec gap = {0, 20 * 1000 * 1000};
    for (int i = 0; i < 40; i++) {
      syscall(SYS_nanosleep, &gap, NULL);
      kill(parent, SIGWINCH);
      __atomic_add_fetch(sent, 1, __ATOMIC_SEQ_CST);
    }
    _exit(write(done[1], "x", 1) == 1 ? 0 : 1);
  }
  struct timespec rem;
  long r = wait_once(mode, &rem);
  const char *result = outcome(r);
  int sent_before_return = __atomic_load_n(sent, __ATOMIC_SEQ_CST);
  fd_set set;
  FD_ZERO(&set);
  FD_SET(done[0], &set);
  struct timeval tv = {0, 0};
  int over = select(done[0] + 1, &set, NULL, NULL, &tv);
  printf("%s: %s; at least 3 signals came first: %s; the storm was still "
         "running when it returned: %s; %d children\n",
         mode, result, sent_before_return >= 3 ? "yes" : "no",
         over == 0 ? "yes" : "no", reap());
  return 0;
}
