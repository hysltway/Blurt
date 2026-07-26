/* Blurt 设置页逻辑 */
'use strict';

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

let cfg = null;
let saveTimer = null;

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

/* ---------- 快捷键显示与捕获 ---------- */
const KEY_PRETTY = {
  ctrl: 'Ctrl', alt: 'Alt', shift: 'Shift', super: 'Win',
  Space: 'Space', Backquote: '`', Minus: '-', Equal: '=',
  BracketLeft: '[', BracketRight: ']', Backslash: '\\',
  Semicolon: ';', Quote: "'", Comma: ',', Period: '.', Slash: '/',
  ArrowUp: '↑', ArrowDown: '↓', ArrowLeft: '←', ArrowRight: '→',
  Home: 'Home', End: 'End', PageUp: 'PgUp', PageDown: 'PgDn',
  Insert: 'Ins', Delete: 'Del',
};

function prettyHotkey(hk) {
  if (!hk) return '—';
  return hk.split('+').map(p => {
    if (KEY_PRETTY[p]) return KEY_PRETTY[p];
    if (/^Key([A-Z])$/.test(p)) return p.slice(3);
    if (/^Digit(\d)$/.test(p)) return p.slice(5);
    return p;
  }).join(' + ');
}

/* 捕获在 Rust 原生层进行（WH_KEYBOARD_LL 低级键盘钩子），网页事件作为兜底。
 * 原生层能覆盖输入法切换(Ctrl+Space)、窗口菜单(Alt+Space)等系统热键；
 * 网页层则保证普通按键不会因为原生线程启动或事件回传竞态而丢失。 */
const MAX_HOTKEY_KEYS = 2;
const MODIFIER_CODES = new Set(['ControlLeft', 'ControlRight', 'AltLeft', 'AltRight',
  'ShiftLeft', 'ShiftRight', 'MetaLeft', 'MetaRight']);

function isSupportedPrimary(code) {
  return /^(Key[A-Z]|Digit[0-9]|F(?:[1-9]|1[0-9]|2[0-4])|Space|Arrow(?:Left|Up|Right|Down)|Home|End|Page(?:Up|Down)|Insert|Delete|Backquote|Minus|Equal|Bracket(?:Left|Right)|Backslash|Semicolon|Quote|Comma|Period|Slash)$/.test(code);
}

function hotkeyFromEvent(e) {
  if (!e.code || MODIFIER_CODES.has(e.code)) return null;
  if (!isSupportedPrimary(e.code)) return { invalid: true };
  const parts = [];
  if (e.ctrlKey) parts.push('ctrl');
  if (e.altKey) parts.push('alt');
  if (e.shiftKey) parts.push('shift');
  if (e.metaKey) parts.push('super');
  parts.push(e.code);
  if (parts.length > MAX_HOTKEY_KEYS) return { invalid: true };
  // Keep the existing F-key-only behavior, but reject bare character keys.
  if (parts.length === 1 && !/^F(?:[1-9]|1[0-9]|2[0-4])$/.test(e.code)) {
    return { invalid: true };
  }
  return { hotkey: parts.join('+') };
}

let capturing = false;
let captureEventsReady = false;

function stopCapture(restore) {
  if (!capturing) return;
  capturing = false;
  if (restore) void invoke('capture_hotkey_end');
  const box = $('hotkeyBox');
  box.classList.remove('capturing');
  box.textContent = prettyHotkey(cfg?.hotkey);
}

async function commitCapturedHotkey(hotkey) {
  if (!capturing) return;
  capturing = false;
  const box = $('hotkeyBox');
  box.classList.remove('capturing');
  box.textContent = prettyHotkey(hotkey);

  try {
    // Always end the capture session first. This also handles capturing the
    // currently configured shortcut, which otherwise stays unregistered.
    await invoke('capture_hotkey_end');
    cfg.hotkey = hotkey;
    await invoke('set_config', { config: cfg });
    toast('快捷键已更新');
  } catch (err) {
    toast('注册失败：' + err);
    cfg = await invoke('get_config');
    render();
  }
}

