/* Blurt HUD — 无文字悬浮指示器（浅色）
 * 声波条 = 最近 9 个 20ms 窗口的真实麦克风能量（纯音频驱动，无装饰动画）：
 *   listen   聆听中（滚动波形 —— “它听到我了”）
 *   process  识别中（行波圆点 + 预计进度线 —— “还要多久”）
 *   success  完成（聚合成点，绿色脉冲）
 *   error    出错（红色抖动）
 *   nothing  没听到有效语音（灰色塌陷）
 *   loading  引擎尚未就绪（琥珀色呼吸点）
 *   cancel   已取消（快速消散）
 */
'use strict';

const W = 360, H = 140;          // 与 Rust 侧窗口尺寸一致（逻辑像素）
const CX = W / 2, CY = 76;       // 胶囊中心
const CAP_W = 210, CAP_H = 50;   // 胶囊尺寸
const N = 9;                     // 波形条数量（= 最近 9 个能量窗口）

const COL = {
  capsule: 'rgba(255,255,255,0.97)',
  border:  'rgba(17,24,39,0.10)',
  bar:     [52, 58, 70],         // 主条色（深石板）
  think:   [99, 102, 241],       // 思考靛蓝
  ok:      [22, 163, 74],        // 成功绿
  err:     [220, 38, 38],        // 错误红
  quiet:   [163, 170, 182],      // 静默灰
  amber:   [217, 119, 6],        // 加载琥珀
  track:   'rgba(17,24,39,0.08)',
};

const canvas = document.getElementById('c');
const ctx = canvas.getContext('2d');
let dpr = Math.max(1, window.devicePixelRatio || 1);
function fitCanvas() {
  dpr = Math.max(1, window.devicePixelRatio || 1);
  canvas.width = W * dpr; canvas.height = H * dpr;
  canvas.style.width = W + 'px'; canvas.style.height = H + 'px';
}
fitCanvas();

/* ---------- 状态机 ---------- */
let state = 'hidden';
let tEnter = 0;
let running = false;

const levels = new Array(N).fill(0); // 滚动能量历史（真实音频）
let rawLevel = 0;
let sinceVoice = 0;
let etaMs = 1500;
let progSnap = null;
let appearT = 0;

const bars = Array.from({ length: N }, () => ({
  h: 6, y: 0, x: 0, a: 0, col: COL.bar.slice(),
}));

const now = () => performance.now() / 1000;
const clamp = (v, a, b) => Math.min(b, Math.max(a, v));
const lerp = (a, b, t) => a + (b - a) * t;
const mixCol = (c1, c2, t) => [lerp(c1[0], c2[0], t), lerp(c1[1], c2[1], t), lerp(c1[2], c2[2], t)];
const rgba = (c, a) => `rgba(${c[0] | 0},${c[1] | 0},${c[2] | 0},${a})`;
const easeOutCubic = t => 1 - Math.pow(1 - t, 3);
const easeOutBack = t => { const c = 1.70158; return 1 + (c + 1) * Math.pow(t - 1, 3) + c * Math.pow(t - 1, 2); };

function setState(s, payload) {
  const fresh = (state === 'hidden');
  if (s === 'process') {
    etaMs = Math.max(400, (payload && payload.eta_ms) || 1500);
    progSnap = null;
  }
  if (s === 'success') progSnap = { from: currentProgress(), t: now() };
  state = s;
  tEnter = now();
  if (fresh) {
    appearT = tEnter;
    rawLevel = 0; sinceVoice = 0;
    levels.fill(0);
  }
  if (!running) { running = true; requestAnimationFrame(frame); }
}

/* ---------- 进度模型：eta 内缓动至 92%，之后缓慢爬行，完成时吸附 100% ---------- */
let procStart = 0;
function currentProgress() {
  if (state !== 'process' && !progSnap) return 0;
  const t = now() - procStart;
  const raw = t / (etaMs / 1000);
  let p;
  if (raw <= 1) p = 0.92 * (1 - Math.pow(1 - raw, 2.2));
  else p = 0.92 + 0.05 * (1 - Math.exp(-(raw - 1) * 0.8));
  return clamp(p, 0, 0.97);
}

/* ---------- 渲染 ---------- */
function roundRect(x, y, w, h, r) {
  ctx.beginPath();
  ctx.moveTo(x + r, y);
  ctx.arcTo(x + w, y, x + w, y + h, r);
  ctx.arcTo(x + w, y + h, x, y + h, r);
  ctx.arcTo(x, y + h, x, y, r);
  ctx.arcTo(x, y, x + w, y, r);
  ctx.closePath();
}

