/* Blurt HUD — 无文字悬浮指示器（丝绸粒子波，对齐 public/01lrjxheswmgiybzm4veqy3137.gif
 * 即 public/语音识别动画.aep 的渲染效果：Trapcode Form 粒子网格丝绸波）
 * 无胶囊、无背景，直接展示动画。
 * 波形 = 最近 ~1.1s 的真实麦克风能量横向展开（右新左旧，纯音频驱动）：
 *   listen   聆听中（能量撑开丝波振幅 —— “它听到我了”）
 *   process  识别中（靛紫行波 + 预计进度线 —— “还要多久”）
 *   success  完成（丝波收拢成绿色亮线，脉冲）
 *   error    出错（红色高频躁动 + 抖动）
 *   nothing  没听到有效语音（灰色塌陷）
 *   loading  引擎尚未就绪（琥珀色呼吸）
 *   cancel   已取消（绸缕散开消融）
 * 实现：AE 里是 300×800 粒子网格平铺成布面；此处以 ~60 条水平“绸缕”曲线
 * 采样同一片 2D 分形噪声（大涌浪 + 错相褶皱 + 细织纹），加色混合叠出褶皱亮部；
 * 两端 edgeFade 收成细线。
 */
'use strict';

const W = 360, H = 140;          // 与 Rust 侧窗口尺寸一致（逻辑像素）
const CX = W / 2;
const WY = 84;                   // 丝波基线（上方留出峰值与取消按钮空间）
const X0 = 10, BW = W - 20;      // 波形横向范围
const NH = 56;                   // 能量历史长度（= 最近 56 个 20ms 窗口 ≈ 1.1s）
const NS = 60;                   // 绸缕条数（网格的“行”）
const SEG = 88;                  // 每条绸缕的分段数
const SPREAD = 22;               // 布面纵向厚度（绸缕基线散布）

