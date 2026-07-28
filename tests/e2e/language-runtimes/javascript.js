#!/usr/bin/env node
/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

"use strict";

const crypto = require("crypto");
const fs = require("fs");
const os = require("os");
const {Worker, isMainThread, workerData} = require("worker_threads");

const THREADS = 4;
const ITERATIONS = 64;
const COUNTER = 0;
const NEXT_RECORD = 1;
const START = 2;
const LOCK = 3;
const RECORDS = 4;

if (!isMainThread) {
  const state = new Int32Array(workerData.buffer);
  while (Atomics.load(state, START) === 0) {
    Atomics.wait(state, START, 0);
  }
  for (let iteration = 0; iteration < ITERATIONS; iteration += 1) {
    while (Atomics.compareExchange(state, LOCK, 0, 1) !== 0) {
      Atomics.wait(state, LOCK, 1);
    }
    Atomics.add(state, COUNTER, 1);
    const record = Atomics.add(state, NEXT_RECORD, 1);
    state[RECORDS + record] = workerData.id;
    Atomics.store(state, LOCK, 0);
    Atomics.notify(state, LOCK, 1);
  }
} else {
  function randomProbe() {
    const token = crypto.randomBytes(16).toString("hex");
    const value = Math.floor(Math.random() * Number.MAX_SAFE_INTEGER);
    console.log(`RANDOM token=${token} prng=${value}`);
  }

  function timeProbe() {
    process.env.TZ = "UTC";
    const now = new Date();
    const minute = Math.floor(now.getTime() / 60_000);
    const bucketed = new Date(minute * 60_000);
    const utc = new Intl.DateTimeFormat("en-US", {
      timeZone: "UTC",
      dateStyle: "full",
      timeStyle: "long",
    }).format(bucketed);
    console.log(`TIME unix_minute=${minute} utc=${bucketed.toISOString()} zone=${utc}`);
  }

  async function threadProbe() {
    const entries = RECORDS + THREADS * ITERATIONS;
    const shared = new SharedArrayBuffer(Int32Array.BYTES_PER_ELEMENT * entries);
    const state = new Int32Array(shared);
    let online = 0;
    const workers = [];
    const completions = [];

    for (let id = 0; id < THREADS; id += 1) {
      const worker = new Worker(__filename, {workerData: {buffer: shared, id}});
      workers.push(worker);
      completions.push(new Promise((resolve, reject) => {
        worker.once("error", reject);
        worker.once("exit", (code) => {
          if (code === 0) {
            resolve();
          } else {
            reject(new Error(`worker ${id} exited with status ${code}`));
          }
        });
      }));
      worker.once("online", () => {
        online += 1;
        if (online === THREADS) {
          Atomics.store(state, START, 1);
          Atomics.notify(state, START, THREADS);
        }
      });
    }

    await Promise.all(completions);
    const expected = THREADS * ITERATIONS;
    if (state[COUNTER] !== expected || state[NEXT_RECORD] !== expected) {
      throw new Error(`thread counter mismatch: ${state[COUNTER]} != ${expected}`);
    }
    const schedule = Buffer.from(state.slice(RECORDS, RECORDS + expected));
    const digest = crypto.createHash("sha256").update(schedule).digest("hex");
    console.log(`THREAD workers=${THREADS} counter=${state[COUNTER]} schedule_sha256=${digest}`);
  }

  function systemProbe() {
    const sentinel = process.env.HERMIT_RUNTIME_SENTINEL;
    const status = Object.fromEntries(
      fs.readFileSync("/proc/self/status", "utf8")
        .split("\n")
        .filter((line) => line.startsWith("Name:") || line.startsWith("Threads:"))
        .map((line) => line.split(/:\s*/, 2)),
    );
    const procHostname = fs.readFileSync("/proc/sys/kernel/hostname", "utf8").trim();
    console.log(
      `SYSTEM uname=${os.type()}/${os.arch()}/${os.hostname()} ` +
      `env=${sentinel} proc=${status.Name}/${status.Threads}/${procHostname}`,
    );
  }

  async function main() {
    randomProbe();
    timeProbe();
    await threadProbe();
    systemProbe();
  }

  main().catch((error) => {
    console.error(error.stack || error);
    process.exitCode = 1;
  });
}
