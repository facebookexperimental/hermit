/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE
#include <inttypes.h>
#include <setjmp.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/auxv.h>
#include <sys/syscall.h>

#if !defined(__linux__) || !defined(__x86_64__) || !defined(__GLIBC__)
#error "This probe requires native x86_64 Linux GNU libc startup"
#endif

struct guards {
    uint64_t canary;
    uint64_t pointer_guard;
    unsigned char at_random[16];
};

static struct guards constructor_guards;
static volatile unsigned constructor_calls;
static unsigned constructor_checksum;
static int constructor_valid;
static volatile unsigned atexit_calls;

/* With -fstack-protector-all these real frames check the loader's canary on
 * return. The volatile array also keeps the protected work observable. */
__attribute__((noinline)) static unsigned protected_checksum(unsigned tag) {
    volatile unsigned char bytes[32];
    unsigned sum = 0;
    for (unsigned i = 0; i < sizeof(bytes); ++i) {
        bytes[i] = (unsigned char)(tag + i);
    }
    for (unsigned i = 0; i < sizeof(bytes); ++i) {
        sum += bytes[i];
    }
    return sum;
}

static int snapshot(struct guards *result) {
    __asm__ volatile("movq %%fs:0x28, %0" : "=r"(result->canary));
    __asm__ volatile("movq %%fs:0x30, %0" : "=r"(result->pointer_guard));
    const unsigned char *random = (const unsigned char *)getauxval(AT_RANDOM);
    if (random == NULL) {
        return 0;
    }
    for (unsigned i = 0; i < sizeof(result->at_random); ++i) {
        result->at_random[i] = random[i];
    }
    return 1;
}

/* Snapshot only: no printing, allocation or entropy request in this phase. */
__attribute__((constructor)) static void observe_constructor(void) {
    ++constructor_calls;
    constructor_valid = snapshot(&constructor_guards);
    constructor_checksum = protected_checksum(constructor_calls);
}

static void print_guards(const struct guards *value) {
    printf("canary=%016" PRIx64 " pointer_guard=%016" PRIx64 " at_random=",
           value->canary, value->pointer_guard);
    for (unsigned i = 0; i < sizeof(value->at_random); ++i) {
        printf("%02x", (unsigned)value->at_random[i]);
    }
    putchar('\n');
}

/* Two explicit guest calls before any stdio or atexit registration can cause
 * libc allocation. Do not use the libc syscall wrapper or retry a short fill. */
static long raw_getrandom(unsigned char bytes[32]) {
    long result;
    __asm__ volatile("syscall"
                     : "=a"(result)
                     : "0"((long)SYS_getrandom), "D"(bytes), "S"(32UL), "d"(0UL)
                     : "rcx", "r11", "memory");
    return result;
}

__attribute__((noinline, noreturn)) static void jump_back(jmp_buf environment) {
    longjmp(environment, 7);
}

static void observe_atexit(void) {
    struct guards value;
    ++atexit_calls;
    if (!snapshot(&value)) {
        _Exit(12);
    }
    unsigned checksum = protected_checksum(2 + atexit_calls);
    fputs("atexit ", stdout);
    print_guards(&value);
    printf("complete protected_atexit=%u atexit_calls=%u\n", checksum,
           (unsigned)atexit_calls);
    if (fflush(stdout) != 0) {
        _Exit(13);
    }
}

int main(void) {
    struct guards main_guards;
    if (!constructor_valid || !snapshot(&main_guards)) {
        return 2;
    }
    unsigned char first[32], second[32];
    if (raw_getrandom(first) != 32 || raw_getrandom(second) != 32) {
        return 4;
    }
    unsigned main_checksum = protected_checksum(1 + constructor_calls);
    jmp_buf environment;
    volatile unsigned jumps = 0;
    int returned = setjmp(environment);
    if (returned == 0) {
        ++jumps;
        jump_back(environment);
    }
    if (returned != 7 || jumps != 1 || atexit(observe_atexit) != 0) {
        return 5;
    }
    fputs("constructor ", stdout);
    print_guards(&constructor_guards);
    /* Preserve the original unprefixed main guard record. */
    print_guards(&main_guards);
    fputs("stream first=", stdout);
    for (unsigned i = 0; i < sizeof(first); ++i) {
        printf("%02x", (unsigned)first[i]);
    }
    fputs(" second=", stdout);
    for (unsigned i = 0; i < sizeof(second); ++i) {
        printf("%02x", (unsigned)second[i]);
    }
    putchar('\n');
    printf("lifecycle constructor_calls=%u protected_ctor=%u protected_main=%u "
           "setjmp_return=%d longjmp_count=%u\n",
           (unsigned)constructor_calls, constructor_checksum, main_checksum,
           returned, (unsigned)jumps);
    /* Return normally: the parent requires the atexit observations too. */
    return ferror(stdout) ? 3 : 0;
}
