/* Two caught signals sent to a thread parked in a restartable call on an
 * internal pipe or socket: Linux runs both handlers, the higher-numbered one
 * first, and ends the call (https://github.com/rrnewton/hermit/issues/4273,
 * R2b-2 stage 2). Hermit used to run only one: two private injections in a
 * row were each stopped by one of the signals, and the backend's single held
 * slot kept only the second.
 *
 * Usage: guest_restartable_two_signals MODE [FIRST SECOND]
 *
 *   read | readv | recv | write  The worker makes that call: a read of an
 *       empty pipe, a readv of it, a recv on an empty socketpair, or a
 *       70000-byte write to an empty pipe, which writes 65536 bytes and waits
 *       for room. Main sends FIRST then SECOND (chld, winch, usr1, usr2) with
 *       tgkill, waits 50 ms and ends the call (writes a byte, or reads the
 *       pipe empty). Without FIRST and SECOND no signal is sent: the
 *       no-signal baseline.
 *   tight  The read, made on a stack placed 128 bytes into a writable page
 *       with an inaccessible page below it: the 128-byte red zone is all the
 *       writable memory below the stack pointer, so nothing can be staged
 *       below it; handlers run on an alternate stack. Signals as for read.
 *   twoturn  Main sends a caught SIGCHLD, waits 50 ms, sends a caught SIGUSR1,
 *       waits 50 ms and writes a byte.
 *   alarmheld  Main sends a caught SIGCHLD, arms a 50 ms ITIMER_REAL whose
 *       SIGALRM main itself blocks, waits 100 ms and writes a byte.
 *   ignheld  Main sends a caught SIGCHLD, waits 50 ms, sets SIGUSR2 to SIG_IGN,
 *       sends it, installs its handler again, waits 50 ms and writes a byte:
 *       Linux discards a signal that is ignored when sent and that the
 *       thread's own mask does not block.
 *   spinwake  Main sends a caught SIGCHLD, waits 50 ms, sends a caught SIGUSR1,
 *       then spins with no syscall, for at most SPIN_BOUND iterations, until
 *       the worker's call returns, and prints whether it did during the spin.
 *       Under Hermit the scheduler's drain must wake the parked read for the
 *       SIGUSR1 at once, though the read blocks every signal for the held
 *       SIGCHLD; it must not wait for the read's backed-off poll turn while
 *       the spinning thread holds the CPU.
 *   maskvis  The worker blocks SIGUSR2 and reads; SIGCHLD's handler has
 *       SA_RESTART, so Linux restarts the read after it. Main sends SIGCHLD,
 *       waits 50 ms, reads the worker's SigBlk from
 *       /proc/self/task/<tid>/status while the read waits, and writes a byte.
 *       The worker's mask is SIGUSR2 alone throughout: under Hermit the
 *       status must report the guest's mask, not the one Detcore installs
 *       while the read blocks every signal for the held SIGCHLD.
 *
 * The worker prints the call's result, the handlers that started before the
 * call returned, and, after main joins it, every handler that started:
 *   <mode>: <ret> <errno or 0>, handlers before return: ..., after join: ...
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/uio.h>
#include <time.h>
#include <unistd.h>

#if !defined(__x86_64__) || SYS_read != 0
#error This fixture specifies the Linux x86_64 read syscall ABI.
#endif

/* read(fd, buf, len) on the stack `new_sp`. No instruction between the stack
 * switch and its restoration touches the stack; r12 keeps the original rsp. */
extern long read_on_stack(void *new_sp, long fd, void *buf, long len);
__asm__(
    ".text\n"
    ".globl read_on_stack\n"
    ".type read_on_stack,@function\n"
    "read_on_stack:\n"
    "push %r12\n"
    "mov %rsp,%r12\n"
    "mov %rdi,%rsp\n"
    "mov %rsi,%rdi\n"
    "mov %rdx,%rsi\n"
    "mov %rcx,%rdx\n"
    "xor %eax,%eax\n"
    "syscall\n"
    "mov %r12,%rsp\n"
    "pop %r12\n"
    "ret\n"
    ".size read_on_stack,.-read_on_stack\n");

static const char *mode;
static int fds[2];
static atomic_int order[8];
static atomic_int handled;
static atomic_int before_return = -1;
static atomic_int returned;
static int returned_during_spin = -1;
static unsigned long long sibling_saw_sigblk;
static int sibling_read_sigblk;

