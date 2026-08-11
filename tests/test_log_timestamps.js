#!/usr/bin/env node
"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

function extractFunction(source, name) {
  const start = source.indexOf(`function ${name}(`);
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

const source = fs.readFileSync(path.join(__dirname, "..", "web", "app.js"), "utf8");
const fn = extractFunction(source, "dateTime");
const localMs = new Date(2026, 7, 11, 18, 34, 5).getTime();
const context = vm.createContext({
  Date,
  I18N: { locale: () => "sv-SE" },
  input: localMs,
  result: null,
});
vm.runInContext(`${fn}; result = dateTime(input);`, context);
assert.equal(context.result, "2026-08-11 18:34:05",
  "historical timestamp must use exact YYYY-MM-DD HH:mm:ss format");

for (const call of ["dateTime(e.ts)", "dateTime(d.createdAt)", "dateTime(a.ts)"]) {
  assert.ok(source.includes(call), `${call} must be used for historical log rows`);
}

console.log("historical log timestamps: date + time passed");
