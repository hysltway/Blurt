//! 会话编排：热键 → 录音 → 识别 → 注入 的状态机。
//!
//! 交互模型（双模式）：
//!   按住 ≥350ms：按住说话，松开即识别（对讲机式）
//!   轻点 <350ms：切换模式，再点一下或超时才结束
//!   录音/识别中 Esc：取消
//!
//! `gen` 是会话代号：任何异步完成（录音、识别、HUD 定时隐藏）都要先核对 gen，
//! 取消/新会话会使旧代号作废，从根上杜绝“迟到的结果注入错窗口”。

use parking_lot::{Mutex, RwLock};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tauri::{AppHandle, Emitter, Manager};

use crate::audio::{RecorderHandle, StopMode};
use crate::config::{Config, Stats};
use crate::media_volume::PlaybackDucker;
use crate::{audio, config, doubao, hotkey, hud, inject, tray};

pub const TARGET_SR_F: f32 = audio::TARGET_SR as f32;

const TAP_MS: u128 = 350;
/// 有效语音的最短时长（裁剪后）
const MIN_SPEECH_S: f32 = 0.35;
/// 仅保留足以触发长音频问题的真实录音，避免无界增长与无意义的隐私留存。
const LONG_RECORDING_MIN_S: f32 = 20.0;
const LONG_RECORDING_KEEP: usize = 5;

pub enum Session {
    Idle,
    Recording {
        gen: u64,
        rec: RecorderHandle,
        t0: Instant,
        /// 轻点进入的切换模式（等第二次按下才结束）
        toggle_mode: bool,
        /// 按下后尚未见到松开事件（吸收键盘自动重复的 Pressed）
        awaiting_release: bool,
    },
    Processing {
        gen: u64,
    },
}

pub struct AppState {
    pub config: RwLock<Config>,
    pub hotkey_capture: AtomicBool,
    pub session: Mutex<Session>,
    pub gen: AtomicU64,
    pub stats: Mutex<Stats>,
    pub playback_ducker: PlaybackDucker,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        Self {
            config: RwLock::new(config),
            hotkey_capture: AtomicBool::new(false),
            session: Mutex::new(Session::Idle),
            gen: AtomicU64::new(0),
            stats: Mutex::new(config::load_stats()),
            playback_ducker: PlaybackDucker::new(),
        }
    }
}

/* ---------------- 豆包 API 状态 ---------------- */

pub fn engine_status_dto(_app: &AppHandle) -> serde_json::Value {
    let (state, detail) = match config::load_doubao_api_key() {
        Ok(Some(_)) => ("ready", "豆包流式语音识别 1.0".to_string()),
        Ok(None) => ("missing", "请先保存 API Key".to_string()),
        Err(error) => ("failed", format!("读取 API Key 失败：{error}")),
    };
    serde_json::json!({
        "state": state,
        "detail": detail,
        "provider": "doubao",
    })
}

pub(crate) fn emit_engine_status(app: &AppHandle) {
    let _ = app.emit("engine:status", engine_status_dto(app));
}

/// 刷新凭据状态；首次启动缺少 API Key 时打开设置页。
pub fn refresh_api_status(app: &AppHandle, open_settings_if_missing: bool) {
    let configured = config::load_doubao_api_key().ok().flatten().is_some();
    emit_engine_status(app);
    if configured {
        tray::set_tooltip(app, "Blurt · 就绪（豆包 API）");
    } else {
        tray::set_tooltip(app, "Blurt · 请在设置中配置豆包 API Key");
        if open_settings_if_missing {
            tray::open_settings(app);
        }
    }
}

/* ---------------- 热键事件 ---------------- */

