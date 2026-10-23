'use strict';

const {
  Worker,
  isMainThread,
  parentPort,
  workerData,
} = require('worker_threads');

const workerCount = 4;

if (!isMainThread) {
  const state = new Int32Array(workerData);
  parentPort.postMessage('ready');
  const result = Atomics.wait(state, 0, 0, 30_000);
  if (result !== 'ok' && result !== 'not-equal') {
    throw new Error(`unexpected Atomics.wait result: ${result}`);
  }
  Atomics.add(state, 1, 1);
  parentPort.postMessage('done');
} else {
  const shared = new SharedArrayBuffer(Int32Array.BYTES_PER_ELEMENT * 2);
  const state = new Int32Array(shared);
  const workers = [];
  let ready = 0;
  let done = 0;

  for (let i = 0; i < workerCount; i += 1) {
    const worker = new Worker(__filename, {workerData: shared});
    workers.push(worker);
    worker.on('error', (error) => {
      throw error;
    });
    worker.on('message', (message) => {
      if (message === 'ready') {
        ready += 1;
        if (ready === workerCount) {
          Atomics.store(state, 0, 1);
          const woken = Atomics.notify(state, 0, workerCount);
          if (woken !== workerCount) {
            throw new Error(`expected ${workerCount} wakees, got ${woken}`);
          }
        }
      } else if (message === 'done') {
        done += 1;
        if (done === workerCount) {
          if (Atomics.load(state, 1) !== workerCount) {
            throw new Error('worker completion count mismatch');
          }
          console.log(`SHARED_FUTEX_NODE_OK workers=${workerCount}`);
        }
      }
    });
  }
}
