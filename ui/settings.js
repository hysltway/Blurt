/* Blurt 设置页逻辑（快捷键在代码里写死为 Ctrl+Alt，页面只做展示） */
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

/* ---------- 滑条填充上色 ---------- */
function paintRange(el) {
  const p = (el.value - el.min) / (el.max - el.min) * 100;
  el.style.setProperty('--p', p + '%');
}

function fmtAutoStop(v) {
  return v > 0 ? v + ' 秒' : '关闭';
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

/* ---------- 引擎状态（页头胶囊 + 引擎卡横幅） ---------- */
const PILL_TEXT = { ready: '就绪', loading: '加载中…', missing: '缺少模型', failed: '加载失败' };

function renderEngine(st) {
  const dotCls = { ready: 'ok', loading: 'loading', missing: 'missing' }[st.state] || 'err';
  $('pillDot').className = 'dot ' + dotCls;
  $('pillText').textContent = PILL_TEXT[st.state] || PILL_TEXT.failed;
  $('modelDot').className = 'dot ' + dotCls;
  $('modelBanner').className = 'model-banner ' + dotCls;

  const copyBtn = $('btnCopyCmd');
  copyBtn.style.display = 'none';
  switch (st.state) {
    case 'ready':
      $('modelText').textContent = '模型已就绪 · Qwen3-ASR-0.6B int8';
      break;
    case 'loading':
      $('modelText').textContent = '模型加载中…（首次约需数秒）';
      break;
    case 'missing':
      $('modelText').textContent = '未找到模型文件 — 请下载后放入模型目录';
      copyBtn.style.display = '';
      break;
    default:
      $('modelText').textContent = '模型加载失败：' + (st.detail || '未知错误');
      copyBtn.style.display = '';
  }
  $('modelPath').textContent = st.model_dir || '';

  const s = [];
  if (st.rtf > 0) s.push(`RTF ${st.rtf.toFixed(2)}`);
  if (st.last_ms != null) s.push(`最近 ${(st.last_ms / 1000).toFixed(2)} 秒`);
  $('stats').textContent = s.join(' · ');
}

/* ---------- 渲染 ---------- */
function render() {
  const radio = document.querySelector(`#injectSeg input[value="${cfg.inject_mode}"]`);
  if (radio) radio.checked = true;
  $('autostart').checked = cfg.autostart;
  $('numThreads').value = String(cfg.num_threads);
  $('hotwords').value = cfg.hotwords || '';
  $('maxRecord').value = cfg.max_record_secs;
  $('maxRecordVal').textContent = cfg.max_record_secs + ' 秒';
  $('autoStop').value = cfg.auto_stop_secs;
  $('autoStopVal').textContent = fmtAutoStop(cfg.auto_stop_secs);
  paintRange($('maxRecord'));
  paintRange($('autoStop'));
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
  $('numThreads').addEventListener('change', e => { cfg.num_threads = parseInt(e.target.value); save(); });
  $('hotwords').addEventListener('input', e => { cfg.hotwords = e.target.value; save(); });
  $('micDevice').addEventListener('change', e => { cfg.mic_device = e.target.value || null; save(); });
  $('maxRecord').addEventListener('input', e => {
    cfg.max_record_secs = parseInt(e.target.value);
    $('maxRecordVal').textContent = cfg.max_record_secs + ' 秒';
    paintRange(e.target);
    save();
  });
  $('autoStop').addEventListener('input', e => {
    cfg.auto_stop_secs = parseFloat(e.target.value);
    $('autoStopVal').textContent = fmtAutoStop(cfg.auto_stop_secs);
    paintRange(e.target);
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
  setupBench();
  loadMics();
  renderEngine(await invoke('engine_status'));
  listen('engine:status', e => renderEngine(e.payload));
})();
