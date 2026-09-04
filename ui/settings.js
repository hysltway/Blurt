/* Blurt 设置页逻辑 */
'use strict';

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

let cfg = null;
let usageStats = null;
let saveTimer = null;
let apiKeyState = { configured: false, active_name: null, count: 0, error: null };
let isApiKeyModalOpen = false;
let voiceprintInfo = { has_voiceprint: false, created_at: null, model_ready: false };
let isVoiceprintModalOpen = false;
let vpAudioCtx = null;
let vpMediaStream = null;
let vpScriptProcessor = null;
let vpAnalyser = null;
let vpRecordedChunks = [];
let vpFinalSamples = null;
let vpRecordStartTime = null;
const VP_RECORD_DURATION = 10.0;
let capturingHotkey = false;
let pendingHotkey = null;
let resizeQueued = false;
let lastRequestedSize = null;

const $ = id => document.getElementById(id);
const SVG_NS = 'http://www.w3.org/2000/svg';
const INJECT_MODE_INDEX = { auto: 0, type: 1, paste: 2 };

function escapeHtml(str) {
  if (!str) return '';
  return String(str)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#039;');
}

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
  const num = Number(v) || 0;
  return num > 0 ? `${parseFloat(num.toFixed(2))} 秒` : '关闭';
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

function extractBriefReason(detail) {
  if (!detail || typeof detail !== 'string') return '';
  if (detail.includes('均可用') || detail.includes('正在检查')) return '';
  if (detail === '请先保存 API Key' || detail.includes('保存 API Key') || detail.includes('配置 API Key')) {
    return '未配置 API Key';
  }

  const parts = detail.split(/[；;]/).map(s => s.trim()).filter(Boolean);
  const reasons = parts.map(part => {
    if (/401|Unauthorized|鉴权失败|认证失败/i.test(part)) return 'API Key 无效';
    if (/403|Forbidden/i.test(part)) return '服务无权限';
    if (/429|TooManyRequests|限流|配额|额度|欠费|arrears/i.test(part)) return '接口额度耗尽或限流';
    if (/超时|timeout|timedout/i.test(part)) return '网络连接超时';
    if (/关闭了连接|closed/i.test(part)) return '连接被断开';
    if (/连接.*失败|无法连接|ConnectionRefused|Failed to connect/i.test(part)) return '豆包连接失败';
    if (/豆包识别失败/i.test(part)) {
      const codeMatch = part.match(/（(\d+)）|\((\d+)\)/);
      const code = codeMatch ? (codeMatch[1] || codeMatch[2]) : '';
      return code ? `识别失败(${code})` : '豆包识别失败';
    }
    if (/读取 API Key 失败/i.test(part)) return 'API Key 读取失败';
    if (/豆包/i.test(part)) return '豆包服务异常';

    if (/未找到.*麦克风|找不到.*麦克风|无可用麦克风/i.test(part)) return '未找到麦克风';
    if (/麦克风.*占用|device in use/i.test(part)) return '麦克风被占用';
    if (/麦克风.*权限|麦克风.*拒绝|permission denied/i.test(part)) return '麦克风无权限';
    if (/无法打开麦克风|无法启动录音|麦克风配置/i.test(part)) return '麦克风启动失败';
    if (/麦克风/i.test(part)) return '麦克风异常';

    if (/未找到可写入的活动窗口|未找到活动窗口/i.test(part)) return '未找到活动窗口';
    if (/SendInput 被系统拒绝|管理员权限/i.test(part)) return '目标窗口权限过高';
    if (/剪贴板/i.test(part)) return '剪贴板访问失败';
    if (/文本写入/i.test(part)) return '文本写入失败';

    let clean = part
      .replace(/^豆包服务（网络或 API Key）：/, '')
      .replace(/^豆包服务：/, '')
      .replace(/^麦克风：/, '')
      .replace(/^文本写入：/, '')
      .replace(/^无法启动可用性检查：/, '')
      .replace(/^读取 API Key 失败：/, '')
      .trim();
    clean = clean.split(/[:：\n]/)[0].trim();
    if (clean.length > 14) clean = clean.slice(0, 14) + '…';
    return clean;
  }).filter(Boolean);

  const unique = [...new Set(reasons)];
  if (!unique.length) return '';
  return unique.join('、');
}

