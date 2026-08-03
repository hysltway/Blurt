/* Blurt 设置页逻辑 */
'use strict';

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

let cfg = null;
let usageStats = null;
let saveTimer = null;
let apiKeyState = { configured: false, error: null };
let apiEditorOpen = false;
let capturingHotkey = false;
let pendingHotkey = null;
let resizeQueued = false;
let lastRequestedSize = null;

const $ = id => document.getElementById(id);
const SVG_NS = 'http://www.w3.org/2000/svg';
const INJECT_MODE_INDEX = { auto: 0, type: 1, paste: 2 };

/* ---------- 提示 ---------- */
let toastTimer = null;
function toast(msg) {
  const el = $('toast');
  el.textContent = msg;
  el.classList.add('show');
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => el.classList.remove('show'), 1600);
}

/* ---------- 响应式窗口尺寸 ---------- */
function measureSettingsSize() {
  const page = document.querySelector('.page-shell');
  const bodyStyle = getComputedStyle(document.body);
  const horizontalPadding = parseFloat(bodyStyle.paddingLeft) + parseFloat(bodyStyle.paddingRight);
  return {
    width: Math.ceil(page.getBoundingClientRect().width + horizontalPadding),
    height: Math.ceil(document.body.scrollHeight),
  };
}

function scheduleSettingsResize() {
  if (resizeQueued) return;
  resizeQueued = true;
  requestAnimationFrame(() => {
    resizeQueued = false;
    const size = measureSettingsSize();
    if (lastRequestedSize &&
        Math.abs(lastRequestedSize.width - size.width) < 1 &&
        Math.abs(lastRequestedSize.height - size.height) < 1) {
      return;
    }
    lastRequestedSize = size;
    invoke('set_settings_size', size).catch(() => {});
  });
}

function installSettingsResize() {
  const page = document.querySelector('.page-shell');
  if (typeof ResizeObserver === 'function') {
    const observer = new ResizeObserver(scheduleSettingsResize);
    observer.observe(page);
  }
  window.addEventListener('resize', scheduleSettingsResize);
  scheduleSettingsResize();
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
      cfg = await invoke('get_config');
      render();
    }
  };
  if (immediate) return doSave();
  saveTimer = setTimeout(doSave, 350);
}

function fmtAutoStop(v) {
  return v > 0 ? v + ' 秒' : '关闭';
}

function renderHotkey(hotkey) {
  const value = $('hotkeyValue');
  value.replaceChildren();
  for (const [index, key] of String(hotkey || 'Ctrl+Alt').split('+').entries()) {
    if (index) {
      const plus = document.createElement('span');
      plus.className = 'plus';
      plus.textContent = '+';
      value.appendChild(plus);
    }
    const keycap = document.createElement('kbd');
    keycap.textContent = key;
    value.appendChild(keycap);
  }
}

function capturedKey(event) {
  const named = {
    ' ': 'Space',
    Spacebar: 'Space',
    Tab: 'Tab',
    Enter: 'Enter',
    Backspace: 'Backspace',
    Insert: 'Insert',
    Delete: 'Delete',
    Home: 'Home',
    End: 'End',
    PageUp: 'PageUp',
    PageDown: 'PageDown',
    ArrowUp: 'Up',
    ArrowDown: 'Down',
    ArrowLeft: 'Left',
    ArrowRight: 'Right',
  };
  if (named[event.key]) return named[event.key];
  if (/^[a-z0-9]$/i.test(event.key)) return event.key.toUpperCase();
  if (/^F(?:[1-9]|1\d|2[0-4])$/.test(event.key)) return event.key.toUpperCase();
  return null;
}

function capturedHotkey(event) {
  const modifiers = [
    event.ctrlKey && 'Ctrl',
    event.altKey && 'Alt',
    event.shiftKey && 'Shift',
    event.metaKey && 'Win',
  ].filter(Boolean);
  const key = capturedKey(event);
  if (!key && modifiers.length < 2) return null;
  if (key && !modifiers.length) return null;
  return { value: [...modifiers, key].filter(Boolean).join('+'), hasPrimary: Boolean(key) };
}

function endHotkeyCapture() {
  if (!capturingHotkey) return;
  capturingHotkey = false;
  pendingHotkey = null;
  $('btnCaptureHotkey').textContent = '修改';
  invoke('set_hotkey_capture', { capturing: false }).catch(() => {});
}

