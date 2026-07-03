const state = {
  status: null,
  sessions: [],
  selected: "default",
  socket: null,
  runningTurn: null,
  pendingApproval: null,
  assistantNode: null,
  tools: new Map(),
};

const els = {
  workspace: document.querySelector("#workspace"),
  sessions: document.querySelector("#sessions"),
  sessionTitle: document.querySelector("#session-title"),
  statusLine: document.querySelector("#status-line"),
  messages: document.querySelector("#messages"),
  tools: document.querySelector("#tools"),
  composer: document.querySelector("#composer"),
  prompt: document.querySelector("#prompt"),
  send: document.querySelector("#send"),
  refresh: document.querySelector("#refresh"),
  reset: document.querySelector("#reset"),
  approval: document.querySelector("#approval"),
  approvalBody: document.querySelector("#approval-body"),
  approvalApprove: document.querySelector("#approval-approve"),
  approvalDeny: document.querySelector("#approval-deny"),
  approvalClose: document.querySelector("#approval-close"),
};

async function fetchJson(url, options) {
  const response = await fetch(url, options);
  if (!response.ok) {
    const body = await response.json().catch(() => ({}));
    throw new Error(body.error || `${response.status} ${response.statusText}`);
  }
  return response.json();
}

async function boot() {
  state.status = await fetchJson("/api/status");
  state.selected = new URLSearchParams(location.search).get("session") || "default";
  els.workspace.textContent = state.status.workspace_root;
  await loadSessions();
  await selectSession(state.selected);
}

async function loadSessions() {
  state.sessions = await fetchJson("/api/sessions");
  if (!state.sessions.some((session) => session.name === state.selected)) {
    state.sessions.unshift({
      name: state.selected,
      turns: 0,
      active_messages: 0,
      has_summary: false,
    });
  }
  renderSessions();
}

function renderSessions() {
  els.sessions.replaceChildren();
  for (const session of state.sessions) {
    const button = document.createElement("button");
    button.type = "button";
    button.className = `session-item${session.name === state.selected ? " active" : ""}`;
    button.innerHTML = `<span class="session-name"></span><span class="session-count"></span>`;
    button.querySelector(".session-name").textContent = session.name;
    button.querySelector(".session-count").textContent = String(session.turns || 0);
    button.addEventListener("click", () => selectSession(session.name));
    els.sessions.append(button);
  }
}

async function selectSession(name) {
  state.selected = name;
  state.runningTurn = null;
  state.pendingApproval = null;
  state.assistantNode = null;
  state.tools.clear();
  els.sessionTitle.textContent = name;
  els.messages.replaceChildren();
  els.tools.replaceChildren();
  renderSessions();
  closeSocket();

  const document = await fetchJson(`/api/sessions/${encodeURIComponent(name)}`);
  renderSession(document.session);
  openSocket(name);
}

function renderSession(session) {
  const records = session.turns || [];
  if (records.length > 0) {
    for (const record of records) {
      for (const message of record.messages || []) {
        renderMessage(message.role, message.content || formatToolCalls(message));
      }
    }
  } else {
    for (const message of session.active_thread?.messages || []) {
      if (message.role !== "system") {
        renderMessage(message.role, message.content || formatToolCalls(message));
      }
    }
  }
  scrollMessages();
}

function formatToolCalls(message) {
  if (message.tool_calls) {
    return JSON.stringify(message.tool_calls, null, 2);
  }
  if (message.tool_call_id) {
    return `tool_call_id: ${message.tool_call_id}`;
  }
  return "";
}

function openSocket(name) {
  const protocol = location.protocol === "https:" ? "wss:" : "ws:";
  const socket = new WebSocket(
    `${protocol}//${location.host}/api/sessions/${encodeURIComponent(name)}/ws`,
  );
  state.socket = socket;

  socket.addEventListener("open", () => setStatus("connected"));
  socket.addEventListener("close", () => {
    if (state.socket === socket) {
      setStatus("disconnected");
      setRunning(null);
    }
  });
  socket.addEventListener("message", (event) => {
    const message = JSON.parse(event.data);
    handleServerMessage(message);
  });
}

function closeSocket() {
  if (state.socket) {
    state.socket.close();
    state.socket = null;
  }
}

function handleServerMessage(message) {
  switch (message.type) {
    case "snapshot":
      setRunning(message.data.running_turn);
      break;
    case "agent_event":
      handleAgentEvent(message.data.event);
      break;
    case "turn_saved":
      loadSessions().catch(showError);
      setRunning(null);
      break;
    case "turn_rejected":
      showError(message.data.reason);
      setRunning(null);
      break;
    case "error":
      showError(message.data.message);
      break;
  }
}

