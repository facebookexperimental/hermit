/* A SIGCHLD that a guest sends a thread (tgkill), while that thread waits in
 * poll, ppoll, epoll_wait, epoll_pwait or rt_sigtimedwait, or reads or writes
 * a pipe: Linux ends the call with EINTR and runs the handler first, or, with
 * SA_RESTART, restarts a pipe read or write. Hermit used to hold such a
 * SIGCHLD until the call returned, so the call completed and the handler ran
 * late (https://github.com/rrnewton/hermit/issues/4273, R2b-2 stage 3).
 *
 * Usage: guest_private_sigchld_waits FAMILY CALL VARIANT
 *
 *   thread  A worker thread makes CALL; main sends it a SIGCHLD with tgkill
 *       (not for VARIANT none), waits 50 ms, records what happened, then ends
 *       the call: a byte for a pipe or socket read, SIGUSR1 for
 *       sigtimedwait, FUTEX_WAKE for futex, a drain for send, a connect for
 *       accept4. CALL: read poll ppoll epoll_wait epoll_pwait select
 *       sigtimedwait futex (on a pipe, epoll_pwait with an empty temporary
 *       mask), readv (pipe), recv send accept4 (a socketpair, a full one for
 *       send, a listening socket), recv-timeo (recv with SO_RCVTIMEO 10 s).
 *       Prints: <call> <variant>: <ret> <errno or 0>, handled before wake N,
 *       ended before wake N, handled after join N
 *   fork  A single-threaded process makes CALL; a forked child sends it a
 *       SIGCHLD with tgkill 50 ms in, then, 50 ms later, ends the call (a byte,
 *       or a drain of the full pipe for write and writev), then pauses until
 *       the parent kills and reaps it after the call, so the child's own exit
 *       SIGCHLD comes after the line is decided. A single-threaded waiter
 *       keeps Detcore's signal-state copy, which a glibc pthread_create drops.
 *       CALL: read readv write writev poll ppoll epoll_wait epoll_pwait
 *       sigtimedwait (a 200 ms timeout). Under VARIANT blocked, ppoll and
 *       epoll_pwait wait with an empty temporary mask.
 *       Prints: <call> <variant>: <ret> <errno text or 0>, handled before
 *       return N, after N
 *       epoll_pwait2 is also a CALL here (the raw syscall, 1 s timeout).
 *   edge  Edge cases of the turn that admits the copy; CALL is the case:
 *       both  A worker parks in poll on an empty pipe; main sends SIGCHLD and
 *           writes the byte back to back: the poll's next turn sees a ready
 *           descriptor and the copy, and the completion wins (1), as Linux
 *           takes a ready descriptor before it checks for a signal.
 *       ebadf  A worker with SIGCHLD pending and blocked calls epoll_pwait on
 *           a bad descriptor with an empty temporary mask: EBADF wins.
 *       insetpoll  A worker that blocks SIGCHLD takes main's copy with
 *           sigwaitinfo, then unblocks it and polls an empty pipe: the copy is
 *           gone, so nothing ends the poll before the byte.
 *       flushpoll  As insetpoll, but the worker discards the copy by setting
 *           SIG_IGN, then reinstalls the handler.
 *       twice  A worker parks in an SA_RESTART read of an empty pipe; main
 *           sends SIGCHLD twice, 50 ms apart, then the byte: two handlers,
 *           each restarting the read.
 *       tightpoll  A worker polls an empty pipe on a stack whose only writable
 *           memory below the stack pointer is the red zone (handlers on an
 *           alternate stack); main sends SIGCHLD 50 ms in, then the byte.
 *   pending  A single thread raises CALL's signal (chld or usr1) while it
 *       blocks it, then calls epoll_pwait on an empty pipe with an empty
 *       temporary mask, which lets it in. Prints: pending <sig>: <ret> <errno
 *       or 0>, handled N
 *
 *   VARIANT: none, caught, restart (caught with SA_RESTART), blocked (by the
 *       waiting thread's own mask), default (SIG_DFL).
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/futex.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/mman.h>
#include <sys/select.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#if !defined(__x86_64__)
#error This fixture specifies the Linux x86_64 syscall ABI.
#endif

/* syscall(nr, a1, a2, a3) on the stack `new_sp`. No instruction between the
 * stack switch and its restoration touches the stack; r12 keeps the original
 * rsp. Returns the raw result (a negative errno on failure). */