async function beginHotkeyCapture() {
  if (capturingHotkey) {
    endHotkeyCapture();
    return;
  }
  try {
    await invoke('set_hotkey_capture', { capturing: true });
    capturingHotkey = true;
    pendingHotkey = null;
    $('btnCaptureHotkey').textContent = '按下按键';
    $('btnCaptureHotkey').focus();
  } catch (e) {
    toast(String(e));
  }
}

function applyCapturedHotkey(hotkey) {
  endHotkeyCapture();
  cfg.hotkey = hotkey;
  renderHotkey(hotkey);
  save(true);
}

/* ---------- 引擎与密钥 ---------- */
const API_PILL_TEXT = {
  ready: '已就绪',
  loading: '检测中…',
  missing: '未配置',
  failed: '不可用',
};

function renderEngine(st) {
  const dotCls = { ready: 'ok', loading: 'loading', missing: 'missing' }[st.state] || 'err';
  $('pillDot').className = 'dot ' + dotCls;
  $('pillText').textContent = API_PILL_TEXT[st.state] || API_PILL_TEXT.failed;
  const detail = st.detail || API_PILL_TEXT[st.state] || API_PILL_TEXT.failed;
  const pill = document.querySelector('.engine-pill');
  pill.title = detail;
  pill.setAttribute('aria-label', detail);
}

async function refreshEngineStatus() {
  try {
    renderEngine(await invoke('refresh_engine_status'));
  } catch (e) {
    renderEngine({ state: 'failed', detail: String(e) });
  }
}

function updateApiEditor(open) {
  apiEditorOpen = open;
  $('credentialEditor').hidden = !open;
  $('btnEditApiKey').setAttribute('aria-expanded', String(open));
  $('btnEditApiKey').textContent = open
    ? '收起'
    : (apiKeyState.configured ? '更换密钥' : '配置密钥');
}

function setApiEditorOpen(open) {
  updateApiEditor(open);
  scheduleSettingsResize();
}

function renderApiKeyState() {
  const keyInput = $('doubaoApiKey');
  keyInput.placeholder = apiKeyState.configured ? '输入新 API Key' : '输入豆包 API Key';
  $('btnRemoveApiKey').style.display = apiKeyState.configured ? '' : 'none';
  updateApiEditor(apiEditorOpen);
}

/* ---------- 使用统计 ---------- */
function dateKey(date) {
  const year = date.getFullYear();
  const month = String(date.getMonth() + 1).padStart(2, '0');
  const day = String(date.getDate()).padStart(2, '0');
  return `${year}-${month}-${day}`;
}

function shiftedDate(date, offset) {
  const result = new Date(date);
  result.setHours(12, 0, 0, 0);
  result.setDate(result.getDate() + offset);
  return result;
}

function shortDate(date) {
  return `${date.getMonth() + 1}月${date.getDate()}日`;
}

function formatDuration(seconds, zeroAsMinutes = false) {
  const value = Math.max(0, Number(seconds) || 0);
  if (value === 0 && zeroAsMinutes) return '0 分钟';
  if (value < 60) return `${Math.round(value)} 秒`;
  if (value < 3600) return `${Math.round(value / 60)} 分钟`;
  const hours = value / 3600;
  return `${hours >= 100 ? Math.round(hours) : hours.toFixed(1)} 小时`;
}

function statsByDate(stats) {
  return new Map((stats.daily_usage || []).map(day => [day.date, day]));
}

function recentDays(stats, count) {
  const byDate = statsByDate(stats);
  const today = new Date();
  today.setHours(12, 0, 0, 0);
  return Array.from({ length: count }, (_, index) => {
    const date = shiftedDate(today, index - count + 1);
    const usage = byDate.get(dateKey(date));
    return {
      date,
      dateKey: dateKey(date),
      audio_secs: Number(usage?.audio_secs) || 0,
      chars: Number(usage?.chars) || 0,
      sessions: Number(usage?.sessions) || 0,
    };
  });
}

function currentStreak(stats) {
  const active = new Set(
    (stats.daily_usage || []).filter(day => day.sessions > 0).map(day => day.date),
  );
  const today = new Date();
  today.setHours(12, 0, 0, 0);
  let cursor = today;
  let includesToday = active.has(dateKey(today));
  if (!includesToday) cursor = shiftedDate(today, -1);

  let days = 0;
  while (active.has(dateKey(cursor))) {
    days += 1;
    cursor = shiftedDate(cursor, -1);
  }
  return { days, includesToday };
}

