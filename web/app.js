// =====================================================================
// NetFyr Server — gränssnitt
//
// Inget byggsteg, inget ramverk. Motiveringen är air-gap: en container
// utan node, npm eller bundler är enklare att paketera, granska och
// återskapa. Växer gränssnittet ur det inför vi ett byggsteg medvetet.
//
// All visningslogik ligger på servern. Klienten färgar efter fältet
// `display` och räknar inte ut något eget — regeln om att rött bara
// betyder bekräftat larm får inte kunna tolkas olika av olika klienter.
//
// Texter hämtas ur i18n.js (t("nyckel")) — svenska/engelska/system,
// se den filen. Dynamiskt innehåll översätts vid rendering, statiskt
// via data-i18n-attribut i index.html.
// =====================================================================

const POLL_MS = 5000;

const $ = (id) => document.getElementById(id);
const t = I18N.t;

// Den inloggade användaren. Sätts vid uppstart via /api/auth/me —
// utan svar där visas inloggningen i stället för gränssnittet.
let me = null;

const api = {
  async get(path) {
    const r = await fetch(`/api${path}`);
    if (r.status === 401) { showLogin(); throw new Error(t("login.notLoggedIn")); }
    if (!r.ok) throw new Error((await r.json().catch(() => ({}))).error || `HTTP ${r.status}`);
    return r.json();
  },
  async send(method, path, body) {
    const r = await fetch(`/api${path}`, {
      method,
      headers: { "Content-Type": "application/json" },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    if (r.status === 401) { showLogin(); throw new Error(t("login.notLoggedIn")); }
    if (!r.ok) throw new Error((await r.json().catch(() => ({}))).error || `HTTP ${r.status}`);
    return r.status === 204 ? null : r.json().catch(() => null);
  },
};

// ---- Inloggning -------------------------------------------------------

let pollTimer = null;

function showLogin() {
  me = null;
  if (pollTimer) { clearInterval(pollTimer); pollTimer = null; }
  if (smsRailTimer) { clearInterval(smsRailTimer); smsRailTimer = null; }
  $("card-sms").hidden = true;
  $("login").hidden = false;
  $("login-pass").value = "";
}

function hideLogin() {
  $("login").hidden = true;
}

$("login-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const btn = $("login-form").querySelector("button[type=submit]");
  btn.disabled = true;
  try {
    me = await api.send("POST", "/auth/login", {
      username: $("login-user").value,
      password: $("login-pass").value,
    });
    hideLogin();
    startApp();
  } catch (err) {
    flash($("login-msg"), String(err.message), false);
  } finally {
    btn.disabled = false;
  }
});

$("btn-logout").addEventListener("click", async () => {
  await api.send("POST", "/auth/logout").catch(() => {});
  showLogin();
});

// ---- Rollstyrning -------------------------------------------------------
//
// Döljer det en user inte får göra. Skyddet sitter på servern — det
// här är enbart så att gränssnittet inte visar knappar som ändå
// skulle få 403 till svar.

function applyRole() {
  $("user-name").textContent = me.username;
  $("user-role").textContent = me.role;
  const isAdmin = me.role === "admin";
  $("nav-settings").hidden = !isAdmin;
  $("chip-users").hidden = !isAdmin;
  $("tg-live").disabled = !isAdmin;
  $("tg-alarms").disabled = !isAdmin;
  $("stats-clear").hidden = !isAdmin;
  // Ett engångslösen måste bytas direkt — visa kontovyn och markera.
  $("must-change-card").hidden = !me.mustChangePassword;
  if (me.mustChangePassword) showView("account");
  else if (view === "settings" && !isAdmin) showView("overview");
}

async function boot() {
  try {
    me = await api.get("/auth/me");
  } catch {
    showLogin();
    return;
  }
  hideLogin();
  startApp();
}

function startApp() {
  applyRole();
  if (!pollTimer) pollTimer = setInterval(refresh, POLL_MS);
  refresh();
  // Versionen i sidopanelen
  api.get("/health").then((h) => { $("version").textContent = `v${h.version}`; }).catch(() => {});
  // Globala gränssnittsinställningar (bl.a. SMS-rutan i sidopanelen).
  // Läsbar för båda rollerna — det är bara skrivandet som kräver admin.
  api.get("/settings").then((s) => { settingsCache = s; applyRailVisibility(); }).catch(() => {});
  if (!smsRailTimer) smsRailTimer = setInterval(loadSmsRail, SMS_RAIL_MS);
}

// ---- Formatering -----------------------------------------------------

/** Uppslag i en ordlistsgren med fallback till råvärdet — en okänd
 *  nyckel (t.ex. ett display-läge servern hittat på) ska synas som
 *  sig självt, inte som en tom ruta. */
function dictText(path, raw) {
  const s = t(path);
  return s === path ? raw : s;
}

const stateText = (d) => dictText(`state.${d}`, d);
const reasonText = (r) => dictText(`reason.${r}`, r);

/** Svarstid kommer i mikrosekunder — millisekunder avrundar bort allt
 *  på ett lokalt nät. Här formateras det för läsbarhet. */
function latency(us) {
  if (us == null) return "–";
  if (us < 1000) return `${us} µs`;
  if (us < 100000) return `${(us / 1000).toFixed(1)} ms`;
  return `${Math.round(us / 1000)} ms`;
}

function clock(ms) {
  if (!ms) return "–";
  return new Date(ms).toLocaleTimeString(I18N.locale());
}

/** "sedan 14:32" är vad en operatör frågar sig, inte en tidsstämpel. */
function since(ms) {
  if (!ms) return "";
  const s = Math.floor((Date.now() - ms) / 1000);
  if (s < 60) return `${s} s`;
  if (s < 3600) return `${Math.floor(s / 60)} min`;
  if (s < 86400) return `${Math.floor(s / 3600)} h`;
  return `${Math.floor(s / 86400)} d`;
}

function flash(el, text, ok = true) {
  el.textContent = text;
  el.className = `msg ${ok ? "ok" : "bad"}`;
  setTimeout(() => {
    el.textContent = "";
    el.className = "msg";
  }, 4000);
}

// ---- Vyer och flikar -------------------------------------------------

let view = "overview";
let tab = "devices";

function tabTitle(name) {
  return dictText(`settings.tabs.${name}`, t("settings.kicker"));
}

// Cache som flera flikar behöver: grupper för select-rutor, inställningar
// för kanalernas på/av-lägen.
let groupsCache = [];
let settingsCache = null;

function showView(next) {
  view = next;
  document.querySelectorAll(".nav-item").forEach((n) => {
    n.classList.toggle("active", n.dataset.view === view);
  });
  document.querySelectorAll(".view").forEach((v) => {
    v.hidden = v.id !== `view-${view}`;
  });
  // Statistiken laddas när vyn visas — den hänger inte på pollingen.
  if (view === "stats") loadStats();
  refresh();
}

function showTab(next) {
  tab = next;
  document.querySelectorAll(".chip").forEach((c) => {
    c.classList.toggle("active", c.dataset.tab === tab);
  });
  document.querySelectorAll(".tabview").forEach((v) => {
    v.hidden = v.id !== `tab-${tab}`;
  });
  $("settings-title").textContent = tabTitle(tab);
  refresh();
}

document.querySelectorAll(".nav-item").forEach((b) =>
  b.addEventListener("click", () => showView(b.dataset.view)),
);
document.querySelectorAll(".chip").forEach((b) =>
  b.addEventListener("click", () => showTab(b.dataset.tab)),
);
document.addEventListener("click", (e) => {
  const g = e.target.closest("[data-goto]");
  if (g) showView(g.dataset.goto);
});

// ---- Språk --------------------------------------------------------------
//
// Port av desktopens i18n. Valet är personligt och ligger i webbläsarens
// localStorage (samma nyckel som desktop: "netfyr.lang") — inte på
// servern. Väljaren finns på två ställen: Inställningar → System →
// Gränssnitt (admin) och Mitt konto (alla roller — en user når inte
// Inställningar men ska kunna byta språk den med).

// Måndag först i visningen, värdet är JS-dagindex (0 = söndag).
const DAY_ORDER = [1, 2, 3, 4, 5, 6, 0];

/** Dagrutorna i underhållsformuläret ritas om vid språkbyte — ikryssade
 *  val behålls, bara etiketterna byts. */
function renderWeekdayBoxes() {
  const weekdays = t("settings.maint.weekdays");
  const checked = new Set(
    [...document.querySelectorAll("#mw-days input:checked")].map((c) => c.value),
  );
  $("mw-days").innerHTML = DAY_ORDER.map(
    (d) => `
  <label class="check inline"><input type="checkbox" value="${d}"${checked.has(String(d)) ? " checked" : ""}><span>${weekdays[d]}</span></label>`,
  ).join("");
}

/** Måla om allt som bär språk: statisk text (data-i18n), dynamiska
 *  ytor (refresh ritar om dem med nya t()-värden) och de delar som
 *  pollar på egna timers. */
function applyLanguage() {
  I18N.applyStatic();
  document.querySelectorAll(".lang-select").forEach((s) => { s.value = I18N.pref; });
  renderWeekdayBoxes();
  if (view === "settings") $("settings-title").textContent = tabTitle(tab);
  refresh();
  loadSmsRail();
  if (view === "stats") loadStats();
}

document.querySelectorAll(".lang-select").forEach((sel) => {
  sel.value = I18N.pref;
  sel.addEventListener("change", () => {
    I18N.setPref(sel.value);
    applyLanguage();
  });
});

renderWeekdayBoxes();

// ---- Översikt --------------------------------------------------------

let overview = null;

function renderOverview(data) {
  overview = data;

  $("tg-live").classList.toggle("on", data.live);
  $("tg-alarms").classList.toggle("on", data.alarms);
  const stamp = new Date().toLocaleTimeString(I18N.locale());
  $("last-sweep").textContent = stamp;
  $("rail-sweep").textContent = stamp;
  $("head-live").textContent = data.live ? t("dash.live") : t("dash.paused");

  // Statuskortet speglar radions kort i desktopvarianten: rubrik,
  // lysdiod, läge, två mätvärden och en tidsstämpel.
  const t0 = data.tally;
  const card = $("card-engine");
  const led = $("engine-led");
  led.className = "led";
  card.className = "rail-card";
  let engineState = t("rail.engineWatching");
  if (!data.live) {
    engineState = t("rail.enginePaused");
  } else if (t0.alarming > 0) {
    engineState = t("rail.engineAlarm");
    led.classList.add("offline");
    card.classList.add("offline");
  } else if (t0.uncertain > 0 || t0.warning > 0) {
    engineState = t("rail.engineUncertain");
    led.classList.add("warning");
    card.classList.add("warning");
  } else {
    led.classList.add("online");
    card.classList.add("online");
  }
  $("engine-state").textContent = engineState;
  $("engine-hosts").textContent = t("rail.devices", { n: t0.active });
  $("engine-alarms").textContent = data.alarms ? t("rail.alarmsOn") : t("rail.alarmsOff");


  const verdict = $("verdict");
  let mood = "calm";
  let state = t("dash.verdictOkTitle");
  let detail = t("dash.verdictOkSub", { n: t0.active });

  if (!data.live) {
    mood = "idle";
    state = t("dash.verdictPausedTitle");
    detail = t("dash.verdictPausedSub");
  } else if (t0.alarming > 0) {
    mood = "alarm";
    state = t("dash.verdictDownTitle", { n: t0.alarming });
    detail = data.alarms ? t("dash.verdictDownAlarms") : t("dash.verdictDownMuted");
  } else if (t0.uncertain > 0 || t0.warning > 0) {
    mood = "watch";
    state = t("dash.verdictWatchTitle");
    detail = [
      t0.warning ? t("dash.verdictSlow", { n: t0.warning }) : null,
      t0.uncertain ? t("dash.verdictUncertain", { n: t0.uncertain }) : null,
    ].filter(Boolean).join(", ");
  } else if (t0.active === 0) {
    mood = "idle";
    state = t("dash.verdictNoneTitle");
    detail = t("dash.verdictNoneSub");
  }

  verdict.className = `verdict ${mood}`;
  $("verdict-state").textContent = state;
  $("verdict-detail").textContent = detail;

  const tally = [
    ["up", t("dash.tallyUp"), t0.online],
    ["warn", t("dash.tallySlow"), t0.warning],
    ["uncertain", t("dash.tallyUncertain"), t0.uncertain],
    ["down", t("dash.tallyDown"), t0.offline],
    ["paused", t("dash.tallyPaused"), t0.paused + t0.suppressed],
  ];
  $("tally").innerHTML = tally
    .map(([cls, label, n]) => `<div class="tally-item ${cls}"><div class="n">${n}</div><div class="l">${label}</div></div>`)
    .join("");

  renderGroupFilters(data);
  renderHostRows();
}

// ---- Översikt: gruppfilter och sortering ------------------------------
//
// Samma upplägg som desktopvariantens översikt: chips per grupp och ett
// valfritt sorteringsläge. Valet sparas i webbläsaren (localStorage) —
// det är ett personligt visningsval, inte en serverinställning.

let ovGroup = localStorage.getItem("netfyr.ovGroup") || "all";
let ovSort = localStorage.getItem("netfyr.ovSort") || "status";

/** Sorteringsnyckel för IP-adresser: oktetter utfyllda till tre siffror
 *  gör att 192.168.1.9 hamnar före 192.168.1.10, till skillnad från en
 *  ren textsortering. Adresser som inte är IPv4 (värdnamn) faller
 *  tillbaka på textsortering. */
function ipKey(addr) {
  const parts = String(addr).split(".");
  if (parts.length === 4 && parts.every((p) => /^\d{1,3}$/.test(p))) {
    return parts.map((p) => p.padStart(3, "0")).join(".");
  }
  return String(addr);
}

/** Gruppchipsen byggs ur själva översiktsdatan — då försvinner en chip
 *  automatiskt om gruppen töms eller tas bort. */
function renderGroupFilters(data) {
  const box = $("ov-group-filters");
  const groups = new Map(); // id -> { name, count }
  let ungrouped = 0;
  for (const h of data.hosts) {
    if (h.groupId == null) { ungrouped++; continue; }
    const g = groups.get(h.groupId) || { name: h.group || "?", count: 0 };
    g.count++;
    groups.set(h.groupId, g);
  }

  // Finns den valda gruppen inte kvar (borttagen/tömd) — fall tillbaka
  // på "alla" i stället för att visa en tom lista utan förklaring.
  if (ovGroup !== "all" && ovGroup !== "none" && !groups.has(Number(ovGroup))) {
    ovGroup = "all";
    localStorage.setItem("netfyr.ovGroup", ovGroup);
  }

  const chips = [`<button class="chip${ovGroup === "all" ? " active" : ""}" data-ovgroup="all">${t("dash.chipAll", { n: data.hosts.length })}</button>`];
  [...groups.entries()]
    .sort((a, b) => a[1].name.localeCompare(b[1].name, I18N.lang()))
    .forEach(([id, g]) => {
      chips.push(`<button class="chip${ovGroup === String(id) ? " active" : ""}" data-ovgroup="${id}">${esc(g.name)} · ${g.count}</button>`);
    });
  if (ungrouped > 0) {
    chips.push(`<button class="chip${ovGroup === "none" ? " active" : ""}" data-ovgroup="none">${t("dash.chipUngrouped", { n: ungrouped })}</button>`);
  }
  box.innerHTML = chips.join("");
}

/** Enhetslistan — filtrerad på vald grupp och sorterad enligt valet. */
function renderHostRows() {
  const rows = $("host-rows");
  let hosts = overview?.hosts ?? [];

  if (hosts.length === 0) {
    rows.innerHTML = `<p class="empty" style="padding:22px">${t("dash.emptyNone")}</p>`;
    return;
  }

  if (ovGroup === "none") hosts = hosts.filter((h) => h.groupId == null);
  else if (ovGroup !== "all") hosts = hosts.filter((h) => String(h.groupId) === ovGroup);

  if (hosts.length === 0) {
    rows.innerHTML = `<p class="empty" style="padding:22px">${t("dash.emptyGroup")}</p>`;
    return;
  }

  // Problem först — ALLTID, även vid namn-/IP-sortering. En nattvakt
  // ska inte behöva leta, och en felande enhet får aldrig drunkna i
  // listan bara för att operatören valt en annan ordning. Sorterings-
  // valet styr ordningen INOM respektive statusgrupp.
  const order = { offline: 0, recovering: 1, uncertain: 2, warning: 3, online: 4, suppressed: 5, paused: 6 };
  const byName = (a, b) => a.name.localeCompare(b.name, I18N.lang()) || ipKey(a.address).localeCompare(ipKey(b.address));
  const byIp = (a, b) => ipKey(a.address).localeCompare(ipKey(b.address)) || a.name.localeCompare(b.name, I18N.lang());
  const chosen = ovSort === "ip" ? byIp : byName;
  const sorted = [...hosts].sort(
    (a, b) => (order[a.display] ?? 9) - (order[b.display] ?? 9) || chosen(a, b),
  );

  rows.innerHTML = sorted.map(row).join("");
}

document.addEventListener("click", (e) => {
  const chip = e.target.closest("[data-ovgroup]");
  if (!chip) return;
  ovGroup = chip.dataset.ovgroup;
  localStorage.setItem("netfyr.ovGroup", ovGroup);
  if (overview) {
    renderGroupFilters(overview);
    renderHostRows();
  }
});

$("ov-sort").value = ovSort;
$("ov-sort").addEventListener("change", () => {
  ovSort = $("ov-sort").value;
  localStorage.setItem("netfyr.ovSort", ovSort);
  renderHostRows();
});

/** En rad i enhetstabellen. */
function row(h) {
  const led = { online: "online", warning: "warning", uncertain: "warning",
                recovering: "offline", offline: "offline" }[h.display] || "";
  const reason = h.reason ? reasonText(h.reason) : "";
  const age = h.changedAt ? t("dash.since", { age: since(h.changedAt) }) : "";
  const note = [reason, age].filter(Boolean).join(" · ");

  return `
    <div class="dev-row body ${h.display}">
      <div class="signal"><span class="led ${led}"></span>${stateText(h.display)}</div>
      <div>
        <div class="host-name">${esc(h.name)}</div>
        <div class="host-addr">${esc(h.address)}${h.group ? ` · ${esc(h.group)}` : ""}</div>
        ${note ? `<div class="host-note">${esc(note)}</div>` : ""}
      </div>
      <div class="rtt">
        <span class="rtt-value">${latency(h.latencyUs)}</span>
        ${spark(h.spark)}
      </div>
      <div class="stamp">${h.checkedAt ? clock(h.checkedAt) : "–"}</div>
      <div class="polls"><b>${h.pollsOk}</b>/${h.polls}</div>
      <div class="row-actions">
        <button class="btn btn-sm ghost" data-snooze="${h.id}">${h.snoozeUntil ? t("dash.wake") : t("dash.snooze")}</button>
      </div>
    </div>`;
}

/** Svarstid över tid som en enkel kurva.
 *
 *  Uteblivna svar ligger som nollor i serien och ritas som hål i
 *  kurvan — att hoppa över dem hade dolt precis det man vill se. */
function spark(values) {
  if (!values || values.length < 2) return `<svg class="spark"></svg>`;
  const max = Math.max(...values, 1);
  const w = 100, hgt = 26;
  const step = w / (values.length - 1);

  const pts = values.map((v, i) => [i * step, hgt - (v / max) * (hgt - 4) - 2]);
  let d = "";
  let pen = false;
  values.forEach((v, i) => {
    if (v === 0) { pen = false; return; }
    const [x, y] = pts[i];
    d += `${pen ? "L" : "M"}${x.toFixed(1)},${y.toFixed(1)}`;
    pen = true;
  });

  const gaps = values
    .map((v, i) => (v === 0 ? `<rect x="${(i * step - step / 2).toFixed(1)}" y="0" width="${step.toFixed(1)}" height="${hgt}" fill="rgba(226,109,92,.18)"/>` : ""))
    .join("");

  return `<svg class="spark" viewBox="0 0 ${w} ${hgt}" preserveAspectRatio="none" aria-hidden="true">
    ${gaps}<path d="${d}" fill="none" stroke="currentColor" stroke-width="1" opacity=".65"/>
  </svg>`;
}

function esc(s) {
  return String(s).replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]),
  );
}

