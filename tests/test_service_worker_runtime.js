#!/usr/bin/env node
"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

function loadWorker({ withBadgeApi = true } = {}) {
  const handlers = {};
  const calls = {
    fetch: 0,
    cacheOpen: 0,
    cacheMatch: 0,
    cachePut: 0,
    notifications: [],
    opened: [],
    badgesSet: 0,
  };
  let fetchImpl = async () => ({ ok: true, clone() { return this; } });
  let lastWaitUntil = null;

  const cache = {
    async addAll() {},
    async put() { calls.cachePut += 1; },
    async keys() { return []; },
  };
  const caches = {
    async open() { calls.cacheOpen += 1; return cache; },
    async match() { calls.cacheMatch += 1; return { offline: true }; },
    async keys() { return []; },
    async delete() { return true; },
  };
  const self = {
    location: { origin: "https://netfyr.test" },
    navigator: withBadgeApi ? {
      async setAppBadge() { calls.badgesSet += 1; },
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
    cacheOpen: 0,
    cacheMatch: 0,
    cachePut: 0,
    notifications: [],
    opened: [],
    badgesSet: 0,
  });

  const staticResponse = worker.dispatch("https://netfyr.test/style.css");
  assert.ok(staticResponse, "allowlisted static file must use respondWith");
  await staticResponse;
  assert.ok(worker.lastWaitUntil, "cache update must extend the fetch event lifetime");
  await worker.lastWaitUntil;
  assert.equal(worker.calls.fetch, 1);
  assert.equal(worker.calls.cachePut, 1);

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
  assert.equal(worker.calls.badgesSet, 1, "push must set the installed app badge");

  // Upp-larm visas som UPP.
  await worker.dispatchPush({ device: "switch-1", status: "up", message: "", address: "10.0.0.6" });
  assert.equal(worker.calls.notifications[1].title, "switch-1 — UPP");
  assert.equal(worker.calls.notifications[1].options.body, "10.0.0.6");

  // Trasig payload får aldrig svälja larmet tyst — generisk notis.
  await worker.dispatchBrokenPush();
  assert.equal(worker.calls.notifications.length, 3);
  assert.equal(worker.calls.notifications[2].title, "NetFyr — NER");

  // Äldre webbläsare utan Badging API ska fortfarande få själva notisen.
  const noBadgeWorker = loadWorker({ withBadgeApi: false });
  await noBadgeWorker.dispatchPush({ device: "legacy", status: "down", message: "larm" });
  assert.equal(noBadgeWorker.calls.notifications.length, 1);
  assert.equal(noBadgeWorker.calls.badgesSet, 0);

  // Klick på notisen öppnar appen när inget fönster finns.
  await worker.dispatchClick({ data: { url: "/" } });
  assert.deepEqual(worker.calls.opened, ["/"]);

  console.log("service worker runtime: bypass + allowlist + offline fallback + push passed");
}

main().catch((err) => {
  console.error(err.stack || err);
  process.exit(1);
});
