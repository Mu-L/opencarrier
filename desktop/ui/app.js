// OpenCarrier desktop client frontend.
//
// Pure remote client: all API traffic goes through Rust commands
// (api_request / chat_stream) — see ../src/main.rs. State kept minimal:
// config in localStorage, current chat in memory.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

// ── State ──────────────────────────────────────────────
const state = {
  config: loadConfig(),        // {server, apiKey, senderId}
  clones: [],                  // GET /api/agents rows
  current: null,               // selected clone row
  history: new Map(),          // agent name -> [{role, text}]
  streaming: false,
};

function loadConfig() {
  try {
    const raw = localStorage.getItem('oc-config');
    if (raw) return JSON.parse(raw);
  } catch {}
  return { server: '', apiKey: '', senderId: '' };
}
function saveConfig() {
  localStorage.setItem('oc-config', JSON.stringify(state.config));
}

// ── DOM ────────────────────────────────────────────────
const $ = (id) => document.getElementById(id);
const contactList = $('contact-list');
const chatBody = $('chat-body');
const statusbar = $('statusbar');
const statusbarText = $('statusbar-text');
const msgInput = $('msg-input');
const sendBtn = $('send-btn');

// ── API helpers (all via Rust proxy) ───────────────────
async function apiGet(path) {
  const r = await invoke('api_request', {
    args: {
      server: state.config.server,
      apiKey: state.config.apiKey,
      method: 'GET',
      path,
    },
  });
  return r;
}
async function apiPost(path, body) {
  return invoke('api_request', {
    args: {
      server: state.config.server,
      apiKey: state.config.apiKey,
      method: 'POST',
      path,
      body,
    },
  });
}

// ── Contact list ───────────────────────────────────────
async function loadClones() {
  if (!state.config.server || !state.config.apiKey) {
    $('list-hint')?.remove();
    contactList.innerHTML = '<div class="empty-hint">先在 ⚙ 里配置服务器与 API Key</div>';
    return;
  }
  try {
    const r = await apiGet('/api/agents');
    if (r.status !== 200) {
      contactList.innerHTML = `<div class="empty-hint">加载失败 HTTP ${r.status}<br>${escapeHtml(
        JSON.stringify(r.body).slice(0, 120),
      )}</div>`;
      setConn(false);
      return;
    }
    state.clones = r.body;
    setConn(true);
    renderContacts();
  } catch (e) {
    setConn(false);
    contactList.innerHTML = `<div class="empty-hint">连接失败：${escapeHtml(String(e))}</div>`;
  }
}

function renderContacts() {
  contactList.innerHTML = '';
  if (state.clones.length === 0) {
    contactList.innerHTML = '<div class="empty-hint">服务器上没有运行中的分身</div>';
    return;
  }
  // Sort: running first, then by last_active desc
  const sorted = [...state.clones].sort((a, b) => {
    const ra = a.ready ? 1 : 0, rb = b.ready ? 1 : 0;
    if (ra !== rb) return rb - ra;
    return (b.last_active || '').localeCompare(a.last_active || '');
  });
  for (const c of sorted) {
    const el = document.createElement('div');
    el.className = 'contact' + (state.current?.name === c.name ? ' active' : '');
    const avatar = c.identity?.avatar_url
      ? `<img src="${escapeHtml(c.identity.avatar_url)}" alt="" />`
      : escapeHtml(c.identity?.emoji || c.display_name?.[0] || c.name?.[0] || '?');
    el.innerHTML = `
      <div class="contact-avatar">${avatar}
        <span class="contact-presence ${c.ready ? 'ready' : ''}"></span>
      </div>
      <div class="contact-meta">
        <div class="contact-name">${escapeHtml(c.display_name || c.name)}</div>
        <div class="contact-sub">${escapeHtml(c.name)} · ${escapeHtml(c.model_name || '')}</div>
      </div>`;
    el.onclick = () => selectClone(c);
    contactList.appendChild(el);
  }
}

