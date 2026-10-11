/* A pselect6 with more than 64 descriptors and a temporary mask, made on a
 * stack with no writable memory below its red zone (Codex re-check of
 * https://github.com/rrnewton/hermit/pull/4053).
 *
 * Usage: pselect_tight_stack <tight|adjacent|ample|crowded>
 *
 * The call runs on a stack placed 144 (tight), 160 (adjacent) or 1024 (ample)
 * bytes into a writable page with an inaccessible page below it. Each mode
 * makes four calls in turn, so the later ones reuse whatever storage the
 * earlier ones left:
 *
 *   ready    a readable pipe; returns 1.
 *   signal   SIGALRM, blocked by the thread's own mask and unblocked by the
 *            temporary one, arrives during the wait; returns -EINTR with the
 *            handler run once, on an alternate stack.
 *   restart  SIGCONT, blocked by the own mask and left to its default action,
 *            is pending when the call starts; the kernel takes it, runs no
 *            handler and restarts the call, which reads its temporary mask
 *            again, then times out after 50 ms and returns 0.
 *   again    the ready call once more.
 *   aliased  the ready call with its fd set and timeout in the top 32 bytes
 *            of the red zone, which a leaf function may use: Linux reads and
 *            writes them, so Hermit must stage nothing there, and the rest of
 *            the red zone must survive too.
 *   crowded  (adjacent and ample only) the ready call with nfds 1024 and its
 *            128-byte fd set filling the red zone; the stack has scratch
 *            below the red zone there, so Hermit stages its copy in that.
 *            Tight mode prints crowded=none and skips it: mode crowded below
 *            makes that call on the tight stack.
 *
 * Then two batches of four threads each make the ready call on a stack of
 * their own at the same depth. The bytes mapped, summed over /proc/self/maps,
 * must be the same before the main thread's calls and after them (maps), and
 * after each batch (threads; glibc reuses the first batch's thread stacks for
 * the second): Hermit must leave no mapping of its own behind, per call or per
 * thread. Every call fills the 128-byte red zone below its stack with a
 * pattern first, and the pattern must be intact after it (red_zone): Hermit
 * may stage there only if it puts the guest's bytes back.
 *
 * Natively every mode prints
 *   mode=<mode> ready=1 signal=-4 alarms=1 restart=0 again=1 aliased=1 crowded=<1, or none in tight mode> maps=1 threads=1 red_zone=1 oracle=1
 *
 * Mode crowded makes one call on the tight stack: the ready call with nfds
 * 1024 and its 128-byte fd set filling the whole red zone, so Hermit has no
 * room for its private copy of the mask. Natively it prints
 *   mode=crowded crowded=1 oracle=1
 */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/select.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

#if !defined(__x86_64__) || !defined(SYS_pselect6) || SYS_pselect6 != 270
#error This fixture specifies the Linux x86_64 pselect6 syscall ABI.
#endif

/* No instruction between the stack switch and its restoration touches the
 * stack. SysV inputs: new_sp, nfds, readfds, timeout, wrapper. Preserves the
 * caller's r12 on the original stack and keeps the original rsp in r12 across
 * the syscall. */
extern long pselect_on_stack(void *, long, fd_set *, struct timespec *, void *);
__asm__(
    ".text\n"
    ".globl pselect_on_stack\n"
    ".type pselect_on_stack,@function\n"
    "pselect_on_stack:\n"
    "push %r12\n"
    "mov %rsp,%r12\n"
    "mov %rdi,%r11\n"
    "mov %rsi,%rdi\n"
    "mov %rdx,%rsi\n"
    "mov %rcx,%rax\n"
    "xor %edx,%edx\n"
    "xor %r10d,%r10d\n"
    "mov %r8,%r9\n"
    "mov %rax,%r8\n"
    "mov %r11,%rsp\n"
    "mov $270,%eax\n"
    "syscall\n"
    "mov %r12,%rsp\n"
    "pop %r12\n"
    "ret\n"
    ".size pselect_on_stack,.-pselect_on_stack\n");

#define NFDS 65

static uint64_t empty_mask;
static struct {
    const uint64_t *mask;
    size_t bytes;
} wrapper = {&empty_mask, sizeof(empty_mask)};
static volatile sig_atomic_t alarms;
static unsigned char alternate[65536];

static void on_alarm(int signo) {
    (void)signo;
    alarms++;
}

/* Red-zone bytes that differed after a call from the pattern written before
 * it, summed over every call and thread. */
static atomic_int red_zone_changed;

/* Makes the call on `stack`, with a pattern in the 128-byte red zone below
 * it, and counts the pattern bytes the call left changed. */
