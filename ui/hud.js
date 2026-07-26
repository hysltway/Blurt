/* Blurt HUD — 无文字悬浮指示器（丝绸粒子波，对齐 public/01lrjxheswmgiybzm4veqy3137.gif
 * 即 public/语音识别动画.aep 的渲染效果：Trapcode Form 粒子网格丝绸波）
 * 无胶囊、无背景，直接展示动画。
 * 整体形态 = 中央穹顶（两头窄、中间高），峰高由降噪后的实时响度决定；
 * 内部起伏 = 共同的行进大波 + 绸缕分层错相（层次感），响度越大布面张得越开：
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

let rawLevel = 0;      // 当前包络电平（已过噪声门）
let sinceVoice = 0;
let noiseFloor = 0.05; // 自适应环境噪声本底（跟踪最小值，跨会话保留）
let envLevel = 0;      // 起音快/释音慢的包络
let loud = 0;          // 显示响度（二级慢包络）：决定中央峰高与布面张开程度
let flowPhase = 0;     // 布面流动相位（按帧积分；电平抖动不会使噪声场跳变）
let lastFrameT = 0;
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
/* 所有噪声场的时间都走第二个坐标轴（相位维），u 轴系数不含 tt：
 * 波形在原地隆起/回落（上下起伏），不会产生从一侧向另一侧推移的方向感 */
