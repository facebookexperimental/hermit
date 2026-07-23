#include <pthread.h>
#include <stdint.h>
#include <stdio.h>

enum { THREADS = 4 };

static int results[THREADS];

static void *worker(void *argument) {
    intptr_t index = (intptr_t)argument;
    results[index] = (int)(index * index);
    return NULL;
}

int main(void) {
    pthread_t threads[THREADS];
    int sum = 0;

    for (intptr_t index = 0; index < THREADS; index++) {
        if (pthread_create(&threads[index], NULL, worker, (void *)index) != 0) {
            return 2;
        }
    }
    for (int index = 0; index < THREADS; index++) {
        if (pthread_join(threads[index], NULL) != 0) {
            return 3;
        }
        sum += results[index];
    }

    printf("threads=%d sum=%d\n", THREADS, sum);
    return 0;
}