static long select_on(unsigned char *stack, int fd, long seconds, long nanos) {
    fd_set readable;
    struct timespec timeout = {seconds, nanos};
    FD_ZERO(&readable);
    FD_SET(fd, &readable);
    for (int i = -128; i < 0; i++)
        stack[i] = (unsigned char)(0xa5 ^ (i & 0xff));
    long result = pselect_on_stack(stack, NFDS, &readable, &timeout, &wrapper);
    int changed = 0;
    for (int i = -128; i < 0; i++)
        changed += stack[i] != (unsigned char)(0xa5 ^ (i & 0xff));
    atomic_fetch_add(&red_zone_changed, changed);
    return result;
}

/* The ready call with its fd set at stack - 16 (two words cover NFDS) and its
 * timeout at stack - 32, inside the red zone, and a pattern in the other 96
 * red-zone bytes. Returns the result if the fd set came back with only `fd`
 * set, and -1000 otherwise; counts changed pattern bytes. */
static long select_aliased(unsigned char *stack, int fd) {
    uint64_t *readable = (uint64_t *)(void *)(stack - 16);
    struct timespec *timeout = (struct timespec *)(void *)(stack - 32);
    for (int i = -128; i < -32; i++)
        stack[i] = (unsigned char)(0xa5 ^ (i & 0xff));
    readable[0] = UINT64_C(1) << fd;
    readable[1] = 0;
    timeout->tv_sec = 2;
    timeout->tv_nsec = 0;
    long result = pselect_on_stack(stack, NFDS, (fd_set *)(void *)readable, timeout, &wrapper);
    int changed = 0;
    for (int i = -128; i < -32; i++)
        changed += stack[i] != (unsigned char)(0xa5 ^ (i & 0xff));
    atomic_fetch_add(&red_zone_changed, changed);
    return readable[0] == (UINT64_C(1) << fd) && readable[1] == 0 ? result : -1000;
}

/* The ready call with nfds 1024 and its fd set occupying all 128 red-zone
 * bytes. Returns the result if the fd set came back with only `fd` set, and
 * -1000 otherwise. */
static long select_crowded(unsigned char *stack, int fd) {
    uint64_t *readable = (uint64_t *)(void *)(stack - 128);
    static struct timespec timeout;
    memset(readable, 0, 128);
    readable[0] = UINT64_C(1) << fd;
    timeout.tv_sec = 2;
    timeout.tv_nsec = 0;
    long result = pselect_on_stack(stack, 1024, (fd_set *)(void *)readable, &timeout, &wrapper);
    int only_fd = readable[0] == (UINT64_C(1) << fd);
    for (int i = 1; i < 16; i++)
        only_fd = only_fd && readable[i] == 0;
    return only_fd ? result : -1000;
}

/* The bytes mapped in this address space, summed over /proc/self/maps.
 * Adjacent anonymous pages with the same protection merge into one line, so a
 * line count could hide a new page; the total cannot. */
static long mapped_bytes(void) {
    FILE *maps = fopen("/proc/self/maps", "r");
    if (!maps)
        return -1;
    long total = 0;
    char line[512];
    while (fgets(line, sizeof(line), maps)) {
        unsigned long start, end;
        if (sscanf(line, "%lx-%lx", &start, &end) != 2) {
            fclose(maps);
            return -1;
        }
        total += (long)(end - start);
        /* Skip the rest of an overlong line. */
        while (!strchr(line, '\n') && fgets(line, sizeof(line), maps))
            ;
    }
    fclose(maps);
    return total;
}

#define WORKERS 4

struct worker {
    unsigned char *stack;
    int fd;
    long result;
};

static void *work(void *arg) {
    struct worker *worker = arg;
    worker->result = select_on(worker->stack, worker->fd, 2, 0);
    return NULL;
}

/* Runs one batch of workers; returns how many saw their pipe ready. */
static int run_batch(struct worker *workers) {
    pthread_t threads[WORKERS];
    for (int i = 0; i < WORKERS; i++)
        if (pthread_create(&threads[i], NULL, work, &workers[i]) != 0)
            return -1;
    int ready = 0;
    for (int i = 0; i < WORKERS; i++) {
        if (pthread_join(threads[i], NULL) != 0)
            return -1;
        ready += workers[i].result == 1;
    }
    return ready;
}

