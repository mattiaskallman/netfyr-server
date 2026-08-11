#!/usr/bin/env node
"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

function loadWorker({
  withBadgeApi = true,
  persistentBadgeStore = {},
  badgeFailure = null,
  cacheKeys = [],
} = {}) {
  const handlers = {};
  const calls = {
    fetch: 0,
    cacheOpens: [],
    cacheMatch: 0,
    cachePut: 0,
    cacheDeletes: [],
    notifications: [],
    opened: [],
    badgeValues: [],
    badgeClears: 0,
  };
  let fetchImpl = async () => ({ ok: true, clone() { return this; } });
  let lastWaitUntil = null;
  const failBadgeOperation = (operation) => {
    if (!badgeFailure || badgeFailure.operation !== operation || badgeFailure.remaining === 0) return false;
    if (Number.isInteger(badgeFailure.remaining)) badgeFailure.remaining -= 1;
    return true;
  };

  class FakeResponse {
    constructor(body) { this.body = String(body); this.ok = true; }
    clone() { return new FakeResponse(this.body); }
    async text() { return this.body; }
  }
  const shellCache = {
    async addAll() {},
    async put() { calls.cachePut += 1; },
    async keys() { return []; },
  };
  const badgeCache = {
    async match() {
      if (failBadgeOperation("match")) throw new Error("badge match failed");
      return persistentBadgeStore.value == null
        ? undefined
        : new FakeResponse(persistentBadgeStore.value);
    },
    async put(_key, response) {
      if (failBadgeOperation("put")) throw new Error("badge put failed");
      persistentBadgeStore.value = await response.text();
    },
    async delete() { delete persistentBadgeStore.value; return true; },
  };
  const caches = {
    async open(name) {
      calls.cacheOpens.push(name);
      if (name === "netfyr-badge-state-v1" && failBadgeOperation("open")) {
        throw new Error("badge open failed");
      }
      return name === "netfyr-badge-state-v1" ? badgeCache : shellCache;
    },
    async match() { calls.cacheMatch += 1; return { offline: true }; },
    async keys() { return cacheKeys; },
    async delete(name) { calls.cacheDeletes.push(name); return true; },
  };
  const self = {
    location: { origin: "https://netfyr.test" },
    navigator: withBadgeApi ? {
      async setAppBadge(value) {
        if (failBadgeOperation("set")) throw new Error("setAppBadge failed");
        calls.badgeValues.push(value);
      },
      async clearAppBadge() { calls.badgeClears += 1; },
    } : {},
    clients: {
      async claim() {},
      async matchAll() { return []; },
      async openWindow(url) { calls.opened.push(url); return { url }; },
    },
    registration: {
      async showNotification(title, options) { calls.notifications.push({ title, options }); },
    },
    addEventListener(type, fn) { handlers[type] = fn; },
    skipWaiting() {},
  };
  const context = vm.createContext({
    self,
    caches,
    URL,
    Response: FakeResponse,
    Promise,
    console,
    fetch(request) { calls.fetch += 1; return fetchImpl(request); },
  });
  const source = fs.readFileSync(path.join(__dirname, "..", "web", "service-worker.js"), "utf8");
  vm.runInContext(source, context, { filename: "service-worker.js" });

  return {
    calls,
    get lastWaitUntil() { return lastWaitUntil; },
    setFetch(fn) { fetchImpl = fn; },
    async dispatchActivate() {
      let done = null;
      handlers.activate({ waitUntil(value) { done = Promise.resolve(value); } });
      await done;
    },
    dispatch(url, { method = "GET", mode = "cors" } = {}) {
      let responsePromise = null;
      lastWaitUntil = null;
      handlers.fetch({
        request: { url, method, mode },
        respondWith(value) { responsePromise = Promise.resolve(value); },
        waitUntil(value) { lastWaitUntil = Promise.resolve(value); },
      });
      return responsePromise;
    },
    async dispatchPush(payload) {
      let done = null;
      handlers.push({
        data: payload === null ? null : { json() { return payload; } },
        waitUntil(value) { done = Promise.resolve(value); },
      });
      await done;
    },
    async dispatchBrokenPush() {
      let done = null;
      handlers.push({
        data: { json() { throw new Error("not json"); } },
        waitUntil(value) { done = Promise.resolve(value); },
      });
      await done;
    },
    async dispatchMessage(data) {
      assert.ok(handlers.message, "worker must handle badge reset messages");
      let done = null;
      handlers.message({
        data,
        waitUntil(value) { done = Promise.resolve(value); },
      });
      await done;
    },
    async dispatchClick(notification) {
      let done = null;
      handlers.notificationclick({
        notification: Object.assign({ close() {} }, notification),
        waitUntil(value) { done = Promise.resolve(value); },
      });
      await done;
    },
  };
}