document.addEventListener("click", async (e) => {
  const ack = e.target.closest("[data-ack]");
  if (ack) {
    await api.send("POST", `/hosts/${ack.dataset.ack}/ack`).catch(() => {});
    refresh();
    return;
  }
  const sn = e.target.closest("[data-snooze]");
  if (sn) {
    const host = overview?.hosts.find((h) => String(h.id) === sn.dataset.snooze);
    const minutes = host?.snoozeUntil ? 0 : 60;
    await api.send("POST", `/hosts/${sn.dataset.snooze}/snooze`, { minutes }).catch(() => {});
    refresh();
    return;
  }
  const del = e.target.closest("[data-del]");
  if (del) {
    if (!confirm(t("dash.confirmDeleteHost"))) return;
    await api.send("DELETE", `/hosts/${del.dataset.del}`).catch(() => {});
    refresh();
  }
});

$("tg-live").addEventListener("click", async () => {
  await api.send("PUT", "/settings", { live: String(!overview.live) });
  refresh();
});
$("tg-alarms").addEventListener("click", async () => {
  await api.send("PUT", "/settings", { alarms: String(!overview.alarms) });
  refresh();
});

// ---- Enheter ---------------------------------------------------------

// Vilka rader som är utfällda. Rent UI-tillstånd, sparas inte.
const expandedHosts = new Set();

