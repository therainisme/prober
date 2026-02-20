const state = {
  agents: [],
  admin: false,
  showDeleted: false,
  search: "",
  selectedAgentId: null,
  selectedHistory: [],
  theme: localStorage.getItem("prober-theme") || "light",
};

const agentsList = document.getElementById("agentsList");
const searchInput = document.getElementById("searchInput");
const showDeleted = document.getElementById("showDeleted");
const themeBtn = document.getElementById("themeBtn");
const authBtn = document.getElementById("authBtn");
const loginOverlay = document.getElementById("loginOverlay");
const loginPassword = document.getElementById("loginPassword");
const loginSubmit = document.getElementById("loginSubmit");
const loginCancel = document.getElementById("loginCancel");
const loginError = document.getElementById("loginError");
const totalCount = document.getElementById("totalCount");
const onlineCount = document.getElementById("onlineCount");
const offlineCount = document.getElementById("offlineCount");
const confirmOverlay = document.getElementById("confirmOverlay");
const confirmMessage = document.getElementById("confirmMessage");
const confirmCancel = document.getElementById("confirmCancel");
const confirmOk = document.getElementById("confirmOk");

function esc(str) {
  if (str == null) return "";
  return String(str)
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#039;");
}

function fmtPercent(v) {
  if (v == null) return "-";
  return `${Number(v).toFixed(1)}%`;
}

function fmtBytes(v) {
  if (v == null) return "-";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let val = Number(v);
  let idx = 0;
  while (val >= 1024 && idx < units.length - 1) {
    val /= 1024;
    idx += 1;
  }
  return `${val.toFixed(idx === 0 ? 0 : 1)} ${units[idx]}`;
}

function fmtRate(v) {
  if (v == null) return "-";
  return `${fmtBytes(v)}/s`;
}

function fmtUsage(used, total) {
  if (used == null || total == null || total === 0) return "-";
  const p = (used / total) * 100;
  return `${p.toFixed(1)}%`;
}

function fmtBytesPair(used, total) {
  if (used == null || total == null || total === 0) return "";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let t = Number(total), idx = 0;
  while (t >= 1024 && idx < units.length - 1) { t /= 1024; idx++; }
  const u = Number(used) / Math.pow(1024, idx);
  return `${u.toFixed(1)}/${t.toFixed(1)} ${units[idx]}`;
}

function usagePercent(used, total) {
  if (used == null || total == null || total <= 0) return null;
  return Math.max(0, Math.min(100, (Number(used) / Number(total)) * 100));
}

function fmtTime(value) {
  if (!value) return "-";
  const d = new Date(value);
  if (Number.isNaN(d.getTime())) return esc(value);
  return new Intl.DateTimeFormat("en-US", {
    hour12: false,
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  }).format(d);
}

function statusMeta(agent) {
  if (agent.deleted_at) return ["Deleted", "status-deleted"];
  if (agent.online) return ["Online", "status-up"];
  return ["Offline", "status-down"];
}

const sunPath = '<path fill="currentColor" d="M12 18a6 6 0 1 1 0-12 6 6 0 0 1 0 12zm0-2a4 4 0 1 0 0-8 4 4 0 0 0 0 8zM11 1h2v3h-2V1zm0 19h2v3h-2v-3zM3.515 4.929l1.414-1.414L7.05 5.636 5.636 7.05 3.515 4.93zM16.95 18.364l1.414-1.414 2.121 2.121-1.414 1.414-2.121-2.121zm2.121-14.85l1.414 1.415-2.121 2.121-1.414-1.414 2.121-2.121zM5.636 16.95l1.414 1.414-2.121 2.121-1.414-1.414 2.121-2.121zM23 11v2h-3v-2h3zM4 11v2H1v-2h3z"/>';
const moonPath = '<path fill="currentColor" d="M10 6a8 8 0 0 0 11.955 6.956C21.474 18.03 17.2 22 12 22 6.477 22 2 17.523 2 12c0-5.2 3.97-9.474 9.044-9.955A7.963 7.963 0 0 0 10 6zm-6 6a8 8 0 0 0 8 8 8.006 8.006 0 0 0 6.957-4.045c-.316.03-.636.045-.957.045-5.523 0-10-4.477-10-10 0-.321.015-.641.045-.957A8.006 8.006 0 0 0 4 12z"/>';

function applyTheme() {
  document.body.dataset.theme = state.theme;
  localStorage.setItem("prober-theme", state.theme);
  themeBtn.innerHTML = state.theme === "light" ? moonPath : sunPath;
}