function renderEngine(st) {
  const dotCls = { ready: 'ok', loading: 'loading', missing: 'missing' }[st.state] || 'err';
  const pill = $('enginePill') || document.querySelector('.engine-pill');
  const dot = $('pillDot');
  const text = $('pillText');
  const reasonEl = $('pillReason');

  if (dot) dot.className = 'dot ' + dotCls;
  if (text) text.textContent = API_PILL_TEXT[st.state] || API_PILL_TEXT.failed;

  if (pill) {
    pill.classList.remove('is-error', 'is-missing');
    if (st.state === 'failed') {
      pill.classList.add('is-error');
    } else if (st.state === 'missing') {
      pill.classList.add('is-missing');
    }
  }

  const briefReason = (st.state === 'failed' || st.state === 'missing')
    ? extractBriefReason(st.detail)
    : '';

  if (reasonEl) {
    if (briefReason) {
      reasonEl.textContent = briefReason;
      reasonEl.hidden = false;
    } else {
      reasonEl.textContent = '';
      reasonEl.hidden = true;
    }
  }

  const detail = st.detail || API_PILL_TEXT[st.state] || API_PILL_TEXT.failed;
  const fullLabel = briefReason ? `${API_PILL_TEXT[st.state] || '不可用'}（${detail}）` : detail;
  if (pill) {
    pill.title = detail;
    pill.setAttribute('aria-label', fullLabel);
  }
}

async function refreshEngineStatus() {
  try {
    renderEngine(await invoke('refresh_engine_status'));
  } catch (e) {
    renderEngine({ state: 'failed', detail: String(e) });
  }
}

function renderApiKeyState() {
  const badge = $('activeKeyBadge');
  const btn = $('btnOpenApiKeyModal');
  if (apiKeyState && apiKeyState.configured) {
    badge.textContent = apiKeyState.active_name || '已配置';
    badge.classList.add('is-configured');
    badge.title = `当前生效密钥：${apiKeyState.active_name || '已配置'}`;
    btn.textContent = '管理密钥';
  } else {
    badge.textContent = '未配置';
    badge.classList.remove('is-configured');
    badge.title = '未配置 API Key';
    btn.textContent = '配置密钥';
  }
}

function setAddKeySectionOpen(open) {
  const section = $('addKeySection');
  const btn = $('btnToggleAddKey');
  if (section) section.hidden = !open;
  if (btn) btn.title = open ? '收起添加' : '添加新密钥';
  if (open) $('newKeyName')?.focus();
}

function openApiKeyModal() {
  const modal = $('apiKeyModal');
  modal.hidden = false;
  modal.setAttribute('aria-hidden', 'false');
  isApiKeyModalOpen = true;
  setAddKeySectionOpen(false);
  loadAndRenderApiKeyList();
}

function closeApiKeyModal() {
  const modal = $('apiKeyModal');
  modal.hidden = true;
  modal.setAttribute('aria-hidden', 'true');
  isApiKeyModalOpen = false;
  setAddKeySectionOpen(false);
  $('newKeyName').value = '';
  $('newKeyValue').value = '';
}