function setConn(ok) {
  $('server-indicator').className = 'dot ' + (ok ? 'dot-on' : 'dot-off');
  $('server-label').textContent = ok
    ? state.config.server.replace(/^https?:\/\//, '')
    : '未连接';
}

// ── Chat ───────────────────────────────────────────────
async function selectClone(c) {
  if (state.streaming) return;
  state.current = c;
  renderContacts();
  $('chat-empty').classList.add('hidden');
  $('chat-active').classList.remove('hidden');
  $('chat-name').textContent = c.display_name || c.name;
  $('chat-sub').textContent = `${c.name} · ${c.model_name || ''} · ${
    c.ready ? '在线' : '空闲'
  }`;
  const avatarEl = $('chat-avatar');
  if (c.identity?.avatar_url) {
    avatarEl.innerHTML = `<img src="${escapeHtml(c.identity.avatar_url)}" style="width:100%;height:100%;border-radius:8px" />`;
  } else {
    avatarEl.textContent = c.identity?.emoji || c.display_name?.[0] || '?';
  }

  chatBody.innerHTML = '';
  const hist = state.history.get(c.name) || [];
  for (const m of hist) appendBubble(m.role, m.text);
  scrollChat();

  // Load server-side history for our sender session (agent-app API G1)
  if (!hist.length) {
    try {
      const r = await apiGet(
        `/api/agents/${encodeURIComponent(c.name)}/session?sender_id=${encodeURIComponent(
          state.config.senderId,
        )}`,
      );
      if (r.status === 200 && Array.isArray(r.body.messages)) {
        for (const m of r.body.messages) {
          if (m.role !== 'User' && m.role !== 'Assistant') continue;
          const text = (m.content || '').trim();
          if (!text) continue;
          appendBubble(m.role === 'User' ? 'user' : 'agent', text);
          (state.history.get(c.name) || state.history.set(c.name, []).get(c.name)).push({
            role: m.role === 'User' ? 'user' : 'agent',
            text,
          });
        }
        scrollChat();
      }
    } catch (e) {
      console.warn('history load failed', e);
    }
  }
}

function appendBubble(role, text, streaming = false) {
  const row = document.createElement('div');
  row.className = `msg-row ${role}`;
  const who = role === 'user' ? '我' : state.current?.identity?.emoji || 'AI';
  row.innerHTML = `
    <div class="msg-avatar">${escapeHtml(String(who))}</div>
    <div class="bubble${streaming ? ' streaming' : ''}"></div>`;
  row.querySelector('.bubble').textContent = text;
  chatBody.appendChild(row);
  return row;
}

function appendToolChip(tool, phase, isError) {
  const chip = document.createElement('div');
  chip.className = 'tool-chip' + (isError ? ' err' : '');
  chip.textContent = phase === 'start' ? `🔧 ${tool} …` : `🔧 ${tool} 完成`;
  chatBody.appendChild(chip);
  scrollChat();
  return chip;
}

function scrollChat() {
  chatBody.scrollTop = chatBody.scrollHeight;
}

function setStatus(text) {
  if (text) {
    statusbar.classList.remove('hidden');
    statusbarText.textContent = text;
  } else {
    statusbar.classList.add('hidden');
  }
}

// ── Send (SSE via Rust command) ────────────────────────
async function sendMessage() {
  const text = msgInput.value.trim();
  if (!text || !state.current || state.streaming) return;

  const agent = state.current.name;
  state.streaming = true;
  sendBtn.disabled = true;
  msgInput.value = '';
  msgInput.focus();

  appendBubble('user', text);
  (state.history.get(agent) || state.history.set(agent, []).get(agent)).push({
    role: 'user',
    text,
  });

  const bubbleRow = appendBubble('agent', '', true);
  const bubble = bubbleRow.querySelector('.bubble');
  let acc = '';
  setStatus('思考中…');

  try {
    await invoke('chat_stream', {
      args: {
        server: state.config.server,
        apiKey: state.config.apiKey,
        agent,
        message: text,
        senderId: state.config.senderId,
        senderName: 'Desktop',
        activeFlow: null,
      },
    });
    // Events appended via listeners below; chat_stream resolves when the
    // turn ends (done frame) or errors out.
  } catch (e) {
    bubble.textContent = `发送失败：${e}`;
    bubble.classList.remove('streaming');
  } finally {
    state.streaming = false;
    sendBtn.disabled = false;
    setStatus('');
  }

  // The bubble content lives in the delta listener; keep a final flush here
  // in case the last done frame carried text we missed.
  if (acc && !bubble.textContent) bubble.textContent = acc;
}

// ── Rust → JS event wiring ─────────────────────────────
async function wireEvents() {
  await listen('chat://delta', (ev) => {
    if (ev.payload.agent !== state.current?.name) return;
    const rows = chatBody.querySelectorAll('.msg-row.agent');
    const last = rows[rows.length - 1];
    if (!last) return;
    const bubble = last.querySelector('.bubble');
    bubble.textContent += ev.payload.text;
    bubble.classList.add('streaming');
    scrollChat();
  });
  await listen('chat://tool', (ev) => {
    if (ev.payload.agent !== state.current?.name) return;
    appendToolChip(ev.payload.tool, ev.payload.phase, false);
    setStatus(`调用工具：${ev.payload.tool}`);
  });
  await listen('chat://phase', (ev) => {
    if (ev.payload.agent !== state.current?.name) return;
    if (ev.payload.detail) setStatus(`${ev.payload.phase} · ${ev.payload.detail}`);
    else setStatus(ev.payload.phase);
  });
  await listen('chat://done', (ev) => {
    if (ev.payload.agent !== state.current?.name) return;
    const rows = chatBody.querySelectorAll('.msg-row.agent');
    const last = rows[rows.length - 1];
    if (last) {
      const bubble = last.querySelector('.bubble');
      bubble.classList.remove('streaming');
      if (!bubble.textContent.trim() && ev.payload.text) bubble.textContent = ev.payload.text;
      const agent = state.current?.name;
      if (agent) {
        (state.history.get(agent) || state.history.set(agent, []).get(agent)).push({
          role: 'agent',
          text: bubble.textContent,
        });
      }
    }
    const u = ev.payload.usage || {};
    setStatus(`完成 · ${u.input_tokens ?? '?'} in / ${u.output_tokens ?? '?'} out`);
    setTimeout(() => setStatus(''), 2600);
  });
  await listen('chat://error', (ev) => {
    if (ev.payload.agent !== state.current?.name) return;
    const rows = chatBody.querySelectorAll('.msg-row.agent');
    const last = rows[rows.length - 1];
    if (last) {
      const bubble = last.querySelector('.bubble');
      bubble.classList.remove('streaming');
      if (!bubble.textContent.trim()) bubble.textContent = `⚠ ${ev.payload.message}`;
    }
    setStatus('');
  });
}

// ── Settings ───────────────────────────────────────────
function openSettings() {
  $('cfg-server').value = state.config.server;
  $('cfg-key').value = state.config.apiKey;
  $('cfg-sender').value = state.config.senderId;
  $('settings-dialog').showModal();
}

$('settings-dialog').addEventListener('close', () => {
  if ($('settings-dialog').returnValue === 'save') {
    state.config = {
      server: $('cfg-server').value.trim(),
      apiKey: $('cfg-key').value.trim(),
      senderId: $('cfg-sender').value.trim() || 'desktop-user',
    };
    saveConfig();
    loadClones();
  }
});

// ── Boot ───────────────────────────────────────────────
function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (ch) => ({
    '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;',
  })[ch]);
}

sendBtn.onclick = sendMessage;
msgInput.addEventListener('keydown', (e) => {
  if (e.key === 'Enter' && !e.shiftKey) {
    e.preventDefault();
    sendMessage();
  }
});
$('settings-btn').onclick = openSettings;
$('refresh-btn').onclick = loadClones;
$('settings-close').onclick = () => $('settings-dialog').close();

wireEvents().then(async () => {
  // First-run provisioning: adopt ~/.opencarrier/desktop.json when the UI
  // has no saved config yet (deploy-script / handoff friendly).
  if (!state.config.server || !state.config.apiKey) {
    try {
      const p = await invoke('get_provision');
      if (p && p.server && p.apiKey) {
        state.config = {
          server: p.server,
          apiKey: p.apiKey,
          senderId: p.senderId || 'desktop-user',
        };
        saveConfig();
      }
    } catch {}
  }
  loadClones();
  if (!state.config.server || !state.config.apiKey) openSettings();
});
