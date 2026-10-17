/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Unix-socket autobind for the stream, datagram and seqpacket socket types,
 * each in a fresh child process (see probe_family.h). This one program
 * replaces the c-programs/unix-autobind-{stream,dgram,seqpacket} rows; every
 * check of run_unix_autobind_probe still runs for every type. A unix
 * autobind takes a port from Detcore's container-wide allocator (the state
 * probe_family.h warns about), but each member binds one socket and closes
 * it, which releases the port, and the allocator hands the same port out
 * next, so each child sees the name a fresh run would (08000 under Hermit
 * on 2026-10-17, ptrace and KVM). No check depends on the name.
 */

#include "probe_family.h"
#include "unix_autobind_common.h"

static int stream(void) {
  return run_unix_autobind_probe(SOCK_STREAM, "stream");
}

static int dgram(void) {
  return run_unix_autobind_probe(SOCK_DGRAM, "dgram");
}

static int seqpacket(void) {
  return run_unix_autobind_probe(SOCK_SEQPACKET, "seqpacket");
}

int main(void) {
  const probe_member members[] = {stream, dgram, seqpacket};
  return run_probe_family(members, sizeof(members) / sizeof(members[0]));
}