function groupOptions(selectedId) {
  const opts = [`<option value="">${t("settings.devices.noGroup")}</option>`];
  for (const g of groupsCache) {
    opts.push(
      `<option value="${g.id}" ${g.id === selectedId ? "selected" : ""}>${esc(g.name)}</option>`,
    );
  }
  return opts.join("");
}

function renderDevices(hosts) {
  const rows = $("device-rows");

  // Gruppvalet i lägg-till-formuläret följer med grupperna.
  $("nh-group").innerHTML = groupOptions(null);

  if (hosts.length === 0) {
    rows.innerHTML = `<p class="empty">${t("common.noDevices")}</p>`;
    return;
  }

  rows.innerHTML = hosts
    .map((h) => {
      const open = expandedHosts.has(h.id);
      const overrides = [
        h.probeType === "tcp" && t("settings.devices.ovProbeTcp", { port: h.probePort }),
        h.probeType === "http" && t("settings.devices.ovProbeHttp", { port: h.probePort }),
        h.intervalSec != null && t("settings.devices.ovInterval", { n: h.intervalSec }),
        h.failPeriodSec != null && t("settings.devices.ovFail", { n: h.failPeriodSec }),
        h.successPeriodSec != null && t("settings.devices.ovSuccess", { n: h.successPeriodSec }),
        h.slowThresholdMs != null && t("settings.devices.ovSlow", { n: h.slowThresholdMs }),
        h.packetSize != null && t("settings.devices.ovPacket", { n: h.packetSize }),
        h.dependsOnAddress && t("settings.devices.ovDepends", { addr: h.dependsOnAddress }),
        h.smsEnabled === false && t("settings.devices.ovSmsOff"),
      ].filter(Boolean);

      const depOptions = [`<option value="">${t("settings.devices.noDependency")}</option>`]
        .concat(
          hosts
            .filter((p) => p.id !== h.id)
            .map(
              (p) =>
                `<option value="${esc(p.address)}" ${
                  p.address === h.dependsOnAddress ? "selected" : ""
                }>${esc(p.name)} (${esc(p.address)})</option>`,
            ),
        )
        .join("");

      const inheritPh = t("settings.devices.inheritPh");
      return `
      <div class="row ${h.enabled ? "" : "paused"}">
        <div class="row-main">
          <div class="row-name">${esc(h.name)}</div>
          <div class="row-meta">${esc(h.address)}${h.group ? ` · ${esc(h.group)}` : ""}${
            overrides.length ? ` · <em>${esc(overrides.join(" · "))}</em>` : ""
          }</div>
        </div>
        <div class="row-actions">
          <button class="btn btn-sm" data-expand="${h.id}">${open ? t("settings.devices.close") : t("settings.devices.edit")}</button>
          <button class="btn btn-sm" data-toggle="${h.id}" data-on="${h.enabled}">${
            h.enabled ? t("settings.devices.pause") : t("settings.devices.start")
          }</button>
          <button class="btn btn-sm btn-danger" data-del="${h.id}">${t("common.remove")}</button>
        </div>
      </div>
      ${
        open
          ? `
      <div class="dev-editor" data-editor="${h.id}">
        <div class="smtp-grid">
          <label><span>${t("settings.devices.name")}</span><input data-f="name" value="${esc(h.name)}"></label>
          <label><span>${t("settings.devices.address")}</span><input data-f="address" value="${esc(h.address)}"></label>
          <label><span>${t("settings.devices.group")}</span><select data-f="groupId">${groupOptions(h.groupId)}</select></label>
          <label><span>${t("settings.devices.note")}</span><input data-f="note" value="${esc(h.note ?? "")}"></label>
          <label><span>${t("settings.devices.interval")}</span><input data-f="intervalSec" type="number" min="1" placeholder="${inheritPh}" value="${h.intervalSec ?? ""}"></label>
          <label><span>${t("settings.devices.fail")}</span><input data-f="failPeriodSec" type="number" min="1" placeholder="${inheritPh}" value="${h.failPeriodSec ?? ""}"></label>
          <label><span>${t("settings.devices.success")}</span><input data-f="successPeriodSec" type="number" min="1" placeholder="${inheritPh}" value="${h.successPeriodSec ?? ""}"></label>
          <label><span>${t("settings.devices.slow")}</span><input data-f="slowThresholdMs" type="number" min="1" placeholder="${inheritPh}" value="${h.slowThresholdMs ?? ""}"></label>
          <label><span>${t("settings.devices.packet")}</span><input data-f="packetSize" type="number" min="16" placeholder="${inheritPh}" value="${h.packetSize ?? ""}"></label>
          <label><span>${t("settings.devices.probe")}</span><select data-f="probeType">
            <option value="icmp" ${h.probeType !== "tcp" && h.probeType !== "http" ? "selected" : ""}>${t("settings.devices.probeIcmp")}</option>
            <option value="tcp" ${h.probeType === "tcp" ? "selected" : ""}>${t("settings.devices.probeTcp")}</option>
            <option value="http" ${h.probeType === "http" ? "selected" : ""}>${t("settings.devices.probeHttp")}</option>
          </select></label>
          <label><span>${t("settings.devices.probePort")}</span><input data-f="probePort" type="number" min="1" max="65535" placeholder="${t("settings.devices.probePortPh")}" value="${h.probePort ?? ""}"></label>
          <label><span>${t("settings.devices.dependsOn")}</span><select data-f="dependsOnAddress">${depOptions}</select></label>
          <label class="check inline"><input data-f="smsEnabled" type="checkbox" ${h.smsEnabled !== false ? "checked" : ""}><span>${t("settings.devices.smsAlarm")}</span></label>
        </div>
        <p class="switch-desc">${t("settings.devices.inheritHint")}</p>
        <div class="ch-status">
          <button class="btn btn-beacon" data-save="${h.id}">${t("common.save")}</button>
          <span class="msg" data-msg="${h.id}"></span>
        </div>
      </div>`
          : ""
      }`;
    })
    .join("");
}