/* 每状态一对色（深部/亮部），绸缕在两色间渐变，加色叠出高光 */
const COL = {
  bar:   { deep: [56, 58, 228], light: [150, 156, 255] },  // GIF 主体深蓝 → 紫蓝亮部
  think: { deep: [98, 88, 242], light: [182, 172, 255] },  // 思考靛紫
  ok:    { deep: [34, 180, 115], light: [150, 240, 190] }, // 成功绿
  err:   { deep: [226, 62, 72], light: [255, 152, 152] },  // 错误红
  quiet: { deep: [116, 122, 138], light: [190, 195, 205] },// 静默灰
  amber: { deep: [222, 150, 40], light: [255, 216, 140] }, // 加载琥珀
  track: 'rgba(255,255,255,0.14)',
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

const levels = new Array(NH).fill(0); // 滚动能量历史（真实音频）
let rawLevel = 0;
let sinceVoice = 0;
let etaMs = 1500;
let progSnap = null;
let appearT = 0;

const now = () => performance.now() / 1000;
const clamp = (v, a, b) => Math.min(b, Math.max(a, v));
const lerp = (a, b, t) => a + (b - a) * t;
const mixCol = (c1, c2, t) => [lerp(c1[0], c2[0], t), lerp(c1[1], c2[1], t), lerp(c1[2], c2[2], t)];
const rgba = (c, a) => `rgba(${c[0] | 0},${c[1] | 0},${c[2] | 0},${a})`;
const easeOutCubic = t => 1 - Math.pow(1 - t, 3);
const smoothstep = (a, b, x) => { const t = clamp((x - a) / (b - a), 0, 1); return t * t * (3 - 2 * t); };

/* ---------- 2D 分形噪声（AE Fractal Field 的布面位移 + 横向流动） ---------- */
function hash2(x, y) { const s = Math.sin(x * 127.1 + y * 311.7) * 43758.5453; return s - Math.floor(s); }
function vnoise2(x, y) {
  const xi = Math.floor(x), yi = Math.floor(y);
  const xf = x - xi, yf = y - yi;
  const tx = xf * xf * (3 - 2 * xf), ty = yf * yf * (3 - 2 * yf);
  const a = hash2(xi, yi), b = hash2(xi + 1, yi), c = hash2(xi, yi + 1), d = hash2(xi + 1, yi + 1);
  return a + (b - a) * tx + (c - a) * ty + (a - b - c + d) * tx * ty; // 0..1
}
function silk(u, v, tt) { // -1..1，低频主导的两个倍频程（GIF 里是大而平滑的起伏）
  const a = vnoise2(u * 2.3 + tt * 0.42, v * 2.8 + tt * 0.10);
  const b = vnoise2(u * 5.1 - tt * 0.65 + 19.7, v * 5.5 + 7.3) * 0.42;
  return ((a + b) / 1.42) * 2 - 1;
}
function swell(u, tt) { // 全体绸缕共享的大涌浪（GIF 中部隆起的大峰）
  return vnoise2(u * 1.0 + tt * 0.30, 3.7) * 2 - 1;
}
function weave(u, v, tt) { // 高 v 频细织纹：让每缕在布面内可见地穿插
  return vnoise2(u * 3.9 + tt * 0.5, v * 9.1 + 3.3) * 2 - 1;
}

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

/* 布面全局平滑量（颜色/振幅/厚度等向状态目标缓动） */
const band = {
  deep: COL.bar.deep.slice(),
  light: COL.bar.light.slice(),
  amp: 0,      // 振幅比例
  spread: 1,   // 布面厚度比例
  aMul: 1,     // 整体透明度系数
};
const strandKick = new Float32Array(NS); // cancel 时每条绸缕的散开速度

// 历史能量按列采样（u=0 最旧/左 … u=1 最新/右），线性插值
function levelAt(u) {
  const f = u * (NH - 1);
  const i = Math.floor(f);
  const a = levels[clamp(i, 0, NH - 1)], b = levels[clamp(i + 1, 0, NH - 1)];
  return a + (b - a) * (f - i);
}
const edgeFade = u => smoothstep(0, 0.12, u) * (1 - smoothstep(0.88, 1, u)); // 两端收成细线

function frame() {
  if (!running) return;
  const t = now();
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, W, H);

  if (state === 'hidden') { running = false; return; }

  /* --- 全局出现/消失包络（GIF：淡入/淡出的平线↔波面） --- */
  const ta = t - appearT;
  let alpha = clamp(ta / 0.22, 0, 1);
  let shakeX = 0;
  const te = t - tEnter;

  if (state === 'success') {
    const k = clamp((te - 0.20) / 0.30, 0, 1);
    alpha *= 1 - k;
  } else if (state === 'error') {
    shakeX = Math.sin(te * 42) * 6 * Math.exp(-te * 4.5);
    const k = clamp((te - 0.55) / 0.35, 0, 1);
    alpha *= 1 - k;
  } else if (state === 'nothing') {
    const k = clamp((te - 0.45) / 0.35, 0, 1);
    alpha *= 1 - k;
  } else if (state === 'cancel') {
    const k = clamp(te / 0.16, 0, 1);
    alpha *= 1 - k;
  } else if (state === 'loading') {
    const k = clamp((te - 1.05) / 0.30, 0, 1);
    alpha *= 1 - k;
  }

  /* --- 静默检测（真实音频：无声就是平的） --- */
  const dt = 1 / 60;
  if (rawLevel > 0.06) sinceVoice = 0; else sinceVoice += dt;
  const quiet = state === 'listen' ? clamp((sinceVoice - 1.6) / 0.8, 0, 1) : 0;

  /* --- 计算布面状态目标 --- */
  let tPair = COL.bar, tAmp = 0, tSpread = 1, tAMul = 1;
  if (state === 'listen') {
    tPair = quiet > 0
      ? { deep: mixCol(COL.bar.deep, COL.quiet.deep, quiet), light: mixCol(COL.bar.light, COL.quiet.light, quiet) }
      : COL.bar;
    tAmp = 1; tSpread = 1;
    tAMul = lerp(1, 0.55 + 0.16 * Math.sin(t * 2.6), quiet); // 久无声 → 呼吸提示
  } else if (state === 'process') {
    tPair = COL.think; tAmp = 1; tSpread = 1;
  } else if (state === 'success') {
    const k = easeOutCubic(clamp(te / 0.22, 0, 1));
    tPair = COL.ok; tAmp = 1 - 0.94 * k; tSpread = 1 - 0.9 * k;
    tAMul = 1 + 0.35 * k;                                     // 收拢成亮线时轻微增亮
  } else if (state === 'error') {
    tPair = COL.err; tAmp = 1; tSpread = 1.1;
  } else if (state === 'nothing') {
    tPair = COL.quiet; tAmp = 0.18; tSpread = 0.5; tAMul = 0.8;
  } else if (state === 'loading') {
    tPair = COL.amber; tAmp = 1; tSpread = 0.9;
    tAMul = 0.62 + 0.30 * Math.sin(t * 3.4);
  } else if (state === 'cancel') {
    tAmp = 0.4; tSpread = 1.6; tAMul = 0.9;                   // 绸缕散开
  }
  const spd = state === 'listen' ? 0.55 : 0.42; // 聆听时更跟手
  band.deep = mixCol(band.deep, tPair.deep, 0.3);
  band.light = mixCol(band.light, tPair.light, 0.3);
  band.amp += (tAmp - band.amp) * spd;
  band.spread += (tSpread - band.spread) * spd;
  band.aMul += (tAMul - band.aMul) * spd;

  /* --- 每列振幅（状态调制 × 真实能量 × 端点收束） --- */
  const noiseT = t * (1 + rawLevel * 1.4); // 说话越响，布面流速越快
  const colAmp = new Float32Array(SEG + 1);
  for (let i = 0; i <= SEG; i++) {
    const u = i / SEG;
    const ef = edgeFade(u);
    let amp;
    if (state === 'listen') {
      amp = 4 + 30 * Math.tanh(levelAt(u) * 1.6);              // 真实能量撑开振幅（软限幅）
    } else if (state === 'process') {
      amp = 12 + 6 * Math.sin(u * 5.6 - t * 3.2);              // 行波脉动
    } else if (state === 'error') {
      amp = 8;
    } else if (state === 'loading') {
      amp = 4.5 + 2.5 * Math.sin(t * 2.4);                     // 缓慢呼吸
    } else { // success / nothing / cancel：残余起伏随 amp 衰减
      amp = 8;
    }
    colAmp[i] = amp * band.amp * ef;
  }

  ctx.save();
  ctx.translate(shakeX, 0);
  ctx.globalCompositeOperation = 'lighter';
  ctx.lineJoin = 'round';

  /* --- 绸缕（网格行）：同一片噪声布面错相采样 + 振幅视差 → 绸缕交错成褶皱 --- */
  const errT = state === 'error' ? t * 5 : noiseT; // 错误时高频躁动
  const px = new Float32Array(SEG + 1);
  const py = new Float32Array(SEG + 1);
  for (let s = 0; s < NS; s++) {
    const v = s / (NS - 1);
    const depth = (v - 0.5) * SPREAD * band.spread;
    const ampMul = 0.75 + 0.35 * v;               // 轻微视差（交叉主要靠 u 向错相）
    // 深部 → 亮部：中间行更亮（折痕高光集中在中部）
    const cMix = 0.25 + 0.75 * Math.sin(v * Math.PI);
    const col = mixCol(band.deep, band.light, cMix * 0.8);
    const aStrand = alpha * band.aMul * (0.075 + 0.08 * Math.sin(v * Math.PI));
    if (aStrand <= 0.004) continue;
    const kick = strandKick[s] * (state === 'cancel' ? te : 0) * 160;

    for (let i = 0; i <= SEG; i++) {
      const u = i / SEG;
      const ef = edgeFade(u);
      // u 向随行错相 + 细织纹：褶皱脊线沿行滑移，绸缕全程可见穿插
      const dy = (swell(u, t) * 0.75 + silk(u + v * 0.5, v * 1.2, errT) * ampMul * 0.85) * colAmp[i]
        + weave(u, v, errT) * 5.5 * band.spread * ef * (0.35 + 0.65 * band.amp);
      px[i] = X0 + u * BW;
      py[i] = WY + depth * ef + clamp(dy, -48, 48) + kick * ef;
    }
    // 中点二次曲线平滑，单次描边（避免节点重叠产生竖纹）
    ctx.beginPath();
    ctx.moveTo(px[0], py[0]);
    for (let i = 1; i < SEG; i++) {
      ctx.quadraticCurveTo(px[i], py[i], (px[i] + px[i + 1]) / 2, (py[i] + py[i + 1]) / 2);
    }
    ctx.lineTo(px[SEG], py[SEG]);
    ctx.strokeStyle = rgba(col, 1);
    ctx.globalAlpha = aStrand;
    ctx.lineWidth = 1.2;
    ctx.stroke();
  }

  ctx.globalCompositeOperation = 'source-over';

  /* --- 成功脉冲（收拢亮线上的绿色扩散环） --- */
  if (state === 'success' && te > 0.20) {
    const k = clamp((te - 0.20) / 0.28, 0, 1);
    ctx.globalAlpha = alpha * (1 - k);
    ctx.strokeStyle = rgba(COL.ok.light, 0.9);
    ctx.lineWidth = 1.6;
    ctx.beginPath();
    ctx.arc(CX, WY, 5 + 20 * easeOutCubic(k), 0, Math.PI * 2);
    ctx.stroke();
    ctx.fillStyle = rgba(COL.ok.light, 1);
    ctx.beginPath();
    ctx.arc(CX, WY, 3, 0, Math.PI * 2);
    ctx.fill();
  }

  /* --- 识别进度线（“还要多久”，波面下方悬浮细线） --- */
  if (state === 'process' || (state === 'success' && progSnap)) {
    let p;
    if (state === 'process') p = currentProgress();
    else {
      const k = clamp((now() - progSnap.t) / 0.15, 0, 1);
      p = lerp(progSnap.from, 1, k);
    }
    const tw = 170;
    const tx0 = CX - tw / 2, tyy = 126;
    ctx.globalAlpha = alpha * 0.9;
    ctx.fillStyle = COL.track;
    roundRect(tx0, tyy, tw, 3, 1.5);
    ctx.fill();
    const grad = ctx.createLinearGradient(tx0, 0, tx0 + tw, 0);
    grad.addColorStop(0, '#8f88ff');
    grad.addColorStop(1, '#4ade80');
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
  if (s === 'cancel') {
    for (let i = 0; i < NS; i++) strandKick[i] = (Math.random() - 0.5) * 2;
  }
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
