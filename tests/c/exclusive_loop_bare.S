/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// aarch64, no C library: adds 40000 times to a counter on the stack through
// a load/store-exclusive retry loop (LDAXR, ADD, STLXR, CBNZ), the shape
// `--rewrite-exclusives` replaces with one LSE atomic, prints
// "counter 40000\n" and exits 0 only when the count is exact.
//
// Built with -nostdlib -static, it holds no ADRP or ADR anywhere: the
// counter and the output live on the stack and the text is made from
// immediates. So the rewrite's image-wide reader scan finds nothing that
// could read the loop's bytes and may rewrite it, where an ordinary
// C-library program's start files hold address computations the scan
// cannot follow (`literal-inside-the-sequence`).

#define INCREMENTS 40000

	.text
	.global	_start
	.type	_start, %function
_start:
	sub	sp, sp, #64
	str	wzr, [sp]
	mov	w19, #INCREMENTS
1:	mov	x0, sp
	bl	add_one
	subs	w19, w19, #1
	b.ne	1b

	// The decimal count backwards from sp+48, which holds the newline,
	// then "counter " (x9) just before its digits.
	movz	x9, #0x6f63
	movk	x9, #0x6e75, lsl #16
	movk	x9, #0x6574, lsl #32
	movk	x9, #0x2072, lsl #48
	ldr	w20, [sp]
	add	x3, sp, #48
	mov	w2, #10
	strb	w2, [x3]
	mov	w4, w20
2:	udiv	w5, w4, w2
	msub	w6, w5, w2, w4
	add	w6, w6, #48		// '0'
	sub	x3, x3, #1
	strb	w6, [x3]
	mov	w4, w5
	cbnz	w4, 2b
	sub	x3, x3, #8
	str	x9, [x3]

	mov	x0, #1
	mov	x1, x3
	add	x2, sp, #49
	sub	x2, x2, x3
	mov	x8, #64			// write
	svc	#0

	mov	w0, #INCREMENTS
	cmp	w20, w0
	cset	w0, ne
	mov	x8, #94			// exit_group
	svc	#0

// Not inlined into the caller's loop, and aligned to 16 bytes, so its four
// instructions are in the image once and never straddle a page (where the
// rewrite leaves a sequence by design).
	.balign	16
	.type	add_one, %function
add_one:
3:	ldaxr	w1, [x0]
	add	w1, w1, #1
	stlxr	w2, w1, [x0]
	cbnz	w2, 3b
	ret
	.size	add_one, . - add_one
	.size	_start, add_one - _start

	.section .note.GNU-stack, "", %progbits