// Fäll ut/in redigeraren.
document.addEventListener("click", (e) => {
  const ex = e.target.closest("[data-expand]");
  if (!ex) return;
  const id = Number(ex.dataset.expand);
  if (expandedHosts.has(id)) expandedHosts.delete(id);
  else expandedHosts.add(id);
  refresh();
});

// Spara en redigerad enhet. Tomma fält skickas som null = ärva globalt.
document.addEventListener("click", async (e) => {
  const sv = e.target.closest("[data-save]");
  if (!sv) return;
  const id = Number(sv.dataset.save);
  const editor = document.querySelector(`[data-editor="${id}"]`);
  const msg = editor.querySelector("[data-msg]");
  const val = (f) => editor.querySelector(`[data-f="${f}"]`).value.trim();
  const numOrNull = (f) => (val(f) === "" ? null : Number(val(f)));

  try {
    await api.send("PATCH", `/hosts/${id}`, {
      name: val("name"),
      address: val("address"),
      groupId: val("groupId") === "" ? null : Number(val("groupId")),
      note: val("note") || null,
      intervalSec: numOrNull("intervalSec"),
      failPeriodSec: numOrNull("failPeriodSec"),
      successPeriodSec: numOrNull("successPeriodSec"),
      slowThresholdMs: numOrNull("slowThresholdMs"),
      packetSize: numOrNull("packetSize"),
      probeType: val("probeType"),
      probePort: numOrNull("probePort"),
      dependsOnAddress: val("dependsOnAddress") || null,
      smsEnabled: editor.querySelector('[data-f="smsEnabled"]').checked,
    });
    flash(msg, t("common.saved"));
    refresh();
  } catch (err) {
    flash(msg, String(err.message), false);
  }
});

document.addEventListener("click", async (e) => {
  const tg = e.target.closest("[data-toggle]");
  if (!tg) return;
  const on = tg.dataset.on === "true";
  await api.send("PATCH", `/hosts/${tg.dataset.toggle}`, { enabled: !on }).catch(() => {});
  refresh();
});

$("nh-add").addEventListener("click", async () => {
  const name = $("nh-name").value.trim();
  const address = $("nh-address").value.trim();
  const groupId = Number($("nh-group").value) || null;
  const note = $("nh-note").value.trim();
  if (!name || !address) {
    flash($("nh-msg"), t("settings.devices.needNameAddr"), false);
    return;
  }
  try {
    await api.send("POST", "/hosts", {
      name,
      address,
      groupId,
      note: note || null,
    });
    $("nh-name").value = "";
    $("nh-address").value = "";
    $("nh-note").value = "";
    flash($("nh-msg"), t("settings.devices.added"));
    refresh();
  } catch (err) {
    flash($("nh-msg"), String(err.message), false);
  }
});

// ---- Logg ------------------------------------------------------------

function renderLog(events, deliveries) {
  const ev = $("events");
  ev.innerHTML = events.length
    ? events
        .map(
          (e) =>
            `<div><time>${clock(e.ts)}</time><span class="lv-${esc(e.level)}">${esc(e.text)}</span></div>`,
        )
        .join("")
    : `<p class="empty">${t("common.nothingLogged")}</p>`;

  const dl = $("deliveries");
  dl.innerHTML = deliveries.length
    ? deliveries
        .map((d) => {
          const lv = d.status === "sent" ? "ok" : d.status === "failed" ? "alarm" : "warn";
          const tail =
            d.status === "sent"
              ? t("terminal.delivered")
              : d.status === "failed"
                ? t("terminal.gaveUp", { n: d.attempts })
                : t("terminal.waiting", { n: d.attempts });
          const err = d.lastError ? ` — ${esc(d.lastError)}` : "";
          return `<div><time>${clock(d.createdAt)}</time><span class="lv-${lv}">${esc(
            d.channel,
          )} → ${esc(d.device)} (${esc(d.event)}) · ${tail}${err}</span></div>`;
        })
        .join("")
    : `<p class="empty">${t("terminal.noDeliveries")}</p>`;
}

// ---- Statistik ---------------------------------------------------------
//
// Port av desktopvariantens Stats-sida. Datat kommer färdigaggregerat
// från servern (/api/stats) — klienten ritar bara. Statistiken laddas
// när vyn visas, när fönstret byts eller när användaren trycker
// Uppdatera — den hänger inte på 5-sekundersloopen, aggregeringen är
// för tung för att polla.

let statsWindow = localStorage.getItem("netfyr.statsWindow") || "24h";

document.querySelectorAll("#stats-windows .chip").forEach((c) => {
  c.classList.toggle("active", c.dataset.window === statsWindow);
  c.addEventListener("click", () => {
    statsWindow = c.dataset.window;
    localStorage.setItem("netfyr.statsWindow", statsWindow);
    document
      .querySelectorAll("#stats-windows .chip")
      .forEach((x) => x.classList.toggle("active", x === c));
    loadStats();
  });
});

$("stats-refresh").addEventListener("click", loadStats);

async function loadStats() {
  const rowsBox = $("stats-rows");
  rowsBox.innerHTML = `<p class="empty" style="padding:22px">${t("stats.loading")}</p>`;
  try {
    renderStats(await api.get(`/stats?window=${statsWindow}`));
  } catch (err) {
    rowsBox.innerHTML = `<p class="empty" style="padding:22px">${esc(String(err.message))}</p>`;
  }
}

function renderStats(d) {
  const rowsBox = $("stats-rows");
  if (d.totalSamples === 0) {
    rowsBox.innerHTML = `<p class="empty" style="padding:22px"><b>${t("stats.emptyTitle")}</b><br>
      ${t("stats.emptyBody")}</p>`;
  } else {
    rowsBox.innerHTML = d.rows.map(statsRow).join("");
  }

  const events = $("stats-events");
  if (d.incidents.length === 0) {
    events.innerHTML = `<p class="events-empty">${t("stats.noEvents")}</p>`;
  } else {
    events.innerHTML = d.incidents.slice(0, 50).map(incidentRow).join("");
  }
}

function statsRow(r) {
  const pct = r.uptimePct;
  const color = pct == null ? "var(--paused)" : pct >= 99 ? "var(--up)" : pct >= 95 ? "var(--warn)" : "var(--down)";
  return `
    <div class="kpi-row body">
      <span>
        <div class="host-name">${esc(r.name)}</div>
        <div class="host-addr">${esc(r.address)}${r.group ? ` · ${esc(r.group)}` : ""}</div>
      </span>
      <span class="uptime">
        <span class="uptime-val">${pct == null ? "—" : `${pct.toFixed(1)} %`}</span>
        <span class="uptime-bar"><i style="width:${pct ?? 0}%;background:${color}"></i></span>
      </span>
      <span class="num">${r.avgMs == null ? "—" : `${Math.round(r.avgMs)} ms`}</span>
      <span class="num">${r.p95Ms == null ? "—" : `${Math.round(r.p95Ms)} ms`}</span>
      <span>${latline(r.line)}</span>
    </div>`;
}

/** Svarstidslinjen — port av desktopens LatencyLine. Värdet -1 är en
 *  buckel med missade mätningar och ritas som ett rött streck, null är
 *  ett hål (ingen data). */
function latline(data) {
  const w = 240, h = 40;
  const vals = data.filter((v) => v != null && v >= 0);
  if (vals.length === 0) {
    return `<svg class="latline" viewBox="0 0 ${w} ${h}" preserveAspectRatio="none"></svg>`;
  }
  const max = Math.max(60, ...vals);
  const n = data.length;
  const x = (i) => (n <= 1 ? 0 : (i / (n - 1)) * w);
  const y = (v) => h - 2 - Math.max(2, (v / max) * (h - 6));
  let path = "";
  let pen = false;
  data.forEach((v, i) => {
    if (v == null || v < 0) { pen = false; return; }
    path += `${pen ? "L" : "M"}${x(i).toFixed(1)} ${y(v).toFixed(1)} `;
    pen = true;
  });
  const gaps = data
    .map((v, i) => (v === -1 ? `<rect x="${x(i) - 1}" y="0" width="2" height="${h}" fill="var(--down)" opacity="0.45"/>` : ""))
    .join("");
  return `<svg class="latline" viewBox="0 0 ${w} ${h}" preserveAspectRatio="none">
    ${gaps}<path d="${path}" fill="none" stroke="var(--beacon)" stroke-width="1.5" vector-effect="non-scaling-stroke"/>
  </svg>`;
}

function incidentRow(inc) {
  const ongoing = inc.end == null;
  const dur = fmtDuration((inc.end ?? Date.now()) - inc.start);
  const timeText = ongoing
    ? `${t("stats.down", { time: fmtTime(inc.start) })} · ${t("stats.ongoing")}`
    : `${t("stats.down", { time: fmtTime(inc.start) })} ${t("stats.up", { time: fmtTime(inc.end) })}`;
  return `
    <div class="event">
      <span class="led offline${ongoing ? " beating" : ""}"></span>
      <span class="event-host">${esc(inc.name)}</span>
      <span class="event-time">${timeText}</span>
      <span class="event-dur">${dur}</span>
    </div>`;
}

function fmtTime(ts) {
  return new Date(ts).toLocaleString(I18N.locale(), {
    month: "short", day: "numeric", hour: "2-digit", minute: "2-digit",
  });
}

