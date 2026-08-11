#!/usr/bin/env python3
"""Regressionskontroller för NetFyrs installerbara mobil-PWA.

Testerna använder bara standardbiblioteket så att de kan köras i air-gap och
på releasearkivet utan Node, npm eller webbläsarautomation.
"""

import json
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]
WEB = ROOT / "web"


class ManifestTests(unittest.TestCase):
    def test_manifest_is_installable_and_all_icons_exist(self):
        manifest = json.loads((WEB / "manifest.webmanifest").read_text(encoding="utf-8"))

        self.assertEqual(manifest["name"], "NetFyr")
        self.assertEqual(manifest["short_name"], "NetFyr")
        self.assertEqual(manifest["start_url"], "/")
        self.assertEqual(manifest["scope"], "/")
        self.assertEqual(manifest["display"], "standalone")
        self.assertEqual(manifest["theme_color"], "#0c1217")

        icons = {(icon["sizes"], icon.get("purpose", "any")): icon for icon in manifest["icons"]}
        self.assertIn(("192x192", "any"), icons)
        self.assertIn(("512x512", "any"), icons)
        self.assertIn(("512x512", "maskable"), icons)
        for icon in manifest["icons"]:
            self.assertTrue((WEB / icon["src"].lstrip("/")).is_file(), icon["src"])

        html = (WEB / "index.html").read_text(encoding="utf-8")
        self.assertIn('rel="manifest" href="/manifest.webmanifest"', html)
        self.assertIn('name="theme-color" content="#0c1217"', html)
        self.assertIn('name="apple-mobile-web-app-capable" content="yes"', html)


class ServiceWorkerTests(unittest.TestCase):
    def test_service_worker_registers_but_never_intercepts_api_requests(self):
        worker = (WEB / "service-worker.js").read_text(encoding="utf-8")
        app = (WEB / "app.js").read_text(encoding="utf-8")

        self.assertIn('navigator.serviceWorker.register("/service-worker.js", { scope: "/" })', app)
        self.assertIn('url.pathname === "/api" || url.pathname.startsWith("/api/")', worker)
        shell = worker.split("const SHELL = [", 1)[1].split("];", 1)[0]
        self.assertNotIn("/api/", shell)
        self.assertIn('request.mode === "navigate"', worker)
        self.assertIn('caches.match("/")', worker)
        self.assertIn('!SHELL.includes(url.pathname)', worker)
        self.assertIn('"/refresh-coordinator.js"', worker)
        self.assertIn('<script src="/refresh-coordinator.js"></script>', (WEB / "index.html").read_text(encoding="utf-8"))


class MobileLayoutTests(unittest.TestCase):
    def test_phone_layout_has_touch_navigation_and_safe_areas(self):
        html = (WEB / "index.html").read_text(encoding="utf-8")
        css = (WEB / "style.css").read_text(encoding="utf-8")
        app = (WEB / "app.js").read_text(encoding="utf-8")

        self.assertIn('id="mobile-nav"', html)
        self.assertIn('data-i18n-aria="nav.primary"', html)
        self.assertIn('class="mobile-nav-item nav-item active" data-view="overview"', html)
        for view in ("stats", "terminal", "account"):
            self.assertIn(f'class="mobile-nav-item nav-item" data-view="{view}"', html)
        self.assertIn('id="mobile-nav-settings"', html)
        self.assertIn('$("mobile-nav-settings").hidden = !isAdmin;', app)

        self.assertIn("@media (max-width: 700px)", css)
        self.assertIn("env(safe-area-inset-bottom)", css)
        self.assertIn("min-height: 44px", css)
        self.assertIn(".mobile-nav", css)
        self.assertIn("repeat(auto-fit, minmax(56px, 1fr))", css)


