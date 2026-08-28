//! 录音管线：cpal 采集（任意采样率/声道）→ 单声道 f32 → 16kHz 重采样 → 静音裁剪
//! 采集线程实时回报 RMS 电平（驱动 HUD 声波条）。
//! 重采样在录音期间随录随做（StreamResampler 增量喂入），松开按键时只需冲洗
//! 不足一块的尾巴——识别开始前的收尾延迟从 O(录音时长) 降为常数。

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_channel::{bounded, Sender};
use parking_lot::Mutex;
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const TARGET_SR: u32 = 16000;

/// SincFixedIn 的固定输入块大小（源采样率下的样本数）
const RS_CHUNK: usize = 1024;
const LONG_RECORDING_PREFIX: &str = "long-";
static RECORDING_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, PartialEq)]
pub enum StopMode {
    /// 正常结束，进入识别
    Finish,
    /// 放弃本次录音
    Abort,
}

pub struct RecorderHandle {
    stop_tx: Sender<StopMode>,
}

impl RecorderHandle {
    pub fn stop(&self, mode: StopMode) {
        let _ = self.stop_tx.send(mode);
    }
}

pub fn list_input_devices() -> Result<Vec<String>, String> {
    let host = cpal::default_host();
    host.input_devices()
        .map_err(|e| format!("枚举麦克风失败：{e}"))?
        .map(|device| {
            device
                .name()
                .map_err(|e| format!("读取麦克风名称失败：{e}"))
        })
        .collect()
}

fn resolve_input_device(device_name: Option<&str>) -> Result<cpal::Device, String> {
    let host = cpal::default_host();
    match device_name {
        Some(name) => host
            .input_devices()
            .map_err(|e| format!("枚举麦克风失败：{e}"))?
            .find_map(|device| match device.name() {
                Ok(device_name) if device_name == name => Some(Ok(device)),
                Ok(_) => None,
                Err(error) => Some(Err(format!("读取麦克风名称失败：{error}"))),
            })
            .transpose()?
            .ok_or_else(|| format!("找不到已选麦克风：{name}")),
        None => host
            .default_input_device()
            .ok_or_else(|| "未找到系统默认麦克风".into()),
    }
}

/// 实际初始化并启动输入流后立即释放，以验证设备、驱动与系统麦克风权限。
pub fn check_input_device(device_name: Option<&str>) -> Result<(), String> {
    let device = resolve_input_device(device_name)?;
    let supported = device
        .default_input_config()
        .map_err(|e| format!("无法读取麦克风配置：{e}"))?;
    let sample_format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let stream = device
        .build_input_stream_raw(&config, sample_format, |_, _| {}, |_| {}, None)
        .map_err(|e| format!("无法打开麦克风：{e}"))?;
    stream.play().map_err(|e| format!("无法启动录音：{e}"))
}

/* ---------------- 流式重采样 ---------------- */

enum ResamplerKind {
    /// 源已是 16kHz，直通
    Passthrough,
    Sinc(SincFixedIn<f32>),
    /// rubato 初始化失败：攒下原始样本，finish 时线性重采样兜底
    LinearFallback(u32),
}

/// 任意采样率单声道 → 16kHz，支持增量喂入：
/// 录音期间每个 tick push 一批，finish() 冲洗 sinc 尾部并交出全部输出。
struct StreamResampler {
    kind: ResamplerKind,
    /// 不足一个 chunk 的输入余量（LinearFallback 下攒全部输入）
    pending: Vec<f32>,
    out: Vec<f32>,
    /// 复用的输出缓冲，避免每个 chunk 都分配
    outbuf: Vec<Vec<f32>>,
    /// 是否喂过样本（从未喂过则 finish 不冲洗，避免凭空输出延迟线里的零）
    fed: bool,
}

impl StreamResampler {
    fn new(sr: u32) -> Self {
        let (kind, outbuf) = if sr == TARGET_SR {
            (ResamplerKind::Passthrough, vec![])
        } else {
            let params = SincInterpolationParameters {
                sinc_len: 128,
                f_cutoff: 0.95,
                interpolation: SincInterpolationType::Linear,
                oversampling_factor: 128,
                window: WindowFunction::BlackmanHarris2,
            };
            match SincFixedIn::<f32>::new(TARGET_SR as f64 / sr as f64, 2.0, params, RS_CHUNK, 1) {
                Ok(rs) => {
                    let cap = rs.output_frames_max();
                    (ResamplerKind::Sinc(rs), vec![vec![0.0f32; cap]])
                }
                Err(_) => (ResamplerKind::LinearFallback(sr), vec![]),
            }
        };
        Self {
            kind,
            pending: Vec::new(),
            out: Vec::new(),
            outbuf,
            fed: false,
        }
    }

    fn push(&mut self, samples: &[f32]) {
        if samples.is_empty() {
            return;
        }
        self.fed = true;
        match &mut self.kind {
            ResamplerKind::Passthrough => self.out.extend_from_slice(samples),
            ResamplerKind::LinearFallback(_) => self.pending.extend_from_slice(samples),
            ResamplerKind::Sinc(rs) => {
                self.pending.extend_from_slice(samples);
                let mut consumed = 0;
                while self.pending.len() - consumed >= RS_CHUNK {
                    let input = [&self.pending[consumed..consumed + RS_CHUNK]];
                    if let Ok((_, n)) = rs.process_into_buffer(&input[..], &mut self.outbuf, None) {
                        self.out.extend_from_slice(&self.outbuf[0][..n]);
                    }
                    consumed += RS_CHUNK;
                }
                if consumed > 0 {
                    self.pending.drain(..consumed);
                }
            }
        }
    }