extern long syscall3_on_stack(void *new_sp, long nr, long a1, long a2, long a3);
__asm__(".text\n"
        ".globl syscall3_on_stack\n"
        ".type syscall3_on_stack,@function\n"
        "syscall3_on_stack:\n"
        "push %r12\n"
        "mov %rsp,%r12\n"
        "mov %rdi,%rsp\n"
        "mov %rsi,%rax\n"
        "mov %rdx,%rdi\n"
        "mov %rcx,%rsi\n"
        "mov %r8,%rdx\n"
        "syscall\n"
        "mov %r12,%rsp\n"
        "pop %r12\n"
        "ret\n");

static const char *call, *variant;
static int fds[2], sv[2], lsock;
static struct sockaddr_un addr;
static atomic_int ready, handled, done, fword;
static pid_t worker_tid;
static long result;
static int err;

static void on_chld(int sig)
{
    (void)sig;
    atomic_fetch_add(&handled, 1);
}

static int is(const char *name)
{
    return !strcmp(call, name);
}

static void sleep_ms(long ms)
{
    struct timespec delay = {ms / 1000, (ms % 1000) * 1000000};
    nanosleep(&delay, NULL);
}

static const char *errno_word(int error)
{
    return error == EINTR ? "EINTR" : (error ? strerror(error) : "0");
}

static void install_chld_handler(void)
{
    struct sigaction action;
    memset(&action, 0, sizeof action);
    action.sa_handler = strcmp(variant, "default") ? on_chld : SIG_DFL;
    if (!strcmp(variant, "restart"))
        action.sa_flags = SA_RESTART;
    sigaction(SIGCHLD, &action, NULL);
}

/* `thread`: see the header. */
static void *worker(void *unused)
{
    (void)unused;
    sigset_t chld;
    sigemptyset(&chld);
    sigaddset(&chld, SIGCHLD);
    pthread_sigmask(strcmp(variant, "blocked") ? SIG_UNBLOCK : SIG_BLOCK, &chld, NULL);
    worker_tid = (pid_t)syscall(SYS_gettid);
    atomic_store(&ready, 1);
    char c;
    static char big[1 << 16];
    if (is("read")) {
        result = read(fds[0], &c, 1);
    } else if (is("readv")) {
        struct iovec v = {&c, 1};
        result = readv(fds[0], &v, 1);
    } else if (is("poll")) {
        struct pollfd p = {fds[0], POLLIN, 0};
        result = poll(&p, 1, -1);
    } else if (is("ppoll")) {
        struct pollfd p = {fds[0], POLLIN, 0};
        result = ppoll(&p, 1, NULL, NULL);
    } else if (is("epoll_wait") || is("epoll_pwait")) {
        int ep = epoll_create1(0);
        struct epoll_event e = {.events = EPOLLIN, .data.fd = fds[0]};
        epoll_ctl(ep, EPOLL_CTL_ADD, fds[0], &e);
        if (is("epoll_wait")) {
            result = epoll_wait(ep, &e, 1, -1);
        } else {
            sigset_t none;
            sigemptyset(&none);
            result = epoll_pwait(ep, &e, 1, -1, &none);
        }
    } else if (is("sigtimedwait")) {
        sigset_t usr1;
        sigemptyset(&usr1);
        sigaddset(&usr1, SIGUSR1);
        result = sigwaitinfo(&usr1, NULL);
    } else if (is("futex")) {
        result = syscall(SYS_futex, &fword, FUTEX_WAIT_PRIVATE, 0, NULL, NULL, 0);
    } else if (is("recv") || is("recv-timeo")) {
        result = recv(sv[0], &c, 1, 0);
    } else if (is("send")) {
        result = send(sv[0], big, sizeof big, 0);
    } else if (is("accept4")) {
        result = accept4(lsock, NULL, NULL, 0);
    } else {
        fd_set readable;
        FD_ZERO(&readable);
        FD_SET(fds[0], &readable);
        result = select(fds[0] + 1, &readable, NULL, NULL, NULL);
    }
    err = result < 0 ? errno : 0;
    atomic_store(&done, 1);
    return NULL;
}

static void fill(int fd)
{
    static char chunk[4096];
    int flags = fcntl(fd, F_GETFL);
    fcntl(fd, F_SETFL, flags | O_NONBLOCK);
    while (write(fd, chunk, sizeof chunk) > 0) {
    }
    while (write(fd, chunk, 1) > 0) {
    }
    fcntl(fd, F_SETFL, flags);
}