class ConnectionStateTests(unittest.TestCase):
    def test_connection_loss_is_visible_and_refresh_driven(self):
        html = (WEB / "index.html").read_text(encoding="utf-8")
        css = (WEB / "style.css").read_text(encoding="utf-8")
        app = (WEB / "app.js").read_text(encoding="utf-8")
        i18n = (WEB / "i18n.js").read_text(encoding="utf-8")

        self.assertIn('id="connection-banner"', html)
        self.assertIn('role="status" aria-live="assertive"', html)
        self.assertIn(".connection-banner", css)
        self.assertIn("function setConnectionState(connected)", app)
        self.assertIn('window.addEventListener("offline", () => setConnectionState(false));', app)
        self.assertIn('window.addEventListener("online", () => refresh());', app)
        self.assertIn("setConnectionState(true);", app)
        self.assertIn("setConnectionState(false);", app)
        self.assertIn("RefreshCoordinator.createRefreshCoordinator(refreshOnce", app)
        self.assertIn("REFRESH_TIMEOUT_MS = 8000", app)
        self.assertIn("async function renderChannels(settings, secretNames, signal)", app)
        self.assertIn('api.get(`/channels/${name}/config`, { signal })', app)
        self.assertIn("async function loadSmsExtras(signal, generation = sessionGeneration)", app)
        self.assertIn('api.get("/sms/sessions", { signal })', app)
        self.assertIn("await loadSmsExtras(signal);", app)
        self.assertIn("if (signal.aborted) throw err;", app)
        self.assertIn("let sessionController = new AbortController();", app)
        self.assertIn("let sessionGeneration = 0;", app)
        self.assertIn("sessionController.abort();", app)
        self.assertIn("generation !== sessionGeneration || !me", app)
        self.assertIn('api.get("/settings", { signal })', app)
        self.assertIn('api.get("/channels/sms/status", { signal })', app)
        self.assertIn('api.get("/health", { signal }).catch((err) => {', app)
        self.assertIn("if (signal.aborted) throw err;", app)
        self.assertNotIn('api.get("/health", { signal }).catch(() => null)', app)
        self.assertIn('timeout: "NetFyr svarade inte inom 8 sekunder', i18n)
        self.assertIn('lost: "Ingen kontakt med NetFyr', i18n)
        self.assertIn('lost: "No connection to NetFyr', i18n)

    def test_logout_clears_session_data_and_sms_html_is_escaped(self):
        app = (WEB / "app.js").read_text(encoding="utf-8")
        self.assertIn("function clearSessionState()", app)
        for marker in (
            '$("sms-gateway-status").replaceChildren();',
            'delete $("sms-gateway-status").dataset.loaded;',
            '$("sms-sessions").replaceChildren();',
            '$("sms-verify-result").replaceChildren();',
            'settingsCache = null;',
            'overview = null;',
            '$("nh-group").replaceChildren();',
            '$("mw-group").replaceChildren();',
            '$("mw-hosts-wrap").replaceChildren();',
        ):
            self.assertIn(marker, app)

        logout = app.split('$("btn-logout").addEventListener', 1)[1].split("// ---- Rollstyrning", 1)[0]
        self.assertIn("showLogin();", logout)
        self.assertIn("await logoutRequest;", logout)
        self.assertLess(logout.index("showLogin();"), logout.index("await logoutRequest;"))
        self.assertIn('api.send("POST", "/channels/sms/verify", undefined, { signal })', app)
        self.assertIn("class StaleSessionError extends Error", app)
        self.assertIn("if (isStaleSession(err)) return;", app)
        self.assertIn("rethrowStale(err);", app)
        self.assertNotIn("const STALE_SESSION = new Promise", app)
        self.assertIn('t("settings.maint.rowGroup", { name: esc(String(w.group ?? "?")) })', app)
        self.assertIn("generation === sessionGeneration && e.target.files[0] === file", app)
        self.assertIn("if (!me) return Promise.resolve();", app)
        refresh = app.split("async function refreshOnce(signal)", 1)[1]
        self.assertIn("const generation = sessionGeneration;", refresh)
        self.assertIn("if (generation !== sessionGeneration || !me) return;", refresh)

        for unsafe in (
            '${s.error}', '${s.operator}', '${s.modemId}',
            '${s.device}', '${s.ackedBy}', '${v.modemId}', '${v.error}',
        ):
            self.assertNotIn(unsafe, app)
        for escaped in (
            'esc(String(value))', 'esc(String(s.error))',
            'esc(String(s.device))', 'esc(String(s.ackedBy))',
            'esc(String(s.sentCount))', 'esc(String(s.recipients.length))',
            'esc(String(v.modemId))', 'esc(String(v.error))',
        ):
            self.assertIn(escaped, app)

    def test_release_verifier_requires_complete_pwa_shell(self):
        verify = (ROOT / "scripts" / "verify-release.sh").read_text()
        for required in (
            "web/manifest.webmanifest",
            "web/service-worker.js",
            "web/refresh-coordinator.js",
            "web/apple-touch-icon.png",
            "web/favicon-32.png",
            "web/favicon.ico",
            "web/icon-192.png",
            "web/icon-512.png",
            "web/icon-maskable-512.png",
        ):
            self.assertIn(required, verify)
        self.assertIn('$repo_root/Cargo.toml', verify)
        self.assertIn("MAX_ARCHIVE_SIZE", verify)
        self.assertIn("MAX_MEMBER_SIZE", verify)
        self.assertIn("MAX_TOTAL_SIZE", verify)
        self.assertIn("os.O_NOFOLLOW", verify)
        self.assertIn("snapshot.tar.gz", verify)
        self.assertIn('tarfile.open(fileobj=archive_file, mode="r|gz")', verify)
        self.assertIn("tf.members.clear()", verify)
        self.assertIn("if tf.members:", verify)
        self.assertIn("members.append(member_signature(member))", verify)
        self.assertNotIn("members.append(member)", verify)
        self.assertIn("for member in tf:", verify)
        self.assertNotIn("tf.getmembers()", verify)
        self.assertIn("len(members) >= MAX_MEMBERS", verify)
        self.assertIn("member.isdir() or member.isreg()", verify)
        self.assertIn("tf.extractfile(member)", verify)
        self.assertNotIn("tar --no-same-owner", verify)