function updateStats(list) {
  const active = list.filter((a) => !a.deleted_at);
  const total = active.length;
  const online = active.filter((a) => a.online).length;
  const offline = active.filter((a) => !a.online).length;
  totalCount.textContent = String(total);
  onlineCount.textContent = String(online);
  offlineCount.textContent = String(offline);
}

function renderList() {
  showDeleted.parentElement.style.display = state.admin ? "" : "none";
  if (!state.admin && state.showDeleted) {
    state.showDeleted = false;
    showDeleted.checked = false;
  }

  const filtered = state.agents.filter((a) => {
    if (a.deleted_at) return false;
    if (!state.showDeleted && !a.online) return false;
    if (!state.search) return true;
    const name = (a.display_name || a.agent_id || "").toLowerCase();
    return name.includes(state.search.toLowerCase());
  });

  updateStats(state.agents);

  if (!filtered.length) {
    agentsList.innerHTML = `<div class="list-empty">No matching agents</div>`;
    return;
  }

  const actionClass = (state.admin && state.showDeleted) ? "has-action" : "";

  const header = `
    <div class="row row-header ${actionClass}">
      <span class="col-name">Host</span>
      <span class="col-metric">CPU</span>
      <span class="col-metric">Memory</span>
      <span class="col-metric">Disk</span>
      <span class="col-net">Network</span>
      <span class="col-time">Last Seen</span>
      ${actionClass ? '<span class="col-action"></span>' : ""}
    </div>`;

  const rows = filtered
    .map((a) => {
      const [, cls] = statusMeta(a);
      const isSelected = state.admin && state.selectedAgentId === a.agent_id;
      const selected = isSelected ? "selected" : "";
      const clickable = state.admin ? "clickable" : "";

      const action = (state.admin && !a.online)
        ? `<button class="action-btn" data-action="delete" data-id="${esc(a.agent_id)}">Delete</button>`
        : "";

      const cpuPct = a.online ? a.metrics?.cpu_usage_percent : null;
      const cpuCores = a.online ? a.metrics?.cpu_cores : null;
      const memPct = a.online ? usagePercent(a.metrics?.memory_used_bytes, a.metrics?.memory_total_bytes) : null;
      const diskPct = a.online ? usagePercent(a.metrics?.disk_used_bytes, a.metrics?.disk_total_bytes) : null;

      let detailHtml = "";
      if (isSelected) {
        detailHtml = renderInlineDetail(a);
      }

      const memText = a.online ? fmtUsage(a.metrics?.memory_used_bytes, a.metrics?.memory_total_bytes) : "-";
      const diskText = a.online ? fmtUsage(a.metrics?.disk_used_bytes, a.metrics?.disk_total_bytes) : "-";
      const netText = a.online ? `↓${fmtRate(a.metrics?.rx_bps)} ↑${fmtRate(a.metrics?.tx_bps)}` : "-";

      const cpuDetail = cpuCores ? `${cpuCores}C` : "";
      const memDetail = a.online ? fmtBytesPair(a.metrics?.memory_used_bytes, a.metrics?.memory_total_bytes) : "";
      const diskDetail = a.online ? fmtBytesPair(a.metrics?.disk_used_bytes, a.metrics?.disk_total_bytes) : "";

      return `
      <div class="row ${selected} ${clickable} ${actionClass}" data-open-id="${esc(a.agent_id)}">
        <span class="col-name"><span class="dot ${cls}"></span>${esc(a.display_name || a.agent_id || "-")}</span>
        <span class="col-metric">${miniBar("CPU", fmtPercent(cpuPct), cpuPct, cpuDetail)}</span>
        <span class="col-metric">${miniBar("Mem", memText, memPct, memDetail)}</span>
        <span class="col-metric">${miniBar("Disk", diskText, diskPct, diskDetail)}</span>
        <span class="col-net">${netText}</span>
        <span class="col-time">${fmtTime(a.last_seen)}</span>
        ${actionClass ? `<span class="col-action">${action}</span>` : ""}
        ${detailHtml}
      </div>`;
    })
    .join("");

  agentsList.innerHTML = header + rows;
}

function miniBar(label, text, percent, detail) {
  const w = percent != null ? percent : 0;
  const detailHtml = detail ? `<span class="mini-detail">${detail}</span>` : "";
  return `<span class="mini-top"><span class="mini-label">${label}</span>${detailHtml}<span class="mini-val">${text}</span></span><span class="mini-bar"><span style="width:${w}%"></span></span>`;
}

