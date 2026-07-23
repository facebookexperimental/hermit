#include <pthread.h>
#include <stdbool.h>
#include <stdio.h>

enum { CAPACITY = 8, ITEMS = 100 };

struct queue {
    int items[CAPACITY];
    int head;
    int tail;
    int count;
    bool done;
    pthread_mutex_t mutex;
    pthread_cond_t not_empty;
    pthread_cond_t not_full;
};

static struct queue queue = {
    .mutex = PTHREAD_MUTEX_INITIALIZER,
    .not_empty = PTHREAD_COND_INITIALIZER,
    .not_full = PTHREAD_COND_INITIALIZER,
};

static void *produce(void *unused) {
    (void)unused;
    for (int item = 1; item <= ITEMS; item++) {
        pthread_mutex_lock(&queue.mutex);
        while (queue.count == CAPACITY) {
            pthread_cond_wait(&queue.not_full, &queue.mutex);
        }
        queue.items[queue.tail] = item;
        queue.tail = (queue.tail + 1) % CAPACITY;
        queue.count++;
        pthread_cond_signal(&queue.not_empty);
        pthread_mutex_unlock(&queue.mutex);
    }

    pthread_mutex_lock(&queue.mutex);
    queue.done = true;
    pthread_cond_signal(&queue.not_empty);
    pthread_mutex_unlock(&queue.mutex);
    return NULL;
}

static void *consume(void *result) {
    int *sum = result;
    for (;;) {
        pthread_mutex_lock(&queue.mutex);
        while (queue.count == 0 && !queue.done) {
            pthread_cond_wait(&queue.not_empty, &queue.mutex);
        }
        if (queue.count == 0 && queue.done) {
            pthread_mutex_unlock(&queue.mutex);
            return NULL;
        }
        *sum += queue.items[queue.head];
        queue.head = (queue.head + 1) % CAPACITY;
        queue.count--;
        pthread_cond_signal(&queue.not_full);
        pthread_mutex_unlock(&queue.mutex);
    }
}

int main(void) {
    pthread_t producer;
    pthread_t consumer;
    int sum = 0;

    if (pthread_create(&producer, NULL, produce, NULL) != 0 ||
        pthread_create(&consumer, NULL, consume, &sum) != 0) {
        return 2;
    }
    if (pthread_join(producer, NULL) != 0 || pthread_join(consumer, NULL) != 0) {
        return 3;
    }

    printf("items=%d sum=%d\n", ITEMS, sum);
    return sum == ITEMS * (ITEMS + 1) / 2 ? 0 : 4;
}