async function loadAndRenderApiKeyList() {
  const listEl = $('apiKeyList');
  try {
    const keys = await invoke('list_doubao_api_keys');
    if (!keys || keys.length === 0) {
      listEl.innerHTML = '<div class="api-key-empty"><span>暂无已保存的 API Key</span></div>';
      return;
    }
    listEl.replaceChildren();
    for (const item of keys) {
      const row = document.createElement('div');
      row.className = 'api-key-item';
      row.setAttribute('role', 'listitem');

      const left = document.createElement('div');
      left.className = 'key-item-left';

      const dot = document.createElement('span');
      dot.className = 'key-status-dot' + (item.is_active ? ' is-active' : '');
      dot.title = item.is_active ? '正在使用' : '';
      left.appendChild(dot);

      const name = document.createElement('span');
      name.className = 'key-item-name';
      name.textContent = item.name;
      name.title = item.name;
      left.appendChild(name);

      const code = document.createElement('code');
      code.className = 'key-masked';
      code.textContent = item.masked_key;
      left.appendChild(code);

      if (item.created_at) {
        const date = document.createElement('span');
        date.className = 'key-date';
        date.textContent = item.created_at;
        left.appendChild(date);
      }
      row.appendChild(left);

      const right = document.createElement('div');
      right.className = 'key-item-right';

      if (!item.is_active) {
        const btnSelect = document.createElement('button');
        btnSelect.className = 'btn btn-sm';
        btnSelect.type = 'button';
        btnSelect.textContent = '使用';
        btnSelect.addEventListener('click', async () => {
          try {
            await invoke('select_doubao_api_key', { id: item.id });
            toast(`已切换至 "${item.name}"`);
            await refreshApiKeyState();
            await loadAndRenderApiKeyList();
            renderEngine(await invoke('engine_status'));
          } catch (e) {
            toast(String(e));
          }
        });
        right.appendChild(btnSelect);
      }

      const btnDelete = document.createElement('button');
      btnDelete.className = 'btn-icon btn-delete-key';
      btnDelete.type = 'button';
      btnDelete.setAttribute('aria-label', `删除密钥 "${item.name}"`);
      btnDelete.title = `删除密钥 "${item.name}"`;
      btnDelete.innerHTML = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M3 6h18M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"></path><line x1="10" y1="11" x2="10" y2="17"></line><line x1="14" y1="11" x2="14" y2="17"></line></svg>';
      btnDelete.addEventListener('click', async () => {
        try {
          await invoke('delete_doubao_api_key', { id: item.id });
          toast(`已删除密钥 "${item.name}"`);
          await refreshApiKeyState();
          await loadAndRenderApiKeyList();
          renderEngine(await invoke('engine_status'));
        } catch (e) {
          toast(String(e));
        }
      });
      right.appendChild(btnDelete);

      row.appendChild(right);
      listEl.appendChild(row);
    }
  } catch (e) {
    listEl.innerHTML = `<div class="api-key-empty"><span>读取列表失败：${escapeHtml(String(e))}</span></div>`;
  }
}

async function handleAddApiKey() {
  const nameInput = $('newKeyName');
  const keyInput = $('newKeyValue');
  const name = nameInput.value.trim();
  const apiKey = keyInput.value.trim();
  if (!apiKey) {
    toast('请输入 API Key');
    keyInput.focus();
    return;
  }
  try {
    await invoke('add_doubao_api_key', { name, apiKey });
    nameInput.value = '';
    keyInput.value = '';
    setAddKeySectionOpen(false);
    toast('已保存并启用该密钥');
    await refreshApiKeyState();
    await loadAndRenderApiKeyList();
    renderEngine(await invoke('engine_status'));
  } catch (e) {
    toast(String(e));
  }
}

/* ---------- 专属声纹防干扰 ---------- */
async function refreshVoiceprintInfo() {
  try {
    voiceprintInfo = await invoke('get_voiceprint_info');
    renderVoiceprintState();
  } catch (e) {
    console.error('读取声纹信息失败:', e);
  }
}