function fmtDuration(ms) {
  const min = Math.round(ms / 60000);
  if (min < 60) return `${min} min`;
  const h = Math.floor(min / 60);
  const m = min % 60;
  return m ? `${h} h ${m} min` : `${h} h`;
}

$("stats-clear").addEventListener("click", async () => {
  if (!confirm(t("stats.confirmClear"))) return;
  try {
    const r = await api.send("DELETE", "/stats/history");
    alert(t("stats.cleared", { n: r.removed }));
    loadStats();
  } catch (err) {
    alert(String(err.message));
  }
});

// ---- Inställningar ---------------------------------------------------

function renderSettings(s, secretNames, health) {
  if (health) {
    $("version").textContent = `v${health.version}`;
    const h = Math.floor(health.uptimeSec / 3600);
    const m = Math.floor((health.uptimeSec % 3600) / 60);
    $("about").textContent = t("settings.sys.about", {
      version: health.version, h, m, n: health.hosts ?? 0,
    });
  }

  if (document.activeElement?.tagName === "INPUT") return; // skriv inte över pågående inmatning
  $("st-sweep").value = s.sweepIntervalSec;
  $("st-fail").value = s.failPeriodSec;
  $("st-success").value = s.successPeriodSec;
  $("st-slow").value = s.slowThresholdMs;
  $("st-slowalarm-sec").value = s.slowAlarmSec;
  $("st-slowalarm").classList.toggle("on", s.slowAlarm === true);
  $("st-packet").value = s.packetSize;
  $("st-timeout").value = s.pingTimeoutSec;
  $("ui-smsrail").classList.toggle("on", s.showSmsRail !== false);
  $("ui-engrail").classList.toggle("on", s.showEngineRail !== false);
  // Motorns språk — global serverinställning, inte per webbläsare.
  $("server-lang").value = s.lang === "en" ? "en" : "sv";
  $("wd-enabled").classList.toggle("on", s.watchdogEnabled === true);
  $("wd-fields").hidden = s.watchdogEnabled !== true;
  $("wd-port").value = s.watchdogPort;
  $("wd-grace").value = s.watchdogGraceMin;

  $("sec-list").innerHTML = secretNames.length
    ? secretNames
        .map(
          (n) =>
            `<span class="pill">${esc(n)}<button data-secdel="${esc(n)}" title="${t("common.removeTitle")}">×</button></span>`,
        )
        .join("")
    : `<p class="empty">${t("common.noneSaved")}</p>`;
}

$("st-save").addEventListener("click", async () => {
  try {
    await api.send("PUT", "/settings", {
      sweepIntervalSec: $("st-sweep").value,
      flapFailSec: $("st-fail").value,
      flapSuccessSec: $("st-success").value,
      slowThresholdMs: $("st-slow").value,
      slowAlarmSec: $("st-slowalarm-sec").value,
      packetSize: $("st-packet").value,
      pingTimeoutSec: $("st-timeout").value,
    });
    flash($("st-msg"), t("common.saved"));
  } catch (err) {
    flash($("st-msg"), String(err.message), false);
  }
});

// ---- Grupper ----------------------------------------------------------

function renderGroups(groups) {
  groupsCache = groups;
  const rows = $("group-rows");
  if (groups.length === 0) {
    rows.innerHTML = `<p class="empty">${t("settings.groups.empty")}</p>`;
    return;
  }
  rows.innerHTML = groups
    .map(
      (g) => `
      <div class="row">
        <div class="row-main">
          <div class="row-name">${esc(g.name)}</div>
          <div class="row-meta">${t("settings.groups.hosts", { n: g.hosts })}</div>
        </div>
        <div class="row-actions">
          <button class="btn btn-sm btn-danger" data-grdel="${g.id}" ${g.hosts > 0 ? `title="${t("settings.groups.deleteTitle")}"` : ""}>${t("common.remove")}</button>
        </div>
      </div>`,
    )
    .join("");
}

$("gr-add").addEventListener("click", async () => {
  const name = $("gr-new").value.trim();
  if (!name) return;
  try {
    await api.send("POST", "/groups", { name });
    $("gr-new").value = "";
    flash($("gr-msg"), t("settings.groups.created"));
    refresh();
  } catch (err) {
    flash($("gr-msg"), String(err.message), false);
  }
});

document.addEventListener("click", async (e) => {
  const d = e.target.closest("[data-grdel]");
  if (!d) return;
  if (!confirm(t("settings.groups.confirmDelete"))) return;
  await api.send("DELETE", `/groups/${d.dataset.grdel}`).catch(() => {});
  refresh();
});

// ---- Underhållsfönster -----------------------------------------------

function mwSyncForm() {
  const target = $("mw-target").value;
  const kind = $("mw-kind").value;
  $("mw-group-wrap").hidden = target !== "group";
  $("mw-hosts-wrap").hidden = target !== "hosts";
  $("mw-once-wrap").hidden = kind !== "once";
  $("mw-daily-wrap").hidden = kind !== "daily";
  $("mw-days-wrap").hidden = kind !== "daily";
}
$("mw-target").addEventListener("change", mwSyncForm);
$("mw-kind").addEventListener("change", mwSyncForm);
mwSyncForm();

function timeToMin(timeStr) {
  const [h, m] = timeStr.split(":").map(Number);
  return (h || 0) * 60 + (m || 0);
}

$("mw-add").addEventListener("click", async () => {
  const label = $("mw-label").value.trim();
  const targetKind = $("mw-target").value;
  const kind = $("mw-kind").value;

  const body = { label, kind, targetKind };
  if (targetKind === "group") body.groupId = Number($("mw-group").value) || null;
  if (targetKind === "hosts") {
    body.hostIds = [...document.querySelectorAll("#mw-hosts-wrap input:checked")].map(
      (c) => Number(c.value),
    );
    if (body.hostIds.length === 0) {
      flash($("mw-msg"), t("settings.maint.needHost"), false);
      return;
    }
  }

  if (kind === "once") {
    const s = $("mw-start").value ? new Date($("mw-start").value).getTime() : NaN;
    const e = $("mw-end").value ? new Date($("mw-end").value).getTime() : NaN;
    if (!Number.isFinite(s) || !Number.isFinite(e) || e <= s) {
      flash($("mw-msg"), t("settings.maint.badTimes"), false);
      return;
    }
    body.startsAt = s;
    body.endsAt = e;
  } else {
    body.startMin = timeToMin($("mw-time").value);
    body.durationMin = Math.max(1, Number($("mw-duration").value) || 1);
    body.days = [...document.querySelectorAll("#mw-days input:checked")].map((c) =>
      Number(c.value),
    );
  }

  try {
    await api.send("POST", "/maintenance", body);
    $("mw-label").value = "";
    flash($("mw-msg"), t("settings.maint.added"));
    refresh();
  } catch (err) {
    flash($("mw-msg"), String(err.message), false);
  }
});

function renderMaintenance(windows, hosts, groups) {
  groupsCache = groups;
  const weekdays = t("settings.maint.weekdays");

  // Fyll målvalen.
  $("mw-group").innerHTML = groups
    .map((g) => `<option value="${g.id}">${esc(g.name)}</option>`)
    .join("");
  $("mw-hosts-wrap").innerHTML = hosts.length
    ? hosts
        .map(
          (h) =>
            `<label class="check inline"><input type="checkbox" value="${h.id}"><span>${esc(h.name)}</span></label>`,
        )
        .join("")
    : `<span class="row-meta">${t("common.noDevices")}</span>`;

  const rows = $("mw-rows");
  if (windows.length === 0) {
    rows.innerHTML = `<p class="empty">${t("settings.maint.empty")}</p>`;
    return;
  }

  const dayText = (days) =>
    days.length === 0 ? t("settings.maint.daysAny") : days.map((d) => weekdays[d]).join(", ");
  const minToTime = (m) =>
    `${String(Math.floor(m / 60)).padStart(2, "0")}:${String(m % 60).padStart(2, "0")}`;

  rows.innerHTML = windows
    .map((w) => {
      const target =
        w.targetKind === "all"
          ? t("settings.maint.targetAll")
          : w.targetKind === "group"
            ? t("settings.maint.rowGroup", { name: w.group ?? "?" })
            : t("settings.maint.rowHosts", { n: w.hostIds.length });
      const schedule =
        w.kind === "once"
          ? `${clock(w.startsAt)} – ${clock(w.endsAt)}`
          : `${minToTime(w.startMin)} +${w.durationMin} min · ${dayText(w.days)}`;
      return `
      <div class="row ${w.enabled ? "" : "paused"}">
        <div class="row-main">
          <div class="row-name">${esc(w.label)}${
            w.activeNow ? ` <span class="pill pill-live">${t("settings.maint.activeNow")}</span>` : ""
          }</div>
          <div class="row-meta">${target} · ${schedule}</div>
        </div>
        <div class="row-actions">
          <button class="btn btn-sm" data-mwtoggle="${w.id}" data-on="${w.enabled}">${
            w.enabled ? t("settings.maint.disable") : t("settings.maint.enable")
          }</button>
          <button class="btn btn-sm btn-danger" data-mwdel="${w.id}">${t("common.remove")}</button>
        </div>
      </div>`;
    })
    .join("");
}