static int thread_family(void)
{
    if (pipe(fds) || socketpair(AF_UNIX, SOCK_STREAM, 0, sv))
        return 2;
    if (is("recv-timeo")) {
        struct timeval timeout = {10, 0};
        setsockopt(sv[0], SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof timeout);
    }
    if (is("send"))
        fill(sv[0]);
    if (is("accept4")) {
        lsock = socket(AF_UNIX, SOCK_STREAM, 0);
        memset(&addr, 0, sizeof addr);
        addr.sun_family = AF_UNIX;
        snprintf(addr.sun_path + 1, sizeof addr.sun_path - 1, "private-sigchld-%d", getpid());
        if (bind(lsock, (struct sockaddr *)&addr, sizeof addr) || listen(lsock, 1))
            return 2;
    }
    install_chld_handler();
    /* Main blocks what it sends, so only the worker takes it; the worker
     * inherits SIGUSR1 blocked for sigtimedwait. */
    sigset_t sent;
    sigemptyset(&sent);
    sigaddset(&sent, SIGCHLD);
    sigaddset(&sent, SIGUSR1);
    pthread_sigmask(SIG_BLOCK, &sent, NULL);
    pthread_t thread;
    pthread_create(&thread, NULL, worker, NULL);
    while (!atomic_load(&ready))
        sched_yield();
    sleep_ms(20);
    if (strcmp(variant, "none"))
        syscall(SYS_tgkill, getpid(), worker_tid, SIGCHLD);
    sleep_ms(50);
    int before = atomic_load(&handled), ended_before = atomic_load(&done);
    if (is("sigtimedwait")) {
        syscall(SYS_tgkill, getpid(), worker_tid, SIGUSR1);
    } else if (is("futex")) {
        atomic_store(&fword, 1);
        syscall(SYS_futex, &fword, FUTEX_WAKE_PRIVATE, 1, NULL, NULL, 0);
    } else if (is("recv") || is("recv-timeo")) {
        if (send(sv[1], "x", 1, 0) != 1)
            return 2;
    } else if (is("send")) {
        static char sink[1 << 16];
        while (!atomic_load(&done))
            if (recv(sv[1], sink, sizeof sink, MSG_DONTWAIT) <= 0)
                sleep_ms(5);
    } else if (is("accept4")) {
        int peer = socket(AF_UNIX, SOCK_STREAM, 0);
        if (connect(peer, (struct sockaddr *)&addr, sizeof addr))
            return 2;
    } else if (write(fds[1], "x", 1) != 1) {
        return 2;
    }
    pthread_join(thread, NULL);
    /* A completed send or accept4 returns a count or a descriptor: N. */
    char ret[32];
    if (result > 1)
        snprintf(ret, sizeof ret, "N");
    else
        snprintf(ret, sizeof ret, "%ld", result);
    printf("%s %s: %s %s, handled before wake %d, ended before wake %d, handled after join %d\n",
           call, variant, ret, errno_word(err), before, ended_before, atomic_load(&handled));
    return 0;
}

/* `fork`: see the header. */
static int fork_family(void)
{
    int blocked = !strcmp(variant, "blocked");
    install_chld_handler();
    sigset_t chld, none;
    sigemptyset(&chld);
    sigaddset(&chld, SIGCHLD);
    sigemptyset(&none);
    sigprocmask(blocked ? SIG_BLOCK : SIG_UNBLOCK, &chld, NULL);
    if (pipe(fds))
        return 3;
    int writing = is("write") || is("writev");
    if (writing)
        fill(fds[1]);
    pid_t parent = getpid();
    pid_t child = fork();
    if (child == 0) {
        sleep_ms(50);
        syscall(SYS_tgkill, parent, parent, SIGCHLD);
        sleep_ms(50);
        if (writing) {
            char sink[4096];
            fcntl(fds[0], F_SETFL, O_NONBLOCK);
            while (read(fds[0], sink, sizeof sink) > 0) {
            }
        } else if (write(fds[1], "x", 1) != 1) {
            _exit(4);
        }
        for (;;)
            pause();
    }
    char c;
    if (is("read")) {
        result = read(fds[0], &c, 1);
    } else if (is("readv")) {
        struct iovec v = {&c, 1};
        result = readv(fds[0], &v, 1);
    } else if (is("write")) {
        result = write(fds[1], "y", 1);
    } else if (is("writev")) {
        struct iovec v = {"y", 1};
        result = writev(fds[1], &v, 1);
    } else if (is("poll")) {
        struct pollfd p = {fds[0], POLLIN, 0};
        result = poll(&p, 1, -1);
    } else if (is("ppoll")) {
        struct pollfd p = {fds[0], POLLIN, 0};
        result = ppoll(&p, 1, NULL, blocked ? &none : NULL);
    } else if (is("epoll_wait") || is("epoll_pwait")) {
        int ep = epoll_create1(0);
        struct epoll_event e = {.events = EPOLLIN, .data.fd = fds[0]};
        epoll_ctl(ep, EPOLL_CTL_ADD, fds[0], &e);
        if (is("epoll_wait"))
            result = epoll_wait(ep, &e, 1, -1);
        else
            result = epoll_pwait(ep, &e, 1, -1, blocked ? &none : NULL);
    } else if (is("epoll_pwait2")) {
        int ep = epoll_create1(0);
        struct epoll_event e = {.events = EPOLLIN, .data.fd = fds[0]};
        epoll_ctl(ep, EPOLL_CTL_ADD, fds[0], &e);
        struct timespec timeout = {1, 0};
        /* The kernel's sigset is 8 bytes. */
        result = syscall(SYS_epoll_pwait2, ep, &e, 1, &timeout, blocked ? &none : NULL, 8);
    } else if (is("sigtimedwait")) {
        sigset_t usr1;
        sigemptyset(&usr1);
        sigaddset(&usr1, SIGUSR1);
        struct timespec timeout = {0, 200 * 1000 * 1000};
        result = sigtimedwait(&usr1, NULL, &timeout);
    } else {
        return 2;
    }
    int error = result < 0 ? errno : 0;
    int before = atomic_load(&handled);
    kill(child, SIGKILL);
    waitpid(child, NULL, 0);
    if (blocked)
        sigprocmask(SIG_UNBLOCK, &chld, NULL);
    printf("%s %s: %ld %s, handled before return %d, after %d\n", call, variant, result,
           error ? strerror(error) : "0", before, atomic_load(&handled));
    return 0;
}