function renderLifetimeMetrics(stats) {
  // 顶部四张卡片展示跨日期累计数据；近期范围只用于趋势图和热力图。
  const totalSeconds = Number(stats.total_audio_secs) || 0;
  const totalChars = Number(stats.total_chars) || 0;
  const typingSeconds = totalChars / 2.45;
  const savedSeconds = Math.max(0, typingSeconds - totalSeconds);
  const streak = currentStreak(stats);

  $('metricDuration').textContent = formatDuration(totalSeconds, true);
  $('metricCharacters').textContent = totalChars >= 10000
    ? `${(totalChars / 10000).toFixed(1)} 万字`
    : `${new Intl.NumberFormat('zh-CN').format(totalChars)} 字`;
  $('metricSaved').textContent = `约 ${formatDuration(savedSeconds, true)}`;
  $('metricStreak').textContent = `${streak.days} 天`;

}

function svgElement(name, attributes = {}, text = '') {
  const node = document.createElementNS(SVG_NS, name);
  for (const [key, value] of Object.entries(attributes)) node.setAttribute(key, value);
  if (text) node.textContent = text;
  return node;
}

function niceMinutes(maxMinutes) {
  const candidates = [1, 2, 5, 10, 15, 30, 60, 120, 240, 480, 720];
  return candidates.find(value => value >= maxMinutes) || Math.ceil(maxMinutes / 240) * 240;
}

function formatAxisMinutes(minutes, maximum) {
  if (maximum <= 1) return `${Math.round(minutes * 60)}s`;
  if (minutes >= 60) return `${Number((minutes / 60).toFixed(1))}h`;
  return `${Number(minutes.toFixed(minutes < 10 ? 1 : 0))}m`;
}

function renderTrend(stats) {
  const days = recentDays(stats, 30);
  const svg = $('durationChart');
  const tooltip = $('chartTooltip');
  const totalSeconds = days.reduce((sum, day) => sum + day.audio_secs, 0);
  const hasData = days.some(day => day.audio_secs > 0);
  const width = 620;
  const height = 190;
  const plot = { left: 42, top: 10, right: 10, bottom: 25 };
  const plotWidth = width - plot.left - plot.right;
  const plotHeight = height - plot.top - plot.bottom;
  const maxMinutes = niceMinutes(Math.max(...days.map(day => day.audio_secs / 60), 1));

  svg.replaceChildren();
  tooltip.hidden = true;
  $('trendEmpty').hidden = hasData;
  $('trendTotal').textContent = `共 ${formatDuration(totalSeconds, true)}`;

  for (let index = 0; index <= 4; index += 1) {
    const y = plot.top + plotHeight * index / 4;
    const minutes = maxMinutes * (1 - index / 4);
    const label = formatAxisMinutes(minutes, maxMinutes);
    svg.appendChild(svgElement('line', {
      x1: plot.left,
      y1: y,
      x2: width - plot.right,
      y2: y,
      class: 'chart-gridline',
    }));
    svg.appendChild(svgElement('text', {
      x: plot.left - 8,
      y: y + 3,
      'text-anchor': 'end',
      class: 'chart-axis-label',
    }, label));
  }

  const points = days.map((day, index) => ({
    ...day,
    x: plot.left + plotWidth * index / (days.length - 1),
    y: plot.top + plotHeight * (1 - (day.audio_secs / 60) / maxMinutes),
  }));
  const linePath = points.map((point, index) => `${index ? 'L' : 'M'} ${point.x.toFixed(2)} ${point.y.toFixed(2)}`).join(' ');
  const areaPath = `${linePath} L ${points.at(-1).x.toFixed(2)} ${(plot.top + plotHeight).toFixed(2)} L ${points[0].x.toFixed(2)} ${(plot.top + plotHeight).toFixed(2)} Z`;
  if (hasData) {
    svg.appendChild(svgElement('path', { d: areaPath, class: 'chart-area' }));
    svg.appendChild(svgElement('path', { d: linePath, class: 'chart-line' }));
  }

  const labelIndexes = new Set([0, 6, 13, 20, 29]);
  for (const [index, point] of points.entries()) {
    if (labelIndexes.has(index)) {
      svg.appendChild(svgElement('text', {
        x: point.x,
        y: height - 6,
        'text-anchor': index === 0 ? 'start' : (index === 29 ? 'end' : 'middle'),
        class: 'chart-axis-label',
      }, `${String(point.date.getMonth() + 1).padStart(2, '0')}-${String(point.date.getDate()).padStart(2, '0')}`));
    }

    if (!hasData) continue;

    if (point.audio_secs > 0) {
      svg.appendChild(svgElement('circle', {
        cx: point.x,
        cy: point.y,
        r: 2.8,
        class: 'chart-point',
      }));
    }

    const hit = svgElement('circle', {
      cx: point.x,
      cy: point.y,
      r: 8,
      class: 'chart-hit',
      tabindex: '0',
      'aria-label': `${shortDate(point.date)}，${formatDuration(point.audio_secs, true)}`,
    });
    const showTooltip = event => {
      const stageRect = $('trendStage').getBoundingClientRect();
      const svgRect = svg.getBoundingClientRect();
      tooltip.textContent = `${shortDate(point.date)} · ${formatDuration(point.audio_secs, true)}`;
      tooltip.style.left = `${svgRect.left - stageRect.left + point.x / width * svgRect.width}px`;
      tooltip.style.top = `${svgRect.top - stageRect.top + point.y / height * svgRect.height}px`;
      tooltip.hidden = false;
      if (event.type === 'focus') hit.setAttribute('r', '9');
    };
    const hideTooltip = () => {
      tooltip.hidden = true;
      hit.setAttribute('r', '8');
    };
    hit.addEventListener('mouseenter', showTooltip);
    hit.addEventListener('mouseleave', hideTooltip);
    hit.addEventListener('focus', showTooltip);
    hit.addEventListener('blur', hideTooltip);
    svg.appendChild(hit);
  }
}