function renderVoiceprintState() {
  const badge = $('voiceprintBadge');
  const btnOpen = $('btnOpenVoiceprintModal');
  const btnDelete = $('btnDeleteVoiceprint');
  const enabledToggle = $('voiceprintEnabled');

  if (voiceprintInfo.has_voiceprint) {
    badge.textContent = '已就绪';
    badge.className = 'active-key-badge vp-badge ready';
    badge.title = voiceprintInfo.created_at ? `录制于 ${voiceprintInfo.created_at}` : '已录制专属声纹';
    btnOpen.textContent = '管理';
    if (btnDelete) btnDelete.hidden = false;
    enabledToggle.disabled = false;
  } else {
    badge.textContent = '未录制';
    badge.className = 'active-key-badge vp-badge';
    badge.title = '尚未录制专属声纹';
    btnOpen.textContent = '录制';
    if (btnDelete) btnDelete.hidden = true;
    enabledToggle.checked = false;
  }
}

function resampleTo16k(samples, srcRate) {
  if (!srcRate || srcRate === 16000) return samples;
  const ratio = 16000 / srcRate;
  const newLen = Math.round(samples.length * ratio);
  const result = new Float32Array(newLen);
  for (let i = 0; i < newLen; i++) {
    const srcIdx = i / ratio;
    const idx0 = Math.floor(srcIdx);
    const idx1 = Math.min(idx0 + 1, samples.length - 1);
    const frac = srcIdx - idx0;
    result[i] = samples[idx0] * (1 - frac) + samples[idx1] * frac;
  }
  return result;
}

function drawIdleWaveform() {
  const canvas = $('vpWaveCanvas');
  if (!canvas) return;
  const ctx = canvas.getContext('2d');
  const w = canvas.width;
  const h = canvas.height;
  ctx.clearRect(0, 0, w, h);
  ctx.lineWidth = 2;
  ctx.strokeStyle = '#D1D5DB';
  ctx.beginPath();
  ctx.moveTo(0, h / 2);
  ctx.lineTo(w, h / 2);
  ctx.stroke();
}

function drawActiveWaveform() {
  if (!isVoiceprintModalOpen || !vpAnalyser) return;
  const canvas = $('vpWaveCanvas');
  if (!canvas) return;
  const ctx = canvas.getContext('2d');
  const w = canvas.width;
  const h = canvas.height;
  const bufferLength = vpAnalyser.fftSize;
  const dataArray = new Uint8Array(bufferLength);
  vpAnalyser.getByteTimeDomainData(dataArray);

  ctx.clearRect(0, 0, w, h);
  ctx.lineWidth = 2;
  ctx.strokeStyle = '#262322';
  ctx.beginPath();

  const sliceWidth = w / bufferLength;
  let x = 0;
  for (let i = 0; i < bufferLength; i++) {
    const v = dataArray[i] / 128.0;
    const y = (v * h) / 2;
    if (i === 0) ctx.moveTo(x, y);
    else ctx.lineTo(x, y);
    x += sliceWidth;
  }
  ctx.lineTo(w, h / 2);
  ctx.stroke();

  vpRecordAnimId = requestAnimationFrame(drawActiveWaveform);
}

function openVoiceprintModal() {
  isVoiceprintModalOpen = true;
  $('voiceprintModal').hidden = false;
  $('voiceprintModal').setAttribute('aria-hidden', 'false');
  resetVoiceprintStage();
  scheduleSettingsResize();
}

function closeVoiceprintModal() {
  stopVoiceprintRecording(false);
  isVoiceprintModalOpen = false;
  $('voiceprintModal').hidden = true;
  $('voiceprintModal').setAttribute('aria-hidden', 'true');
  scheduleSettingsResize();
}