async function main() {
  const worker = loadWorker();
  const bypass = [
    ["https://netfyr.test/api", {}],
    ["https://netfyr.test/api/", {}],
    ["https://netfyr.test/api/overview?window=24h", {}],
    ["https://netfyr.test/style.css", { method: "POST" }],
    ["https://other.test/style.css", {}],
    ["https://netfyr.test/private-data.json", {}],
    ["https://netfyr.test/style.css?v=2", {}],
    ["https://netfyr.test/other-page", { mode: "navigate" }],
  ];
  for (const [url, options] of bypass) {
    assert.equal(worker.dispatch(url, options), null, `${url} must bypass respondWith`);
  }
  assert.deepEqual(worker.calls, {
    fetch: 0,
    cacheOpens: [],
    cacheMatch: 0,
    cachePut: 0,
    cacheDeletes: [],
    notifications: [],
    opened: [],
    badgeValues: [],
    badgeClears: 0,
  });

  const activationWorker = loadWorker({
    cacheKeys: ["netfyr-shell-v2", "netfyr-shell-v3", "netfyr-badge-state-v1"],
  });
  await activationWorker.dispatchActivate();
  assert.deepEqual(activationWorker.calls.cacheDeletes, ["netfyr-shell-v2"],
    "activation must preserve current shell and persistent badge state caches");

  const staticResponse = worker.dispatch("https://netfyr.test/style.css");
  assert.ok(staticResponse, "allowlisted static file must use respondWith");
  await staticResponse;
  assert.ok(worker.lastWaitUntil, "cache update must extend the fetch event lifetime");
  await worker.lastWaitUntil;
  assert.equal(worker.calls.fetch, 1);
  assert.equal(worker.calls.cachePut, 1);
  assert.deepEqual(worker.calls.cacheOpens, ["netfyr-shell-v3"]);

  worker.setFetch(async () => { throw new Error("offline"); });
  const navigation = worker.dispatch("https://netfyr.test/", { mode: "navigate" });
  assert.ok(navigation, "root navigation must use offline fallback");
  assert.deepEqual(await navigation, { offline: true });
  assert.equal(worker.calls.cacheMatch, 1);

  // Push: ett riktigt larmpaket blir en notis med enhet + status.
  await worker.dispatchPush({
    device: "brandlarm-server",
    status: "down",
    message: "svarar inte på ping",
    address: "10.0.0.5",
    time: "2026-08-08 12:00:00",
  });
  assert.equal(worker.calls.notifications.length, 1);
  assert.equal(worker.calls.notifications[0].title, "brandlarm-server — NER");
  assert.equal(worker.calls.notifications[0].options.body, "svarar inte på ping");
  assert.equal(worker.calls.notifications[0].options.data.url, "/");
  assert.deepEqual(worker.calls.badgeValues, [1], "first push must set badge count 1");

  // Upp-larm visas som UPP.
  await worker.dispatchPush({ device: "switch-1", status: "up", message: "", address: "10.0.0.6" });
  assert.equal(worker.calls.notifications[1].title, "switch-1 — UPP");
  assert.equal(worker.calls.notifications[1].options.body, "10.0.0.6");
  assert.deepEqual(worker.calls.badgeValues, [1, 2], "each unread push must increment the badge");

  // Trasig payload får aldrig svälja larmet tyst — generisk notis.
  await worker.dispatchBrokenPush();
  assert.equal(worker.calls.notifications.length, 3);
  assert.equal(worker.calls.notifications[2].title, "NetFyr — NER");

  // Äldre webbläsare utan Badging API ska fortfarande få själva notisen.
  const noBadgeWorker = loadWorker({ withBadgeApi: false });
  await noBadgeWorker.dispatchPush({ device: "legacy", status: "down", message: "larm" });
  assert.equal(noBadgeWorker.calls.notifications.length, 1);
  assert.deepEqual(noBadgeWorker.calls.badgeValues, []);

  // iOS får avsluta service workern mellan pushar. Antalet måste därför
  // överleva en ny worker-instans i persistent lagring.
  const persistentBadgeStore = {};
  const firstWorker = loadWorker({ persistentBadgeStore });
  await firstWorker.dispatchPush({ device: "persist-1", status: "down", message: "ett" });
  assert.deepEqual(firstWorker.calls.badgeValues, [1]);
  const restartedWorker = loadWorker({ persistentBadgeStore });
  await restartedWorker.dispatchPush({ device: "persist-2", status: "down", message: "två" });
  assert.deepEqual(restartedWorker.calls.badgeValues, [2], "badge count must survive worker restart");

  await restartedWorker.dispatchMessage({ type: "CLEAR_APP_BADGE" });
  assert.equal(restartedWorker.calls.badgeClears, 1);
  assert.equal(persistentBadgeStore.value, undefined, "badge reset must clear persistent count");
  const afterClearWorker = loadWorker({ persistentBadgeStore });
  await afterClearWorker.dispatchPush({ device: "persist-3", status: "down", message: "ny" });
  assert.deepEqual(afterClearWorker.calls.badgeValues, [1], "first push after app open must restart at 1");

  // Samtidiga push-event ska serialiseras utan lost update.
  const concurrentStore = {};
  const concurrentWorker = loadWorker({ persistentBadgeStore: concurrentStore });
  await Promise.all([
    concurrentWorker.dispatchPush({ device: "parallel-1", status: "down", message: "ett" }),
    concurrentWorker.dispatchPush({ device: "parallel-2", status: "down", message: "två" }),
  ]);
  assert.deepEqual(concurrentWorker.calls.badgeValues, [1, 2]);
  assert.equal(concurrentStore.value, "2");

  // Badge-API och dess lagring är best effort: inget sådant fel får stoppa
  // den obligatoriska systemnotisen eller lämna kön permanent avvisad.
  for (const operation of ["open", "match", "put", "set"]) {
    const failure = { operation, remaining: 1 };
    const resilientWorker = loadWorker({ badgeFailure: failure });
    await resilientWorker.dispatchPush({ device: `fail-${operation}`, status: "down", message: "larm" });
    assert.equal(resilientWorker.calls.notifications.length, 1, `${operation} failure must not suppress notification`);
    await resilientWorker.dispatchPush({ device: `recover-${operation}`, status: "down", message: "nytt" });
    assert.equal(resilientWorker.calls.notifications.length, 2, `${operation} failure must not poison queue`);
    assert.equal(resilientWorker.calls.badgeValues.length, 1, `${operation} queue must recover on next push`);
  }

  // Klick på notisen öppnar appen när inget fönster finns.
  await worker.dispatchClick({ data: { url: "/" } });
  assert.deepEqual(worker.calls.opened, ["/"]);

  console.log("service worker runtime: bypass + allowlist + offline fallback + push passed");
}

main().catch((err) => {
  console.error(err.stack || err);
  process.exit(1);
});
