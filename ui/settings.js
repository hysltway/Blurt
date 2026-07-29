/* Blurt 设置页逻辑（快捷键在代码里写死为 Ctrl+Alt，页面只做展示） */
'use strict';

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

let cfg = null;
let saveTimer = null;
let apiKeyState = { configured: false, error: null };

const $ = id => document.getElementById(id);

/* ---------- 提示 ---------- */
let toastTimer = null;
function toast(msg) {
  const el = $('toast');
  el.textContent = msg;
  el.classList.add('show');
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => el.classList.remove('show'), 1600);
}

/* ---------- 保存（防抖） ---------- */
function save(immediate = false) {
  clearTimeout(saveTimer);
  const doSave = async () => {
    try {
      await invoke('set_config', { config: cfg });
      toast('已保存');
    } catch (e) {
      toast('保存失败：' + e);
      cfg = await invoke('get_config');   // 回滚显示
      render();
    }
  };
  if (immediate) doSave();
  else saveTimer = setTimeout(doSave, 350);
}

function fmtAutoStop(v) {
  return v > 0 ? v + ' 秒' : '关闭';
}

/* ---------- 豆包 API 状态 ---------- */
const API_PILL_TEXT = { ready: 'API 就绪', loading: '连接中…', missing: '缺少密钥', failed: '凭据错误' };

function renderEngine(st) {
  const dotCls = { ready: 'ok', loading: 'loading', missing: 'missing' }[st.state] || 'err';
  $('pillDot').className = 'dot ' + dotCls;
  $('pillText').textContent = API_PILL_TEXT[st.state] || API_PILL_TEXT.failed;
  $('apiDot').className = 'dot ' + dotCls;
  $('apiBanner').className = 'model-banner ' + dotCls;
  $('apiStatusText').textContent = st.state === 'ready'
    ? 'API Key 已就绪 · 豆包流式语音识别 1.0'
    : (st.detail || '请先保存 API Key');
}

function renderApiKeyState() {
  const keyInput = $('doubaoApiKey');
  keyInput.placeholder = apiKeyState.configured ? '已安全保存' : '输入豆包 API Key';
  $('btnRemoveApiKey').style.display = apiKeyState.configured ? '' : 'none';
}

/* ---------- 渲染 ---------- */
function render() {
  const radio = document.querySelector(`#injectSeg input[value="${cfg.inject_mode}"]`);
  if (radio) radio.checked = true;
  $('autostart').checked = cfg.autostart;
  $('hotwords').value = cfg.hotwords || '';
  $('maxRecord').value = cfg.max_record_secs;
  $('maxRecordVal').textContent = cfg.max_record_secs + ' 秒';
  $('autoStop').value = cfg.auto_stop_secs;
  $('autoStopVal').textContent = fmtAutoStop(cfg.auto_stop_secs);
  renderApiKeyState();
}

async function refreshApiKeyState() {
  apiKeyState = await invoke('doubao_api_key_status');
  renderApiKeyState();
}

async function loadMics() {
  try {
    const mics = await invoke('list_input_devices');
    const sel = $('micDevice');
    sel.innerHTML = '<option value="">系统默认</option>';
    for (const m of mics) {
      const o = document.createElement('option');
      o.value = m; o.textContent = m;
      sel.appendChild(o);
    }
    sel.value = cfg.mic_device || '';
  } catch (_) {}
}

/* ---------- 事件绑定 ---------- */
function bind() {
  for (const r of document.querySelectorAll('#injectSeg input')) {
    r.addEventListener('change', e => {
      if (e.target.checked) { cfg.inject_mode = e.target.value; save(); }
    });
  }
  $('autostart').addEventListener('change', e => { cfg.autostart = e.target.checked; save(); });
  $('hotwords').addEventListener('input', e => { cfg.hotwords = e.target.value; save(); });
  $('micDevice').addEventListener('change', e => { cfg.mic_device = e.target.value || null; save(); });
  $('maxRecord').addEventListener('input', e => {
    cfg.max_record_secs = parseInt(e.target.value);
    $('maxRecordVal').textContent = cfg.max_record_secs + ' 秒';
    save();
  });
  $('autoStop').addEventListener('input', e => {
    cfg.auto_stop_secs = parseFloat(e.target.value);
    $('autoStopVal').textContent = fmtAutoStop(cfg.auto_stop_secs);
    save();
  });

  const saveApiKey = async () => {
    const input = $('doubaoApiKey');
    const apiKey = input.value.trim();
    if (!apiKey) {
      toast('请输入 API Key');
      return;
    }
    try {
      await invoke('set_doubao_api_key', { apiKey });
      input.value = '';
      await refreshApiKeyState();
      renderEngine(await invoke('engine_status'));
      toast('API Key 已安全保存');
    } catch (e) {
      toast(String(e));
    }
  };
  $('btnSaveApiKey').addEventListener('click', saveApiKey);
  $('doubaoApiKey').addEventListener('keydown', e => {
    if (e.key === 'Enter') saveApiKey();
  });
  $('btnRemoveApiKey').addEventListener('click', async () => {
    await invoke('set_doubao_api_key', { apiKey: '' });
    await refreshApiKeyState();
    renderEngine(await invoke('engine_status'));
    toast('API Key 已移除');
  });

  $('btnOpenLogs').addEventListener('click', () => invoke('open_log_dir'));
}

/* ---------- 启动 ---------- */
(async function init() {
  cfg = await invoke('get_config');
  apiKeyState = await invoke('doubao_api_key_status');
  render();
  bind();
  loadMics();
  renderEngine(await invoke('engine_status'));
  listen('engine:status', e => renderEngine(e.payload));
})();