document.addEventListener("click", async (e) => {
  const tg = e.target.closest("[data-mwtoggle]");
  if (tg) {
    await api
      .send("PATCH", `/maintenance/${tg.dataset.mwtoggle}`, {
        enabled: tg.dataset.on !== "true",
      })
      .catch(() => {});
    refresh();
    return;
  }
  const d = e.target.closest("[data-mwdel]");
  if (d) {
    await api.send("DELETE", `/maintenance/${d.dataset.mwdel}`).catch(() => {});
    refresh();
  }
});

// ---- Larmkanaler -------------------------------------------------------

const CHANNELS = ["webhook", "smtp", "mqtt", "sms"];
// Kanalens hemlighet heter samma sak som kanalen.
const SECRET_FIELD = { webhook: "wh-url", smtp: "smtp-pass", mqtt: "mqtt-pass", sms: "sms-pass" };
// Vilka inställningsfält som hör till respektive kanals konfig-JSON.
const CONFIG_FIELDS = {
  webhook: () => ({}),
  smtp: () => ({
    host: $("smtp-host").value.trim(),
    port: Number($("smtp-port").value) || 587,
    security: $("smtp-security").value,
    from: $("smtp-from").value.trim(),
    to: $("smtp-to").value.trim(),
    user: $("smtp-user").value.trim(),
  }),
  mqtt: () => ({
    host: $("mqtt-host").value.trim(),
    port: Number($("mqtt-port").value) || 1883,
    clientId: $("mqtt-clientid").value.trim(),
    topic: $("mqtt-topic").value.trim(),
    user: $("mqtt-user").value.trim(),
  }),
  sms: () => {
    const recipients = $("sms-recipients").value
      .split(",")
      .map((s) => s.trim())
      .filter(Boolean);
    // Skydd: ett maskat eller felskrivet nummer ska stoppas här, inte
    // upptäckas av gatewayen vid första riktiga larmet.
    if (recipients.some((n) => !/^\+?[0-9][0-9 ()-]*$/.test(n))) {
      throw new Error(t("settings.ch.smsBadRecipient"));
    }
    return {
      host: $("sms-host").value.trim(),
      scheme: $("sms-scheme").value,
      username: $("sms-user").value.trim(),
      modemId: $("sms-modemid").value.trim(),
      timeoutMs: Number($("sms-timeout").value) || 15000,
      allowSelfSigned: smsToggles.selfsigned,
      recipients,
      messageTemplate: $("sms-template").value.trim(),
      recoveryTemplate: $("sms-recovery").value.trim(),
      escalationEnabled: smsToggles.escalation,
      escalationSec: Number($("sms-escsec").value) || 300,
      ackWindowMin: Number($("sms-ackwindow").value) || 60,
      pollSec: Number($("sms-pollsec").value) || 30,
      deleteAfterAck: smsToggles.delack,
    };
  },
};

// Lokala reglage på SMS-kortet. Laddas från servern tills användaren
// rör dem — därefter styr utkastet, precis som i desktopvarianten.
const smsToggles = { selfsigned: true, escalation: false, delack: false };
let smsDirty = false;

function paintSmsToggles() {
  $("sms-selfsigned").classList.toggle("on", smsToggles.selfsigned);
  $("sms-escalation").classList.toggle("on", smsToggles.escalation);
  $("sms-delack").classList.toggle("on", smsToggles.delack);
  $("sms-esc-settings").hidden = !smsToggles.escalation;
  // Desktopvarningen: utan {id} i mallen vet mottagaren inte vad den
  // ska svara med.
  $("sms-id-warning").hidden =
    !smsToggles.escalation || $("sms-template").value.includes("{id}");
}

$("sms-selfsigned").addEventListener("click", () => {
  smsToggles.selfsigned = !smsToggles.selfsigned;
  smsDirty = true;
  paintSmsToggles();
});
$("sms-escalation").addEventListener("click", () => {
  smsToggles.escalation = !smsToggles.escalation;
  smsDirty = true;
  paintSmsToggles();
});
$("sms-delack").addEventListener("click", () => {
  smsToggles.delack = !smsToggles.delack;
  smsDirty = true;
  paintSmsToggles();
});
$("sms-template").addEventListener("input", paintSmsToggles);

const CH_PREFIX = { webhook: "wh", smtp: "smtp", mqtt: "mqtt", sms: "sms" };

function channelActive(name) {
  return (settingsCache?.channels ?? []).includes(name);
}

async function setChannelActive(name, on) {
  const current = new Set(settingsCache?.channels ?? []);
  if (on) current.add(name);
  else current.delete(name);
  await api.send("PUT", "/settings", { channels: [...current].join(", ") });
}

async function renderChannels(settings, secretNames) {
  settingsCache = settings;

  for (const name of CHANNELS) {
    const p = CH_PREFIX[name];
    const toggle = $(`${p}-toggle`);
    toggle.classList.toggle("on", channelActive(name));

    const hasSecret = secretNames.includes(name);
    // Hämta kanalens konfiguration (admin-endpoint).
    let configured = hasSecret;
    try {
      const cfg = await api.get(`/channels/${name}/config`);
      configured = configured || Object.keys(cfg).length > 0;
      // Fyll bara i fält om ingen skriver i dem just nu.
      if (document.activeElement?.tagName !== "INPUT") {
        if (name === "smtp" && cfg.host != null) {
          $("smtp-host").value = cfg.host ?? "";
          $("smtp-port").value = cfg.port ?? 587;
          $("smtp-security").value = cfg.security ?? "starttls";
          $("smtp-from").value = cfg.from ?? "";
          $("smtp-to").value = cfg.to ?? "";
          $("smtp-user").value = cfg.user ?? "";
        }
        if (name === "mqtt" && cfg.host != null) {
          $("mqtt-host").value = cfg.host ?? "";
          $("mqtt-port").value = cfg.port ?? 1883;
          $("mqtt-clientid").value = cfg.clientId ?? "";
          $("mqtt-topic").value = cfg.topic ?? "";
          $("mqtt-user").value = cfg.user ?? "";
        }
        if (name === "sms" && cfg.host != null && !smsDirty) {
          $("sms-host").value = cfg.host ?? "";
          $("sms-scheme").value = cfg.scheme ?? "https";
          $("sms-timeout").value = cfg.timeoutMs ?? 15000;
          $("sms-user").value = cfg.username ?? "";
          $("sms-modemid").value = cfg.modemId ?? "";
          $("sms-recipients").value = (cfg.recipients ?? []).join(", ");
          $("sms-template").value = cfg.messageTemplate ?? "";
          $("sms-recovery").value = cfg.recoveryTemplate ?? "";
          $("sms-escsec").value = cfg.escalationSec ?? 300;
          $("sms-ackwindow").value = cfg.ackWindowMin ?? 60;
          $("sms-pollsec").value = cfg.pollSec ?? 30;
          smsToggles.selfsigned = cfg.allowSelfSigned ?? true;
          smsToggles.escalation = cfg.escalationEnabled ?? false;
          smsToggles.delack = cfg.deleteAfterAck ?? false;
          paintSmsToggles();
        }
      }
    } catch {
      /* saknar behörighet eller kanal — lämna fälten */
    }
    $(`${p}-status`).textContent = configured ? t("common.configured") : t("common.notConfigured");
  }
}

document.querySelectorAll(
  "#sms-host, #sms-scheme, #sms-timeout, #sms-user, #sms-modemid, #sms-recipients, #sms-template, #sms-recovery, #sms-escsec, #sms-ackwindow, #sms-pollsec",
).forEach((el) => el.addEventListener("input", () => { smsDirty = true; }));

for (const name of CHANNELS) {
  const p = CH_PREFIX[name];

  $(`${p}-toggle`).addEventListener("click", async () => {
    try {
      await setChannelActive(name, !channelActive(name));
      refresh();
    } catch (err) {
      flash($(`${p}-msg`), String(err.message), false);
    }
  });

  $(`${p}-save`).addEventListener("click", async () => {
    try {
      await api.send("PUT", `/channels/${name}/config`, CONFIG_FIELDS[name]());
      const secret = $(SECRET_FIELD[name]).value;
      if (secret) {
        await api.send("PUT", `/secrets/${name}`, { value: secret });
        $(SECRET_FIELD[name]).value = "";
      }
      // En nysparad kanal slås på direkt — samma beteende som desktop.
      if (!channelActive(name)) await setChannelActive(name, true);
      if (name === "sms") smsDirty = false;
      flash($(`${p}-msg`), t("common.saved"));
      refresh();
    } catch (err) {
      flash($(`${p}-msg`), String(err.message), false);
    }
  });

  $(`${p}-test`).addEventListener("click", async () => {
    const msg = $(`${p}-msg`);
    msg.textContent = t("settings.ch.testing");
    msg.className = "msg";
    try {
      const r = await api.send("POST", `/channels/${name}/test`);
      flash(msg, r.ok ? t("settings.ch.testOk") : t("settings.ch.testFail", { error: r.error }), r.ok);
    } catch (err) {
      flash(msg, String(err.message), false);
    }
  });
}

// ---- SMS: verifiering, gateway-status och sessioner -------------------
// Speglar desktopens TrbChannelCard: behörighetskontrollen kostar inga
// SMS, statusen läses på begäran, och sessionslistan visar eskaleringar.