function resetVoiceprintStage() {
  stopVoiceprintRecording(false);
  vpFinalSamples = null;
  drawIdleWaveform();
  $('vpStatusDot').className = 'dot';
  $('vpStatusText').textContent = voiceprintInfo.has_voiceprint
    ? '已录制专属声纹（点击“重新录制”可覆盖更新）'
    : '准备就绪（点击“开始录音”，推荐朗读 8~10 秒）';
  $('vpTimer').hidden = true;
  $('vpTimer').textContent = '10.0s';
  $('vpProgressWrap').hidden = true;
  $('vpProgressBar').style.width = '0%';
  $('btnRecordVoiceprint').textContent = '开始录音';
  $('btnRecordVoiceprint').hidden = false;
  $('btnRerecordVoiceprint').hidden = true;
  $('btnSaveVoiceprint').hidden = true;
  if ($('voiceprintThreshold')) {
    const th = cfg.voiceprint_threshold ?? 0.30;
    $('voiceprintThreshold').value = th;
    if ($('voiceprintThresholdVal')) {
      $('voiceprintThresholdVal').textContent = Number(th).toFixed(2);
    }
  }
  if ($('btnDeleteVoiceprint')) {
    $('btnDeleteVoiceprint').hidden = !voiceprintInfo.has_voiceprint;
  }
}

async function startVoiceprintRecording() {
  if (vpRecordStartTime) {
    const elapsed = (Date.now() - vpRecordStartTime) / 1000;
    if (elapsed >= 3.0) {
      stopVoiceprintRecording(true);
      return;
    } else {
      toast('建议至少朗读 3 秒以上以提取足够声纹特征');
      return;
    }
  }

  try {
    const stream = await navigator.mediaDevices.getUserMedia({
      audio: {
        channelCount: 1,
        echoCancellation: false,
        noiseSuppression: false,
        autoGainControl: false,
      },
    });

    vpMediaStream = stream;
    vpAudioCtx = new (window.AudioContext || window.webkitAudioContext)();
    const source = vpAudioCtx.createMediaStreamSource(stream);

    vpAnalyser = vpAudioCtx.createAnalyser();
    vpAnalyser.fftSize = 512;
    source.connect(vpAnalyser);

    vpRecordedChunks = [];
    vpScriptProcessor = vpAudioCtx.createScriptProcessor(4096, 1, 1);
    vpScriptProcessor.onaudioprocess = e => {
      if (!isVoiceprintModalOpen) return;
      const input = e.inputBuffer.getChannelData(0);
      vpRecordedChunks.push(new Float32Array(input));
    };
    source.connect(vpScriptProcessor);
    // 静音防啸叫：静音增益节点接入 destination，确保 onaudioprocess 持续触发且扬声器不产生回音
    const muteGain = vpAudioCtx.createGain();
    muteGain.gain.value = 0;
    vpScriptProcessor.connect(muteGain);
    muteGain.connect(vpAudioCtx.destination);

    vpRecordStartTime = Date.now();
    $('vpStatusDot').className = 'dot loading';
    $('vpStatusText').textContent = '正在录音，请朗读示范文本（读完可点“完成录音”）…';
    $('vpTimer').hidden = false;
    $('vpProgressWrap').hidden = false;
    $('btnRecordVoiceprint').hidden = false;
    $('btnRecordVoiceprint').textContent = '完成录音';
    $('btnRerecordVoiceprint').hidden = true;
    $('btnSaveVoiceprint').hidden = true;

    drawActiveWaveform();

    const checkInterval = setInterval(() => {
      if (!isVoiceprintModalOpen || !vpRecordStartTime) {
        clearInterval(checkInterval);
        return;
      }
      const elapsed = (Date.now() - vpRecordStartTime) / 1000;
      const remaining = Math.max(0, VP_RECORD_DURATION - elapsed);
      $('vpTimer').textContent = remaining.toFixed(1) + 's';
      $('vpProgressBar').style.width = `${Math.min(100, (elapsed / VP_RECORD_DURATION) * 100)}%`;

      if (elapsed >= VP_RECORD_DURATION) {
        clearInterval(checkInterval);
        stopVoiceprintRecording(true);
      }
    }, 80);
  } catch (e) {
    toast('无法访问麦克风：' + e);
    resetVoiceprintStage();
  }
}