function buildSparkline(points, color, formatter, fixedMax) {
  if (!points.length) {
    return `<div class="chart-empty">No history data</div>`;
  }
  const values = points.map((p) => Number(p.value || 0));
  const min = fixedMax != null ? 0 : Math.min(...values);
  const max = fixedMax != null ? fixedMax : Math.max(...values);
  const range = max - min || 1;
  const width = 360;
  const height = 100;

  const path = points
    .map((p, i) => {
      const x = (i / (points.length - 1 || 1)) * width;
      const y = height - ((Number(p.value || 0) - min) / range) * height;
      return `${i === 0 ? "M" : "L"}${x.toFixed(2)} ${y.toFixed(2)}`;
    })
    .join(" ");

  const latest = points[points.length - 1]?.value;
  const lastX = (points.length - 1) / (points.length - 1 || 1) * width;
  const fillPath = `${path} L${lastX.toFixed(2)} ${height} L0 ${height} Z`;
  return `
    <div class="chart-headline">${formatter(latest)}</div>
    <svg class="sparkline" viewBox="0 0 ${width} ${height}" preserveAspectRatio="none">
      <path d="${fillPath}" fill="${color}" opacity="0.1" stroke="none"></path>
      <path d="${path}" fill="none" stroke="${color}" stroke-width="2" stroke-linecap="round"></path>
    </svg>`;
}

function renderInlineDetail(agent) {
  const points = state.selectedHistory || [];
  const cpuPoints = points.map((p) => ({ t: p.timestamp, value: p.metrics?.cpu_usage_percent || 0 }));
  const memoryPoints = points.map((p) => {
    const used = p.metrics?.memory_used_bytes;
    const total = p.metrics?.memory_total_bytes;
    return { t: p.timestamp, value: total ? (used / total) * 100 : 0 };
  });
  const diskPoints = points.map((p) => {
    const used = p.metrics?.disk_used_bytes;
    const total = p.metrics?.disk_total_bytes;
    return { t: p.timestamp, value: total ? (used / total) * 100 : 0 };
  });
  const netPoints = points.map((p) => ({
    t: p.timestamp,
    value: (p.metrics?.rx_bps || 0) + (p.metrics?.tx_bps || 0),
  }));

  return `
    <div class="inline-detail">
      <div class="chart-grid">
        <section class="chart-card"><h4>CPU</h4>${buildSparkline(cpuPoints, "#0f172a", fmtPercent, 100)}</section>
        <section class="chart-card"><h4>Memory</h4>${buildSparkline(memoryPoints, "#1d4ed8", fmtPercent, 100)}</section>
        <section class="chart-card"><h4>Disk</h4>${buildSparkline(diskPoints, "#0f766e", fmtPercent, 100)}</section>
        <section class="chart-card"><h4>Network</h4>${buildSparkline(netPoints, "#7c3aed", fmtRate)}</section>
      </div>
      <div class="detail-footer">
        <span>First seen ${fmtTime(agent.first_seen)}</span>
        <span>Last seen ${fmtTime(agent.last_seen)}</span>
      </div>
    </div>`;
}

async function fetchAgents() {
  const url = state.showDeleted ? "/api/agents?include_deleted=true" : "/api/agents";
  const res = await fetch(url);
  if (!res.ok) return;
  const data = await res.json();
  state.agents = data.agents || [];
  renderList();
}

async function fetchHistory(agentId) {
  const to = new Date();
  const from = new Date(to.getTime() - 24 * 60 * 60 * 1000);
  const url = `/api/agents/${encodeURIComponent(agentId)}/history?from=${encodeURIComponent(from.toISOString())}&to=${encodeURIComponent(to.toISOString())}&limit=1200`;
  const res = await fetch(url);
  if (!res.ok) {
    state.selectedHistory = [];
    renderList();
    return;
  }
  const data = await res.json();
  state.selectedHistory = data.points || [];
  renderList();
}

