#include <pthread.h>
#include <stdint.h>
#include <stdio.h>

static uint64_t results[4];

static void *worker(void *arg) {
    uintptr_t index = (uintptr_t)arg;
    uint64_t total = 0;
    for (uint64_t n = 0; n < 100000; n++) {
        total += n ^ index;
    }
    results[index] = total;
    return NULL;
}

int main(void) {
    pthread_t threads[4];
    uint64_t total = 0;
    for (uintptr_t i = 0; i < 4; i++) {
        if (pthread_create(&threads[i], NULL, worker, (void *)i) != 0) return 2;
    }
    for (int i = 0; i < 4; i++) {
        if (pthread_join(threads[i], NULL) != 0) return 3;
        total += results[i];
    }
    printf("gcc-ok %llu\n", (unsigned long long)total);
    return 0;
}