/* `edge`: see the header. */
static atomic_int stage;
static long first;
static int first_err;

static void *edge_worker(void *unused)
{
    (void)unused;
    sigset_t chld, none;
    sigemptyset(&chld);
    sigaddset(&chld, SIGCHLD);
    sigemptyset(&none);
    worker_tid = (pid_t)syscall(SYS_gettid);
    if (is("insetpoll") || is("flushpoll")) {
        pthread_sigmask(SIG_BLOCK, &chld, NULL);
        atomic_store(&ready, 1);
        while (atomic_load(&stage) < 1)
            sched_yield();
        if (is("insetpoll")) {
            first = sigwaitinfo(&chld, NULL);
            first_err = first < 0 ? errno : 0;
        } else {
            signal(SIGCHLD, SIG_IGN);
            install_chld_handler();
        }
        pthread_sigmask(SIG_UNBLOCK, &chld, NULL);
        atomic_store(&stage, 2);
        struct pollfd p = {fds[0], POLLIN, 0};
        result = poll(&p, 1, -1);
    } else if (is("ebadf")) {
        pthread_sigmask(SIG_BLOCK, &chld, NULL);
        atomic_store(&ready, 1);
        while (atomic_load(&stage) < 1)
            sched_yield();
        struct epoll_event e;
        result = epoll_pwait(fds[1] + 100, &e, 1, -1, &none);
        err = result < 0 ? errno : 0;
        first = atomic_load(&handled);
        pthread_sigmask(SIG_UNBLOCK, &chld, NULL);
        return NULL;
    } else if (is("tightpoll")) {
        pthread_sigmask(SIG_UNBLOCK, &chld, NULL);
        stack_t alternate = {.ss_sp = malloc(SIGSTKSZ * 4), .ss_size = SIGSTKSZ * 4};
        if (!alternate.ss_sp || sigaltstack(&alternate, NULL) != 0)
            return NULL;
        char *pages = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (pages == MAP_FAILED || mprotect(pages, 4096, PROT_NONE) != 0)
            return NULL;
        static struct pollfd p;
        p.fd = fds[0];
        p.events = POLLIN;
        atomic_store(&ready, 1);
        result = syscall3_on_stack(pages + 4096 + 128, SYS_poll, (long)&p, 1, -1);
        if (result < 0) {
            errno = (int)-result;
            result = -1;
        }
    } else if (is("both")) {
        pthread_sigmask(SIG_UNBLOCK, &chld, NULL);
        atomic_store(&ready, 1);
        struct pollfd p = {fds[0], POLLIN, 0};
        result = poll(&p, 1, -1);
    } else {
        pthread_sigmask(SIG_UNBLOCK, &chld, NULL);
        atomic_store(&ready, 1);
        char c;
        result = read(fds[0], &c, 1);
    }
    err = result < 0 ? errno : 0;
    return NULL;
}

