// NetFyr PWA — cachar endast det statiska appskalet.
// API-anrop lämnas helt till webbläsarens nätverksstack: operativ status får
// aldrig ersättas med ett gammalt cachesvar som ser aktuellt ut.
const CACHE_NAME = "netfyr-shell-v4";
const BADGE_STATE_CACHE = "netfyr-badge-state-v1";
const BADGE_STATE_KEY = "/.netfyr-badge-state";
const SHELL = [
  "/",
  "/style.css",
  "/i18n.js",
  "/refresh-coordinator.js",
  "/session-activity.js",
  "/app.js",
  "/manifest.webmanifest",
  "/favicon-32.png",
  "/favicon.ico",
  "/apple-touch-icon.png",
  "/icon-192.png",
  "/icon-512.png",
  "/icon-maskable-512.png",
];

self.addEventListener("install", (event) => {
  event.waitUntil(caches.open(CACHE_NAME).then((cache) => cache.addAll(SHELL)));
  self.skipWaiting();
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    caches.keys().then((keys) => Promise.all(
      keys.filter((key) => key.startsWith("netfyr-shell-") && key !== CACHE_NAME)
        .map((key) => caches.delete(key)),
    )).then(() => self.clients.claim()),
  );
});

self.addEventListener("fetch", (event) => {
  const { request } = event;
  if (request.method !== "GET") return;

  const url = new URL(request.url);
  if (url.origin !== self.location.origin) return;
  if (url.pathname === "/api" || url.pathname.startsWith("/api/")) return;

  if (request.mode === "navigate") {
    if (url.pathname !== "/") return;
    event.respondWith(
      fetch(request).catch(() => caches.match("/")),
    );
    return;
  }

  // Endast versionskontrollerade appskalsfiler får gå via Cache Storage.
  // Querysträngar lämnas till nätverket för att undvika obegränsade cachekeys.
  if (url.search || !SHELL.includes(url.pathname)) return;

  const networkResponse = fetch(request);
  const cacheUpdate = networkResponse
    .then((response) => {
      if (!response.ok) return undefined;
      const copy = response.clone();
      return caches.open(CACHE_NAME).then((cache) => cache.put(request, copy));
    })
    .catch(() => undefined);

  event.waitUntil(cacheUpdate);
  event.respondWith(
    networkResponse.catch(() => caches.match(request)),
  );
});

// Web Push: visa larm som systemnotis även när appen är stängd.
// Payloaden är AlarmPayload-JSON från servern. Ett parse-fel får aldrig
// svälja larmet tyst — då visas en generisk notis så att användaren
// ändå uppmärksammar händelsen.
// Badgeuppdateringar serialiseras så att samtidiga push-event inte kan läsa
// samma gamla antal. Cache Storage bevarar räknaren när iOS avslutar workern.
let badgeUpdateQueue = Promise.resolve();

function incrementAppBadge() {
  if (typeof self.navigator?.setAppBadge !== "function") return Promise.resolve();

  const update = async () => {
    const cache = await caches.open(BADGE_STATE_CACHE);
    const stored = await cache.match(BADGE_STATE_KEY);
    const raw = stored ? Number(await stored.text()) : 0;
    const previous = Number.isSafeInteger(raw) && raw >= 0 ? raw : 0;
    const count = Math.min(previous + 1, Number.MAX_SAFE_INTEGER);
    await cache.put(BADGE_STATE_KEY, new Response(String(count)));
    await self.navigator.setAppBadge(count);
  };

  const task = badgeUpdateQueue.then(update, update);
  badgeUpdateQueue = task.catch(() => {});
  return task.catch(() => {});
}

function clearStoredAppBadge() {
  const reset = async () => {
    const cache = await caches.open(BADGE_STATE_CACHE);
    await cache.delete(BADGE_STATE_KEY);
    if (typeof self.navigator?.clearAppBadge === "function") {
      await self.navigator.clearAppBadge();
    }
  };

  const task = badgeUpdateQueue.then(reset, reset);
  badgeUpdateQueue = task.catch(() => {});
  return task.catch(() => {});
}

self.addEventListener("message", (event) => {
  if (event.data?.type !== "CLEAR_APP_BADGE") return;
  event.waitUntil(clearStoredAppBadge());
});

self.addEventListener("push", (event) => {
  let data = {};
  try {
    data = event.data ? event.data.json() : {};
  } catch {
    data = {};
  }
  const device = typeof data.device === "string" && data.device ? data.device : "NetFyr";
  const status = data.status === "up" ? "UPP" : "NER";
  const title = `${device} — ${status}`;
  const body = typeof data.message === "string" && data.message
    ? data.message
    : (typeof data.address === "string" ? data.address : "");
  const notification = self.registration.showNotification(title, {
    body,
    icon: "/icon-192.png",
    badge: "/icon-192.png",
    tag: `netfyr-${device}`,
    renotify: true,
    data: { url: "/" },
  });
  const appBadge = incrementAppBadge();
  event.waitUntil(Promise.all([notification, appBadge]));
});

self.addEventListener("notificationclick", (event) => {
  event.notification.close();
  event.waitUntil(
    self.clients.matchAll({ type: "window", includeUncontrolled: true }).then((clients) => {
      const existing = clients.find((c) => new URL(c.url).origin === self.location.origin);
      if (existing) return existing.focus();
      return self.clients.openWindow((event.notification.data && event.notification.data.url) || "/");
    }),
  );
});
