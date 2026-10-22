// Prints the task IDs a guest is given: a child that forks a grandchild, a
// second fork, a thread that starts a nested thread, and two forks after exec.
// Under Hermit every backend must report what the ptrace backend does.
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static void *nested(void *arg) {
    (void)arg;
    printf("nested %ld\n", (long)syscall(SYS_gettid));
    return NULL;
}

static void *thread(void *arg) {
    (void)arg;
    printf("thread %ld\n", (long)syscall(SYS_gettid));
    pthread_t t;
    pthread_create(&t, NULL, nested, NULL);
    pthread_join(t, NULL);
    return NULL;
}

static void fork_once(const char *label, int grandchild) {
    fflush(stdout);
    pid_t child = fork();
    if (child == 0) {
        if (grandchild) {
            pid_t inner = fork();
            if (inner == 0) {
                _exit(0);
            }
            waitpid(inner, NULL, 0);
            printf("grandchild %d\n", inner);
            fflush(stdout);
        }
        _exit(0);
    }
    waitpid(child, NULL, 0);
    printf("%s %d\n", label, child);
}

int main(int argc, char **argv) {
    if (argc > 1 && strcmp(argv[1], "after-exec") == 0) {
        fork_once("fork-after-exec", 0);
        fork_once("fork-after-exec", 0);
        return 0;
    }
    printf("root %d\n", getpid());
    fork_once("fork", 1);
    fork_once("fork", 0);
    pthread_t t;
    pthread_create(&t, NULL, thread, NULL);
    pthread_join(t, NULL);
    fflush(stdout);
    char *next[] = {argv[0], "after-exec", NULL};
    execv(argv[0], next);
    return 99;
}
