// Sonos Bridge control panel - polls the daemon state and posts commands.
const $ = (id) => document.getElementById(id);
const state = { mode: "off", players: [], active: [], wanted: [], error: "", note: "" };

async function getJSON(path) {
  const r = await fetch(path, { cache: "no-store" });
  const text = await r.text();
  try { return { ok: r.ok, text, data: JSON.parse(text) }; }
  catch (e) { return { ok: r.ok, text, data: null }; }
}
async function post(path) {
  const r = await fetch(path, { method: "POST" });
  const text = await r.text();
  return { ok: r.ok, text: text.trim() };
}

function banner(text, kind) {
  const b = $("banner");
  if (!text) { b.className = "banner"; b.textContent = ""; return; }
  b.className = "banner show " + (kind || "info");
  b.textContent = text;
}

function setModeButtons(mode) {
  document.querySelectorAll("#routes button").forEach((b) => {
    b.classList.toggle("active", b.dataset.mode === mode);
  });
}

function renderPlayers() {
  const tb = $("players").querySelector("tbody");
  tb.innerHTML = "";
  const dl = $("playerlist");
  dl.innerHTML = "";
  for (const p of state.players) {
    const playing = (state.active || []).includes(p.udn);
    const tr = document.createElement("tr");
    if (playing) tr.className = "playing";
    tr.innerHTML =
      `<td>${playing ? '<span class="ok-dot">&#9654;</span>' : ""}</td>` +
      `<td><b>${esc(p.name || "(unnamed)")}</b></td>` +
      `<td class="mono">${esc(p.ip)}:${p.port}</td>` +
      `<td>${p.members > 1
        ? `<span class="badge ${p.coordinator ? "co" : "slave"}">${p.coordinator ? "coordinator of " + p.members : "grouped"}</span>`
        : '<span class="badge">standalone</span>'}</td>` +
      `<td>${p.volume === null || p.volume === undefined ? "-" : p.volume}</td>` +
      `<td><button data-play="${esc(p.name || p.udn)}">${playing ? "Reconnect" : "Play here"}</button></td>`;
    tb.appendChild(tr);
    const opt = document.createElement("option");
    opt.value = p.name || p.udn;
    dl.appendChild(opt);
  }
  tb.querySelectorAll("button[data-play]").forEach((b) => {
    b.onclick = () => {
      $("player").value = b.dataset.play;
      route(state.mode === "off" ? "sonos" : state.mode);
    };
  });
  $("scanned").textContent = state.players.length
    ? `${state.players.length} speaker(s) found`
    : "none found";
}

function esc(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}

function renderState() {
  const s = state;
  const lines = [
    `mode            ${s.mode}`,
    `output device   ${s.sink} "${s.sink_description}" ${s.sink_loaded ? "registered" : "not registered"}`,
    `local path      ${s.local_kind} -> ${s.hw_sink || "(none)"}`,
    `sonos           ${s.sonos_state} ${s.active && s.active.length ? "-> " + s.active.join(", ") : ""}`,
    `stream          ${s.url}  (${s.stream_format}, port ${s.stream_port})`,
    `capture         backend=${s.capture_backend} pid=${s.capture_pid}  dropped=${s.dropped_bytes}B`,
    `encoder         pid=${s.encoder_pid} restarts=${s.enc_restarts}  clients=${s.stream_clients}`,
    `counters        captured=${s.bytes_captured}B streamed=${s.bytes_streamed}B plays=${s.sonos_plays}`,
    `uptime          ${s.uptime_s}s`,
  ];
  if (s.note) lines.push(`note            ${s.note}`);
  if (s.error) lines.push(`ERROR           ${s.error}`);
  $("state").textContent = lines.join("\n");
  $("foot").textContent = `sonos-bridge ${s.version} - daemon ${s.uptime_s}s - ${s.mode}`;
}