function handleAgentEvent(event) {
  switch (event.type) {
    case "turn_started":
      state.assistantNode = null;
      break;
    case "text_delta":
      appendAssistantDelta(event.data);
      break;
    case "agent_message":
      state.assistantNode = null;
      break;
    case "tool_call_started":
      upsertTool(event.data.id, event.data.name, "running");
      break;
    case "tool_call_finished":
      upsertTool(event.data.id, event.data.name, event.data.ok ? "ok" : "error");
      break;
    case "approval_requested":
      showApproval(event.data);
      break;
    case "approval_resolved":
      hideApproval();
      break;
    case "turn_completed":
      setRunning(null);
      break;
    case "error":
      showError(event.data);
      setRunning(null);
      break;
  }
}

function renderMessage(role, content) {
  if (!content) return null;
  const row = document.createElement("div");
  row.className = `message ${role}`;
  const label = document.createElement("div");
  label.className = "role";
  label.textContent = role;
  const bubble = document.createElement("div");
  bubble.className = "bubble";
  bubble.textContent = content;
  row.append(label, bubble);
  els.messages.append(row);
  return bubble;
}

function appendAssistantDelta(text) {
  if (!state.assistantNode) {
    state.assistantNode = renderMessage("assistant", "");
  }
  state.assistantNode.textContent += text;
  scrollMessages();
}

function upsertTool(id, name, status) {
  state.tools.set(id, { name, status });
  els.tools.replaceChildren();
  for (const [toolId, tool] of state.tools) {
    const pill = document.createElement("div");
    pill.className = `tool-pill ${tool.status}`;
    pill.textContent = `${tool.name} · ${tool.status}`;
    pill.title = toolId;
    els.tools.append(pill);
  }
}

function showApproval(request) {
  state.pendingApproval = request;
  els.approvalBody.textContent = formatApproval(request);
  els.approval.classList.remove("hidden");
}

function hideApproval() {
  state.pendingApproval = null;
  els.approval.classList.add("hidden");
}

function formatApproval(request) {
  const action = request.action || {};
  if (action.kind === "shell_command") {
    return [
      request.reason,
      "",
      `command: ${action.command}`,
      `cwd: ${action.cwd}`,
      `timeout: ${action.timeout_secs}s`,
    ].join("\n");
  }
  if (action.kind === "file_changes") {
    const files = (action.files || [])
      .map((file) => `${file.path} (${file.operation})`)
      .join("\n");
    return [request.reason, "", files, "", action.diff || ""].join("\n");
  }
  return JSON.stringify(request, null, 2);
}

function sendApproval(approved) {
  if (!state.pendingApproval || !state.socket) return;
  state.socket.send(
    JSON.stringify({
      type: "approval_decision",
      data: {
        request_id: state.pendingApproval.id,
        approved,
      },
    }),
  );
  hideApproval();
}

function setRunning(runningTurn) {
  state.runningTurn = runningTurn;
  const running = Boolean(runningTurn);
  els.send.disabled = running;
  els.prompt.disabled = running;
  els.reset.disabled = running;
  setStatus(running ? "running" : "ready");
}

function setStatus(text) {
  const permission = state.status
    ? `${state.status.permissions.mode}, shell ${state.status.permissions.shell}`
    : "";
  els.statusLine.textContent = [text, permission].filter(Boolean).join(" · ");
}

function showError(error) {
  const text = typeof error === "string" ? error : error.message;
  renderMessage("tool", text);
  scrollMessages();
}

function scrollMessages() {
  els.messages.scrollTop = els.messages.scrollHeight;
}

els.composer.addEventListener("submit", (event) => {
  event.preventDefault();
  const prompt = els.prompt.value.trim();
  if (!prompt || !state.socket || state.socket.readyState !== WebSocket.OPEN) return;
  renderMessage("user", prompt);
  state.socket.send(
    JSON.stringify({
      type: "start_turn",
      data: {
        request_id: `request-${Date.now()}`,
        prompt,
      },
    }),
  );
  els.prompt.value = "";
  setRunning({ turn_id: "pending" });
});

els.refresh.addEventListener("click", () => {
  loadSessions().then(() => selectSession(state.selected)).catch(showError);
});

els.reset.addEventListener("click", async () => {
  try {
    await fetchJson(`/api/sessions/${encodeURIComponent(state.selected)}/reset`, {
      method: "POST",
    });
    await selectSession(state.selected);
  } catch (error) {
    showError(error);
  }
});

els.approvalApprove.addEventListener("click", () => sendApproval(true));
els.approvalDeny.addEventListener("click", () => sendApproval(false));
els.approvalClose.addEventListener("click", () => sendApproval(false));

boot().catch(showError);
