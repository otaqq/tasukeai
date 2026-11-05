// tasukeai admin page: reads the JSON API (the same calls as the CLI and other callers) and draws it.
// Read-only. With an API key on the server, the key typed here is kept in this tab (sessionStorage) only.
"use strict";

const $ = (id) => document.getElementById(id);
const KEY = "tasukeai-api-key";
let timer = null;
let typed = ""; // the key when the browser keeps no session storage

function key() {
  try { return sessionStorage.getItem(KEY) || typed; } catch { return typed; }
}

async function get(path) {
  const headers = key() ? { Authorization: "Bearer " + key() } : {};
  const r = await fetch(path, { headers });
  if (r.status === 401) throw Object.assign(new Error("API key needed"), { auth: true });
  if (!r.ok) throw new Error(path + ": " + r.status);
  return r.json();
}

function el(tag, attrs = {}, ...kids) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k === "class") e.className = v; else e.setAttribute(k, v);
  }
  for (const k of kids) e.append(k instanceof Node ? k : document.createTextNode(String(k)));
  return e;
}

function when(t, now) {
  if (t == null) return "until removed";
  const s = t - now;
  if (s <= 0) return "now";
  if (s < 120) return `in ${s} s`;
  if (s < 7200) return `in ${Math.round(s / 60)} min`;
  return `in ${(s / 3600).toFixed(1)} h`;
}

function table(t, head, rows, empty) {
  t.replaceChildren();
  if (!rows.length) {
    t.append(el("tr", {}, el("td", { class: "empty" }, empty)));
    return;
  }
  t.append(el("tr", {}, ...head.map((h) => el("th", {}, h))));
  for (const r of rows) t.append(r);
}

function drawJobs(jobs, assessments) {
  const box = $("jobs");
  box.replaceChildren();
  jobs.forEach((j, i) => {
    const a = assessments[i];
    const card = el("div", { class: "job" }, el("h3", {}, j.name, " ", el("small", {}, j.mode)));
    if (!a) {
      card.append(el("p", { class: "reason" }, "could not assess"));
    } else {
      const usable = el("div");
      for (const c of a.usable) {
        const slots = j.mode === "rank" ? "" : ` (${c.free_slots} free)`;
        usable.append(el("span", { class: "chip ok" }, c.machine + slots));
      }
      if (!a.usable.length) usable.append(el("span", { class: "chip bad" }, "no machine now"));
      card.append(usable);
      for (const r of a.rejected) {
        card.append(el("p", { class: "reason" }, el("b", {}, r.machine), ": " + r.reasons.join("; ")));
      }
    }
    box.append(card);
  });
}

// History for the small lines, kept in this tab only: machine -> signal -> [[collected at, value], ...], one point
// per collection (the same collected-at is not added twice), the last HISTORY_SECONDS only. About 50 bytes a point:
// 75 signals x 240 points (one hour, collected every 15 s) is about 1 MB.
const HISTORY_SECONDS = 3600;
const history = {};

function remember(m) {
  for (const [machine, signals] of Object.entries(m.snapshot || {})) {
    const h = (history[machine] ||= {});
    for (const [signal, [value, at]] of Object.entries(signals)) {
      const pts = (h[signal] ||= []);
      if (!pts.length || pts[pts.length - 1][0] !== at) pts.push([at, value]);
      while (pts.length && pts[0][0] < m.now - HISTORY_SECONDS) pts.shift();
    }
  }
}

const SVG = "http://www.w3.org/2000/svg";
const fmt = (v) => (Number.isInteger(v) ? String(v) : v.toFixed(2));