    fn output_len(&self) -> usize {
        self.out.len()
    }

    fn output(&self) -> &[f32] {
        &self.out
    }

    fn finish(mut self) -> Vec<f32> {
        match &mut self.kind {
            ResamplerKind::Passthrough => {}
            ResamplerKind::LinearFallback(sr) => return linear_resample(&self.pending, *sr),
            ResamplerKind::Sinc(rs) => {
                if !self.fed {
                    return Vec::new();
                }
                if !self.pending.is_empty() {
                    let input = [&self.pending[..]];
                    if let Ok((_, n)) =
                        rs.process_partial_into_buffer(Some(&input[..]), &mut self.outbuf, None)
                    {
                        self.out.extend_from_slice(&self.outbuf[0][..n]);
                    }
                }
                let flush: Option<&[&[f32]]> = None;
                if let Ok((_, n)) = rs.process_partial_into_buffer(flush, &mut self.outbuf, None) {
                    self.out.extend_from_slice(&self.outbuf[0][..n]);
                }
            }
        }
        self.out
    }
}

/* ---------------- 录音 ---------------- */

/// 启动录音线程。`on_level` 约 50Hz 回调每 tick 的原始 RMS（未做感知映射，
/// 供调用方做 HUD 映射与噪声统计）；
/// `make_on_samples` 在录音线程内创建增量样本处理器；处理器接收已经重采样为
/// 16kHz 单声道的样本，供在线识别和语音端点检测；
/// `on_done` 仅在 Finish（含超时自动结束）时回调，交付 16kHz 单声道样本。
pub fn start_recording<S>(
    device_name: Option<String>,
    max_secs: u64,
    mut on_level: impl FnMut(f32) + Send + 'static,
    make_on_samples: impl FnOnce() -> S + Send + 'static,
    on_done: impl FnOnce(Result<Vec<f32>, String>) + Send + 'static,
) -> Result<RecorderHandle, String>
where
    S: FnMut(&[f32]) + 'static,
{
    let (stop_tx, stop_rx) = bounded::<StopMode>(2);

    std::thread::Builder::new()
        .name("blurt-recorder".into())
        .spawn(move || {
            let started = Instant::now();
            let device = match resolve_input_device(device_name.as_deref()) {
                Ok(device) => device,
                Err(error) => {
                    on_done(Err(error));
                    return;
                }
            };
            let sup = match device.default_input_config() {
                Ok(c) => c,
                Err(e) => {
                    on_done(Err(format!("无法读取麦克风配置：{e}")));
                    return;
                }
            };
            let sr = sup.sample_rate().0;
            let channels = sup.channels() as usize;
            let sample_format = sup.sample_format();
            let config: cpal::StreamConfig = sup.into();

            // 采集缓冲（已混为单声道，原始采样率）。实时回调只做 push；
            // 电平计算与重采样都在本线程的 20ms tick 里，回调保持轻量。
            // 缓冲经双缓冲交换每 tick 清空，容量稳定在积压峰值后不再分配。
            let buf: Arc<Mutex<Vec<f32>>> =
                Arc::new(Mutex::new(Vec::with_capacity(sr as usize / 4)));

            // 注意：Arc 必须以宏参数传入（宏体内的自由标识符按定义处解析，
            // 会捕获外层原始 Arc 而非闭包内的克隆）。
            macro_rules! push_frames {
                ($buf:expr, $data:expr, $channels:expr, $to_f32:expr) => {{
                    let mut b = $buf.lock();
                    for frame in $data.chunks($channels) {
                        let mut s = 0.0f32;
                        for v in frame {
                            s += $to_f32(*v);
                        }
                        b.push(s / $channels as f32);
                    }
                }};
            }

            let err_flag: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
            let err_cb = {
                let err_flag = err_flag.clone();
                move |e: cpal::StreamError| {
                    *err_flag.lock() = Some(format!("音频流错误:{e}"));
                }
            };

            let stream = match sample_format {
                cpal::SampleFormat::F32 => {
                    let b = buf.clone();
                    device.build_input_stream(
                        &config,
                        move |data: &[f32], _| {
                            push_frames!(b, data, channels, |v: f32| v);
                        },
                        err_cb,
                        None,
                    )
                }
                cpal::SampleFormat::I16 => {
                    let b = buf.clone();
                    device.build_input_stream(
                        &config,
                        move |data: &[i16], _| {
                            push_frames!(b, data, channels, |v: i16| v as f32 / 32768.0);
                        },
                        err_cb,
                        None,
                    )
                }
                cpal::SampleFormat::U16 => {
                    let b = buf.clone();
                    device.build_input_stream(
                        &config,
                        move |data: &[u16], _| {
                            push_frames!(b, data, channels, |v: u16| (v as f32 - 32768.0)
                                / 32768.0);
                        },
                        err_cb,
                        None,
                    )
                }
                f => {
                    on_done(Err(format!("不支持的采样格式:{f:?}")));
                    return;
                }
            };
            let stream = match stream {
                Ok(s) => s,
                Err(e) => {
                    on_done(Err(format!("无法打开麦克风：{e}")));
                    return;
                }
            };
            let mut on_samples = make_on_samples();
            if let Err(e) = stream.play() {
                on_done(Err(format!("无法启动录音：{e}")));
                return;
            }

            // 流式重采样器：录音过程中增量处理，结束时只剩尾部冲洗
            let mut rs = StreamResampler::new(sr);
            let mut streamed_samples = 0usize;
            // 与 buf 同容量：交换进回调侧后无需再扩容
            let mut spare: Vec<f32> = Vec::with_capacity(sr as usize / 4);

            // 主循环：等停止信号，同时每 20ms 取走积压样本 → 报电平 → 增量重采样
            let mode = loop {
                match stop_rx.recv_timeout(Duration::from_millis(20)) {
                    Ok(m) => break m,
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                        if let Some(e) = err_flag.lock().take() {
                            drop(stream);
                            on_done(Err(e));
                            return;
                        }
                        // 双缓冲交换：临界区只有一次指针交换，spare 容量跨 tick 复用
                        std::mem::swap(&mut *buf.lock(), &mut spare);
                        if !spare.is_empty() {
                            let sq: f64 = spare.iter().map(|&s| s as f64 * s as f64).sum();
                            let rms = (sq / spare.len() as f64).sqrt() as f32;
                            on_level(rms);
                            rs.push(&spare);
                            if rs.output_len() > streamed_samples {
                                on_samples(&rs.output()[streamed_samples..]);
                                streamed_samples = rs.output_len();
                            }
                            spare.clear();
                        }
                        if started.elapsed().as_secs() >= max_secs {
                            break StopMode::Finish; // 超时自动结束
                        }
                    }
                    Err(_) => break StopMode::Abort,
                }
            };

            drop(stream); // 停止采集
            if mode == StopMode::Abort {
                return;
            }

            // 冲洗停止信号到达前最后未处理的余量
            let tail = std::mem::take(&mut *buf.lock());
            rs.push(&tail);
            if rs.output_len() > streamed_samples {
                on_samples(&rs.output()[streamed_samples..]);
                streamed_samples = rs.output_len();
            }
            let samples = rs.finish();
            if samples.len() > streamed_samples {
                on_samples(&samples[streamed_samples..]);
            }
            on_done(Ok(samples));
        })
        .map_err(|e| format!("无法创建录音线程：{e}"))?;

    Ok(RecorderHandle { stop_tx })
}