async function setupHotkeyCapture() {
  const box = $('hotkeyBox');

  box.addEventListener('click', async () => {
    if (capturing || !captureEventsReady) return;
    capturing = true;
    box.classList.add('capturing');
    box.textContent = '请按下新的组合键…（Esc 取消）';
    box.focus();
    try {
      await invoke('capture_hotkey_begin');
    } catch (err) {
      stopCapture(false);
      toast('无法开始监听：' + err);
    }
  });

  // Do not cancel on blur: Alt+Space can open the native window menu and
  // briefly move focus even though the global capture session is still valid.
  window.addEventListener('pointerdown', e => {
    if (capturing && !box.contains(e.target)) stopCapture(true);
  });
  window.addEventListener('keydown', e => {
    if (!capturing) return;
    e.preventDefault();
    e.stopPropagation();
    if (e.code === 'Escape') {
      stopCapture(true);
      return;
    }
    const result = hotkeyFromEvent(e);
    if (!result) return;
    if (result.invalid) {
      box.textContent = '仅支持最多两个按键（字符键需配合 Ctrl / Alt / Shift / Win）';
      return;
    }
    void commitCapturedHotkey(result.hotkey);
  }, true);
  window.addEventListener('beforeunload', () => { if (capturing) invoke('capture_hotkey_end'); });

  await Promise.all([
    listen('hotkey:captured', e => {
      const hotkey = e.payload?.hotkey;
      if (typeof hotkey === 'string') void commitCapturedHotkey(hotkey);
    }),
    listen('hotkey:capture_invalid', () => {
      if (capturing) box.textContent = '仅支持最多两个按键（字符键需配合 Ctrl / Alt / Shift / Win）';
    }),
    listen('hotkey:capture_cancel', () => stopCapture(true)),
    listen('hotkey:capture_error', e => {
      if (!capturing) return;
      const message = e.payload?.message || '键盘监听器异常退出';
      stopCapture(true);
      toast(message);
    }),
  ]);
  captureEventsReady = true;
}

/* ---------- 线程测速 ---------- */
let bestThreads = 0;

function setupBench() {
  $('btnBench').addEventListener('click', async () => {
    try {
      await invoke('bench_threads');
    } catch (e) {
      toast(String(e));
      return;
    }
    $('btnBench').disabled = true;
    $('btnApplyBest').style.display = 'none';
    $('benchStatus').textContent = '准备中…';
    const t = $('benchTable');
    t.style.display = '';
    t.innerHTML = '<tr><th>线程数</th><th>识别耗时</th><th>RTF（越小越快）</th></tr>';
  });

  listen('bench:progress', e => {
    const p = e.payload;
    $('benchStatus').textContent = `正在测试 ${p.threads} 线程（${p.idx}/${p.total}，含模型加载约数秒）…`;
  });

  listen('bench:result', e => {
    const r = e.payload;
    const tr = document.createElement('tr');
    tr.dataset.threads = r.threads;
    tr.innerHTML = r.error
      ? `<td>${r.threads}</td><td colspan="2">失败</td>`
      : `<td>${r.threads}</td><td>${(r.ms / 1000).toFixed(2)} 秒</td><td>${r.rtf.toFixed(3)}</td>`;
    $('benchTable').appendChild(tr);
  });

  listen('bench:done', e => {
    $('btnBench').disabled = false;
    const best = e.payload.best;
    if (best > 0) {
      bestThreads = best;
      $('benchStatus').textContent = `测试完成：${best} 线程最快`;
      for (const tr of $('benchTable').rows) {
        tr.classList.toggle('best', tr.dataset.threads == String(best));
      }
      const b = $('btnApplyBest');
      b.textContent = `应用最快（${best} 线程）`;
      b.style.display = '';
    } else {
      $('benchStatus').textContent = '测速失败，详见日志';
    }
  });

  $('btnApplyBest').addEventListener('click', () => {
    if (!bestThreads) return;
    cfg.num_threads = bestThreads;
    $('numThreads').value = String(bestThreads);
    save(true);
  });
}