static int edge_family(void)
{
    if (pipe(fds))
        return 2;
    variant = is("twice") ? "restart" : "caught";
    install_chld_handler();
    if (is("tightpoll")) {
        struct sigaction onstack;
        memset(&onstack, 0, sizeof onstack);
        onstack.sa_handler = on_chld;
        onstack.sa_flags = SA_ONSTACK;
        sigaction(SIGCHLD, &onstack, NULL);
    }
    sigset_t chld;
    sigemptyset(&chld);
    sigaddset(&chld, SIGCHLD);
    pthread_sigmask(SIG_BLOCK, &chld, NULL);
    pthread_t thread;
    pthread_create(&thread, NULL, edge_worker, NULL);
    while (!atomic_load(&ready))
        sched_yield();
    sleep_ms(20);
    if (is("insetpoll") || is("flushpoll")) {
        syscall(SYS_tgkill, getpid(), worker_tid, SIGCHLD);
        sleep_ms(20);
        atomic_store(&stage, 1);
        while (atomic_load(&stage) < 2)
            sched_yield();
        sleep_ms(50);
        int before = atomic_load(&handled);
        if (write(fds[1], "x", 1) != 1)
            return 2;
        pthread_join(thread, NULL);
        printf("%s: first %ld %s, poll %ld %s, handled before byte %d, after %d\n", call, first,
               errno_word(first_err), result, errno_word(err), before, atomic_load(&handled));
    } else if (is("both")) {
        sleep_ms(30);
        syscall(SYS_tgkill, getpid(), worker_tid, SIGCHLD);
        if (write(fds[1], "x", 1) != 1)
            return 2;
        pthread_join(thread, NULL);
        printf("both: poll %ld %s, handled %d\n", result, errno_word(err), atomic_load(&handled));
    } else if (is("ebadf")) {
        syscall(SYS_tgkill, getpid(), worker_tid, SIGCHLD);
        sleep_ms(20);
        atomic_store(&stage, 1);
        pthread_join(thread, NULL);
        printf("ebadf: %ld %s, handled at return %ld, after %d\n", result, errno_word(err), first,
               atomic_load(&handled));
    } else if (is("twice") || is("tightpoll")) {
        sleep_ms(30);
        syscall(SYS_tgkill, getpid(), worker_tid, SIGCHLD);
        sleep_ms(50);
        if (is("twice")) {
            syscall(SYS_tgkill, getpid(), worker_tid, SIGCHLD);
            sleep_ms(50);
        }
        int before = atomic_load(&handled);
        if (write(fds[1], "x", 1) != 1)
            return 2;
        pthread_join(thread, NULL);
        printf("%s: %ld %s, handled before byte %d, after %d\n", call, result, errno_word(err), before,
               atomic_load(&handled));
    } else {
        return 2;
    }
    return 0;
}

/* `pending`: see the header. */
static int pending_family(void)
{
    int sig = !strcmp(call, "chld") ? SIGCHLD : SIGUSR1;
    if (pipe(fds))
        return 2;
    struct sigaction action;
    memset(&action, 0, sizeof action);
    action.sa_handler = on_chld;
    sigaction(sig, &action, NULL);
    sigset_t blocked, none;
    sigemptyset(&blocked);
    sigaddset(&blocked, sig);
    sigprocmask(SIG_BLOCK, &blocked, NULL);
    raise(sig);
    int ep = epoll_create1(0);
    struct epoll_event e = {.events = EPOLLIN, .data.fd = fds[0]};
    epoll_ctl(ep, EPOLL_CTL_ADD, fds[0], &e);
    sigemptyset(&none);
    result = epoll_pwait(ep, &e, 1, -1, &none);
    err = result < 0 ? errno : 0;
    printf("pending %s: %ld %s, handled %d\n", call, result, errno_word(err), atomic_load(&handled));
    return 0;
}

int main(int argc, char **argv)
{
    int short_form = argc >= 2 && (!strcmp(argv[1], "pending") || !strcmp(argv[1], "edge"));
    if (argc < 3) {
        fprintf(stderr, "usage: %s thread|fork|edge|pending CALL [VARIANT]\n", argv[0]);
        return 2;
    }
    if (argc != 4 && !(argc == 3 && short_form)) {
        fprintf(stderr, "usage: %s thread|fork|edge|pending CALL [VARIANT]\n", argv[0]);
        return 2;
    }
    call = argv[2];
    variant = argc == 4 ? argv[3] : "";
    setvbuf(stdout, NULL, _IONBF, 0);
    if (!strcmp(argv[1], "thread"))
        return thread_family();
    if (!strcmp(argv[1], "fork"))
        return fork_family();
    if (!strcmp(argv[1], "pending"))
        return pending_family();
    if (!strcmp(argv[1], "edge"))
        return edge_family();
    fprintf(stderr, "unknown family %s\n", argv[1]);
    return 2;
}