async function poll() {
  const res = await getJSON("/api/state");
  if (!res.ok || !res.data) { banner("cannot reach the daemon (is it running?)", "err"); return; }
  Object.assign(state, res.data);
  setModeButtons(state.mode);
  const live = $("live");
  live.className = "dot" + (state.error ? " err" : (state.mode !== "off" ? " on" : ""));
  const peak = Math.max(0, Math.min(1, state.peak || 0));
  $("meter").style.width = Math.round(peak * 100) + "%";
  $("metertxt").textContent = state.audio_active
    ? `audio ${Math.round(peak * 100)}%`
    : "silence";
  banner(state.error ? "Error: " + state.error : (state.wanted && state.wanted.length === 0 && (state.mode === "sonos" || state.mode === "both") ? "No speaker selected - type one and press Apply." : ""),
    state.error ? "err" : "info");
  renderPlayers();
  renderState();
}

async function route(mode) {
  const p = $("player").value.trim();
  const url = `/api/route?mode=${encodeURIComponent(mode)}${p ? "&player=" + encodeURIComponent(p) : ""}`;
  const r = await post(url);
  banner(r.text, r.ok ? "info" : "err");
  poll();
}

async function scan() {
  $("scan").disabled = true;
  $("scanned").textContent = "scanning...";
  const r = await post("/api/discover");
  $("scan").disabled = false;
  if (!r.ok) banner(r.text, "err");
  await poll();
}

async function setVolume() {
  const v = $("vol").value;
  const r = await post(`/api/volume?v=${encodeURIComponent(v)}`);
  banner(r.text, r.ok ? "info" : "err");
  poll();
}

async function loadSinks() {
  const res = await getJSON("/api/sinks");
  const sel = $("hw");
  sel.innerHTML = "";
  const auto = document.createElement("option");
  auto.value = "auto";
  auto.textContent = "auto (current default output)";
  sel.appendChild(auto);
  if (res.data) {
    for (const s of res.data) {
      if (s.name === state.sink) continue;
      const o = document.createElement("option");
      o.value = s.name;
      o.textContent = `${s.description || s.name}${s.is_default ? "  (default)" : ""}`;
      sel.appendChild(o);
    }
    if (state.hw_sink) sel.value = state.hw_sink;
  }
  sel.onchange = async () => {
    const r = await post(`/api/cmd?line=${encodeURIComponent("hw " + sel.value)}`);
    banner(r.text, r.ok ? "info" : "err");
    poll();
  };
}

async function runChecks() {
  const res = await getJSON("/api/check");
  const tb = $("checks").querySelector("tbody");
  tb.innerHTML = "";
  for (const c of (res.data || [])) {
    const tr = document.createElement("tr");
    tr.innerHTML = `<td class="${c.ok ? "ok-dot" : "bad-dot"}">${c.ok ? "ok" : "!"}</td><td>${esc(c.name)}</td><td class="mono small">${esc(c.detail)}</td>`;
    tb.appendChild(tr);
  }
}

async function showUrl() {
  const res = await getJSON("/api/url?full=1");
  $("url").value = res.text.trim();
  $("showurl").textContent = "Reveal token";
  $("copyurl").onclick = async () => {
    try { await navigator.clipboard.writeText($("url").value); $("copyurl").textContent = "Copied"; }
    catch (e) { $("url").select(); }
    setTimeout(() => { $("copyurl").textContent = "Copy"; }, 1500);
  };
}

$("routes").addEventListener("click", (e) => {
  const b = e.target.closest("button");
  if (b) route(b.dataset.mode);
});
$("apply").onclick = () => route(state.mode === "off" ? "sonos" : state.mode);
$("scan").onclick = scan;
$("volbtn").onclick = setVolume;
$("vol").oninput = () => { $("volval").textContent = $("vol").value; };
$("check").onclick = runChecks;
$("showurl").onclick = showUrl;

poll();
setInterval(poll, 1000);
loadSinks();
setInterval(loadSinks, 10000);
runChecks();