/// 读取任意 wav → 16kHz 单声道 f32（自检与测速共用）
#[cfg(test)]
pub fn read_wav_16k_mono(path: &str) -> Result<Vec<f32>, String> {
    let mut reader = hound::WavReader::open(path).map_err(|e| format!("打开 wav 失败：{e}"))?;
    let spec = reader.spec();
    let raw: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .collect::<Result<_, _>>()
            .map_err(|e| e.to_string())?,
        hound::SampleFormat::Int => {
            let max = (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 / max))
                .collect::<Result<_, _>>()
                .map_err(|e| e.to_string())?
        }
    };
    let mono = interleaved_to_mono(&raw, spec.channels);
    Ok(to_16k(&mono, spec.sample_rate))
}

/// 将真实采集到的 16kHz 单声道样本保存为 float WAV，并滚动保留最近 `keep` 条。
/// float WAV 避免诊断样本发生二次量化，便于无损重放录音管线。
pub fn save_recent_recording(samples: &[f32], dir: &Path, keep: usize) -> Result<PathBuf, String> {
    if samples.is_empty() {
        return Err("录音样本为空".into());
    }
    if keep == 0 {
        return Err("录音保留数量必须大于 0".into());
    }

    fs::create_dir_all(dir).map_err(|e| format!("创建录音留存目录失败：{e}"))?;

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let seq = RECORDING_SEQ.fetch_add(1, Ordering::Relaxed);
    let duration_ms = samples.len() as u64 * 1000 / TARGET_SR as u64;
    let file_name = format!(
        "{LONG_RECORDING_PREFIX}{now_ms:013}-{:05}-{seq:06}-{duration_ms}ms.wav",
        std::process::id()
    );
    let path = dir.join(&file_name);
    let temp_path = dir.join(format!(".{file_name}.tmp"));

    let write_result = (|| -> Result<(), String> {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: TARGET_SR,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut writer = hound::WavWriter::create(&temp_path, spec)
            .map_err(|e| format!("创建录音 WAV 失败：{e}"))?;
        for &sample in samples {
            writer
                .write_sample(sample)
                .map_err(|e| format!("写入录音 WAV 失败：{e}"))?;
        }
        writer
            .finalize()
            .map_err(|e| format!("完成录音 WAV 失败：{e}"))
    })();

    if let Err(e) = write_result {
        let _ = fs::remove_file(&temp_path);
        return Err(e);
    }
    if let Err(e) = fs::rename(&temp_path, &path) {
        let _ = fs::remove_file(&temp_path);
        return Err(format!("提交录音 WAV 失败：{e}"));
    }

    let mut recordings: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(|e| format!("读取录音留存目录失败：{e}"))?
        .flatten()
        .map(|entry| entry.path())
        .filter(|candidate| {
            candidate.extension().and_then(|s| s.to_str()) == Some("wav")
                && candidate
                    .file_name()
                    .and_then(|s| s.to_str())
                    .is_some_and(|name| name.starts_with(LONG_RECORDING_PREFIX))
        })
        .collect();
    recordings.sort();

    let remove_count = recordings.len().saturating_sub(keep);
    for stale in recordings
        .into_iter()
        .filter(|candidate| candidate != &path)
        .take(remove_count)
    {
        fs::remove_file(&stale).map_err(|e| format!("清理旧录音 {} 失败：{e}", stale.display()))?;
    }

    Ok(path)
}