enum PressAction {
    Start,
    Stop,
    Flash(&'static str, u64, bool), // (HUD 状态, 显示时长, 是否顺带打开设置)
    Nothing,
}

pub fn hotkey_pressed(app: &AppHandle) {
    let state = app.state::<AppState>();
    let mut session = state.session.lock();

    let act = match &mut *session {
        Session::Idle => match config::load_doubao_api_key() {
            Ok(Some(_)) => PressAction::Start,
            _ => PressAction::Flash("error", 1200, true),
        },
        Session::Recording {
            awaiting_release, ..
        } => {
            if *awaiting_release {
                PressAction::Nothing // 键盘自动重复
            } else {
                // 切换模式的第二次按下 → 结束并识别
                *awaiting_release = true;
                PressAction::Stop
            }
        }
        Session::Processing { .. } => PressAction::Nothing,
    };

    match act {
        PressAction::Start => start_recording(app, &mut session),
        PressAction::Stop => stop_and_recognize(app, &mut session),
        PressAction::Flash(kind, ms, open_settings) => {
            drop(session);
            let gen = state.gen.load(Ordering::SeqCst);
            hud::position_on_active_monitor(app);
            hud::show(app);
            hud::emit_state(app, kind, None);
            hud::hide_later(app, ms, gen);
            if open_settings {
                tray::open_settings(app);
            }
        }
        PressAction::Nothing => {}
    }
}

pub fn hotkey_released(app: &AppHandle, expected_gen: Option<u64>) {
    let state = app.state::<AppState>();
    let mut session = state.session.lock();

    let stop = match &mut *session {
        Session::Recording {
            gen,
            t0,
            toggle_mode,
            awaiting_release,
            ..
        } => {
            if expected_gen.map_or(false, |g| g != *gen) {
                return;
            }
            if !*awaiting_release {
                return; // 已处理过（插件事件与轮询兜底可能都来一次）
            }
            *awaiting_release = false;
            if t0.elapsed().as_millis() >= TAP_MS {
                true // 按住说话：松开 → 识别
            } else {
                *toggle_mode = true; // 轻点：进入切换模式，继续录音
                false
            }
        }
        _ => return,
    };

    if stop {
        stop_and_recognize(app, &mut session);
    }
}

/// 已激活的快捷键又按下其他键时，用户实际是在执行更长的组合键。仅当这次按住直接开启的录音尚未松开时静默取消，避免把快捷键操作
/// 误录成语音；轻点进入的切换模式与识别阶段不受影响。
pub fn chord_broken(app: &AppHandle) {
    let state = app.state::<AppState>();
    let cancel = matches!(
        &*state.session.lock(),
        Session::Recording {
            awaiting_release: true,
            ..
        }
    );
    if cancel {
        esc_pressed(app);
    }
}

pub fn esc_pressed(app: &AppHandle) {
    let state = app.state::<AppState>();
    let mut session = state.session.lock();
    match std::mem::replace(&mut *session, Session::Idle) {
        Session::Recording { rec, .. } => {
            state.gen.fetch_add(1, Ordering::SeqCst);
            rec.stop(StopMode::Abort);
            drop(session);
            finish_ui_cancel(app);
        }
        Session::Processing { .. } => {
            state.gen.fetch_add(1, Ordering::SeqCst);
            drop(session);
            finish_ui_cancel(app);
        }
        Session::Idle => {}
    }
}

fn finish_ui_cancel(app: &AppHandle) {
    let state = app.state::<AppState>();
    state.playback_ducker.restore();
    let gen = state.gen.load(Ordering::SeqCst);
    hud::emit_state(app, "cancel", None);
    hud::hide_later(app, 220, gen);
    tray::set_tooltip(app, ready_tooltip(app));
}

fn ready_tooltip(_app: &AppHandle) -> &'static str {
    "Blurt · 就绪（豆包 API）"
}

/* ---------------- 会话流转 ---------------- */