function silk(u, v, tt) { // -1..1，绸缕分层错相用
  const a = vnoise2(u * 3.4, v * 2.8 + tt * 0.50);
  const b = vnoise2(u * 6.8 + 19.7, v * 5.5 + 7.3 + tt * 0.80) * 0.42;
  return ((a + b) / 1.42) * 2 - 1;
}
function swell(u, tt) { // 共同大波：两个倍频程原地演变（GIF 的 3-5 个波峰节奏）
  const a = vnoise2(u * 3.2, 3.7 + tt * 0.42);
  const b = vnoise2(u * 6.4 + 11.3, 8.9 + tt * 0.66) * 0.55;
  return ((a + b) / 1.55) * 2 - 1;
}
function weave(u, v, tt) { // 高 v 频细织纹：让每缕在布面内可见地穿插
  return vnoise2(u * 3.9, v * 9.1 + 3.3 + tt * 0.60) * 2 - 1;
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
    rawLevel = 0; envLevel = 0; loud = 0; sinceVoice = 0; // noiseFloor 保留（环境不变）
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

/* 降噪链：本底跟踪 → 噪声门 → 起音/释音包络 → 二级慢包络（显示响度）
 * 环境噪声（风扇/底噪）被门限吃掉，波形只响应真实语音；
 * 起音快（跟手）、释音慢（丝滑收尾），消除逐样本的剧烈抖动。 */
function pushLevel(v) {
  if (v < noiseFloor) noiseFloor += (v - noiseFloor) * 0.12;            // 快速下探
  else if (v < noiseFloor + 0.10) noiseFloor += (v - noiseFloor) * 0.02; // 近本底缓升
  else noiseFloor += 0.0004;                                             // 说话时几乎不动
  noiseFloor = clamp(noiseFloor, 0.02, 0.4);
  const gated = clamp((v - noiseFloor - 0.07) * 1.35, 0, 1); // 门限抬高：残余噪声不再引起波动
  envLevel += (gated - envLevel) * (gated > envLevel ? 0.55 : 0.10);
  rawLevel = envLevel;
  loud += (envLevel - loud) * (envLevel > loud ? 0.35 : 0.055);          // 峰高呼吸感
}

// 中央穹顶：两头窄、中间高（振幅包络）；厚度衰减更缓，让中段层次可见
const domeAt = u => Math.pow(Math.max(0, Math.sin(Math.PI * clamp(u, 0, 1))), 0.85);
const edgeFade = u => smoothstep(0, 0.12, u) * (1 - smoothstep(0.88, 1, u)); // 两端收成细线

function frame() {
  if (!running) return;
  const t = now();
  const dt = clamp(t - lastFrameT, 1 / 240, 1 / 20); // 真实帧间隔（防切后台/首帧突跳）
  lastFrameT = t;
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

  /* --- 静默检测（包络已过噪声门：环境噪音不算有声） --- */
  if (rawLevel > 0.03) sinceVoice = 0; else sinceVoice += dt;
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

  /* --- 布面流动相位：按帧积分（说话越响流得越快），电平抖动只影响流速不瞬移场 --- */
  flowPhase += dt * (state === 'error' ? 5 : 1 + loud * 1.6);
  const noiseT = flowPhase;

  /* --- 中央峰高：实时响度决定“中间有多高”；heightK 控制布面张合（层次开度） --- */
  let peakAmp;
  if (state === 'listen') peakAmp = 5 + 27 * Math.tanh(loud * 2.2);
  else if (state === 'process') peakAmp = 13;
  else if (state === 'error') peakAmp = 10;
  else if (state === 'loading') peakAmp = 5.5 + 2.5 * Math.sin(t * 2.4);
  else peakAmp = 9; // success / nothing / cancel：残余起伏随 band.amp 衰减
  const heightK = clamp((peakAmp - 5) / 27, 0.10, 1); // 0=安静细线 → 1=全开

  /* --- 每列振幅：穹顶包络（两头窄、中间高） --- */
  const colAmp = new Float32Array(SEG + 1);
  for (let i = 0; i <= SEG; i++) {
    const u = i / SEG;
    let amp = peakAmp;
    if (state === 'process') amp = peakAmp + 5 * Math.sin(u * 5.6 - t * 3.2); // 行波脉动
    colAmp[i] = amp * domeAt(u) * band.amp;
  }

  ctx.save();
  ctx.translate(shakeX, 0);
  ctx.globalCompositeOperation = 'lighter';
  ctx.lineJoin = 'round';

  /* --- 绸缕（网格行）：同一片噪声布面错相采样 + 振幅视差 → 绸缕交错成褶皱 --- */
  const px = new Float32Array(SEG + 1);
  const py = new Float32Array(SEG + 1);
  for (let s = 0; s < NS; s++) {
    const v = s / (NS - 1);
    const depth = (v - 0.5) * SPREAD * band.spread;
    const ampMul = 0.65 + 0.5 * v;                // 行间视差：上层摆幅大，褶皱处绸缕交叉
    // 深部 → 亮部：中间行更亮（折痕高光集中在中部）
    const cMix = 0.25 + 0.75 * Math.sin(v * Math.PI);
    const col = mixCol(band.deep, band.light, cMix * 0.8);
    const aStrand = alpha * band.aMul * (0.075 + 0.08 * Math.sin(v * Math.PI));
    if (aStrand <= 0.004) continue;
    const kick = strandKick[s] * (state === 'cancel' ? te : 0) * 160;

    for (let i = 0; i <= SEG; i++) {
      const u = i / SEG;
      const dome = domeAt(u);
      // 共同大波给出原地上下起伏的主节奏（无横向推移）；
      // 绸缕只小幅错相分层（层次而不散），细织纹随布面张开才显现
      const common = swell(u, noiseT);
      const layer = silk(u + v * 0.6, v * 1.2, noiseT) * ampMul; // 层间错相大：各层波峰错开
      const dy = (common * 0.65 + layer * 0.70) * colAmp[i]
        + weave(u, v, noiseT) * 3.6 * band.spread * dome * (0.25 + 0.75 * heightK);
      px[i] = X0 + u * BW;
      py[i] = WY + depth * Math.pow(dome, 0.7) * (0.3 + 0.6 * heightK)
        + clamp(dy, -42, 42) + kick * edgeFade(u);
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

// 每 20ms 一帧真实能量：经降噪链处理后推入滚动历史（右新左旧）
listen('hud:level', e => pushLevel(clamp(e.payload.v, 0, 1)));

/* 光标移入 HUD（Rust 侧解除点击穿透后）→ 浮现取消按钮 */
listen('hud:hover', e => {
  const active = state === 'listen' || state === 'process';
  closeBtn.classList.toggle('show', !!e.payload.v && active);
});

closeBtn.addEventListener('click', () => {
  closeBtn.classList.remove('show');
  invoke('cancel_session');
});