/// 单声道任意采样率 → 16kHz（rubato sinc 重采样）
#[cfg(test)]
pub fn to_16k(mono: &[f32], sr: u32) -> Vec<f32> {
    if sr == TARGET_SR {
        return mono.to_vec();
    }
    let mut rs = StreamResampler::new(sr);
    rs.push(mono);
    rs.finish()
}

/// 兜底线性重采样（仅在 rubato 初始化失败时使用）
fn linear_resample(mono: &[f32], sr: u32) -> Vec<f32> {
    if mono.is_empty() {
        return vec![];
    }
    let ratio = sr as f64 / TARGET_SR as f64;
    let n = (mono.len() as f64 / ratio) as usize;
    (0..n)
        .map(|i| {
            let x = i as f64 * ratio;
            let j = x as usize;
            let f = (x - j as f64) as f32;
            let a = mono[j.min(mono.len() - 1)];
            let b = mono[(j + 1).min(mono.len() - 1)];
            a + (b - a) * f
        })
        .collect()
}

/// 交错多声道 → 单声道
#[cfg(test)]
pub fn interleaved_to_mono(samples: &[f32], channels: u16) -> Vec<f32> {
    if channels <= 1 {
        return samples.to_vec();
    }
    let ch = channels as usize;
    samples
        .chunks(ch)
        .map(|f| f.iter().sum::<f32>() / ch as f32)
        .collect()
}

/// 裁掉首尾静音（保留 pad），全静音则返回空。
/// 接管所有权原地裁剪，避免对整段音频再做一次分配拷贝。
pub fn trim_silence(mut samples: Vec<f32>) -> Vec<f32> {
    const FRAME: usize = 320; // 20ms @16k
    const PAD_FRAMES: usize = 8; // 160ms
    if samples.len() < FRAME * 4 {
        return Vec::new();
    }
    let energies: Vec<f32> = samples
        .chunks(FRAME)
        .map(|c| (c.iter().map(|s| s * s).sum::<f32>() / c.len() as f32).sqrt())
        .collect();
    let mut sorted = energies.clone();
    sorted.sort_by(|a, b| a.total_cmp(b)); // total_cmp：NaN 样本不会引发 panic
    let floor = sorted[sorted.len() / 10]; // 10 分位当噪声地板
    let thresh = (floor * 3.0).max(0.004);
    let first = energies.iter().position(|&e| e > thresh);
    let last = energies.iter().rposition(|&e| e > thresh);
    match (first, last) {
        (Some(f), Some(l)) if l >= f => {
            let s = f.saturating_sub(PAD_FRAMES) * FRAME;
            let e = ((l + 1 + PAD_FRAMES) * FRAME).min(samples.len());
            samples.truncate(e);
            if s > 0 {
                samples.drain(..s);
            }
            samples
        }
        _ => Vec::new(),
    }
}

/* ---------------- 静音端点检测 ---------------- */

/// 感知化映射：人声 RMS 动态范围压缩到 0..1（HUD 声波条与噪声本底持久化共用）
pub fn perceptual_level(rms: f32) -> f32 {
    (rms * 11.0).powf(0.65).min(1.0)
}

/// `perceptual_level` 的逆映射：持久化的感知域本底还原为原始 RMS
pub fn perceptual_to_rms(level: f32) -> f32 {
    level.max(0.0).powf(1.0 / 0.65) / 11.0
}

/// 静音端点检测（自动停止）：原始 RMS 域的乘性噪声门。
///
/// 地板 = 最近 3s RMS 的滑动窗最小值：说话在字间必回落、不抬地板，
/// 环境整体变吵则 3s 内自动上调（涵盖旧实现的「冗余再学习」）；
/// 窗未满时与上会话持久化的种子取小者，陈旧种子最迟窗满即被纠正。
/// 判有声用 `rms > max(3×地板, 0.005)`：乘性门限自适应环境噪声强弱，
/// 不像感知域加性门限那样被映射放大的底噪波动反复重置静音计时。
/// 静音按会话时钟计秒，回调节奏波动不影响时长；有声需连续 3 帧（60ms）
/// 才重置计时，单帧毛刺（键盘声、碰撞声）不会打断倒计时。
pub struct SilenceGate {
    stop_secs: f32,
    seed_floor: f32,
    ring: Vec<f32>,
    pos: usize,
    voiced_run: u32,
    heard_speech: bool,
    /// 当前静音段起点（会话内秒），None = 处于语音段
    silent_since: Option<f32>,
    done: bool,
}

impl SilenceGate {
    /// 噪声地板滑动窗长度（50Hz 回调 × 150 帧 = 3s）
    const WIN: usize = 150;
    /// 判有声：RMS 超过噪声地板的倍数（与 trim_silence 同源）
    const RATIO: f32 = 3.0;
    /// 门限绝对下限：地板极低时防呼吸声/电噪声全程算有声
    const MIN_THRESH: f32 = 0.005;
    /// 连续有声帧数达到该值才算语音恢复（重置静音计时）
    const DEBOUNCE: u32 = 3;
    /// 连续有声帧数达到该值才认定「说过话」
    const CONFIRM: u32 = 5;
    /// 从未开口时的额外宽限秒数
    const GRACE_SECS: f32 = 2.0;

