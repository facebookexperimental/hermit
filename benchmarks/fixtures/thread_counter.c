#include <inttypes.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

struct worker_args {
    uint64_t iterations;
};

static atomic_uint_fast64_t counter;

static void *increment_counter(void *raw_args) {
    const struct worker_args *args = raw_args;

    for (uint64_t i = 0; i < args->iterations; ++i) {
        atomic_fetch_add_explicit(&counter, 1, memory_order_relaxed);
    }
    return NULL;
}

static uint64_t parse_positive(const char *value, const char *name) {
    char *end = NULL;
    const unsigned long long parsed = strtoull(value, &end, 10);

    if (value[0] == '\0' || *end != '\0' || parsed == 0) {
        fprintf(stderr, "%s must be a positive integer\n", name);
        exit(2);
    }
    return (uint64_t)parsed;
}

int main(int argc, char **argv) {
    if (argc != 3) {
        fprintf(stderr, "usage: %s THREADS ITERATIONS_PER_THREAD\n", argv[0]);
        return 2;
    }

    const uint64_t thread_count = parse_positive(argv[1], "threads");
    const uint64_t iterations = parse_positive(argv[2], "iterations");
    if (thread_count > 256 || iterations > UINT64_MAX / thread_count) {
        fprintf(stderr, "counter configuration is too large\n");
        return 2;
    }

    pthread_t *threads = calloc((size_t)thread_count, sizeof(*threads));
    if (threads == NULL) {
        perror("calloc");
        return 1;
    }

    const struct worker_args args = {.iterations = iterations};
    for (uint64_t i = 0; i < thread_count; ++i) {
        const int error = pthread_create(&threads[i], NULL, increment_counter, (void *)&args);
        if (error != 0) {
            fprintf(stderr, "pthread_create failed: %d\n", error);
            free(threads);
            return 1;
        }
    }

    for (uint64_t i = 0; i < thread_count; ++i) {
        const int error = pthread_join(threads[i], NULL);
        if (error != 0) {
            fprintf(stderr, "pthread_join failed: %d\n", error);
            free(threads);
            return 1;
        }
    }
    free(threads);

    const uint_fast64_t actual = atomic_load_explicit(&counter, memory_order_relaxed);
    const uint64_t expected = thread_count * iterations;
    if (actual != expected) {
        fprintf(stderr, "counter mismatch: expected %" PRIu64 ", got %" PRIuFAST64 "\n",
                expected, actual);
        return 1;
    }

    printf("%" PRIuFAST64 "\n", actual);
    return 0;
}