function stopVoiceprintRecording(finished = false) {
  if (vpRecordAnimId) {
    cancelAnimationFrame(vpRecordAnimId);
    vpRecordAnimId = null;
  }
  if (vpScriptProcessor) {
    vpScriptProcessor.disconnect();
    vpScriptProcessor = null;
  }
  if (vpAnalyser) {
    vpAnalyser.disconnect();
    vpAnalyser = null;
  }
  if (vpMediaStream) {
    vpMediaStream.getTracks().forEach(t => t.stop());
    vpMediaStream = null;
  }
  vpRecordStartTime = null;

  if (finished && vpRecordedChunks.length > 0) {
    const totalLen = vpRecordedChunks.reduce((acc, c) => acc + c.length, 0);
    const merged = new Float32Array(totalLen);
    let offset = 0;
    for (const chunk of vpRecordedChunks) {
      merged.set(chunk, offset);
      offset += chunk.length;
    }
    const srcRate = vpAudioCtx ? vpAudioCtx.sampleRate : 16000;
    vpFinalSamples = resampleTo16k(merged, srcRate);

    if (vpAudioCtx) {
      vpAudioCtx.close().catch(() => {});
      vpAudioCtx = null;
    }

    $('vpStatusDot').className = 'dot ok';
    $('vpStatusText').textContent = '录音完成！请确认文本完整后点击“保存声纹”';
    $('vpTimer').textContent = `${(vpFinalSamples.length / 16000).toFixed(1)}s`;
    $('vpProgressBar').style.width = '100%';
    $('btnRecordVoiceprint').textContent = '开始录音';
    $('btnRecordVoiceprint').hidden = true;
    $('btnRerecordVoiceprint').hidden = false;
    $('btnSaveVoiceprint').hidden = false;
    drawIdleWaveform();
  } else {
    if (vpAudioCtx) {
      vpAudioCtx.close().catch(() => {});
      vpAudioCtx = null;
    }
  }
}

async function handleSaveVoiceprint() {
  if (!vpFinalSamples || vpFinalSamples.length < 16000 * 2) {
    toast('录音时长不足，请重新朗读完整范例文本');
    return;
  }
  try {
    $('btnSaveVoiceprint').disabled = true;
    $('btnSaveVoiceprint').textContent = '正在计算…';
    await invoke('save_voiceprint_from_audio', { samples: Array.from(vpFinalSamples) });
    toast('专属声纹已保存');
    cfg.voiceprint_enabled = true;
    await save(true);
    await refreshVoiceprintInfo();
    closeVoiceprintModal();
  } catch (e) {
    toast('保存声纹失败：' + e);
  } finally {
    $('btnSaveVoiceprint').disabled = false;
    $('btnSaveVoiceprint').textContent = '保存声纹';
  }
}

