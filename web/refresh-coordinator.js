// Single-flight-koordinator för NetFyrs operativa refresh-svep.
// Browsern får RefreshCoordinator globalt; Node får CommonJS-export för
// deterministiska tester utan frontendramverk eller byggsteg.
(function exposeRefreshCoordinator(root, factory) {
  const api = factory();
  if (typeof module === "object" && module.exports) module.exports = api;
  else root.RefreshCoordinator = api;
})(typeof globalThis !== "undefined" ? globalThis : self, () => {
  function createRefreshCoordinator(task, { timeoutMs = 8000 } = {}) {
    if (typeof task !== "function") throw new TypeError("task must be a function");
    if (!Number.isFinite(timeoutMs) || timeoutMs <= 0) throw new TypeError("timeoutMs must be positive");

    let inFlight = null;
    let pending = false;
    let controller = null;

    function run() {
      if (inFlight) {
        pending = true;
        return inFlight;
      }

      const current = new AbortController();
      controller = current;
      let timeoutId;
      const timeoutError = new DOMException("Refresh timed out", "TimeoutError");
      const timeout = new Promise((_, reject) => {
        timeoutId = setTimeout(() => {
          reject(timeoutError);
          current.abort(timeoutError);
        }, timeoutMs);
      });

      let taskPromise;
      try {
        // Starta tasken synkront så att den hinner observera signalen innan
        // en omedelbar logout/abort kan inträffa.
        taskPromise = Promise.resolve(task(current.signal));
      } catch (err) {
        taskPromise = Promise.reject(err);
      }
      inFlight = Promise.race([taskPromise, timeout]).finally(() => {
        clearTimeout(timeoutId);
        if (controller === current) controller = null;
        inFlight = null;

        if (pending) {
          pending = false;
          // En eller flera triggers under svepet kollapsar till exakt ett nytt
          // svep. Tasken i appen hanterar sina egna fel; catch skyddar även
          // generisk användning från ett oobserverat köat promise.
          queueMicrotask(() => { run().catch(() => {}); });
        }
      });

      return inFlight;
    }

    function abort() {
      pending = false;
      controller?.abort(new DOMException("Refresh cancelled", "AbortError"));
    }

    return { run, abort };
  }

  return { createRefreshCoordinator };
});