/// 开始录音（调用方持有 session 锁，杜绝并发按键竞态）
fn start_recording(app: &AppHandle, session: &mut Session) {
    let state = app.state::<AppState>();
    let gen = state.gen.fetch_add(1, Ordering::SeqCst) + 1;
    let cfg = state.config.read().clone();
    let api_stream = match config::load_doubao_api_key() {
        Ok(Some(api_key)) => doubao::Stream::start(api_key, cfg.hotwords.clone()),
        Ok(None) => {
            tracing::error!("豆包 API Key 未配置");
            hud::emit_state(app, "error", None);
            hud::hide_later(app, 1200, gen);
            tray::open_settings(app);
            return;
        }
        Err(e) => {
            tracing::error!("读取豆包 API Key 失败：{e:#}");
            hud::emit_state(app, "error", None);
            hud::hide_later(app, 1200, gen);
            return;
        }
    };
    let api_sender = api_stream.audio_sender();

    // HUD 必须立刻出现 —— “它听到我了”
    hud::position_on_active_monitor(app);
    hud::show(app);
    hud::emit_state(app, "listen", None);
    tray::set_tooltip(app, "Blurt · 正在聆听…（Esc 取消）");

    // 自动停止由 16kHz 流上的 FSMN-VAD 驱动，RMS 只负责 HUD 与噪声统计。
    // 神经 VAD 无法初始化时保留原 RMS 门作为兜底；两条路径都只在切换模式结束会话。
    let auto_stop = cfg.auto_stop_secs.clamp(0.0, 10.0);
    let level_app = app.clone();
    let sample_app = app.clone();
    let done_app = app.clone();
    let seed = audio::perceptual_to_rms(state.stats.lock().noise_floor.clamp(0.02, 0.4));
    let mut gate = audio::SilenceGate::new(auto_stop, seed);
    let use_rms_fallback = Arc::new(AtomicBool::new(false));
    let level_rms_fallback = Arc::clone(&use_rms_fallback);
    let sample_rms_fallback = Arc::clone(&use_rms_fallback);
    let vad_t0 = Instant::now();
    let mut vad_tick = 0u32;
    let rec = audio::start_recording(
        cfg.mic_device.clone(),
        cfg.max_record_secs,
        move |rms| {
            hud::emit_level(&level_app, audio::perceptual_level(rms));
            let rms_stop = gate.update(rms, vad_t0.elapsed().as_secs_f32());
            if level_rms_fallback.load(Ordering::Relaxed) && rms_stop {
                auto_stop_session(&level_app, gen);
            }
            vad_tick += 1;
            if auto_stop > 0.0 && vad_tick % 25 == 0 {
                let mapped = audio::perceptual_level(gate.floor()).clamp(0.02, 0.4);
                level_app.state::<AppState>().stats.lock().noise_floor = mapped;
            }
        },
        move || {
            let mut endpoint = if auto_stop > 0.0 {
                match crate::endpoint::SpeechEndpoint::create(auto_stop) {
                    Ok(endpoint) => Some(endpoint),
                    Err(error) => {
                        tracing::error!("FSMN-VAD 不可用，自动暂停降级为 RMS 噪声门：{error:#}");
                        sample_rms_fallback.store(true, Ordering::Relaxed);
                        None
                    }
                }
            } else {
                None
            };
            move |samples: &[f32]| {
                match endpoint.as_mut().map(|endpoint| endpoint.update(samples)) {
                    Some(Ok(true)) => auto_stop_session(&sample_app, gen),
                    Some(Err(error)) => {
                        tracing::error!("FSMN-VAD 运行失败，自动暂停降级为 RMS 噪声门：{error:#}");
                        endpoint = None;
                        sample_rms_fallback.store(true, Ordering::Relaxed);
                    }
                    _ => {}
                }
                api_sender.push(samples);
            }
        },
        move |res| on_audio_ready(&done_app, gen, res, api_stream),
    );

    match rec {
        Ok(handle) => {
            *session = Session::Recording {
                gen,
                rec: handle,
                t0: Instant::now(),
                toggle_mode: false,
                awaiting_release: true,
            };
            state.playback_ducker.duck();
            // 兜底监视快捷键松开 + 会话期 Esc 取消 + HUD 悬停出 ✕ 按钮
            hotkey::spawn_release_watcher(app, gen, cfg.hotkey);
            hotkey::spawn_esc_watcher(app, gen);
            hud::spawn_hover_watcher(app, gen);
        }
        Err(e) => {
            tracing::error!("录音启动失败：{e}");
            hud::emit_state(app, "error", None);
            hud::hide_later(app, 1000, gen);
            tray::set_tooltip(app, "Blurt · 麦克风不可用");
        }
    }
}

/// 语音端点触发的自动结束（录音线程回调里调用）。
/// 仅切换模式生效：按住说话时松开按键就是明确的结束动作，不做自动打断。
fn auto_stop_session(app: &AppHandle, gen: u64) {
    let state = app.state::<AppState>();
    let mut session = state.session.lock();
    match &*session {
        Session::Recording {
            gen: g,
            toggle_mode: true,
            ..
        } if *g == gen => {
            tracing::info!("检测到用户说完，自动结束录音进入识别");
            stop_and_recognize(app, &mut session);
        }
        _ => {}
    }
}

/// 结束录音进入识别（调用方持有 session 锁）
fn stop_and_recognize(app: &AppHandle, session: &mut Session) {
    let old = std::mem::replace(session, Session::Idle);
    if let Session::Recording { gen, rec, .. } = old {
        *session = Session::Processing { gen };
        rec.stop(StopMode::Finish);
        app.state::<AppState>().playback_ducker.restore();
        tray::set_tooltip(app, "Blurt · 正在识别…");
        // HUD 保持 listen，等 on_audio_ready 携带音频时长后切 process（含预计耗时）
    } else {
        *session = old;
    }
}