    /// `stop_secs` ≤0 表示禁用；`seed_floor` 为上会话学习的原始 RMS 本底
    pub fn new(stop_secs: f32, seed_floor: f32) -> Self {
        Self {
            stop_secs,
            seed_floor: seed_floor.clamp(0.0002, 0.05),
            ring: Vec::with_capacity(Self::WIN),
            pos: 0,
            voiced_run: 0,
            heard_speech: false,
            silent_since: None,
            done: false,
        }
    }

    /// 当前噪声地板（原始 RMS 域，供持久化回写）
    pub fn floor(&self) -> f32 {
        let win_min = self
            .ring
            .iter()
            .fold(f32::INFINITY, |a, &b| if b < a { b } else { a });
        let f = if self.ring.len() < Self::WIN {
            win_min.min(self.seed_floor)
        } else {
            win_min
        };
        f.clamp(0.0002, 0.05)
    }

    /// 喂入一帧原始 RMS 与会话内时刻（秒）。
    /// 静音达限的那一帧返回 true（之后恒 false，触发后自锁）。
    pub fn update(&mut self, rms: f32, t: f32) -> bool {
        if self.stop_secs <= 0.0 || self.done {
            return false;
        }
        if self.ring.len() < Self::WIN {
            self.ring.push(rms);
        } else {
            self.ring[self.pos] = rms;
        }
        self.pos = (self.pos + 1) % Self::WIN;

        let thresh = (self.floor() * Self::RATIO).max(Self::MIN_THRESH);
        if rms > thresh {
            self.voiced_run += 1;
            if self.voiced_run >= Self::DEBOUNCE {
                self.silent_since = None;
            }
            if self.voiced_run >= Self::CONFIRM {
                self.heard_speech = true;
            }
        } else {
            self.voiced_run = 0;
            let since = *self.silent_since.get_or_insert(t);
            // 说完后静音 stop_secs 秒自动结束；从未开口则多给宽限后同样结束
            let limit = if self.heard_speech {
                self.stop_secs
            } else {
                self.stop_secs + Self::GRACE_SECS
            };
            if t - since >= limit {
                self.done = true;
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 确定性测试波形：300Hz 正弦
    fn sine(n: usize, sr: u32) -> Vec<f32> {
        (0..n)
            .map(|i| (i as f32 / sr as f32 * 300.0 * std::f32::consts::TAU).sin() * 0.5)
            .collect()
    }

    /// 流式（不规则小批次）与整段一次性重采样必须输出一致
    #[test]
    fn stream_matches_batch_resample() {
        let sr = 48000u32;
        let mono = sine(sr as usize + 517, sr); // 刻意非整块长度
        let batch = to_16k(&mono, sr);

        let mut st = StreamResampler::new(sr);
        let steps = [960usize, 941, 1024, 313, 2048, 7];
        let mut pos = 0usize;
        let mut i = 0usize;
        while pos < mono.len() {
            let end = (pos + steps[i % steps.len()]).min(mono.len());
            st.push(&mono[pos..end]);
            pos = end;
            i += 1;
        }
        let streamed = st.finish();

        assert_eq!(batch.len(), streamed.len());
        for (a, b) in batch.iter().zip(&streamed) {
            assert!((a - b).abs() < 1e-6);
        }
    }

    #[test]
    fn passthrough_at_16k() {
        let mono = sine(16000, TARGET_SR);
        assert_eq!(to_16k(&mono, TARGET_SR), mono);
    }

    #[test]
    fn empty_input_resamples_to_empty() {
        assert!(to_16k(&[], 48000).is_empty());
        assert!(to_16k(&[], TARGET_SR).is_empty());
    }

    #[test]
    fn trim_keeps_speech_with_pad() {
        let sr = TARGET_SR as usize;
        let mut s = vec![0.0f32; sr]; // 1s 静音
        s.extend(sine(sr / 2, TARGET_SR)); // 0.5s 语音
        s.extend(vec![0.0f32; sr]); // 1s 静音
        let trimmed = trim_silence(s);
        let secs = trimmed.len() as f32 / TARGET_SR as f32;
        // 0.5s 语音 + 前后各 ≤160ms pad
        assert!(secs > 0.5 && secs < 0.9, "裁剪后时长异常：{secs}");
    }

    #[test]
    fn trim_all_silence_returns_empty() {
        assert!(trim_silence(vec![0.0; 16000]).is_empty());
    }

    #[test]
    fn trim_handles_nan_without_panic() {
        let mut s = sine(16000, TARGET_SR);
        s[8000] = f32::NAN;
        let _ = trim_silence(s);
    }

    #[test]
    fn recent_recordings_are_lossless_and_pruned() {
        let unique = RECORDING_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "blurt-recording-test-{}-{unique}",
            std::process::id()
        ));
        let samples = vec![-0.75f32, -0.25, 0.0, 0.25, 0.75];
        let mut paths = Vec::new();

        for _ in 0..6 {
            paths.push(save_recent_recording(&samples, &dir, 5).expect("保存诊断录音"));
        }

        let wavs: Vec<_> = fs::read_dir(&dir)
            .expect("读取测试目录")
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|s| s.to_str()) == Some("wav"))
            .collect();
        assert_eq!(wavs.len(), 5);
        assert!(!paths[0].exists());
        assert!(paths[5].exists());

