"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

const source = fs.readFileSync(path.join(__dirname, "..", "web", "session-activity.js"), "utf8");

function harness(role) {
  let now = 0;
  let nextId = 1;
  const timers = new Map();
  const calls = [];
  const context = vm.createContext({
    globalThis: {},
    Promise,
    Date,
    console,
  });
  vm.runInContext(source, context, { filename: "session-activity.js" });
  const activity = context.globalThis.NetFyrSessionActivity.create({
    getRole: () => role,
    touch: () => { calls.push("touch"); },
    logout: () => { calls.push("logout"); },
    confirmStay: () => { calls.push("confirm"); return false; },
    now: () => now,
    setTimer(fn, delay) {
      const id = nextId++;
      timers.set(id, { fn, at: now + delay });
      return id;
    },
    clearTimer(id) { timers.delete(id); },
  });
  function advance(ms) {
    now += ms;
    let due;
    do {
      due = [...timers.entries()].find(([, timer]) => timer.at <= now);
      if (due) {
        timers.delete(due[0]);
        due[1].fn();
      }
    } while (due);
  }
  return { activity, calls, advance };
}

{
  const h = harness(null);
  h.activity.start();
  assert.equal(h.activity.recordHumanActivity(), false,
    "en oinloggad flik ska inte få publicera sessionsaktivitet");
  assert.deepEqual(h.calls, []);
}

{
  const h = harness("admin");
  h.activity.start();
  h.activity.recordHumanActivity();
  h.advance(4_999);
  h.activity.recordHumanActivity();
  assert.deepEqual(h.calls, ["touch"], "adminaktivitet ska strypas inom fem sekunder");
  h.advance(1);
  h.activity.recordHumanActivity();
  assert.deepEqual(h.calls, ["touch", "touch"],
    "adminaktivitet ska synkas minst var femte sekund under användning");
}

{
  const h = harness("admin");
  h.activity.start({ adminIdleMs: 10 * 60 * 1000 });
  h.advance(9 * 60 * 1000);
  assert.deepEqual(h.calls, ["confirm", "logout"],
    "adminvarningen ska följa serverns konfigurerade idle-tid");
}

{
  const h = harness("admin");
  h.activity.start();
  h.advance(14 * 60 * 1000);
  assert.deepEqual(h.calls, ["confirm", "logout"],
    "admin ska varnas efter 14 minuter och kunna välja utloggning");
}

{
  const h = harness("admin");
  h.activity.start();
  h.advance(10 * 60 * 1000);
  h.activity.recordSharedActivity();
  h.advance(4 * 60 * 1000);
  assert.deepEqual(h.calls, [], "aktivitet i en annan flik ska skjuta fram varningen");
  h.advance(10 * 60 * 1000);
  assert.deepEqual(h.calls, ["confirm", "logout"],
    "varningen ska räknas från den senast delade flikaktiviteten");
}

{
  const h = harness("user");
  h.activity.start();
  h.advance(24 * 60 * 60 * 1000);
  assert.deepEqual(h.calls, [], "operatören ska inte få inaktivitetsvarning");
  h.activity.recordHumanActivity();
  assert.deepEqual(h.calls, ["touch"], "mänsklig aktivitet ska förnya operatörens session");
}

console.log("session activity: admin idle warning and operator continuity passed");