int main(int argc, char **argv) {
    size_t offset;
    if (argc != 2)
        return 2;
    int crowded_mode = !strcmp(argv[1], "crowded");
    if (!strcmp(argv[1], "tight") || crowded_mode)
        offset = 144; /* 16 bytes below rsp minus the 128-byte red zone */
    else if (!strcmp(argv[1], "adjacent"))
        offset = 160;
    else if (!strcmp(argv[1], "ample"))
        offset = 1024;
    else
        return 2;
    long page_long = sysconf(_SC_PAGESIZE);
    if (page_long <= 0 || (size_t)page_long <= offset)
        return 3;
    size_t page = (size_t)page_long;
    unsigned char *area =
        mmap(NULL, page * 2, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (area == MAP_FAILED)
        return 4;
    unsigned char *writable = area + page;
    if (mprotect(writable, page, PROT_READ | PROT_WRITE) != 0)
        return 5;
    unsigned char *stack = writable + offset;

    stack_t alt = {.ss_sp = alternate, .ss_size = sizeof(alternate), .ss_flags = 0};
    if (sigaltstack(&alt, NULL) != 0)
        return 6;
    struct sigaction action;
    memset(&action, 0, sizeof(action));
    action.sa_handler = on_alarm;
    action.sa_flags = SA_ONSTACK;
    if (sigaction(SIGALRM, &action, NULL) != 0)
        return 7;
    sigset_t own;
    sigemptyset(&own);
    sigaddset(&own, SIGALRM);
    sigaddset(&own, SIGCONT);
    if (sigprocmask(SIG_SETMASK, &own, NULL) != 0)
        return 8;

    int ready_pipe[2], idle_pipe[2];
    if (pipe(ready_pipe) != 0 || pipe(idle_pipe) != 0 || ready_pipe[0] >= 64 ||
        idle_pipe[0] >= 64)
        return 9;
    if (write(ready_pipe[1], "x", 1) != 1)
        return 10;
    if (crowded_mode) {
        long crowded = select_crowded(stack, ready_pipe[0]);
        printf("mode=crowded crowded=%ld oracle=%d\n", crowded, crowded == 1);
        return crowded == 1 ? 0 : 42;
    }

    /* Every mapping the run needs is made before the first count. */
    struct worker workers[WORKERS];
    for (int i = 0; i < WORKERS; i++) {
        unsigned char *worker_area =
            mmap(NULL, page * 2, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (worker_area == MAP_FAILED ||
            mprotect(worker_area + page, page, PROT_READ | PROT_WRITE) != 0)
            return 13;
        workers[i].stack = worker_area + page + offset;
        workers[i].fd = ready_pipe[0];
    }
    long maps_before = mapped_bytes();

    long ready = select_on(stack, ready_pipe[0], 2, 0);

    struct itimerval timer = {{0, 0}, {0, 100000}};
    if (setitimer(ITIMER_REAL, &timer, NULL) != 0)
        return 11;
    long signal = select_on(stack, idle_pipe[0], 2, 0);
    int alarms_seen = alarms;

    if (kill(getpid(), SIGCONT) != 0)
        return 12;
    long restart = select_on(stack, idle_pipe[0], 0, 50000000);

    long again = select_on(stack, ready_pipe[0], 2, 0);
    long aliased = select_aliased(stack, ready_pipe[0]);
    int tight = offset == 144;
    long crowded = tight ? 1 : select_crowded(stack, ready_pipe[0]);
    long maps_after = mapped_bytes();

    int first_batch = run_batch(workers);
    long maps_first = mapped_bytes();
    int second_batch = run_batch(workers);
    long maps_second = mapped_bytes();

    int maps = maps_before > 0 && maps_after == maps_before;
    int threads = first_batch == WORKERS && second_batch == WORKERS && maps_first > 0 &&
                  maps_second == maps_first;
    int red_zone = atomic_load(&red_zone_changed) == 0;
    int oracle = ready == 1 && signal == -EINTR && alarms_seen == 1 && restart == 0 &&
                 again == 1 && aliased == 1 && crowded == 1 && maps && threads && red_zone;
    char crowded_text[24];
    if (tight)
        snprintf(crowded_text, sizeof(crowded_text), "none");
    else
        snprintf(crowded_text, sizeof(crowded_text), "%ld", crowded);
    printf("mode=%s ready=%ld signal=%ld alarms=%d restart=%ld again=%ld aliased=%ld "
           "crowded=%s maps=%d threads=%d red_zone=%d oracle=%d\n",
           argv[1], ready, signal, alarms_seen, restart, again, aliased, crowded_text, maps,
           threads, red_zone, oracle);
    if (!oracle)
        fprintf(stderr, "maps before=%ld after=%ld first=%ld second=%ld batches=%d,%d\n",
                maps_before, maps_after, maps_first, maps_second, first_batch,
                second_batch);
    return oracle ? 0 : 42;
}
