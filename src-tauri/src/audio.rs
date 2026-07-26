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
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const TARGET_SR: u32 = 16000;

/// SincFixedIn 的固定输入块大小（源采样率下的样本数）
const RS_CHUNK: usize = 1024;

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

/// 启动录音线程。`on_level` 约 50Hz 回调电平 [0,1]；
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
            if let Err(e) = stream.play() {
                on_done(Err(format!("无法启动录音：{e}")));
                return;
            }

            // 流式重采样器：录音过程中增量处理，结束时只剩尾部冲洗
            let mut rs = StreamResampler::new(sr);
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
                            // 感知化映射：人声 RMS 动态范围压缩到 0..1
                            on_level((rms * 11.0).powf(0.65).min(1.0));
                            rs.push(&spare);
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
            on_done(Ok(rs.finish()));
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
}