$("sms-verify").addEventListener("click", async () => {
  const box = $("sms-verify-result");
  box.hidden = false;
  box.innerHTML = `<span class="field-hint">${t("settings.ch.smsVerifying")}</span>`;
  try {
    const v = await api.send("POST", "/channels/sms/verify");
    const row = (ok, label) =>
      `<div class="verify-row"><span class="pill ${ok ? "ok" : "alarm"}">${ok ? "✓" : "✗"}</span> ${label}</div>`;
    box.innerHTML =
      row(v.loginOk, t("settings.ch.smsVerifyLogin")) +
      row(v.modemsOk, t("settings.ch.smsVerifyModems")) +
      row(v.messagesReadOk, t("settings.ch.smsVerifyRead")) +
      row(v.registered, t("settings.ch.smsVerifyRegistered")) +
      (v.modemId ? `<div class="field-hint">${t("settings.ch.smsModemIdLabel", { id: v.modemId })}</div>` : "") +
      (v.error ? `<div class="field-hint warn">${v.error}</div>` : "") +
      (!v.messagesReadOk && v.loginOk
        ? `<div class="field-hint">${t("settings.ch.smsVerifyAclHint")}</div>`
        : "");
  } catch (err) {
    box.innerHTML = `<span class="field-hint warn">${err.message}</span>`;
  }
});

function renderSmsStatus(s) {
  const box = $("sms-gateway-status");
  if (s.error) {
    box.innerHTML = `<span class="field-hint warn">${s.error}</span>`;
    return;
  }
  const item = (label, value) =>
    value == null || value === "" ? "" : `<div class="status-item"><span>${label}</span><b>${value}</b></div>`;
  box.innerHTML =
    item(t("settings.ch.smsStOperator"), s.operator) +
    item(t("settings.ch.smsStState"), s.operatorState) +
    item(t("settings.ch.smsStConn"), s.connType ?? s.networkType) +
    item(t("settings.ch.smsStSignal"), s.signalQuality != null ? `${s.signalQuality} %` : s.rssi != null ? `${s.rssi} dBm` : null) +
    item(t("settings.ch.smsStSim"), s.simState) +
    item(t("settings.ch.smsStModem"), s.modemId) +
    item(t("settings.ch.smsStTemp"), s.temperature != null ? `${s.temperature} °C` : null) ||
    `<span class="field-hint">${t("settings.ch.smsNoStatus")}</span>`;
}

async function loadSmsStatus() {
  try {
    renderSmsStatus(await api.get("/channels/sms/status"));
  } catch {
    /* saknar behörighet — lämna rutan */
  }
}

$("sms-refresh-status").addEventListener("click", loadSmsStatus);

function renderSmsSessions(list) {
  const box = $("sms-sessions");
  if (!list.length) {
    box.innerHTML = `<span class="field-hint">${t("settings.ch.smsNoSessions")}</span>`;
    return;
  }
  box.innerHTML = list
    .map((s) => {
      const state = !s.closed
        ? `<span class="pill alarm">${t("settings.ch.sessOpen")}</span>`
        : s.closedReason === "ack"
          ? `<span class="pill ok">${t("settings.ch.sessAck")}${s.ackedBy ? t("settings.ch.sessAckBy", { by: s.ackedBy }) : ""}</span>`
          : `<span class="pill">${dictText(`settings.ch.sess${cap(s.closedReason)}`, t("settings.ch.sessClosed"))}</span>`;
      const when = new Date(s.createdAt).toLocaleString(I18N.locale(), {
        month: "short", day: "numeric", hour: "2-digit", minute: "2-digit",
      });
      return `<div class="session-row">
        <span class="session-device">${s.device}</span>
        <span class="pill">[${s.id}]</span>
        <span class="field-hint">${t("settings.ch.sessSent", { sent: s.sentCount, total: s.recipients.length })}</span>
        ${state}
        <span class="field-hint">${when}</span>
      </div>`;
    })
    .join("");
}

function cap(s) {
  return s ? s.charAt(0).toUpperCase() + s.slice(1) : s;
}

async function loadSmsExtras() {
  try {
    renderSmsSessions(await api.get("/sms/sessions"));
  } catch {
    /* icke-admin eller gammal server — lämna listan */
  }
  // Gateway-statusen hämtas bara om rutan ännu inte visar något —
  // annars får användaren trycka Uppdatera (samma som desktop).
  if ($("sms-gateway-status").dataset.loaded !== "1") {
    await loadSmsStatus();
    $("sms-gateway-status").dataset.loaded = "1";
  }
}

// ---- SMS-gateway i sidopanelen -----------------------------------------
//
// Kortversionen av gateway-statusen från Larm-fliken, för den som vill
// ha den synlig hela tiden. Visas/döljs via Inställningar → System och
// är en global serverinställning (showSmsRail) — inte per webbläsare.
//
// Statusen pollar på en egen, långsam timer: varje anrop loggar in mot
// gatewayen, så den ska inte hänga på översiktens 5-sekundersloop.

const SMS_RAIL_MS = 30000;
let smsRailTimer = null;

function applyRailVisibility() {
  // Tills inställningarna laddats hålls SMS-rutan dold — ett kort glapp
  // vid inloggning är bättre än att den blinkar till för den som stängt
  // av den. Motorkortet däremot är standardläget och får vara synligt
  // direkt; det döljs först när inställningen säger annat.
  const sms = settingsCache != null && settingsCache.showSmsRail !== false;
  $("card-sms").hidden = !sms;
  if (sms) loadSmsRail();
  if (settingsCache != null) {
    $("card-engine").hidden = settingsCache.showEngineRail === false;
  }
}

async function loadSmsRail() {
  const card = $("card-sms");
  if (card.hidden) return;
  const led = $("sms-led");
  try {
    const s = await api.get("/channels/sms/status");
    if (s.error) throw new Error(s.error);
    led.className = "led online";
    card.className = "rail-card online";
    card.title = "";
    $("sms-rail-state").textContent = t("rail.smsConnected");
    $("sms-rail-op").textContent = s.operator || t("rail.smsUnknownOperator");
    $("sms-rail-signal").textContent =
      s.signalQuality != null ? t("rail.smsSignal", { n: s.signalQuality }) : s.rssi != null ? `${s.rssi} dBm` : "";
  } catch (err) {
    const msg = String(err.message ?? "");
    led.className = "led offline";
    card.className = "rail-card offline";
    card.title = msg; // hela felet som tooltip — metaraden är för smal
    // Serverns feltext är svensk oavsett UI-språk — matcha brett.
    $("sms-rail-state").textContent = /konfigurer/i.test(msg) ? t("rail.smsNotConfigured") : t("rail.smsNotResponding");
    $("sms-rail-op").textContent = "–";
    $("sms-rail-signal").textContent = "";
  }
}

$("ui-smsrail").addEventListener("click", async () => {
  // Inställningen är global och sparas på servern — svaret innehåller
  // det nya läget, så cache och kort kan uppdateras direkt.
  const next = settingsCache?.showSmsRail === false;
  try {
    const s = await api.send("PUT", "/settings", { showSmsRail: next ? "1" : "0" });
    settingsCache = s;
    $("ui-smsrail").classList.toggle("on", s.showSmsRail !== false);
    applyRailVisibility();
  } catch (err) {
    alert(String(err.message));
  }
});

$("ui-engrail").addEventListener("click", async () => {
  const next = settingsCache?.showEngineRail === false;
  try {
    const s = await api.send("PUT", "/settings", { showEngineRail: next ? "1" : "0" });
    settingsCache = s;
    $("ui-engrail").classList.toggle("on", s.showEngineRail !== false);
    applyRailVisibility();
  } catch (err) {
    alert(String(err.message));
  }
});

// Motorns språk — globalt, sparas på servern (settings-nyckeln "lang").
$("server-lang").addEventListener("change", async () => {
  try {
    const s = await api.send("PUT", "/settings", { lang: $("server-lang").value });
    settingsCache = s;
    $("server-lang").value = s.lang === "en" ? "en" : "sv";
  } catch (err) {
    alert(String(err.message));
  }
});

// Latenslarmet (etapp 8): varning när enheten svarar långsamt
// TILLRÄCKLIGT LÄNGE — inte vid enstaka jittertoppar. Eskalerar
// aldrig via SMS, det är ett varningslarm.
$("st-slowalarm").addEventListener("click", async () => {
  const next = settingsCache?.slowAlarm !== true;
  try {
    const s = await api.send("PUT", "/settings", { slowAlarm: next ? "1" : "0" });
    settingsCache = s;
    $("st-slowalarm").classList.toggle("on", s.slowAlarm === true);
  } catch (err) {
    alert(String(err.message));
  }
});

// Vakthunden (desktopens heartbeat): porten svarar bara medan
// bevakningen pågår, så en extern vakthund ser om servern faktiskt
// övervakar — inte bara att den är igång.
$("wd-enabled").addEventListener("click", async () => {
  const next = settingsCache?.watchdogEnabled !== true;
  try {
    const s = await api.send("PUT", "/settings", { watchdogEnabled: next ? "1" : "0" });
    settingsCache = s;
    $("wd-enabled").classList.toggle("on", s.watchdogEnabled === true);
    $("wd-fields").hidden = s.watchdogEnabled !== true;
  } catch (err) {
    alert(String(err.message));
  }
});

