/* Blurt HUD — 悬浮指示器
 * 支持两种多线条律动波浪动画：
 * 1. ring: 环形律动波（iOS Siri / ChatGPT 语音模式风格的多层闭环丝绸光环）
 * 2. line: 水平丝浪（经典 Trapcode Form 粒子网格水平丝绸波）
 *
 * 状态说明：
 *   listen   聆听中（实时响应麦克风音量，光环呼吸扩张，丝缕波动起伏）
 *   process  识别中（Siri/ChatGPT 环形轨道旋转谐波 / 双频驻波拍频，蓝紫向青绿平滑渐变）
 *   success  完成（丝波收拢，并发射青绿光晕脉冲环）
 *   error    出错（偏蓝红高频躁动 + 抖动）
 *   nothing  没听到有效语音（灰阶塌陷）
 *   loading  引擎尚未就绪（呼吸提示）
 *   cancel   已取消（多缕径向向外散开消融）
 */
'use strict';

const W = 360, H = 140;          // 与 Rust 侧窗口尺寸一致（逻辑像素）
const CX = W / 2, CY = H / 2;    // 中心点 (180, 70)
const WY = 76;                   // 水平波基线
const X0 = 10, BW = W - 20;      // 水平波横向范围
const NS = 54;                   // 绸缕条数
const SEG_LINE = 88;             // 水平分段数
const SEG_RING = 96;             // 环形分段数
const SPREAD_LINE = 22;          // 水平布面厚度
const SPREAD_RING = 16;          // 环形布面径向厚度
const RING_BASE_R = 35;          // 环形基础半径