async function handleDeleteVoiceprint() {
  try {
    await invoke('delete_voiceprint');
    cfg.voiceprint_enabled = false;
    await save(true);
    await refreshVoiceprintInfo();
    toast('专属声纹已清除');
  } catch (e) {
    toast('清除声纹失败：' + e);
  }
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
  $('voiceprintEnabled').checked = Boolean(cfg.voiceprint_enabled && voiceprintInfo.has_voiceprint);
  $('voiceprintThreshold').value = cfg.voiceprint_threshold ?? 0.30;
  $('voiceprintThresholdVal').textContent = (cfg.voiceprint_threshold ?? 0.30).toFixed(2);
  renderVoiceprintState();
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
    if (event.key === 'Escape') {
      if (capturingHotkey) {
        endHotkeyCapture();
        return;
      }
      if (isApiKeyModalOpen) {
        closeApiKeyModal();
        return;
      }
      if (isVoiceprintModalOpen) {
        closeVoiceprintModal();
        return;
      }
    }
    if (!capturingHotkey) return;
    event.preventDefault();
    event.stopPropagation();
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

  // 专属声纹防干扰事件绑定
  $('voiceprintEnabled').addEventListener('change', async event => {
    if (event.target.checked && !voiceprintInfo.has_voiceprint) {
      event.target.checked = false;
      toast('请先录制专属声纹');
      openVoiceprintModal();
      return;
    }
    cfg.voiceprint_enabled = event.target.checked;
    await save();
  });

  $('voiceprintThreshold').addEventListener('input', event => {
    cfg.voiceprint_threshold = parseFloat(event.target.value);
    $('voiceprintThresholdVal').textContent = cfg.voiceprint_threshold.toFixed(2);
    save();
  });

  $('btnOpenVoiceprintModal').addEventListener('click', openVoiceprintModal);
  if ($('btnDeleteVoiceprint')) {
    $('btnDeleteVoiceprint').addEventListener('click', handleDeleteVoiceprint);
  }
  $('btnCloseVoiceprintModal').addEventListener('click', closeVoiceprintModal);
  if ($('btnDoneVoiceprintModal')) {
    $('btnDoneVoiceprintModal').addEventListener('click', closeVoiceprintModal);
  }
  $('voiceprintModal').addEventListener('click', event => {
    if (event.target === $('voiceprintModal')) {
      closeVoiceprintModal();
    }
  });

  $('btnRecordVoiceprint').addEventListener('click', startVoiceprintRecording);
  $('btnRerecordVoiceprint').addEventListener('click', startVoiceprintRecording);
  $('btnSaveVoiceprint').addEventListener('click', handleSaveVoiceprint);

  // API 密钥二级弹窗事件
  $('btnOpenApiKeyModal').addEventListener('click', openApiKeyModal);
  $('btnCloseApiKeyModal').addEventListener('click', closeApiKeyModal);
  $('btnToggleAddKey').addEventListener('click', () => {
    setAddKeySectionOpen($('addKeySection').hidden);
  });
  $('btnCancelAddKey').addEventListener('click', () => {
    setAddKeySectionOpen(false);
  });
  $('apiKeyModal').addEventListener('click', event => {
    if (event.target === $('apiKeyModal')) {
      closeApiKeyModal();
    }
  });

  $('btnAddApiKey').addEventListener('click', handleAddApiKey);
  $('newKeyName').addEventListener('keydown', event => {
    if (event.key === 'Enter') {
      event.preventDefault();
      $('newKeyValue').focus();
    }
  });
  $('newKeyValue').addEventListener('keydown', event => {
    if (event.key === 'Enter') {
      event.preventDefault();
      handleAddApiKey();
    }
  });

  $('btnOpenLogs').addEventListener('click', () => invoke('open_log_dir'));

  $('enginePill').addEventListener('click', () => {
    if (apiKeyState && !apiKeyState.configured) {
      openApiKeyModal();
    } else {
      const detail = $('enginePill')?.title || '';
      if (/API Key|401|403|鉴权|凭据|密钥/i.test(detail)) {
        openApiKeyModal();
      } else if (/麦克风|mic|audio/i.test(detail)) {
        $('micDevice')?.focus();
      }
    }
  });
}

/* ---------- 启动 ---------- */
(async function init() {
  [cfg, apiKeyState, usageStats] = await Promise.all([
    invoke('get_config'),
    invoke('doubao_api_key_status'),
    invoke('get_usage_stats'),
  ]);
  await refreshVoiceprintInfo();
  render();
  renderUsageStats(usageStats);
  bind();
  loadMics();
  installSettingsResize();
  renderEngine(await invoke('engine_status'));
  if (!apiKeyState.configured) {
    openApiKeyModal();
  }
  await listen('engine:status', event => renderEngine(event.payload));
  await listen('usage:updated', event => renderUsageStats(event.payload));
  void refreshEngineStatus();
})();