function showConfirm(message) {
  return new Promise((resolve) => {
    confirmMessage.textContent = message;
    confirmOverlay.classList.remove("hidden");
    const cleanup = () => {
      confirmOverlay.classList.add("hidden");
      confirmOk.removeEventListener("click", onOk);
      confirmCancel.removeEventListener("click", onCancel);
      confirmOverlay.removeEventListener("click", onBackdrop);
    };
    const onOk = () => { cleanup(); resolve(true); };
    const onCancel = () => { cleanup(); resolve(false); };
    const onBackdrop = (e) => { if (e.target === confirmOverlay) { cleanup(); resolve(false); } };
    confirmOk.addEventListener("click", onOk);
    confirmCancel.addEventListener("click", onCancel);
    confirmOverlay.addEventListener("click", onBackdrop);
  });
}

function showLoginOverlay() {
  loginPassword.value = "";
  loginError.textContent = "";
  loginOverlay.classList.remove("hidden");
  loginPassword.focus();
}

function hideLoginOverlay() {
  loginOverlay.classList.add("hidden");
}

async function login() {
  const password = loginPassword.value;
  if (!password) return;
  loginError.textContent = "";
  const res = await fetch("/api/admin/login", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    credentials: "same-origin",
    body: JSON.stringify({ password }),
  });
  if (!res.ok) {
    loginError.textContent = "Invalid password";
    state.admin = false;
    renderList();
    return;
  }
  state.admin = true;
  authBtn.textContent = "Admin";
  hideLoginOverlay();
  renderList();
}

async function logout() {
  await fetch("/api/admin/logout", {
    method: "POST",
    credentials: "same-origin",
  });
  state.admin = false;
  state.selectedAgentId = null;
  state.selectedHistory = [];
  authBtn.textContent = "Guest";
  renderList();
}

async function refreshSessionState() {
  const res = await fetch("/api/admin/session", {
    credentials: "same-origin",
  });
  if (!res.ok) {
    state.admin = false;
    state.selectedAgentId = null;
    state.selectedHistory = [];
    authBtn.textContent = "Guest";
    return;
  }
  const data = await res.json();
  state.admin = !!data.active;
  if (!state.admin) {
    state.selectedAgentId = null;
    state.selectedHistory = [];
  }
  authBtn.textContent = state.admin ? "Admin" : "Guest";
}

function connectSse() {
  const es = new EventSource("/api/events");
  es.addEventListener("snapshot", fetchAgents);
  es.addEventListener("agent_update", fetchAgents);
  es.onerror = () => {
    setTimeout(connectSse, 2000);
    es.close();
  };
}

agentsList.addEventListener("click", async (e) => {
  const target = e.target;
  if (!(target instanceof HTMLElement)) return;

  const actionBtn = target.closest("[data-action]");
  if (actionBtn && actionBtn.dataset.action === "delete" && actionBtn.dataset.id && state.admin) {
    e.stopPropagation();
    const id = actionBtn.dataset.id;
    const ok = await showConfirm(`Delete agent "${id}"?`);
    if (!ok) return;
    const res = await fetch(`/api/admin/agents/${encodeURIComponent(id)}/delete`, {
      method: "POST",
      credentials: "same-origin",
    });
    if (res.ok) {
      fetchAgents();
    } else if (res.status === 401) {
      state.admin = false;
      authBtn.textContent = "Guest";
      renderList();
    }
    return;
  }

  if (!state.admin) return;

  const card = target.closest("[data-open-id]");
  if (!card) return;
  const agentId = card.getAttribute("data-open-id");
  if (!agentId) return;

  if (state.selectedAgentId === agentId) {
    state.selectedAgentId = null;
    state.selectedHistory = [];
    renderList();
    return;
  }

  state.selectedAgentId = agentId;
  state.selectedHistory = [];
  renderList();
  await fetchHistory(agentId);
});

searchInput.addEventListener("input", () => {
  state.search = searchInput.value;
  renderList();
});

showDeleted.addEventListener("change", () => {
  state.showDeleted = showDeleted.checked;
  fetchAgents();
});

authBtn.addEventListener("click", () => {
  if (state.admin) {
    logout();
  } else {
    showLoginOverlay();
  }
});

loginSubmit.addEventListener("click", login);
loginCancel.addEventListener("click", hideLoginOverlay);

loginPassword.addEventListener("keydown", (e) => {
  if (e.key === "Enter") login();
  if (e.key === "Escape") hideLoginOverlay();
});

loginOverlay.addEventListener("click", (e) => {
  if (e.target === loginOverlay) hideLoginOverlay();
});

themeBtn.addEventListener("click", () => {
  state.theme = state.theme === "light" ? "dark" : "light";
  applyTheme();
});

applyTheme();
fetchAgents();
refreshSessionState();
connectSse();
