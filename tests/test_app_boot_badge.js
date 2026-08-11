#!/usr/bin/env node
"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

function extractFunction(source, name) {
  const start = source.indexOf(`async function ${name}(`);
  assert.notEqual(start, -1, `${name} must exist`);
  const bodyStart = source.indexOf("{", start);
  let depth = 0;
  for (let i = bodyStart; i < source.length; i += 1) {
    if (source[i] === "{") depth += 1;
    if (source[i] === "}") {
      depth -= 1;
      if (depth === 0) return source.slice(start, i + 1);
    }
  }
  throw new Error(`unterminated function ${name}`);
}

async function main() {
  const source = fs.readFileSync(path.join(__dirname, "..", "web", "app.js"), "utf8");
  const boot = extractFunction(source, "boot");
  const calls = [];
  const context = vm.createContext({
    calls,
    me: null,
    api: { async get(pathname) { calls.push(`get:${pathname}`); throw new Error("expired"); } },
    clearInstalledAppBadge() { calls.push("clearBadge"); },
    showLogin() { calls.push("showLogin"); },
    hideLogin() { calls.push("hideLogin"); },
    startApp() { calls.push("startApp"); },
  });
  vm.runInContext(`${boot}; pending = boot();`, context);
  await context.pending;
  assert.deepEqual(calls, ["clearBadge", "get:/auth/me", "showLogin"],
    "expired session must still clear badge before showing login");
  console.log("app boot badge reset: expired session passed");
}

main().catch((err) => {
  console.error(err.stack || err);
  process.exit(1);
});
