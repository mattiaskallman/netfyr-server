// Mänsklig sessionsaktivitet, skild från NetFyrs automatiska pollning.
// Modulen har inga DOM-beroenden och kan därför beteendetestas direkt.
(function (root) {
  "use strict";

  function create(options) {
    const defaultAdminIdleMs = options.adminIdleMs ?? 15 * 60 * 1000;
    let adminIdleMs = defaultAdminIdleMs;
    const warningBeforeMs = options.warningBeforeMs ?? 60 * 1000;
    const adminTouchMinIntervalMs = options.adminTouchMinIntervalMs ?? 5 * 1000;
    const operatorTouchMinIntervalMs = options.operatorTouchMinIntervalMs ?? 60 * 1000;
    const now = options.now ?? (() => Date.now());
    const setTimer = options.setTimer ?? ((fn, delay) => setTimeout(fn, delay));
    const clearTimer = options.clearTimer ?? ((id) => clearTimeout(id));

    let running = false;
    let warningTimer = null;
    let lastTouchAt = null;

    function clearWarning() {
      if (warningTimer !== null) clearTimer(warningTimer);
      warningTimer = null;
    }

    function scheduleAdminWarning() {
      clearWarning();
      if (!running || options.getRole() !== "admin") return;
      warningTimer = setTimer(() => {
        warningTimer = null;
        if (!running || options.getRole() !== "admin") return;
        if (options.confirmStay()) recordHumanActivity(true);
        else options.logout();
      }, Math.max(0, adminIdleMs - warningBeforeMs));
    }

    function sendTouch(force) {
      const timestamp = now();
      const touchMinIntervalMs = options.getRole() === "admin"
        ? adminTouchMinIntervalMs
        : operatorTouchMinIntervalMs;
      if (!force && lastTouchAt !== null && timestamp - lastTouchAt < touchMinIntervalMs) return;
      lastTouchAt = timestamp;
      Promise.resolve(options.touch()).catch(() => {});
    }

    function recordHumanActivity(force = false) {
      if (!running || !options.getRole()) return false;
      sendTouch(force);
      scheduleAdminWarning();
      return true;
    }

    function recordSharedActivity() {
      if (!running || !options.getRole()) return;
      scheduleAdminWarning();
    }

    function start(config = {}) {
      running = true;
      lastTouchAt = null;
      adminIdleMs = config.adminIdleMs ?? defaultAdminIdleMs;
      scheduleAdminWarning();
    }

    function stop() {
      running = false;
      lastTouchAt = null;
      clearWarning();
    }

    return { start, stop, recordHumanActivity, recordSharedActivity };
  }

  root.NetFyrSessionActivity = { create };
})(globalThis);