/* Natively the read returns at once. Under Hermit a prompt drain wake ends it
 * within one of the spinning thread's timeslices (about 20 million iterations
 * of a debug build); waiting for the read's backed-off poll turn took about
 * 1.3 billion. */
#define SPIN_BOUND 300000000L
static volatile pid_t worker_tid;

static void handler(int signal)
{
    int slot = atomic_fetch_add(&handled, 1);
    if (slot < 8)
        atomic_store(&order[slot], signal);
}

static void die(const char *what)
{
    perror(what);
    exit(2);
}

static int signal_named(const char *name)
{
    if (!strcmp(name, "chld"))
        return SIGCHLD;
    if (!strcmp(name, "winch"))
        return SIGWINCH;
    if (!strcmp(name, "usr1"))
        return SIGUSR1;
    if (!strcmp(name, "usr2"))
        return SIGUSR2;
    fprintf(stderr, "unknown signal %s\n", name);
    exit(2);
}

static void print_handlers(int count)
{
    if (count == 0)
        printf(" none");
    for (int i = 0; i < count && i < 8; i++)
        printf(" %d", atomic_load(&order[i]));
}

static void sleep_ms(long ms)
{
    struct timespec delay = {ms / 1000, (ms % 1000) * 1000000L};
    while (nanosleep(&delay, &delay) != 0 && errno == EINTR) {
    }
}

static void *worker(void *unused)
{
    (void)unused;
    sigset_t mask;
    sigemptyset(&mask);
    sigaddset(&mask, SIGCHLD);
    sigaddset(&mask, SIGWINCH);
    sigaddset(&mask, SIGUSR1);
    sigaddset(&mask, SIGUSR2);
    sigaddset(&mask, SIGALRM);
    if (pthread_sigmask(SIG_UNBLOCK, &mask, NULL) != 0)
        die("pthread_sigmask");
    if (!strcmp(mode, "maskvis")) {
        sigset_t usr2;
        sigemptyset(&usr2);
        sigaddset(&usr2, SIGUSR2);
        if (pthread_sigmask(SIG_BLOCK, &usr2, NULL) != 0)
            die("pthread_sigmask");
    }
    worker_tid = syscall(SYS_gettid);
    static char buffer[70000];
    long ret;
    if (!strcmp(mode, "readv")) {
        struct iovec iov = {buffer, 1};
        ret = readv(fds[0], &iov, 1);
    } else if (!strcmp(mode, "recv")) {
        ret = recv(fds[0], buffer, 1, 0);
    } else if (!strcmp(mode, "write")) {
        ret = write(fds[1], buffer, sizeof buffer);
    } else if (!strcmp(mode, "tight")) {
        stack_t alternate = {.ss_sp = malloc(SIGSTKSZ * 4), .ss_size = SIGSTKSZ * 4};
        if (!alternate.ss_sp || sigaltstack(&alternate, NULL) != 0)
            die("sigaltstack");
        char *pages = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (pages == MAP_FAILED || mprotect(pages, 4096, PROT_NONE) != 0)
            die("tight stack");
        ret = read_on_stack(pages + 4096 + 128, fds[0], buffer, 1);
        if (ret < 0) {
            errno = (int)-ret;
            ret = -1;
        }
    } else {
        ret = read(fds[0], buffer, 1);
    }
    int error = ret < 0 ? errno : 0;
    atomic_store(&before_return, atomic_load(&handled));
    atomic_store(&returned, 1);
    printf("%s: %ld %s, handlers before return:", mode, ret, error ? strerrorname_np(error) : "0");
    print_handlers(atomic_load(&before_return));
    fflush(stdout);
    return NULL;
}

