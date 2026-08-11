#!/usr/bin/env node
"use strict";

const assert = require("node:assert/strict");
const { createRefreshCoordinator } = require("../web/refresh-coordinator.js");

const tick = () => new Promise((resolve) => setImmediate(resolve));

async function testSingleFlightQueuesExactlyOneFollowUp() {
  const releases = [];
  let calls = 0;
  const coordinator = createRefreshCoordinator(
    () => new Promise((resolve) => {
      calls += 1;
      releases.push(resolve);
    }),
    { timeoutMs: 1000 },
  );

  const first = coordinator.run();
  const same = coordinator.run();
  assert.strictEqual(first, same, "overlapping refresh must share the in-flight promise");
  await tick();
  assert.equal(calls, 1, "only one refresh may execute at a time");

  releases.shift()();
  await first;
  await tick();
  assert.equal(calls, 2, "overlap must queue exactly one fresh sweep");

  releases.shift()();
  await tick();
  assert.equal(calls, 2, "queued triggers must collapse into one sweep");
}

async function testTimeoutAbortsHungSweep() {
  let aborted = false;
  const coordinator = createRefreshCoordinator(
    (signal) => new Promise((resolve, reject) => {
      signal.addEventListener("abort", () => {
        aborted = true;
        reject(new DOMException("aborted", "AbortError"));
      }, { once: true });
    }),
    { timeoutMs: 20 },
  );

  await assert.rejects(coordinator.run(), { name: "TimeoutError" });
  assert.equal(aborted, true, "hung refresh must be aborted at timeout");
}

async function testManualAbortDoesNotRunQueuedWork() {
  let calls = 0;
  const coordinator = createRefreshCoordinator(
    (signal) => new Promise((resolve, reject) => {
      calls += 1;
      signal.addEventListener("abort", () => reject(new DOMException("aborted", "AbortError")), { once: true });
    }),
    { timeoutMs: 1000 },
  );

  const first = coordinator.run();
  coordinator.run();
  coordinator.abort();
  await assert.rejects(first, { name: "AbortError" });
  await tick();
  assert.equal(calls, 1, "logout abort must discard queued refresh work");
}

(async () => {
  await testSingleFlightQueuesExactlyOneFollowUp();
  await testTimeoutAbortsHungSweep();
  await testManualAbortDoesNotRunQueuedWork();
  console.log("refresh coordinator: 3/3 passed");
})().catch((err) => {
  console.error(err.stack || err);
  process.exit(1);
});