function heatLevel(seconds, maximum) {
  if (seconds <= 0 || maximum <= 0) return 0;
  const ratio = seconds / maximum;
  if (ratio <= 0.2) return 1;
  if (ratio <= 0.45) return 2;
  if (ratio <= 0.75) return 3;
  return 4;
}

function renderHeatmap(stats) {
  const heatmap = $('usageHeatmap');
  const byDate = statsByDate(stats);
  const today = new Date();
  today.setHours(12, 0, 0, 0);
  const end = shiftedDate(today, 6 - today.getDay());
  const start = shiftedDate(end, -(12 * 7 - 1));
  const days = Array.from({ length: 84 }, (_, index) => {
    const date = shiftedDate(start, index);
    const usage = byDate.get(dateKey(date));
    return {
      date,
      audio_secs: Number(usage?.audio_secs) || 0,
      sessions: Number(usage?.sessions) || 0,
      outside: date > today,
    };
  });
  const maximum = Math.max(...days.filter(day => !day.outside).map(day => day.audio_secs), 0);
  const activeDays = days.filter(day => !day.outside && day.sessions > 0).length;

  heatmap.replaceChildren();
  heatmap.style.setProperty('--weeks', '12');
  for (const day of days) {
    const cell = document.createElement('span');
    const level = day.outside ? 0 : heatLevel(day.audio_secs, maximum);
    const description = day.outside
      ? `${shortDate(day.date)}，尚未到来`
      : `${shortDate(day.date)}，${day.sessions} 次，${formatDuration(day.audio_secs, true)}`;
    cell.className = 'heatmap-cell' + (day.outside ? ' is-outside' : '');
    cell.dataset.level = String(level);
    cell.tabIndex = 0;
    cell.setAttribute('aria-label', description);
    cell.title = description;
    heatmap.appendChild(cell);
  }

  $('heatmapTotal').textContent = `${activeDays} 个活跃日`;
  $('heatmapRange').textContent = `${shortDate(start)} - ${shortDate(today)}`;
  heatmap.setAttribute('aria-label', `最近 12 周使用热力图，共 ${activeDays} 个活跃日`);
}

function renderUsageStats(stats) {
  usageStats = stats;
  renderLifetimeMetrics(stats);
  renderTrend(stats);
  renderHeatmap(stats);
}

/* ---------- 设置渲染 ---------- */
function syncInjectSegment(mode) {
  $('injectSeg').style.setProperty('--seg-index', String(INJECT_MODE_INDEX[mode] ?? 0));
}