/* 每状态一对色（深部/亮部），绸缕在两色间渐变，加色叠出高光 */
const COL = {
  bar:     { deep: [161, 140, 209], light: [255, 154, 158] }, // 聆听：Night Fade梦幻紫 + Warm Flame蜜桃粉
  think:   { deep: [175, 145, 215], light: [251, 194, 235] }, // 识别：梦幻粉紫动效
  ok:      { deep: [255, 160, 165], light: [252, 190, 170] },  // 完成：自然春日暖杏蜜桃
  err:     { deep: [177, 42, 91], light: [255, 129, 119] },   // 错误：Young Passion 浆果红向珊瑚红
  quiet:   { deep: [160, 150, 175], light: [220, 205, 215] }, // 静默柔灰紫
  loading: { deep: [161, 140, 209], light: [252, 182, 159] }, // 加载紫杏
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

/* ---------- 样式与状态机 ---------- */
let hudStyle = 'ring'; // 'ring' | 'line'
let state = 'hidden';
let tEnter = 0;
let running = false;

let rawLevel = 0;      // 当前包络电平（已过噪声门）
let sinceVoice = 0;
let noiseFloor = 0.05; // 自适应环境噪声本底（跟踪最小值，跨会话保留）
let envLevel = 0;      // 起音快/释音慢的包络
let loud = 0;          // 显示响度（二级慢包络）：决定峰高与张开程度
let sessionT0 = 0;     // 会话开始时刻
let winMin = 1;        // 滑动窗最小电平
let winCount = 0;
let flowPhase = 0;     // 流动相位（按帧积分）
let lastFrameT = 0;
let etaMs = 1500;
let appearT = 0;

const now = () => performance.now() / 1000;
const clamp = (v, a, b) => Math.min(b, Math.max(a, v));
const lerp = (a, b, t) => a + (b - a) * t;
const mixCol = (c1, c2, t) => [lerp(c1[0], c2[0], t), lerp(c1[1], c2[1], t), lerp(c1[2], c2[2], t)];
const rgba = (c, a) => `rgba(${c[0] | 0},${c[1] | 0},${c[2] | 0},${a})`;
const easeOutCubic = t => 1 - Math.pow(1 - t, 3);
const smoothstep = (a, b, x) => { const t = clamp((x - a) / (b - a), 0, 1); return t * t * (3 - 2 * t); };

/* ---------- 2D 分形噪声 ---------- */
function hash2(x, y) { const s = Math.sin(x * 127.1 + y * 311.7) * 43758.5453; return s - Math.floor(s); }
function vnoise2(x, y) {
  const xi = Math.floor(x), yi = Math.floor(y);
  const xf = x - xi, yf = y - yi;
  const tx = xf * xf * (3 - 2 * xf), ty = yf * yf * (3 - 2 * yf);
  const a = hash2(xi, yi), b = hash2(xi + 1, yi), c = hash2(xi, yi + 1), d = hash2(xi + 1, yi + 1);
  return a + (b - a) * tx + (c - a) * ty + (a - b - c + d) * tx * ty; // 0..1
}

/* 水平波形专用噪声 */
function lineSilk(u, v, tt) {
  const a = vnoise2(u * 3.4, v * 2.8 + tt * 0.50);
  const b = vnoise2(u * 6.8 + 19.7, v * 5.5 + 7.3 + tt * 0.80) * 0.42;
  return ((a + b) / 1.42) * 2 - 1;
}
function lineSwell(u, tt) {
  const a = vnoise2(u * 3.2, 3.7 + tt * 0.60);
  const b = vnoise2(u * 6.4 + 11.3, 8.9 + tt * 0.95) * 0.55;
  return ((a + b) / 1.55) * 2 - 1;
}
function lineWeave(u, v, tt) {
  return vnoise2(u * 3.9, v * 9.1 + 3.3 + tt * 0.60) * 2 - 1;
}

/* 环形波专用周期性极坐标噪声（基于单位圆 nx, ny 采样，保证闭环 100% 连续无缝） */
function ringSilk(nx, ny, v, tt) {
  const a = vnoise2(nx * 1.8 + 12.3, ny * 1.8 + v * 1.4 + tt * 0.48);
  const b = vnoise2(nx * 3.6 + 37.1, ny * 3.6 + v * 2.5 + tt * 0.78) * 0.45;
  return ((a + b) / 1.45) * 2 - 1;
}
function ringSwell(nx, ny, tt) {
  const a = vnoise2(nx * 1.6 + 5.7, ny * 1.6 + tt * 0.55);
  const b = vnoise2(nx * 3.2 + 23.4, ny * 3.2 + tt * 0.90) * 0.5;
  return ((a + b) / 1.5) * 2 - 1;
}
function ringWeave(nx, ny, v, tt) {
  return vnoise2(nx * 2.8 + 7.1, ny * 2.8 + v * 3.5 + tt * 0.65) * 2 - 1;
}

function setState(s, payload) {
  const fresh = (state === 'hidden');
  if (s === 'process') {
    etaMs = Math.max(400, (payload && payload.eta_ms) || 1500);
  }
  state = s;
  tEnter = now();
  if (fresh) {
    appearT = tEnter;
    sessionT0 = tEnter;
    rawLevel = 0; envLevel = 0; loud = 0; sinceVoice = 0;
    winMin = 1; winCount = 0;
  }
  if (!running) { running = true; requestAnimationFrame(frame); }
}

/* ---------- 进度模型 ---------- */
let procStart = 0;
function currentProgress() {
  if (state !== 'process') return 0;
  const t = now() - procStart;
  const raw = t / (etaMs / 1000);
  let p;
  if (raw <= 1) p = 0.92 * (1 - Math.pow(1 - raw, 2.2));
  else p = 0.92 + 0.05 * (1 - Math.exp(-(raw - 1) * 0.8));
  return clamp(p, 0, 0.97);
}

/* ---------- 渲染数据缓存 ---------- */
const band = {
  deep: COL.bar.deep.slice(),
  light: COL.bar.light.slice(),
  amp: 0,
  spread: 1,
  aMul: 1,
};
const strandKick = new Float32Array(NS);
const pxLine = new Float32Array(SEG_LINE + 1);
const pyLine = new Float32Array(SEG_LINE + 1);
const pxRing = new Float32Array(SEG_RING + 1);
const pyRing = new Float32Array(SEG_RING + 1);

/* 预计算单位圆三角函数表，节省逐帧开销 */
const ringCos = new Float32Array(SEG_RING + 1);
const ringSin = new Float32Array(SEG_RING + 1);
for (let i = 0; i <= SEG_RING; i++) {
  const theta = (i / SEG_RING) * Math.PI * 2;
  ringCos[i] = Math.cos(theta);
  ringSin[i] = Math.sin(theta);
}

/* 降噪链 */
function pushLevel(v) {
  const age = sessionT0 ? now() - sessionT0 : 1;
  const warm = clamp((age - 0.12) / 0.30, 0, 1);
  const fastLearn = age < 0.30;
  if (v < noiseFloor) noiseFloor += (v - noiseFloor) * 0.12;
  else if (v < noiseFloor + 0.10) noiseFloor += (v - noiseFloor) * (fastLearn ? 0.10 : 0.02);
  else noiseFloor += fastLearn ? (v - noiseFloor) * 0.02 : 0.0004;

  winMin = Math.min(winMin, v);
  if (++winCount >= 150) {
    if (winMin > noiseFloor + 0.05) noiseFloor += (winMin - noiseFloor) * 0.8;
    winMin = 1; winCount = 0;
  }
  noiseFloor = clamp(noiseFloor, 0.02, 0.4);
  const gated = clamp((v - noiseFloor - 0.07) * 1.6, 0, 1) * warm;
  envLevel += (gated - envLevel) * (gated > envLevel ? 0.55 : 0.10);
  rawLevel = envLevel;
  loud += (envLevel - loud) * (envLevel > loud ? 0.5 : 0.12);
}

const domeAt = u => Math.pow(Math.max(0, Math.sin(Math.PI * clamp(u, 0, 1))), 0.85);
const edgeFade = u => smoothstep(0, 0.12, u) * (1 - smoothstep(0.88, 1, u));

/* ---------- 水平丝绸波渲染器 ---------- */
function renderLine(t, te, alpha, noiseT, heightK, peakAmp, procBlend) {
  const colAmp = new Float32Array(SEG_LINE + 1);
  for (let i = 0; i <= SEG_LINE; i++) {
    colAmp[i] = peakAmp * domeAt(i / SEG_LINE) * band.amp;
  }

  for (let s = 0; s < NS; s++) {
    const v = s / (NS - 1);
    const depth = (v - 0.5) * SPREAD_LINE * band.spread;
    const ampMul = 0.5 + 0.75 * v;
    const cMix = 0.25 + 0.75 * Math.sin(v * Math.PI);
    const col = mixCol(band.deep, band.light, cMix * 0.8);
    const aStrand = alpha * band.aMul * (0.075 + 0.08 * Math.sin(v * Math.PI));
    if (aStrand <= 0.004) continue;
    const kick = strandKick[s] * (state === 'cancel' ? te : 0) * 160;

    for (let i = 0; i <= SEG_LINE; i++) {
      const u = i / SEG_LINE;
      const dome = domeAt(u);
      const layerNoise = lineSilk(u + v * 1.0, v * 1.6, noiseT) * ampMul;
      const procW = () =>
        (Math.sin(u * Math.PI * 3 + noiseT * 0.18) * Math.sin(t * 4.6 - v * 1.2)
          + Math.sin(u * Math.PI * 5 - 0.9) * Math.sin(t * 3.1 + 1.1 - v * 0.8) * 0.6)
        * 0.55 * (0.8 + 0.35 * v)
        + layerNoise * 0.55;
      let wave;
      if (procBlend >= 1) {
        wave = procW();
      } else {
        wave = lineSwell(u, noiseT) * 0.55 + layerNoise * 0.85;
        if (procBlend > 0) wave = wave * (1 - procBlend) + procW() * procBlend;
      }
      const dy = wave * colAmp[i]
        + lineWeave(u, v, noiseT) * 4.5 * band.spread * dome * (0.25 + 0.75 * heightK);
      pxLine[i] = X0 + u * BW;
      pyLine[i] = WY + depth * Math.pow(dome, 0.7) * (0.25 + 0.75 * heightK)
        + clamp(dy, -50, 50) + kick * edgeFade(u);
    }

    ctx.beginPath();
    ctx.moveTo(pxLine[0], pyLine[0]);
    for (let i = 1; i < SEG_LINE; i++) {
      ctx.quadraticCurveTo(pxLine[i], pyLine[i], (pxLine[i] + pxLine[i + 1]) / 2, (pyLine[i] + pyLine[i + 1]) / 2);
    }
    ctx.lineTo(pxLine[SEG_LINE], pyLine[SEG_LINE]);
    ctx.strokeStyle = rgba(col, 1);
    ctx.globalAlpha = aStrand;
    ctx.lineWidth = 1.2;
    ctx.stroke();
  }

  ctx.globalCompositeOperation = 'source-over';
}

/* ---------- 环形/球体：iOS Siri 纯净流体彩色光晕 (Clean Vibrant Fluid Chroma) ---------- */
function renderRing(t, te, alpha, noiseT, heightK, peakAmp, procBlend) {
  const sig = state === 'listen' ? clamp(0.45 * loud + 0.55 * envLevel, 0, 1) : 0;

  // 基础半径与轨道扩散距离随音量与状态动态扩张（在安全视口内）
  let orbitD = 9 + 13 * Math.pow(sig, 0.7);
  let blobR = 28 + 16 * Math.pow(sig, 0.7);

  if (state === 'process') {
    orbitD = 10 + 2.5 * Math.sin(t * 3.2);
    blobR = 30 + 3 * Math.sin(t * 2.8);
  } else if (state === 'loading') {
    orbitD = 7 + 2 * Math.sin(t * 2.4);
    blobR = 25 + 3 * Math.sin(t * 2.4);
  } else if (state === 'success') {
    const k = clamp(te / 0.65, 0, 1);
    const easeK = easeOutCubic(k);
    orbitD = 8 * (1 - 0.85 * easeK);
    blobR = 27 * (1 - 0.55 * easeK);
  }

  // 纯正高饱和柔和渐变调色板（基于 WebGradients 暖粉紫/蜜桃杏/梦幻紫高级配色）
  let c1, c2, c3, c4, cBase;
  if (state === 'listen') {
    // 经典暖粉紫梦幻流体：Warm Flame 暖蜜桃粉 + Night Fade 梦幻紫罗兰 + Spring Warmth 柔粉兰 + Juicy Peach 温润杏粉
    c1 = [255, 140, 150];  // #ff8c96 Warm Flame 暖阳蜜桃粉
    c2 = [161, 140, 209];  // #a18cd1 Night Fade 优雅紫罗兰
    c3 = [251, 194, 235];  // #fbc2eb Spring Warmth 梦幻粉紫
    c4 = [252, 182, 159];  // #fcb69f Juicy Peach 温润蜜桃杏
    cBase = [178, 148, 220]; // 梦幻紫罗兰柔和基底
  } else if (state === 'process') {
    const p = currentProgress();
    // 识别中：紫罗兰与粉紫向温润晨光浅杏粉优雅流转
    c1 = mixCol([161, 140, 209], [255, 154, 158], p);
    c2 = mixCol([251, 194, 235], [252, 182, 159], p);
    c3 = mixCol([178, 148, 220], [255, 209, 255], p);
    c4 = mixCol([255, 154, 158], [255, 225, 205], p);
    cBase = mixCol([161, 140, 209], [245, 175, 185], p);
  } else if (state === 'success') {
    const k = clamp(te / 0.28, 0, 1);
    // 自然温润春日暖杏蜜桃（Spring Warmth & Juicy Peach Pearl）
    const okPeach = [255, 165, 168];      // 鲜嫩蜜桃粉
    const okApricot = [252, 192, 172];    // 柔润蜜桃杏
    const okPearl = [255, 216, 235];      // 珍珠柔粉白
    const okGold = [255, 210, 185];       // 晨曦浅金杏
    const okBase = [248, 172, 180];       // 温润暖蜜桃基底
    c1 = mixCol([255, 154, 158], okPeach, k);
    c2 = mixCol([252, 182, 159], okApricot, k);
    c3 = mixCol([255, 209, 255], okPearl, k);
    c4 = mixCol([255, 225, 205], okGold, k);
    cBase = mixCol([245, 175, 185], okBase, k);
  } else if (state === 'error') {
    c1 = [255, 129, 119]; // Young Passion 珊瑚红
    c2 = [177, 42, 91];   // 浆果红
    c3 = [245, 158, 11];  // 琥珀金
    c4 = [220, 38, 38];   // 玫瑰红
    cBase = [177, 42, 91];
  } else if (state === 'loading') {
    c1 = [161, 140, 209];
    c2 = [251, 194, 235];
    c3 = [252, 182, 159];
    c4 = [255, 154, 158];
    cBase = [161, 140, 209];
  } else {
    c1 = [160, 150, 175];
    c2 = [185, 175, 200];
    c3 = [220, 205, 215];
    c4 = [170, 160, 185];
    cBase = [160, 150, 175];
  }

  // --- 第 1 层：通透柔和的基底光晕 (Smooth Base Ambient Glow) ---
  ctx.save();
  ctx.globalCompositeOperation = 'source-over';
  const ambientR = blobR * 1.32;
  const ambientGrad = ctx.createRadialGradient(CX, CY, 0, CX, CY, ambientR);
  ambientGrad.addColorStop(0, rgba(cBase, 0.42 * alpha * band.aMul));
  ambientGrad.addColorStop(0.40, rgba(cBase, 0.25 * alpha * band.aMul));
  ambientGrad.addColorStop(0.75, rgba(cBase, 0.07 * alpha * band.aMul));
  ambientGrad.addColorStop(1, rgba(cBase, 0));
  ctx.fillStyle = ambientGrad;
  ctx.beginPath();
  ctx.arc(CX, CY, ambientR, 0, Math.PI * 2);
  ctx.fill();

  // --- 第 2 层：4 个旋转高饱和流体色斑 (4 Saturated Swirling Chroma Blobs) ---
  // 使用 source-over 保持纯正色彩饱和度，绝无加色发白发灰与浑浊死白
  const blobs = [
    {
      x: CX + Math.cos(noiseT * 1.15) * orbitD,
      y: CY + Math.sin(noiseT * 1.15) * orbitD,
      r: blobR * 1.05,
      col: c1,
      a: 0.68 * alpha * band.aMul,
    },
    {
      x: CX + Math.cos(-noiseT * 1.35 + 2.1) * (orbitD * 0.95),
      y: CY + Math.sin(-noiseT * 1.35 + 2.1) * (orbitD * 0.95),
      r: blobR * 0.98,
      col: c2,
      a: 0.70 * alpha * band.aMul,
    },
    {
      x: CX + Math.cos(noiseT * 0.90 + 4.2) * (orbitD * 1.02),
      y: CY + Math.sin(noiseT * 0.90 + 4.2) * (orbitD * 1.02),
      r: blobR * 1.02,
      col: c3,
      a: 0.72 * alpha * band.aMul,
    },
    {
      x: CX + Math.cos(-noiseT * 0.80 + 5.4) * (orbitD * 0.88),
      y: CY + Math.sin(-noiseT * 0.80 + 5.4) * (orbitD * 0.88),
      r: blobR * 0.94,
      col: c4,
      a: 0.65 * alpha * band.aMul,
    },
  ];

  for (const b of blobs) {
    if (b.a <= 0.005) continue;
    const g = ctx.createRadialGradient(b.x, b.y, 0, b.x, b.y, b.r);
    g.addColorStop(0, rgba(b.col, b.a));
    g.addColorStop(0.35, rgba(b.col, b.a * 0.75));
    g.addColorStop(0.68, rgba(b.col, b.a * 0.30));
    g.addColorStop(0.88, rgba(b.col, b.a * 0.06));
    g.addColorStop(1, rgba(b.col, 0));
    ctx.fillStyle = g;
    ctx.beginPath();
    ctx.arc(b.x, b.y, b.r, 0, Math.PI * 2);
    ctx.fill();
  }

  // --- 第 3 层：核心极温和的通透微光 (Subtle Soft Core Specular) ---
  const coreR = blobR * 0.45;
  const coreGrad = ctx.createRadialGradient(CX, CY, 0, CX, CY, coreR);
  coreGrad.addColorStop(0, rgba([255, 255, 255], 0.16 * alpha * band.aMul));
  coreGrad.addColorStop(0.5, rgba([255, 255, 255], 0.06 * alpha * band.aMul));
  coreGrad.addColorStop(1, 'rgba(255, 255, 255, 0)');
  ctx.fillStyle = coreGrad;
  ctx.beginPath();
  ctx.arc(CX, CY, coreR, 0, Math.PI * 2);
  ctx.fill();

  ctx.restore();
}

/* ---------- 动画主循环 ---------- */
function frame() {
  if (!running) return;
  const t = now();
  const dt = clamp(t - lastFrameT, 1 / 240, 1 / 20);
  lastFrameT = t;
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, W, H);

  if (state === 'hidden') { running = false; return; }

  /* 全局出现/消失包络 */
  const ta = t - appearT;
  let alpha = clamp(ta / 0.22, 0, 1);
  let shakeX = 0;
  const te = t - tEnter;

  if (state === 'success') {
    // 纯粹自然凝聚消融：0.15s~0.65s 舒缓平滑淡出
    const k = clamp((te - 0.15) / 0.50, 0, 1);
    alpha *= 1 - easeOutCubic(k);
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

  /* 静默检测 */
  if (rawLevel > 0.03) sinceVoice = 0; else sinceVoice += dt;
  const quiet = state === 'listen' ? clamp((sinceVoice - 1.6) / 0.8, 0, 1) : 0;

  /* 计算布面状态目标 */
  let tPair = COL.bar, tAmp = 0, tSpread = 1, tAMul = 1;
  if (state === 'listen') {
    tPair = quiet > 0
      ? { deep: mixCol(COL.bar.deep, COL.quiet.deep, quiet), light: mixCol(COL.bar.light, COL.quiet.light, quiet) }
      : COL.bar;
    tAmp = 1; tSpread = 1;
    tAMul = lerp(1, 0.55 + 0.16 * Math.sin(t * 2.6), quiet);
  } else if (state === 'process') {
    const p = currentProgress();
    tPair = {
      deep: mixCol(COL.think.deep, COL.ok.deep, p * 0.65),
      light: mixCol(COL.think.light, COL.ok.light, p * 0.65),
    };
    tAmp = 1; tSpread = 1;
  } else if (state === 'success') {
    const k = easeOutCubic(clamp(te / 0.22, 0, 1));
    tPair = COL.ok; tAmp = 1 - 0.94 * k; tSpread = 1 - 0.9 * k;
    tAMul = 1 + 0.35 * k;
  } else if (state === 'error') {
    tPair = COL.err; tAmp = 1; tSpread = 1.1;
  } else if (state === 'nothing') {
    tPair = COL.quiet; tAmp = 0.18; tSpread = 0.5; tAMul = 0.8;
  } else if (state === 'loading') {
    tPair = COL.loading; tAmp = 1; tSpread = 0.9;
    tAMul = 0.62 + 0.30 * Math.sin(t * 3.4);
  } else if (state === 'cancel') {
    tAmp = 0.4; tSpread = 1.6; tAMul = 0.9;
  }
  const spd = state === 'listen' ? 0.55 : 0.42;
  band.deep = mixCol(band.deep, tPair.deep, 0.3);
  band.light = mixCol(band.light, tPair.light, 0.3);
  band.amp += (tAmp - band.amp) * spd;
  band.spread += (tSpread - band.spread) * spd;
  band.aMul += (tAMul - band.aMul) * spd;

  /* 布面流动相位 */
  const flowSpd = state === 'error' ? 5
    : state === 'process' ? 1.7 + currentProgress() * 1.5
      : (hudStyle === 'ring' ? 1.5 + loud * 3.5 : 1 + loud * 3.2);
  flowPhase += dt * flowSpd;
  const noiseT = flowPhase;

  /* 振幅峰高 */
  let peakAmp;
  if (state === 'listen') {
    const sig = clamp(0.45 * loud + 0.55 * envLevel, 0, 1);
    peakAmp = hudStyle === 'ring'
      ? (14 + 28 * Math.pow(sig, 0.7))
      : (4 + 44 * Math.pow(sig, 0.7));
  } else if (state === 'process') peakAmp = 20;
  else if (state === 'error') peakAmp = 10;
  else if (state === 'loading') peakAmp = hudStyle === 'ring' ? 12 : (5.5 + 2.5 * Math.sin(t * 2.4));
  else peakAmp = 9;
  const heightK = clamp((peakAmp - 4) / 44, 0.10, 1);

  const procBlend = state === 'process' ? easeOutCubic(clamp(te / 0.4, 0, 1)) : 0;

  ctx.save();
  ctx.translate(shakeX, 0);
  ctx.globalCompositeOperation = 'lighter';
  ctx.lineJoin = 'round';

  if (hudStyle === 'line') {
    renderLine(t, te, alpha, noiseT, heightK, peakAmp, procBlend);
  } else {
    renderRing(t, te, alpha, noiseT, heightK, peakAmp, procBlend);
  }

  ctx.restore();
  requestAnimationFrame(frame);
}

/* ---------- 事件接线 ---------- */
const { listen } = window.__TAURI__.event;
const { invoke } = window.__TAURI__.core;
const closeBtn = document.getElementById('closeBtn');
invoke('get_noise_floor').then(v => {
  if (typeof v === 'number' && isFinite(v)) noiseFloor = clamp(v, 0.02, 0.4);
}).catch(() => {});

listen('hud:state', e => {
  const s = e.payload.state;
  if (s === 'process') procStart = now();
  if (s === 'cancel') {
    for (let i = 0; i < NS; i++) strandKick[i] = (Math.random() - 0.5) * 2;
  }
  if (s === 'hidden') { state = 'hidden'; closeBtn.classList.remove('show'); return; }
  setState(s, e.payload);
});

// 每 20ms 一帧真实能量
listen('hud:level', e => pushLevel(clamp(e.payload.v, 0, 1)));

/* 光标移入 HUD → 浮现取消按钮 */
listen('hud:hover', e => {
  const active = state === 'listen' || state === 'process';
  closeBtn.classList.toggle('show', !!e.payload.v && active);
});

closeBtn.addEventListener('click', () => {
  closeBtn.classList.remove('show');
  invoke('cancel_session');
});