        let mut reader = hound::WavReader::open(&paths[5]).expect("打开诊断 WAV");
        let spec = reader.spec();
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.sample_rate, TARGET_SR);
        assert_eq!(spec.bits_per_sample, 32);
        assert_eq!(spec.sample_format, hound::SampleFormat::Float);
        let decoded: Vec<f32> = reader
            .samples::<f32>()
            .collect::<Result<_, _>>()
            .expect("读取诊断 WAV 样本");
        assert_eq!(decoded, samples);

        fs::remove_dir_all(dir).expect("清理测试目录");
    }

    /* ---------------- SilenceGate ---------------- */

    /// 以 50Hz 节奏喂入分段 RMS 序列，返回触发时刻（秒）
    fn run_gate(gate: &mut SilenceGate, segments: &[(f32, f32)]) -> Option<f32> {
        let mut t = 0.0f32;
        for &(rms, secs) in segments {
            let frames = (secs * 50.0) as usize;
            for _ in 0..frames {
                if gate.update(rms, t) {
                    return Some(t);
                }
                t += 0.02;
            }
        }
        None
    }

    #[test]
    fn gate_disabled_never_fires() {
        let mut g = SilenceGate::new(0.0, 0.002);
        assert_eq!(run_gate(&mut g, &[(0.001, 30.0)]), None);
    }

    #[test]
    fn gate_noise_only_fires_after_grace() {
        let mut g = SilenceGate::new(2.0, 0.002);
        let t = run_gate(&mut g, &[(0.002, 10.0)]).expect("应在宽限后触发");
        assert!(
            (3.9..4.2).contains(&t),
            "纯静音应在 stop+2s≈4s 触发，实际 {t}"
        );
    }

    #[test]
    fn gate_speech_then_silence_fires_after_stop_secs() {
        let mut g = SilenceGate::new(2.0, 0.002);
        let t = run_gate(&mut g, &[(0.002, 1.0), (0.08, 2.0), (0.002, 10.0)])
            .expect("说话后静音应触发");
        // 语音结束于 3.0s，其后 2s 触发
        assert!(
            (4.9..5.2).contains(&t),
            "应在语音结束 2s 后≈5s 触发，实际 {t}"
        );
    }

    #[test]
    fn gate_fluctuating_ambient_still_fires() {
        // 用户实际故障场景：底噪在 0.010-0.022 波动（映射域加性门限会被反复重置），
        // 且种子过时偏低。乘性门限下窗满后 3×0.010=0.03 > 0.022，全部判静音。
        let mut g = SilenceGate::new(2.0, 0.001);
        let noisy: Vec<(f32, f32)> = (0..300)
            .map(|i| {
                let r = match i % 5 {
                    0 => 0.010,
                    1 => 0.018,
                    2 => 0.012,
                    3 => 0.022,
                    _ => 0.015,
                };
                (r, 0.02)
            })
            .collect();
        let t = run_gate(&mut g, &noisy).expect("波动底噪应在窗满+计时后触发");
        assert!(t < 5.5, "波动底噪应在 ≈窗满(3s)+2s 内触发，实际 {t}");
    }

    #[test]
    fn gate_single_spike_does_not_reset_countdown() {
        let mut g = SilenceGate::new(2.0, 0.002);
        // 语音结束于 2.0s；3.0s 处一帧 0.5 毛刺
        let t = run_gate(
            &mut g,
            &[
                (0.002, 1.0),
                (0.08, 1.0),
                (0.002, 1.0),
                (0.5, 0.02),
                (0.002, 10.0),
            ],
        )
        .expect("毛刺不应阻止触发");
        assert!(t < 4.3, "单帧毛刺不应重置倒计时（应≈4s 触发），实际 {t}");
    }

    #[test]
    fn gate_resumed_speech_resets_countdown() {
        let mut g = SilenceGate::new(2.0, 0.002);
        // 第一段语音后仅停 1s（< stop_secs）又继续说 0.5s，结束于 3.5s
        let t = run_gate(
            &mut g,
            &[
                (0.002, 1.0),
                (0.08, 1.0),
                (0.002, 1.0),
                (0.08, 0.5),
                (0.002, 10.0),
            ],
        )
        .expect("第二段语音后应触发");
        assert!(
            (5.4..5.7).contains(&t),
            "应在第二段语音结束 2s 后≈5.5s 触发，实际 {t}"
        );
    }

    #[test]
    fn gate_fires_only_once() {
        let mut g = SilenceGate::new(2.0, 0.002);
        assert!(run_gate(&mut g, &[(0.002, 10.0)]).is_some());
        assert_eq!(run_gate(&mut g, &[(0.002, 10.0)]), None, "触发后应自锁");
    }

    #[test]
    fn perceptual_map_roundtrip() {
        for rms in [0.0005f32, 0.002, 0.01, 0.05] {
            let back = perceptual_to_rms(perceptual_level(rms));
            assert!((back - rms).abs() / rms < 1e-3, "{rms} -> {back}");
        }
    }

    #[test]
    fn benchmark_real_recordings_performance() {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use std::io::Write;
        use std::time::Instant;

        let rec_dir = dirs::config_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("Blurt")
            .join("logs")
            .join("recordings");

        if !rec_dir.exists() {
            println!("No recordings directory found at {:?}", rec_dir);
            return;
        }

        let mut entries: Vec<_> = fs::read_dir(&rec_dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("wav"))
            .collect();
        entries.sort();

        if entries.is_empty() {
            println!("No WAV recordings found in {:?}", rec_dir);
            return;
        }

        println!("\n==========================================================================================");
        println!("                         BLURT 真实长录音性能分析报告");
        println!("==========================================================================================");
        println!(
            "找到 {} 条真实长录音文件，开始逐项性能压测分析...\n",
            entries.len()
        );

        for (idx, path) in entries.iter().enumerate() {
            let file_name = path.file_name().unwrap().to_string_lossy();
            let mut reader = hound::WavReader::open(path).expect("打开 WAV 录音");
            let spec = reader.spec();
            let samples: Vec<f32> = reader.samples::<f32>().map(|s| s.unwrap()).collect();
            let duration_s = samples.len() as f64 / spec.sample_rate as f64;

            println!("------------------------------------------------------------------------------------------");
            println!("[录音 #{}] {}", idx + 1, file_name);
            println!(
                "  规格: 采样率={}Hz, 声道={}, 时长={:.2}s, 样本数={}",
                spec.sample_rate,
                spec.channels,
                duration_s,
                samples.len()
            );

            // 1. 静音裁剪 (trim_silence)
            let mut trim_times = Vec::new();
            let mut trimmed_len = 0;
            for _ in 0..20 {
                let input = samples.clone();
                let t0 = Instant::now();
                let trimmed = trim_silence(input);
                trim_times.push(t0.elapsed().as_micros() as f64);
                trimmed_len = trimmed.len();
            }
            trim_times.sort_by(|a, b| a.total_cmp(b));
            let trim_median = trim_times[trim_times.len() / 2];
            let trimmed_s = trimmed_len as f64 / TARGET_SR as f64;
            println!("  [1. 静音裁剪 (trim_silence)]");
            println!(
                "      裁剪后有效时长: {:.2}s (裁减: {:.2}s)",
                trimmed_s,
                duration_s - trimmed_s
            );
            println!(
                "      耗时: 中位数 {:.2} μs ({:.3} ms), 速率: {:.1}x 实时",
                trim_median,
                trim_median / 1000.0,
                (duration_s * 1_000_000.0) / trim_median
            );

            // 2. 流式重采样 (StreamResampler: 48kHz -> 16kHz)
            // 构造 48kHz 模拟信号
            let upsampled_len = (samples.len() as f64 * (48000.0 / 16000.0)) as usize;
            let mut fake_48k = Vec::with_capacity(upsampled_len);
            for i in 0..upsampled_len {
                let orig_idx = (i as f64 / 3.0) as usize;
                fake_48k.push(samples[orig_idx.min(samples.len() - 1)]);
            }
            let chunk_48k = (48000.0 * 0.02) as usize; // 20ms @ 48k = 960 样本
            let mut resample_chunk_times = Vec::new();
            let mut rs = StreamResampler::new(48000);
            let t_rs_total = Instant::now();
            for chunk in fake_48k.chunks(chunk_48k) {
                let t0 = Instant::now();
                rs.push(chunk);
                resample_chunk_times.push(t0.elapsed().as_micros() as f64);
            }
            let t_rs_finish = Instant::now();
            let _rs_out = rs.finish();
            let rs_finish_us = t_rs_finish.elapsed().as_micros() as f64;
            let rs_total_us = t_rs_total.elapsed().as_micros() as f64;
            resample_chunk_times.sort_by(|a, b| a.total_cmp(b));
            let rs_chunk_p50 = resample_chunk_times[resample_chunk_times.len() / 2];
            let rs_chunk_p99 =
                resample_chunk_times[(resample_chunk_times.len() as f64 * 0.99) as usize];
            let rs_chunk_max = *resample_chunk_times.last().unwrap();
            println!("  [2. 流式重采样 (StreamResampler: 48kHz -> 16kHz, 20ms chunk)]");
            println!(
                "      20ms 分块处理耗时: p50={:.2} μs, p99={:.2} μs, max={:.2} μs",
                rs_chunk_p50, rs_chunk_p99, rs_chunk_max
            );
            println!(
                "      录音结束收尾 finish() 耗时: {:.2} μs ({:.3} ms)",
                rs_finish_us,
                rs_finish_us / 1000.0
            );
            println!(
                "      总耗时: {:.2} ms, 吞吐量: {:.1}x 实时",
                rs_total_us / 1000.0,
                (duration_s * 1_000_000.0) / rs_total_us
            );

            // 3. FSMN-VAD 神经端点流式推理 (SpeechEndpoint / vad-burn)
            let mut endpoint = match crate::endpoint::SpeechEndpoint::create(2.0) {
                Ok(ep) => ep,
                Err(e) => {
                    println!("  [3. FSMN-VAD 神经端点推理]: 初始化失败: {e}");
                    continue;
                }
            };
            let chunk_16k = (16000.0 * 0.02) as usize; // 20ms @ 16k = 320 样本
            let mut vad_chunk_times = Vec::new();
            let t_vad_total = Instant::now();
            for chunk in samples.chunks(chunk_16k) {
                let t0 = Instant::now();
                let _ = endpoint.update(chunk);
                vad_chunk_times.push(t0.elapsed().as_micros() as f64);
            }
            let vad_total_us = t_vad_total.elapsed().as_micros() as f64;
            vad_chunk_times.sort_by(|a, b| a.total_cmp(b));
            let vad_p50 = vad_chunk_times[vad_chunk_times.len() / 2];
            let vad_p95 = vad_chunk_times[(vad_chunk_times.len() as f64 * 0.95) as usize];
            let vad_p99 = vad_chunk_times[(vad_chunk_times.len() as f64 * 0.99) as usize];
            let vad_max = *vad_chunk_times.last().unwrap();
            let vad_mean = vad_chunk_times.iter().sum::<f64>() / vad_chunk_times.len() as f64;
            let vad_rtf = (vad_total_us / 1_000_000.0) / duration_s;
            println!("  [3. FSMN-VAD 神经端点流式推理 (vad-burn, 20ms chunk = 320 样本)]");
            println!("      20ms 单帧推理耗时: 均值={:.2} μs ({:.3} ms), p50={:.2} μs, p95={:.2} μs, p99={:.2} μs, max={:.2} μs", vad_mean, vad_mean / 1000.0, vad_p50, vad_p95, vad_p99, vad_max);
            println!(
                "      20ms 帧预算利用率: {:.2}% (单核占用)",
                (vad_mean / 20000.0) * 100.0
            );
            println!(
                "      总推理计算耗时: {:.2} ms, RTF (Real-Time Factor): {:.4} ({:.1}x 实时)",
                vad_total_us / 1000.0,
                vad_rtf,
                1.0 / vad_rtf
            );

            // 4. 豆包 WebSocket 音频打包与 Gzip 压缩 (doubao.rs)
            let chunk_200ms = 3200; // 200ms @ 16k
            let mut pcm_convert_times = Vec::new();
            let mut gzip_times = Vec::new();
            let mut total_raw_bytes = 0usize;
            let mut total_compressed_bytes = 0usize;
            let t_doubao_total = Instant::now();
            for chunk in samples.chunks(chunk_200ms) {
                let t_pcm0 = Instant::now();
                let pcm_i16: Vec<i16> = chunk
                    .iter()
                    .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0).round() as i16)
                    .collect();
                let mut pcm_bytes = Vec::with_capacity(pcm_i16.len() * 2);
                for s in &pcm_i16 {
                    pcm_bytes.extend_from_slice(&s.to_le_bytes());
                }
                pcm_convert_times.push(t_pcm0.elapsed().as_micros() as f64);
                total_raw_bytes += pcm_bytes.len();

                let t_gz0 = Instant::now();
                let mut encoder =
                    GzEncoder::new(Vec::with_capacity(pcm_bytes.len()), Compression::fast());
                encoder.write_all(&pcm_bytes).unwrap();
                let compressed = encoder.finish().unwrap();
                gzip_times.push(t_gz0.elapsed().as_micros() as f64);
                total_compressed_bytes += compressed.len();
            }
            let doubao_total_us = t_doubao_total.elapsed().as_micros() as f64;
            pcm_convert_times.sort_by(|a, b| a.total_cmp(b));
            gzip_times.sort_by(|a, b| a.total_cmp(b));
            let pcm_p50 = pcm_convert_times[pcm_convert_times.len() / 2];
            let gz_p50 = gzip_times[gzip_times.len() / 2];
            let gz_p99 = gzip_times[(gzip_times.len() as f64 * 0.99) as usize];
            let gz_max = *gzip_times.last().unwrap();
            let gz_mean = gzip_times.iter().sum::<f64>() / gzip_times.len() as f64;
            println!("  [4. 豆包 WebSocket 音频打包与 Gzip 压缩 (200ms chunk = 6400 bytes)]");
            println!("      PCM 格式转换耗时: p50={:.2} μs", pcm_p50);
            println!("      Gzip 压缩耗时 (200ms chunk): 均值={:.2} μs ({:.3} ms), p50={:.2} μs, p99={:.2} μs, max={:.2} μs", gz_mean, gz_mean / 1000.0, gz_p50, gz_p99, gz_max);
            println!(
                "      压缩效果: 原始 {} KB -> 压缩后 {} KB (压缩率: {:.1}%)",
                total_raw_bytes / 1024,
                total_compressed_bytes / 1024,
                (total_compressed_bytes as f64 / total_raw_bytes as f64) * 100.0
            );
            println!(
                "      总处理耗时: {:.2} ms, 吞吐量: {:.1}x 实时",
                doubao_total_us / 1000.0,
                (duration_s * 1_000_000.0) / doubao_total_us
            );

            // 5. 录音保存与磁盘 I/O (save_recent_recording)
            let temp_test_dir =
                std::env::temp_dir().join(format!("blurt_perf_bench_{}", std::process::id()));
            let t_save0 = Instant::now();
            let _ = save_recent_recording(&samples, &temp_test_dir, 5);
            let save_ms = t_save0.elapsed().as_secs_f64() * 1000.0;
            let _ = fs::remove_dir_all(&temp_test_dir);
            println!("  [5. 诊断录音持久化 (save_recent_recording 磁盘写入与轮转)]");
            println!(
                "      WAV 编码 + 写盘 + 目录扫描清理耗时: {:.2} ms",
                save_ms
            );

            println!();
        }
        println!("==========================================================================================\n");
    }
}