function frame() {
  if (!running) return;
  const t = now();
  const dt = 1 / 60;
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, W, H);

  if (state === 'hidden') { running = false; return; }

  /* --- 全局出现/消失包络 --- */
  const ta = t - appearT;
  const appear = easeOutBack(clamp(ta / 0.26, 0, 1));
  let alpha = clamp(ta / 0.18, 0, 1);
  let capScale = lerp(0.86, 1, appear);
  let capDY = lerp(14, 0, appear);
  let shakeX = 0;
  const te = t - tEnter;

  if (state === 'success') {
    const k = clamp((te - 0.20) / 0.30, 0, 1);
    alpha *= 1 - k; capDY += 10 * k; capScale *= 1 - 0.05 * k;
  } else if (state === 'error') {
    shakeX = Math.sin(te * 42) * 7 * Math.exp(-te * 4.5);
    const k = clamp((te - 0.55) / 0.35, 0, 1);
    alpha *= 1 - k;
  } else if (state === 'nothing') {
    const k = clamp((te - 0.45) / 0.35, 0, 1);
    alpha *= 1 - k; capDY += 6 * k;
  } else if (state === 'cancel') {
    const k = clamp(te / 0.16, 0, 1);
    alpha *= 1 - k; capScale *= 1 - 0.04 * k;
  } else if (state === 'loading') {
    const k = clamp((te - 1.05) / 0.30, 0, 1);
    alpha *= 1 - k;
  }

  /* --- 静默检测（真实音频：无声就是平的） --- */
  if (rawLevel > 0.06) sinceVoice = 0; else sinceVoice += dt;
  const quiet = state === 'listen' ? clamp((sinceVoice - 1.6) / 0.8, 0, 1) : 0;

  /* --- 计算每条目标形态 --- */
  const gap = 11, bw = 5;
  const spread = (N - 1) * gap;
  for (let i = 0; i < N; i++) {
    const b = bars[i];
    let tx = CX - spread / 2 + i * gap;
    let th = 6, ty = 0, tcol = COL.bar, tA = 1;

    if (state === 'listen') {
      // 真实波形：第 i 条 = 第 i 个历史能量窗口（右侧最新，向左滚动）
      th = 6 + 36 * levels[i];
      tcol = mixCol(COL.bar, COL.quiet, quiet);
      tA = lerp(1, 0.5 + 0.16 * Math.sin(t * 2.6), quiet); // 久无声 → 呼吸提示
    } else if (state === 'process') {
      th = 7;
      ty = Math.sin(t * 4.6 - i * 0.5) * 5.5;
      const cph = 0.5 + 0.5 * Math.sin(t * 2.8 - i * 0.5);
      tcol = mixCol(COL.bar, COL.think, cph);
    } else if (state === 'success') {
      const k = easeOutCubic(clamp(te / 0.22, 0, 1));
      tx = lerp(tx, CX, k); th = 7;
      tcol = COL.ok; tA = i === Math.floor(N / 2) ? 1 : 1 - k;
    } else if (state === 'error') {
      th = 7; ty = 2; tcol = COL.err;
    } else if (state === 'nothing') {
      th = 6; ty = 4; tcol = COL.quiet; tA = 0.8;
    } else if (state === 'loading') {
      th = 7; tcol = COL.amber;
      tA = i === Math.floor(N / 2) ? 0.55 + 0.45 * Math.sin(t * 5.2) : 0;
    } else if (state === 'cancel') {
      th = 6;
    }

    const spd = state === 'listen' ? 0.55 : 0.42; // 聆听时更跟手
    b.h += (th - b.h) * spd;
    b.y += (ty - b.y) * spd;
    b.x = b.x === 0 ? tx : b.x + (tx - b.x) * spd;
    b.a += (tA - b.a) * spd;
    b.col = mixCol(b.col, tcol, 0.3);
  }

  /* --- 绘制胶囊（先落影，再彩色光晕） --- */
  ctx.save();
  ctx.translate(CX + shakeX, CY + capDY);
  ctx.scale(capScale, capScale);
  ctx.translate(-CX, -CY);
  ctx.globalAlpha = alpha;

  // 底部落影（浅色主题靠阴影脱离背景）
  ctx.shadowColor = 'rgba(15,23,42,0.22)';
  ctx.shadowBlur = 28;
  ctx.shadowOffsetY = 10;
  ctx.fillStyle = COL.capsule;
  roundRect(CX - CAP_W / 2, CY - CAP_H / 2, CAP_W, CAP_H, CAP_H / 2);
  ctx.fill();
  ctx.shadowOffsetY = 0;

  // 状态色光晕
  let glow = 'rgba(99,102,241,0.25)';
  if (state === 'success') glow = 'rgba(22,163,74,0.30)';
  else if (state === 'error') glow = 'rgba(220,38,38,0.30)';
  else if (state === 'loading') glow = 'rgba(217,119,6,0.28)';
  else if (state === 'nothing') glow = 'rgba(120,128,140,0.20)';
  ctx.shadowColor = glow;
  ctx.shadowBlur = 22;
  roundRect(CX - CAP_W / 2, CY - CAP_H / 2, CAP_W, CAP_H, CAP_H / 2);
  ctx.fill();
  ctx.shadowBlur = 0;

  ctx.strokeStyle = COL.border;
  ctx.lineWidth = 1;
  roundRect(CX - CAP_W / 2 + 0.5, CY - CAP_H / 2 + 0.5, CAP_W - 1, CAP_H - 1, (CAP_H - 1) / 2);
  ctx.stroke();

  /* --- 绘制条/点 --- */
  const stagger = 0.03;
  for (let i = 0; i < N; i++) {
    const b = bars[i];
    const pop = easeOutBack(clamp((ta - i * stagger) / 0.24, 0, 1));
    const h = Math.max(bw, b.h) * pop;
    ctx.globalAlpha = alpha * b.a * clamp(pop, 0, 1);
    ctx.fillStyle = rgba(b.col, 1);
    roundRect(b.x - bw / 2, CY + b.y - h / 2, bw, h, bw / 2);
    ctx.fill();
  }

  /* --- 成功脉冲 --- */
  if (state === 'success' && te > 0.20) {
    const k = clamp((te - 0.20) / 0.28, 0, 1);
    ctx.globalAlpha = alpha * (1 - k);
    ctx.fillStyle = rgba(COL.ok, 1);
    ctx.beginPath();
    ctx.arc(CX, CY, 4 + 14 * easeOutCubic(k), 0, Math.PI * 2);
    ctx.fill();
  }

  /* --- 识别进度线（“还要多久”） --- */
  if (state === 'process' || (state === 'success' && progSnap)) {
    let p;
    if (state === 'process') p = currentProgress();
    else {
      const k = clamp((now() - progSnap.t) / 0.15, 0, 1);
      p = lerp(progSnap.from, 1, k);
    }
    const tw = CAP_W - 60;
    const tx0 = CX - tw / 2, tyy = CY + CAP_H / 2 - 9;
    ctx.globalAlpha = alpha * 0.95;
    ctx.fillStyle = COL.track;
    roundRect(tx0, tyy, tw, 3, 1.5);
    ctx.fill();
    const grad = ctx.createLinearGradient(tx0, 0, tx0 + tw, 0);
    grad.addColorStop(0, '#6366f1');
    grad.addColorStop(1, '#16a34a');
    ctx.fillStyle = grad;
    roundRect(tx0, tyy, Math.max(3, tw * p), 3, 1.5);
    ctx.fill();
  }

  ctx.restore();
  requestAnimationFrame(frame);
}

/* ---------- 事件接线 ---------- */
const { listen } = window.__TAURI__.event;
const { invoke } = window.__TAURI__.core;
const closeBtn = document.getElementById('closeBtn');

listen('hud:state', e => {
  const s = e.payload.state;
  if (s === 'process') procStart = now();
  if (s === 'hidden') { state = 'hidden'; closeBtn.classList.remove('show'); return; }
  setState(s, e.payload);
});

// 每 20ms 一帧真实能量：推入滚动历史（右新左旧）
listen('hud:level', e => {
  const v = clamp(e.payload.v, 0, 1);
  rawLevel = v;
  levels.push(v);
  levels.shift();
});

/* 光标移入 HUD（Rust 侧解除点击穿透后）→ 浮现取消按钮 */
listen('hud:hover', e => {
  const active = state === 'listen' || state === 'process';
  closeBtn.classList.toggle('show', !!e.payload.v && active);
});

closeBtn.addEventListener('click', () => {
  closeBtn.classList.remove('show');
  invoke('cancel_session');
});
