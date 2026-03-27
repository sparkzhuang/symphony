const stateUrl = "/api/v1/state";
const events = new EventSource('/api/v1/events');
const issueBaseUrl = "/api/v1/";

const statusEl = document.querySelector("#connection-status");
const statusCopyEl = document.querySelector("#connection-copy");
const runningBody = document.querySelector("#running-body");
const retryBody = document.querySelector("#retry-body");
const issueDetailsBody = document.querySelector("#issue-details-body");

let selectedIssueIdentifier = null;
let latestPayload = null;

bootstrap();

async function bootstrap() {
  await refreshSnapshot();
  attachEvents();
}

async function refreshSnapshot() {
  try {
    const response = await fetch(stateUrl, { headers: { Accept: "application/json" } });
    const payload = await response.json();
    applyPayload(payload, "Initial snapshot loaded.");
  } catch (error) {
    renderConnection("error", "Snapshot request failed. Waiting for SSE reconnect.");
  }
}

function attachEvents() {
  events.addEventListener("state", (event) => {
    const payload = JSON.parse(event.data);
    applyPayload(payload, "Receiving live updates.");
  });

  events.onopen = () => {
    renderConnection("live", "Receiving live updates.");
  };

  events.onerror = () => {
    renderConnection("error", "Connection interrupted. The browser will retry automatically.");
  };
}

function applyPayload(payload, connectionCopy) {
  latestPayload = payload;

  if (payload.error) {
    renderConnection("error", payload.error.message);
    return;
  }

  renderConnection("live", connectionCopy);
  renderMetrics(payload);
  renderRunning(payload.running || []);
  renderRetrying(payload.retrying || []);

  if (selectedIssueIdentifier) {
    loadIssueDetails(selectedIssueIdentifier);
  }
}

function renderConnection(kind, message) {
  statusEl.textContent = kind === "live" ? "Live" : kind === "error" ? "Issue" : "Connecting";
  statusEl.className = `status-pill${kind === "live" ? " live" : ""}${kind === "error" ? " error" : ""}`;
  statusCopyEl.textContent = message;
}

function renderMetrics(payload) {
  setMetric("running", payload.counts.running);
  setMetric("retrying", payload.counts.retrying);
  setMetric("total_tokens", formatNumber(payload.codex_totals.total_tokens));
  setMetric("runtime", formatDuration(payload.codex_totals.seconds_running));
}

function setMetric(name, value) {
  const el = document.querySelector(`[data-metric="${name}"]`);
  if (el) {
    el.textContent = value;
  }
}

function renderRunning(entries) {
  if (entries.length === 0) {
    runningBody.innerHTML = '<tr class="empty-row"><td colspan="6">No active sessions.</td></tr>';
    return;
  }

  runningBody.innerHTML = entries
    .map((entry) => {
      const issueButton = issueButtonHtml(entry.issue_identifier);
      return `
        <tr>
          <td>${issueButton}</td>
          <td><span class="badge">${escapeHtml(entry.state)}</span></td>
          <td class="mono">${escapeHtml(entry.session_id || "n/a")}</td>
          <td>${escapeHtml(formatRuntimeAndTurns(entry.started_at, entry.turn_count))}</td>
          <td>${escapeHtml(entry.last_message || entry.last_event || "n/a")}</td>
          <td>${escapeHtml(formatNumber(entry.tokens.total_tokens))}</td>
        </tr>
      `;
    })
    .join("");

  bindIssueButtons();
}

function renderRetrying(entries) {
  if (entries.length === 0) {
    retryBody.innerHTML =
      '<tr class="empty-row"><td colspan="4">No issues are currently backing off.</td></tr>';
    return;
  }

  retryBody.innerHTML = entries
    .map(
      (entry) => `
        <tr>
          <td>${issueButtonHtml(entry.issue_identifier)}</td>
          <td>${escapeHtml(String(entry.attempt))}</td>
          <td class="mono">${escapeHtml(entry.due_at)}</td>
          <td>${escapeHtml(entry.error || "n/a")}</td>
        </tr>
      `,
    )
    .join("");

  bindIssueButtons();
}

function issueButtonHtml(issueIdentifier) {
  return `<button class="issue-button" data-issue-id="${escapeHtml(issueIdentifier)}">${escapeHtml(issueIdentifier)}</button>`;
}

function bindIssueButtons() {
  document.querySelectorAll("[data-issue-id]").forEach((button) => {
    button.onclick = () => {
      selectedIssueIdentifier = button.dataset.issueId;
      loadIssueDetails(selectedIssueIdentifier);
    };
  });
}

async function loadIssueDetails(issueIdentifier) {
  if (!issueIdentifier) {
    return;
  }

  try {
    const response = await fetch(`${issueBaseUrl}${encodeURIComponent(issueIdentifier)}`, {
      headers: { Accept: "application/json" },
    });
    if (!response.ok) {
      throw new Error("issue request failed");
    }
    const payload = await response.json();
    renderIssueDetails(payload);
  } catch (error) {
    issueDetailsBody.innerHTML =
      "<p>Issue details are temporarily unavailable. Keep the dashboard open and retry from the table.</p>";
  }
}

function renderIssueDetails(payload) {
  const recentEvents = (payload.recent_events || [])
    .map(
      (event) =>
        `<div class="detail-list"><strong>${escapeHtml(event.event)}</strong><span>${escapeHtml(event.message || "No message")}</span><span class="mono">${escapeHtml(event.at)}</span></div>`,
    )
    .join("");

  issueDetailsBody.innerHTML = `
    <div>
      <h3>${escapeHtml(payload.issue_identifier)}</h3>
      <p class="detail-copy">${escapeHtml(payload.status)}</p>
    </div>
    <div class="detail-section">
      <span class="detail-term">Workspace</span>
      <span class="mono">${escapeHtml(payload.workspace.path || "n/a")}</span>
    </div>
    <div class="detail-section">
      <span class="detail-term">Attempts</span>
      <span>Restarted ${escapeHtml(String(payload.attempts.restart_count))} times</span>
      <span>Current retry ${escapeHtml(String(payload.attempts.current_retry_attempt))}</span>
    </div>
    <div class="detail-section">
      <span class="detail-term">Recent events</span>
      ${recentEvents || "<p>No recent events recorded.</p>"}
    </div>
    <div class="detail-section">
      <span class="detail-term">Last error</span>
      <span>${escapeHtml(payload.last_error || "n/a")}</span>
    </div>
  `;
}

function formatRuntimeAndTurns(startedAt, turns) {
  if (!startedAt) {
    return `n/a / ${turns} turns`;
  }

  const seconds = Math.max(0, Math.floor((Date.now() - new Date(startedAt).getTime()) / 1000));
  return `${formatDuration(seconds)} / ${turns} turns`;
}

function formatDuration(seconds) {
  const hours = Math.floor(seconds / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  const remainder = seconds % 60;

  if (hours > 0) {
    return `${hours}h ${minutes}m`;
  }
  if (minutes > 0) {
    return `${minutes}m ${remainder}s`;
  }
  return `${remainder}s`;
}

function formatNumber(value) {
  return new Intl.NumberFormat("en-US").format(Number(value || 0));
}

function escapeHtml(value) {
  return String(value)
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");
}