int main(int argc, char **argv)
{
    if (argc != 2 && argc != 4) {
        fprintf(stderr, "usage: %s MODE [FIRST SECOND]\n", argv[0]);
        return 2;
    }
    mode = argv[1];
    struct sigaction action;
    memset(&action, 0, sizeof action);
    action.sa_handler = handler;
    action.sa_flags = SA_ONSTACK;
    int caught[] = {SIGCHLD, SIGWINCH, SIGUSR1, SIGUSR2, SIGALRM};
    for (size_t i = 0; i < sizeof caught / sizeof caught[0]; i++)
        if (sigaction(caught[i], &action, NULL) != 0)
            die("sigaction");
    if (!strcmp(mode, "maskvis")) {
        struct sigaction restart = action;
        restart.sa_flags |= SA_RESTART;
        if (sigaction(SIGCHLD, &restart, NULL) != 0)
            die("sigaction");
    }
    if (!strcmp(mode, "recv")) {
        if (socketpair(AF_UNIX, SOCK_STREAM, 0, fds) != 0)
            die("socketpair");
    } else if (pipe(fds) != 0) {
        die("pipe");
    }
    /* Main blocks every signal it sends, so only the worker takes them. */
    sigset_t mask;
    sigemptyset(&mask);
    for (size_t i = 0; i < sizeof caught / sizeof caught[0]; i++)
        sigaddset(&mask, caught[i]);
    if (pthread_sigmask(SIG_BLOCK, &mask, NULL) != 0)
        die("pthread_sigmask");
    pthread_t thread;
    if (pthread_create(&thread, NULL, worker, NULL) != 0)
        die("pthread_create");
    while (!worker_tid)
        sleep_ms(1);
    sleep_ms(50);
    pid_t pid = getpid();
    if (!strcmp(mode, "twoturn")) {
        syscall(SYS_tgkill, pid, worker_tid, SIGCHLD);
        sleep_ms(50);
        syscall(SYS_tgkill, pid, worker_tid, SIGUSR1);
        sleep_ms(50);
    } else if (!strcmp(mode, "ignheld")) {
        syscall(SYS_tgkill, pid, worker_tid, SIGCHLD);
        sleep_ms(50);
        struct sigaction ignore;
        memset(&ignore, 0, sizeof ignore);
        ignore.sa_handler = SIG_IGN;
        if (sigaction(SIGUSR2, &ignore, NULL) != 0)
            die("sigaction");
        syscall(SYS_tgkill, pid, worker_tid, SIGUSR2);
        if (sigaction(SIGUSR2, &action, NULL) != 0)
            die("sigaction");
        sleep_ms(50);
    } else if (!strcmp(mode, "spinwake")) {
        syscall(SYS_tgkill, pid, worker_tid, SIGCHLD);
        sleep_ms(50);
        syscall(SYS_tgkill, pid, worker_tid, SIGUSR1);
        for (long i = 0; i < SPIN_BOUND && !atomic_load(&returned); i++) {
        }
        returned_during_spin = atomic_load(&returned);
    } else if (!strcmp(mode, "maskvis")) {
        syscall(SYS_tgkill, pid, worker_tid, SIGCHLD);
        sleep_ms(50);
        char path[128];
        snprintf(path, sizeof path, "/proc/self/task/%d/status", (int)worker_tid);
        FILE *status = fopen(path, "r");
        if (!status)
            die("fopen status");
        char line[512];
        while (fgets(line, sizeof line, status))
            if (sscanf(line, "SigBlk: %llx", &sibling_saw_sigblk) == 1)
                sibling_read_sigblk = 1;
        fclose(status);
    } else if (!strcmp(mode, "alarmheld")) {
        syscall(SYS_tgkill, pid, worker_tid, SIGCHLD);
        struct itimerval timer = {{0, 0}, {0, 50000}};
        if (setitimer(ITIMER_REAL, &timer, NULL) != 0)
            die("setitimer");
        sleep_ms(100);
    } else if (argc == 4) {
        syscall(SYS_tgkill, pid, worker_tid, signal_named(argv[2]));
        syscall(SYS_tgkill, pid, worker_tid, signal_named(argv[3]));
        sleep_ms(50);
    } else {
        sleep_ms(50);
    }
    /* End the call if it still waits: data for a reader, room for the writer. */
    static char drain[70000];
    if (!strcmp(mode, "write")) {
        /* Drain what is there until the write returns, however many bytes it
         * wrote before it waited. */
        int flags = fcntl(fds[0], F_GETFL);
        if (flags < 0 || fcntl(fds[0], F_SETFL, flags | O_NONBLOCK) != 0)
            die("fcntl");
        while (!atomic_load(&returned)) {
            while (read(fds[0], drain, sizeof drain) > 0) {
            }
            sleep_ms(5);
        }
    } else if (write(fds[1], "x", 1) != 1) {
        die("write");
    }
    pthread_join(thread, NULL);
    printf(", after join:");
    print_handlers(atomic_load(&handled));
    if (sibling_read_sigblk)
        printf(", sibling saw SigBlk %016llx", sibling_saw_sigblk);
    if (returned_during_spin >= 0)
        printf(", returned during the spin: %s", returned_during_spin ? "yes" : "no");
    printf("\n");
    return 0;
}