/// 录音线程交付音频（Finish 或超时自动结束时）
pub fn on_audio_ready(
    app: &AppHandle,
    gen: u64,
    res: Result<Vec<f32>, String>,
    api_stream: doubao::Stream,
) {
    let state = app.state::<AppState>();
    {
        let mut session = state.session.lock();
        match &*session {
            // 超时自动结束时仍处于 Recording
            Session::Recording { gen: g, .. } if *g == gen => {
                *session = Session::Processing { gen };
            }
            Session::Processing { gen: g } if *g == gen => {}
            _ => return, // 已取消/换代
        }
    }
    state.playback_ducker.restore();

    let samples = match res {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("录音失败：{e}");
            hud::emit_state(app, "error", None);
            finish_session(app, gen, 1000, ready_tooltip(app));
            return;
        }
    };

    let raw_s = samples.len() as f32 / TARGET_SR_F;
    // 保存静音裁剪前的真实采集音频，便于复现长音频问题。
    // 仅长录音需要克隆；几 MB 的短期内存换取无损、可复现的诊断样本。
    let retained_recording = (raw_s >= LONG_RECORDING_MIN_S).then(|| samples.clone());
    let speech = audio::trim_silence(samples);
    let speech_s = speech.len() as f32 / TARGET_SR_F;
    let retained_recording = retained_recording.filter(|_| speech_s >= LONG_RECORDING_MIN_S);
    if speech_s < MIN_SPEECH_S {
        // 没听到有效语音：灰色塌陷提示，而非报错
        hud::emit_state(app, "nothing", None);
        finish_session(app, gen, 900, ready_tooltip(app));
        return;
    }

    hud::emit_state(app, "process", Some(1600));

    let app = app.clone();
    std::thread::spawn(move || {
        let result = api_stream.finish();
        let state = app.state::<AppState>();

        // 识别期间可能被 Esc 取消
        if state.gen.load(Ordering::SeqCst) != gen {
            return;
        }

        match result {
            Ok((text, elapsed)) => {
                let stats_now = {
                    let mut stats = state.stats.lock();
                    if !text.is_empty() {
                        stats.record_usage(speech_s, &text);
                    }
                    stats.clone()
                };
                tracing::info!(
                    "豆包识别完成 {:.2}s（音频 {:.2}s）：{}",
                    elapsed,
                    speech_s,
                    text
                );

                if text.is_empty() {
                    hud::emit_state(&app, "nothing", None);
                    finish_session(&app, gen, 900, ready_tooltip(&app));
                } else {
                    let cfg = state.config.read().clone();
                    match inject::inject(&text, &cfg.inject_mode, cfg.type_threshold) {
                        Ok(()) => {
                            hud::emit_state(&app, "success", None);
                            finish_session(&app, gen, 650, ready_tooltip(&app));
                        }
                        Err(e) => {
                            tracing::error!("注入失败：{e}（已复制到剪贴板兜底）");
                            // 兜底：至少把文本放进剪贴板
                            if let Ok(mut cb) = arboard::Clipboard::new() {
                                let _ = cb.set_text(text);
                            }
                            hud::emit_state(&app, "error", None);
                            finish_session(&app, gen, 1100, "Blurt · 注入失败，文本已在剪贴板");
                        }
                    }
                }

                config::save_stats(&stats_now);
                let _ = app.emit("usage:updated", &stats_now);
                emit_engine_status(&app);
            }
            Err(e) => {
                tracing::error!("识别失败：{e:#}");
                hud::emit_state(&app, "error", None);
                finish_session(&app, gen, 1100, "Blurt · 识别失败");
            }
        }

        // 放在识别结果注入之后，写盘和旧文件清理不会增加用户等待时间。
        if let Some(recording) = retained_recording {
            let dir = config::logs_dir().join("recordings");
            match audio::save_recent_recording(&recording, &dir, LONG_RECORDING_KEEP) {
                Ok(path) => tracing::info!(
                    "已保留长录音样本（原始 {:.2}s，有效 {:.2}s）：{}",
                    raw_s,
                    speech_s,
                    path.display()
                ),
                Err(e) => tracing::warn!("保留长录音样本失败：{e}"),
            }
        }
    });
}

/// 会话收尾：回 Idle、延时隐藏 HUD、恢复托盘提示（Esc 看门线程见 Idle 自行退出）
fn finish_session(app: &AppHandle, gen: u64, hide_delay_ms: u64, tooltip: &str) {
    let state = app.state::<AppState>();
    state.playback_ducker.restore();
    {
        let mut session = state.session.lock();
        if let Session::Processing { gen: g } = &*session {
            if *g == gen {
                *session = Session::Idle;
            }
        }
    }
    hud::hide_later(app, hide_delay_ms, gen);
    tray::set_tooltip(app, tooltip);
}