// A line over the last hour, scaled to its own minimum and maximum; with fewer than 2 points, nothing.
function sparkline(pts, now) {
  if (!pts || pts.length < 2) return el("span", { class: "spark" });
  const w = 80, h = 18, values = pts.map((p) => p[1]);
  const lo = Math.min(...values), hi = Math.max(...values);
  const x = (t) => (w * (t - (now - HISTORY_SECONDS))) / HISTORY_SECONDS;
  const y = (v) => (hi === lo ? h / 2 : h - 1 - ((h - 2) * (v - lo)) / (hi - lo));
  const svg = document.createElementNS(SVG, "svg");
  svg.setAttribute("class", "spark");
  svg.setAttribute("viewBox", `0 0 ${w} ${h}`);
  svg.setAttribute("width", w);
  svg.setAttribute("height", h);
  const line = document.createElementNS(SVG, "polyline");
  line.setAttribute("points", pts.map(([t, v]) => `${x(t).toFixed(1)},${y(v).toFixed(1)}`).join(" "));
  svg.append(line);
  const title = document.createElementNS(SVG, "title");
  const mins = Math.round((now - pts[0][0]) / 60);
  title.textContent = `last ${mins} min: min ${fmt(lo)}, max ${fmt(hi)}, now ${fmt(values[values.length - 1])}`;
  svg.append(title);
  return svg;
}

function drawMachines(m, stale) {
  const snap = m.snapshot || {};
  const names = Object.keys(snap).sort();
  const signals = [...new Set(names.flatMap((n) => Object.keys(snap[n])))].sort();
  const rows = signals.map((s) =>
    el("tr", {}, el("th", {}, s), ...names.map((n) => {
      const v = snap[n][s];
      if (!v) return el("td", { class: "num muted" }, "–");
      const age = Math.max(0, m.now - v[1]);
      return el("td", { class: "num" + (age > stale ? " stale" : ""), title: `${age} s old` },
        sparkline((history[n] || {})[s], m.now), fmt(v[0]), el("span", { class: "muted" }, ` ${age}s`));
    })));
  const t = $("machines");
  t.replaceChildren();
  if (!names.length) {
    t.append(el("tr", {}, el("td", { class: "empty" }, "no signals yet")));
  } else {
    t.append(el("tr", {}, el("th", {}, "signal"), ...names.map((n) => el("th", {}, n))));
    for (const r of rows) t.append(r);
  }
  const errors = m.errors || [];
  $("errors").replaceChildren(...errors.map((e) => el("li", {}, e)));
  $("errorsum").textContent = `${errors.length} signal(s) could not be collected`;
  $("errorbox").hidden = !errors.length;
}

async function refresh() {
  try {
    const [jobs, machines, leases, holds] = await Promise.all([get("jobs"), get("machines"), get("leases"), get("holds")]);
    const assessments = await Promise.all(
      jobs.jobs.map((j) => get("assess?job=" + encodeURIComponent(j.name)).catch(() => null)));
    const now = machines.now;
    drawJobs(jobs.jobs, assessments);
    remember(machines);
    drawMachines(machines, jobs.stale_after_seconds);
    table($("leases"), ["machine", "job", "ends", "endpoint", "id"],
      leases.map((l) => el("tr", {}, el("td", {}, l.machine), el("td", {}, l.job), el("td", {}, when(l.until, now)),
        el("td", {}, l.endpoint || ""), el("td", { class: "muted" }, l.id))), "no leases");
    table($("holds"), ["machine", "job", "ends", "note", "id"],
      holds.map((h) => el("tr", {}, el("td", {}, h.machine), el("td", {}, h.job || "every job"),
        el("td", {}, when(h.until, now)), el("td", {}, h.note || ""), el("td", { class: "muted" }, h.id))), "no holds");
    $("keyform").hidden = true;
    $("status").textContent = "updated " + new Date().toLocaleTimeString();
  } catch (e) {
    if (e.auth) $("keyform").hidden = false;
    $("status").textContent = e.message;
  }
}

function schedule() {
  clearInterval(timer);
  if ($("auto").checked) timer = setInterval(refresh, 5000);
}

$("auto").addEventListener("change", schedule);
$("keyform").addEventListener("submit", (ev) => {
  ev.preventDefault();
  typed = $("key").value.trim();
  try { sessionStorage.setItem(KEY, typed); } catch { /* no storage: kept in memory until the tab closes */ }
  $("key").value = "";
  refresh();
});
refresh();
schedule();