function render() {
  renderHotkey(cfg.hotkey);
  const radio = document.querySelector(`#injectSeg input[value="${cfg.inject_mode}"]`);
  if (radio) radio.checked = true;
  syncInjectSegment(cfg.inject_mode);
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
    for (const mic of mics) {
      const option = document.createElement('option');
      option.value = mic;
      option.textContent = mic;
      sel.appendChild(option);
    }
    sel.value = cfg.mic_device || '';
    sel.disabled = false;
  } catch (e) {
    const sel = $('micDevice');
    sel.innerHTML = '<option value="">麦克风不可用</option>';
    sel.disabled = true;
    toast('读取麦克风失败：' + e);
  }
}

/* ---------- 事件绑定 ---------- */
function bind() {
  $('btnCaptureHotkey').addEventListener('click', beginHotkeyCapture);
  document.addEventListener('keydown', event => {
    if (!capturingHotkey) return;
    event.preventDefault();
    event.stopPropagation();
    if (event.key === 'Escape') {
      endHotkeyCapture();
      return;
    }
    const hotkey = capturedHotkey(event);
    if (!hotkey) return;
    pendingHotkey = hotkey.value;
    if (hotkey.hasPrimary) applyCapturedHotkey(hotkey.value);
  }, true);
  document.addEventListener('keyup', event => {
    if (!capturingHotkey) return;
    event.preventDefault();
    event.stopPropagation();
    if (!event.ctrlKey && !event.altKey && !event.shiftKey && !event.metaKey && pendingHotkey) {
      applyCapturedHotkey(pendingHotkey);
    }
  }, true);
  window.addEventListener('blur', endHotkeyCapture);

  for (const radio of document.querySelectorAll('#injectSeg input')) {
    radio.addEventListener('change', async event => {
      if (!event.target.checked) return;
      cfg.inject_mode = event.target.value;
      syncInjectSegment(cfg.inject_mode);
      await save(true);
      void refreshEngineStatus();
    });
  }
  $('autostart').addEventListener('change', event => {
    cfg.autostart = event.target.checked;
    save();
  });
  $('hotwords').addEventListener('input', event => {
    cfg.hotwords = event.target.value;
    save();
  });
  $('micDevice').addEventListener('change', async event => {
    cfg.mic_device = event.target.value || null;
    await save(true);
    void refreshEngineStatus();
  });
  $('maxRecord').addEventListener('input', event => {
    cfg.max_record_secs = parseInt(event.target.value);
    $('maxRecordVal').textContent = cfg.max_record_secs + ' 秒';
    save();
  });
  $('autoStop').addEventListener('input', event => {
    cfg.auto_stop_secs = parseFloat(event.target.value);
    $('autoStopVal').textContent = fmtAutoStop(cfg.auto_stop_secs);
    save();
  });

  $('btnEditApiKey').addEventListener('click', async () => {
    const opening = !apiEditorOpen;
    setApiEditorOpen(opening);
    if (opening) $('doubaoApiKey').focus();
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
      apiEditorOpen = false;
      await refreshApiKeyState();
      setApiEditorOpen(false);
      renderEngine(await invoke('engine_status'));
      toast('API Key 已安全保存');
    } catch (e) {
      toast(String(e));
    }
  };
  $('btnSaveApiKey').addEventListener('click', saveApiKey);
  $('doubaoApiKey').addEventListener('keydown', event => {
    if (event.key === 'Enter') saveApiKey();
  });
  $('btnRemoveApiKey').addEventListener('click', async () => {
    await invoke('set_doubao_api_key', { apiKey: '' });
    apiEditorOpen = true;
    await refreshApiKeyState();
    setApiEditorOpen(true);
    renderEngine(await invoke('engine_status'));
    toast('API Key 已移除');
  });

  $('btnOpenLogs').addEventListener('click', () => invoke('open_log_dir'));
}

/* ---------- 启动 ---------- */
(async function init() {
  [cfg, apiKeyState, usageStats] = await Promise.all([
    invoke('get_config'),
    invoke('doubao_api_key_status'),
    invoke('get_usage_stats'),
  ]);
  apiEditorOpen = !apiKeyState.configured;
  render();
  renderUsageStats(usageStats);
  bind();
  loadMics();
  setApiEditorOpen(apiEditorOpen);
  installSettingsResize();
  renderEngine(await invoke('engine_status'));
  await listen('engine:status', event => renderEngine(event.payload));
  await listen('usage:updated', event => renderUsageStats(event.payload));
  void refreshEngineStatus();
})();