class PushHandlerTests(unittest.TestCase):
    def test_service_worker_handles_push_and_click(self):
        worker = (WEB / "service-worker.js").read_text(encoding="utf-8")
        app = (WEB / "app.js").read_text(encoding="utf-8")

        self.assertIn('self.addEventListener("push"', worker)
        self.assertIn("showNotification", worker)
        self.assertIn('self.addEventListener("notificationclick"', worker)
        self.assertIn("openWindow", worker)
        self.assertIn("setAppBadge", worker)
        self.assertIn("clearAppBadge", app)
        self.assertIn('type: "CLEAR_APP_BADGE"', app)
        self.assertIn('self.addEventListener("message"', worker)
        # Parse-fel får inte ge tyst svikt — generisk fallback måste finnas.
        self.assertIn("catch", worker)
        # Notisdata renderas av OS:et via showNotification — aldrig via DOM.
        self.assertNotIn("innerHTML", worker)

    def test_settings_ui_can_subscribe_and_unsubscribe(self):
        html = (WEB / "index.html").read_text(encoding="utf-8")
        app = (WEB / "app.js").read_text(encoding="utf-8")
        i18n = (WEB / "i18n.js").read_text(encoding="utf-8")

        for element in ("push-toggle", "push-device", "push-test", "push-msg", "push-subinfo"):
            self.assertIn(f'id="{element}"', html)

        self.assertIn("pushManager.subscribe", app)
        self.assertIn("applicationServerKey", app)
        self.assertIn("function urlBase64ToUint8Array", app)
        self.assertIn("userVisibleOnly: true", app)
        self.assertIn('api.send("POST", "/push/subscriptions"', app)
        self.assertIn('api.send("DELETE", "/push/subscriptions"', app)
        self.assertIn('api.get("/push/status"', app)
        self.assertIn('api.send("POST", "/channels/push/test")', app)
        # requestPermission får bara anropas i användargesten (klick).
        self.assertIn("Notification.requestPermission", app)

        # Knappen ligger i kontovyn så även rollen user kan nå den.
        self.assertGreater(html.index('id="push-device"'), html.index('id="view-account"'))
        self.assertIn('$("push-toggle").hidden = !isAdmin;', app)
        self.assertIn('$("push-test").hidden = !isAdmin;', app)
        # Kontovyn måste förladda status även för rollen user.
        account_refresh = app.split('if (view === "account")', 1)[1].split('if (view === "settings"', 1)[0]
        self.assertIn("await renderPush(signal);", account_refresh)

        # Vid ny prenumeration måste permission-anropet ske före varje await.
        device_handler = app.split('$("push-device").addEventListener', 1)[1].split('$("push-test")', 1)[0]
        self.assertLess(device_handler.index("Notification.requestPermission()"),
                        device_handler.index("navigator.serviceWorker.ready"))
        self.assertNotIn('api.get("/push/status")', device_handler)
        # Serverregistrering måste rullas tillbaka lokalt; borttagning sker server först.
        subscribe_path = device_handler.split("const sub = await", 1)[1].split("} else", 1)[0]
        self.assertIn("await sub.unsubscribe()", subscribe_path)
        unsubscribe_path = device_handler.split("if (existing)", 1)[1].split("} else", 1)[0]
        self.assertLess(unsubscribe_path.index('api.send("DELETE"'), unsubscribe_path.index("existing.unsubscribe()"))

        # Sessionsägda push-cachevärden får inte ärvas av nästa inloggning.
        start_app = app.split("function startApp()", 1)[1].split("// ---- Formatering", 1)[0]
        self.assertIn("pushStatusCache = null;", start_app)
        self.assertIn("pushSubscriptionCache = null;", start_app)

        # Pushflöden får inte mutera en ny session efter något await.
        push = app.split("// ---- Pushnotiser", 1)[1].split("// ---- SMS:", 1)[0]
        self.assertIn("const signal = sessionController.signal;", push)
        self.assertIn("const generation = sessionGeneration;", push)
        self.assertGreaterEqual(
            push.count("if (signal.aborted || generation !== sessionGeneration || !me) return;"),
            4,
        )

        # i18n-nycklarna måste finnas på båda språken.
        for key in ("pushTitle", "pushDeviceOn", "pushDeviceOff", "pushSubCount",
                    "pushUnsupported", "pushDenied", "pushNeedChannel"):
            self.assertEqual(i18n.count(f"{key}:"), 2, key)


if __name__ == "__main__":
    unittest.main(verbosity=2)
