//! 录音管线：cpal 采集（任意采样率/声道）→ 单声道 f32 → 16kHz 重采样 → 静音裁剪
//! 采集线程实时回报 RMS 电平（驱动 HUD 声波条）。

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_channel::{bounded, Sender};
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const TARGET_SR: u32 = 16000;

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

pub fn list_input_devices() -> Vec<String> {
    let host = cpal::default_host();
    match host.input_devices() {
        Ok(devs) => devs.filter_map(|d| d.name().ok()).collect(),
        Err(_) => vec![],
    }
}

/// 启动录音线程。`on_level` 约 30Hz 回调电平 [0,1]；
/// `on_done` 仅在 Finish（含超时自动结束）时回调，交付 16kHz 单声道样本。
pub fn start_recording(
    device_name: Option<String>,
    max_secs: u64,
    mut on_level: impl FnMut(f32) + Send + 'static,
    on_done: impl FnOnce(Result<Vec<f32>, String>) + Send + 'static,
) -> Result<RecorderHandle, String> {
    let (stop_tx, stop_rx) = bounded::<StopMode>(2);

    std::thread::Builder::new()
        .name("blurt-recorder".into())
        .spawn(move || {
            let started = Instant::now();
            let host = cpal::default_host();
            let device = match &device_name {
                Some(name) => host
                    .input_devices()
                    .ok()
                    .and_then(|mut it| it.find(|d| d.name().map(|n| &n == name).unwrap_or(false)))
                    .or_else(|| host.default_input_device()),
                None => host.default_input_device(),
            };
            let Some(device) = device else {
                on_done(Err("未找到可用的麦克风设备".into()));
                return;
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

            // 采集缓冲（已混为单声道，原始采样率）
            let buf: Arc<Mutex<Vec<f32>>> =
                Arc::new(Mutex::new(Vec::with_capacity(sr as usize * 32)));
            let level_acc: Arc<Mutex<(f64, u64)>> = Arc::new(Mutex::new((0.0, 0))); // (平方和, 样本数)

            // 注意：Arc 必须以宏参数传入（宏体内的自由标识符按定义处解析，
            // 会捕获外层原始 Arc 而非闭包内的克隆）。
            macro_rules! push_frames {
                ($buf:expr, $acc:expr, $data:expr, $channels:expr, $to_f32:expr) => {{
                    let mut b = $buf.lock();
                    let mut acc = $acc.lock();
                    for frame in $data.chunks($channels) {
                        let mut s = 0.0f32;
                        for v in frame {
                            s += $to_f32(*v);
                        }
                        s /= $channels as f32;
                        b.push(s);
                        acc.0 += (s as f64) * (s as f64);
                        acc.1 += 1;
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
                    let a = level_acc.clone();
                    device.build_input_stream(
                        &config,
                        move |data: &[f32], _| {
                            push_frames!(b, a, data, channels, |v: f32| v);
                        },
                        err_cb,
                        None,
                    )
                }
                cpal::SampleFormat::I16 => {
                    let b = buf.clone();
                    let a = level_acc.clone();
                    device.build_input_stream(
                        &config,
                        move |data: &[i16], _| {
                            push_frames!(b, a, data, channels, |v: i16| v as f32 / 32768.0);
                        },
                        err_cb,
                        None,
                    )
                }
                cpal::SampleFormat::U16 => {
                    let b = buf.clone();
                    let a = level_acc.clone();
                    device.build_input_stream(
                        &config,
                        move |data: &[u16], _| {
                            push_frames!(b, a, data, channels, |v: u16| (v as f32 - 32768.0)
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
            if let Err(e) = stream.play() {
                on_done(Err(format!("无法启动录音：{e}")));
                return;
            }

            // 主循环：等停止信号，同时每 20ms 上报一次真实电平（驱动 HUD 波形滚动）
            let mode = loop {
                match stop_rx.recv_timeout(Duration::from_millis(20)) {
                    Ok(m) => break m,
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                        if let Some(e) = err_flag.lock().take() {
                            drop(stream);
                            on_done(Err(e));
                            return;
                        }
                        let (sq, n) = {
                            let mut acc = level_acc.lock();
                            let v = *acc;
                            *acc = (0.0, 0);
                            v
                        };
                        if n > 0 {
                            let rms = (sq / n as f64).sqrt() as f32;
                            // 感知化映射：人声 RMS 动态范围压缩到 0..1
                            let v = (rms * 11.0).powf(0.65).min(1.0);
                            on_level(v);
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

            let raw = std::mem::take(&mut *buf.lock());
            on_done(Ok(to_16k(&raw, sr)));
        })
        .map_err(|e| format!("无法创建录音线程：{e}"))?;

    Ok(RecorderHandle { stop_tx })
}

/// 读取任意 wav → 16kHz 单声道 f32（自检与测速共用）
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

/// 单声道任意采样率 → 16kHz（rubato sinc 重采样）
pub fn to_16k(mono: &[f32], sr: u32) -> Vec<f32> {
    if sr == TARGET_SR || mono.is_empty() {
        return mono.to_vec();
    }
    use rubato::{
        Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
    };
    let params = SincInterpolationParameters {
        sinc_len: 128,
        f_cutoff: 0.95,
        interpolation: SincInterpolationType::Linear,
        oversampling_factor: 128,
        window: WindowFunction::BlackmanHarris2,
    };
    let chunk = 1024usize;
    let mut rs = match SincFixedIn::<f32>::new(TARGET_SR as f64 / sr as f64, 2.0, params, chunk, 1)
    {
        Ok(r) => r,
        Err(_) => return linear_resample(mono, sr),
    };
    let mut out: Vec<f32> = Vec::with_capacity(mono.len() * TARGET_SR as usize / sr as usize + 16);
    let mut inbuf = vec![vec![0.0f32; chunk]];
    let mut pos = 0usize;
    while pos + chunk <= mono.len() {
        inbuf[0].copy_from_slice(&mono[pos..pos + chunk]);
        if let Ok(o) = rs.process(&inbuf, None) {
            out.extend_from_slice(&o[0]);
        }
        pos += chunk;
    }
    let rest = &mono[pos..];
    if !rest.is_empty() {
        let tail = vec![rest.to_vec()];
        if let Ok(o) = rs.process_partial(Some(&tail), None) {
            out.extend_from_slice(&o[0]);
        }
    }
    if let Ok(o) = rs.process_partial::<Vec<f32>>(None, None) {
        out.extend_from_slice(&o[0]);
    }
    out
}

/// 兜底线性重采样（仅在 rubato 初始化失败时使用）
fn linear_resample(mono: &[f32], sr: u32) -> Vec<f32> {
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

/// 裁掉首尾静音（保留 pad），全静音则返回空
pub fn trim_silence(samples: &[f32]) -> Vec<f32> {
    const FRAME: usize = 320; // 20ms @16k
    const PAD_FRAMES: usize = 8; // 160ms
    if samples.len() < FRAME * 4 {
        return vec![];
    }
    let energies: Vec<f32> = samples
        .chunks(FRAME)
        .map(|c| (c.iter().map(|s| s * s).sum::<f32>() / c.len() as f32).sqrt())
        .collect();
    let mut sorted = energies.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let floor = sorted[sorted.len() / 10]; // 10 分位当噪声地板
    let thresh = (floor * 3.0).max(0.004);
    let first = energies.iter().position(|&e| e > thresh);
    let last = energies.iter().rposition(|&e| e > thresh);
    match (first, last) {
        (Some(f), Some(l)) if l >= f => {
            let s = f.saturating_sub(PAD_FRAMES) * FRAME;
            let e = ((l + 1 + PAD_FRAMES) * FRAME).min(samples.len());
            samples[s..e].to_vec()
        }
        _ => vec![],
    }
}