/* ---------- 引擎状态 ---------- */
function renderEngine(st) {
  const dot = $('modelDot'), txt = $('modelText'), path = $('modelPath');
  const copyBtn = $('btnCopyCmd');
  copyBtn.style.display = 'none';
  dot.className = 'dot';
  switch (st.state) {
    case 'ready':
      dot.classList.add('ok');
      txt.textContent = '模型已就绪 · Qwen3-ASR-0.6B int8';
      break;
    case 'loading':
      dot.classList.add('loading');
      txt.textContent = '模型加载中…（首次约需数秒）';
      break;
    case 'missing':
      dot.classList.add('missing');
      txt.textContent = '未找到模型文件 — 请下载后放入模型目录';
      copyBtn.style.display = '';
      break;
    default:
      dot.classList.add('err');
      txt.textContent = '模型加载失败：' + (st.detail || '未知错误');
      copyBtn.style.display = '';
  }
  path.textContent = st.model_dir || '';
  const s = [];
  if (st.rtf > 0) s.push(`RTF ${st.rtf.toFixed(2)}`);
  if (st.last_ms != null) s.push(`最近识别耗时 ${(st.last_ms / 1000).toFixed(2)} 秒`);
  $('stats').textContent = s.join(' · ');
}

/* ---------- 渲染 ---------- */
function render() {
  $('hotkeyBox').textContent = prettyHotkey(cfg.hotkey);
  $('injectMode').value = cfg.inject_mode;
  $('autostart').checked = cfg.autostart;
  $('numThreads').value = String(cfg.num_threads);
  $('hotwords').value = cfg.hotwords || '';
  $('maxRecord').value = cfg.max_record_secs;
  $('maxRecordVal').textContent = cfg.max_record_secs + ' 秒';
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
  $('injectMode').addEventListener('change', e => { cfg.inject_mode = e.target.value; save(); });
  $('autostart').addEventListener('change', e => { cfg.autostart = e.target.checked; save(); });
  $('numThreads').addEventListener('change', e => { cfg.num_threads = parseInt(e.target.value); save(); });
  $('hotwords').addEventListener('input', e => { cfg.hotwords = e.target.value; save(); });
  $('micDevice').addEventListener('change', e => { cfg.mic_device = e.target.value || null; save(); });
  $('maxRecord').addEventListener('input', e => {
    cfg.max_record_secs = parseInt(e.target.value);
    $('maxRecordVal').textContent = cfg.max_record_secs + ' 秒';
    save();
  });

  $('btnReload').addEventListener('click', async () => {
    await invoke('reload_engine');
    toast('正在重新加载引擎…');
  });
  $('btnOpenModelDir').addEventListener('click', () => invoke('open_model_dir'));
  $('btnOpenLogs').addEventListener('click', () => invoke('open_log_dir'));
  $('btnCopyCmd').addEventListener('click', async () => {
    const cmd = [
      '# 在 PowerShell 中执行，下载 Qwen3-ASR-0.6B 模型（约 937 MB）',
      '$d="$env:APPDATA\\Blurt\\models"; mkdir -Force $d | Out-Null',
      'curl.exe -L -o "$d\\m.tar.bz2" https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-qwen3-asr-0.6B-int8-2026-03-25.tar.bz2',
      'tar -xjf "$d\\m.tar.bz2" -C $d; del "$d\\m.tar.bz2"',
    ].join('\n');
    await invoke('copy_text', { text: cmd });
    toast('下载命令已复制到剪贴板');
  });
}

/* ---------- 启动 ---------- */
(async function init() {
  cfg = await invoke('get_config');
  render();
  bind();
  await setupHotkeyCapture();
  setupBench();
  loadMics();
  renderEngine(await invoke('engine_status'));
  listen('engine:status', e => renderEngine(e.payload));
})();
