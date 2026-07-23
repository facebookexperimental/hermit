/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * NONDET_SOURCE: OpenMP dynamic scheduling changes the floating-point
 * reduction order without changing the input values.
 */

#include <omp.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#define ELEMENTS 65536

static uint32_t mix(uint32_t value) {
    value ^= value >> 16;
    value *= 0x7feb352du;
    value ^= value >> 15;
    value *= 0x846ca68bu;
    return value ^ (value >> 16);
}

/*
 * Give iterations deterministic but unequal costs. Native OpenMP workers then
 * acquire dynamic chunks in a timing-dependent order. Hermit makes the same
 * thread schedule repeatable.
 */
static void variable_work(uint32_t index) {
    uint32_t state = index * 747796405u + 2891336453u;
    unsigned int rounds = (state >> 22) + 32u;

    for (unsigned int i = 0; i < rounds; ++i) {
        state = state * 1664525u + 1013904223u;
        __asm__ volatile("" : "+r"(state));
    }
}

int main(void) {
    float sum = 0.0f;

    omp_set_dynamic(0);
    omp_set_num_threads(4);

#pragma omp parallel for schedule(dynamic, 1) reduction(+ : sum)
    for (uint32_t i = 0; i < ELEMENTS; ++i) {
        variable_work(i);

        /*
         * Every value is positive, so differences come from rounding order
         * rather than cancellation or changes to the input.
         */
        float value =
            (float)((mix(i) & 0xffffu) + 1u) * (1.0f / 65536.0f);
        sum += value;
    }

    uint32_t bits;
    memcpy(&bits, &sum, sizeof(bits));
    printf("threads=%d bits=%08x sum=%a\n", omp_get_max_threads(), bits,
           (double)sum);
    return 0;
}