$("wd-save").addEventListener("click", async () => {
  try {
    const s = await api.send("PUT", "/settings", {
      watchdogPort: $("wd-port").value,
      watchdogGraceMin: $("wd-grace").value,
    });
    settingsCache = s;
    // Visa det servern faktiskt sparade (klampade värden).
    $("wd-port").value = s.watchdogPort;
    $("wd-grace").value = s.watchdogGraceMin;
    flash($("wd-msg"), t("common.saved"));
  } catch (err) {
    flash($("wd-msg"), String(err.message), false);
  }
});

// ---- Export och import -------------------------------------------------

$("xp-export").addEventListener("click", async () => {
  try {
    const data = await api.get("/export");
    const blob = new Blob([JSON.stringify(data, null, 2)], { type: "application/json" });
    const a = document.createElement("a");
    a.href = URL.createObjectURL(blob);
    a.download = `netfyr-export-${new Date().toISOString().slice(0, 10)}.json`;
    a.click();
    URL.revokeObjectURL(a.href);
    flash($("xp-msg"), t("settings.sys.exported"));
  } catch (err) {
    flash($("xp-msg"), String(err.message), false);
  }
});

$("xp-import").addEventListener("click", () => $("xp-file").click());
$("xp-file").addEventListener("change", async (e) => {
  const file = e.target.files[0];
  if (!file) return;
  try {
    const data = JSON.parse(await file.text());
    const r = await api.send("POST", "/import", data);
    flash($("xp-msg"), t("settings.sys.imported", { imported: r.imported, skipped: r.skipped }));
    refresh();
  } catch (err) {
    flash($("xp-msg"), String(err.message), false);
  }
  e.target.value = "";
});

$("sec-save").addEventListener("click", async () => {
  const name = $("sec-name").value.trim();
  const value = $("sec-value").value;
  if (!name || !value) {
    flash($("sec-msg"), t("settings.ch.secretNeedBoth"), false);
    return;
  }
  try {
    await api.send("PUT", `/secrets/${encodeURIComponent(name)}`, { value });
    $("sec-value").value = "";
    flash($("sec-msg"), t("common.saved"));
    refresh();
  } catch (err) {
    flash($("sec-msg"), String(err.message), false);
  }
});

document.addEventListener("click", async (e) => {
  const d = e.target.closest("[data-secdel]");
  if (!d) return;
  await api.send("DELETE", `/secrets/${encodeURIComponent(d.dataset.secdel)}`).catch(() => {});
  refresh();
});

// ---- Användare (admin) ---------------------------------------------

function renderUsers(users, auditRows) {
  const rows = $("user-rows");
  rows.innerHTML = users.length
    ? users
        .map(
          (u) => `
      <div class="row ${u.disabled ? "paused" : ""}">
        <div class="row-main">
          <div class="row-name">${esc(u.username)} <span class="user-role">${esc(u.role)}</span></div>
          <div class="row-meta">${u.disabled ? `${t("settings.users.disabled")} · ` : ""}${
            u.mustChangePassword ? `${t("settings.users.oneTimeTag")} · ` : ""
          }${u.lastLoginAt ? t("settings.users.lastLogin", { time: clock(u.lastLoginAt) }) : t("settings.users.neverLoggedIn")}</div>
        </div>
        <div class="row-actions">
          <button class="btn btn-sm" data-urole="${u.id}" data-role="${u.role === "admin" ? "user" : "admin"}">${t("settings.users.makeRole", { role: u.role === "admin" ? "user" : "admin" })}</button>
          <button class="btn btn-sm" data-ureset="${u.id}">${t("settings.users.resetPw")}</button>
          <button class="btn btn-sm" data-udisable="${u.id}" data-on="${u.disabled}">${u.disabled ? t("settings.users.activate") : t("settings.users.deactivate")}</button>
          <button class="btn btn-sm btn-danger" data-udel="${u.id}">${t("common.remove")}</button>
        </div>
      </div>`,
        )
        .join("")
    : `<p class="empty">${t("settings.users.empty")}</p>`;

  $("audit").innerHTML = auditRows.length
    ? auditRows
        .map((a) => {
          const what = dictText(`settings.users.actions.${a.action}`, a.action);
          const target = a.target ? ` ${esc(a.target)}` : "";
          const detail = a.detail ? ` — ${esc(a.detail)}` : "";
          const ip = a.ip ? ` · ${esc(a.ip)}` : "";
          const lv = a.action === "login_fail" ? "warn" : "info";
          return `<div><time>${clock(a.ts)}</time><span class="lv-${lv}"><b>${esc(a.username)}</b> ${what}${target}${detail}${ip}</span></div>`;
        })
        .join("")
    : `<p class="empty">${t("common.nothingLogged")}</p>`;
}

$("nu-add").addEventListener("click", async () => {
  const username = $("nu-name").value.trim();
  const password = $("nu-pass").value;
  const role = $("nu-role").value;
  if (!username || !password) {
    flash($("nu-msg"), t("settings.users.needBoth"), false);
    return;
  }
  try {
    await api.send("POST", "/users", { username, password, role });
    $("nu-name").value = "";
    $("nu-pass").value = "";
    flash($("nu-msg"), t("settings.users.created"));
    refresh();
  } catch (err) {
    flash($("nu-msg"), String(err.message), false);
  }
});

document.addEventListener("click", async (e) => {
  const urole = e.target.closest("[data-urole]");
  if (urole) {
    await api.send("PATCH", `/users/${urole.dataset.urole}`, { role: urole.dataset.role }).catch((err) => alert(err.message));
    refresh();
    return;
  }
  const ureset = e.target.closest("[data-ureset]");
  if (ureset) {
    const pw = prompt(t("settings.users.resetPrompt"));
    if (!pw) return;
    await api.send("PATCH", `/users/${ureset.dataset.ureset}`, { password: pw }).catch((err) => alert(err.message));
    refresh();
    return;
  }
  const udis = e.target.closest("[data-udisable]");
  if (udis) {
    const disabled = udis.dataset.on !== "true";
    await api.send("PATCH", `/users/${udis.dataset.udisable}`, { disabled }).catch((err) => alert(err.message));
    refresh();
    return;
  }
  const udel = e.target.closest("[data-udel]");
  if (udel) {
    if (!confirm(t("settings.users.confirmDelete"))) return;
    await api.send("DELETE", `/users/${udel.dataset.udel}`).catch((err) => alert(err.message));
    refresh();
  }
});

// ---- Mitt konto -------------------------------------------------------

$("pw-save").addEventListener("click", async () => {
  const current = $("pw-current").value;
  const pw1 = $("pw-new").value;
  const pw2 = $("pw-new2").value;
  if (pw1 !== pw2) {
    flash($("pw-msg"), t("settings.account.mismatch"), false);
    return;
  }
  try {
    await api.send("POST", "/auth/password", { current, newPassword: pw1 });
    me.mustChangePassword = false;
    $("must-change-card").hidden = true;
    $("pw-current").value = "";
    $("pw-new").value = "";
    $("pw-new2").value = "";
    flash($("pw-msg"), t("settings.account.changed"));
  } catch (err) {
    flash($("pw-msg"), String(err.message), false);
  }
});

// ---- Uppdatering -----------------------------------------------------

async function refresh() {
  try {
    // Översikten hämtas alltid: sidopanelens lägen, statuskortet och
    // tidsstämpeln gäller oavsett vilken vy som visas.
    renderOverview(await api.get("/overview"));

    if (view === "terminal") {
      const [ev, dl] = await Promise.all([
        api.get("/events?limit=150"),
        api.get("/deliveries?limit=60"),
      ]);
      renderLog(ev, dl);
    }

    if (view === "settings" && tab === "devices") {
      const [hosts, groups] = await Promise.all([api.get("/hosts"), api.get("/groups")]);
      groupsCache = groups;
      renderDevices(hosts);
    }

    if (view === "settings" && tab === "groups") {
      renderGroups(await api.get("/groups"));
    }

    if (view === "settings" && tab === "maintenance") {
      const [mw, hosts, groups] = await Promise.all([
        api.get("/maintenance"),
        api.get("/hosts"),
        api.get("/groups"),
      ]);
      renderMaintenance(mw.windows, hosts, groups);
    }

    if (view === "settings" && tab === "channels") {
      const [s, sec] = await Promise.all([api.get("/settings"), api.get("/secrets")]);
      renderSettings(s, sec.names, null);
      await renderChannels(s, sec.names);
      loadSmsExtras();
    }

    if (view === "settings" && tab === "system") {
      const [s, sec, health] = await Promise.all([
        api.get("/settings"),
        api.get("/secrets"),
        api.get("/health").catch(() => null),
      ]);
      settingsCache = s;
      renderSettings(s, sec.names, health);
    }

    if (view === "settings" && tab === "users" && me?.role === "admin") {
      const [users, auditRows] = await Promise.all([
        api.get("/users"),
        api.get("/audit?limit=200"),
      ]);
      renderUsers(users, auditRows);
    }
  } catch (err) {
    if (!me) return; // utloggad — inloggningen visas redan
    $("verdict").className = "verdict alarm";
    $("verdict-state").textContent = t("dash.noContactTitle");
    $("verdict-detail").textContent = String(err.message);
    $("engine-led").className = "led offline";
    $("engine-state").textContent = t("rail.engineNoContact");
  }
}

boot();
