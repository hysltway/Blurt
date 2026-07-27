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
use std::time::Instant;
use tauri::{AppHandle, Manager};

use crate::asr::EngineSlot;
use crate::audio::{RecorderHandle, StopMode};
use crate::config::{Config, Stats};
use crate::{asr, audio, config, hotkey, hud, inject, tray};

pub const TARGET_SR_F: f32 = audio::TARGET_SR as f32;

const TAP_MS: u128 = 350;
/// 有效语音的最短时长（裁剪后）
const MIN_SPEECH_S: f32 = 0.35;

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
    pub engine: RwLock<EngineSlot>,
    pub session: Mutex<Session>,
    pub gen: AtomicU64,
    pub stats: Mutex<Stats>,
    pub benching: AtomicBool,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        Self {
            config: RwLock::new(config),
            engine: RwLock::new(EngineSlot::Loading),
            session: Mutex::new(Session::Idle),
            gen: AtomicU64::new(0),
            stats: Mutex::new(config::load_stats()),
            benching: AtomicBool::new(false),
        }
    }
}

/* ---------------- 引擎加载 ---------------- */

pub fn engine_status_dto(app: &AppHandle) -> serde_json::Value {
    let state = app.state::<AppState>();
    let stats = *state.stats.lock();
    let (st, detail, dir) = match &*state.engine.read() {
        EngineSlot::Loading => ("loading", String::new(), String::new()),
        EngineSlot::Ready(e) => ("ready", String::new(), e.model_dir.display().to_string()),
        EngineSlot::Missing(d) => ("missing", String::new(), d.clone()),
        EngineSlot::Failed(msg) => ("failed", msg.clone(), String::new()),
    };
    serde_json::json!({
        "state": st,
        "detail": detail,
        "model_dir": dir,
        "rtf": stats.rtf_ema,
        "last_ms": stats.last_ms,
    })
}

fn emit_engine_status(app: &AppHandle) {
    use tauri::Emitter;
    let _ = app.emit("engine:status", engine_status_dto(app));
}

/// 后台加载（或重载）引擎。`open_settings_if_missing`：首次启动缺模型时弹设置页引导。
pub fn spawn_engine_load(app: &AppHandle, open_settings_if_missing: bool) {
    let state = app.state::<AppState>();
    *state.engine.write() = EngineSlot::Loading;
    emit_engine_status(app);
    tray::set_tooltip(app, "Blurt · 模型加载中…");

    let app = app.clone();
    std::thread::spawn(move || {
        let state = app.state::<AppState>();
        let cfg = state.config.read().clone();
        let Some(dir) = config::resolve_model_dir(&cfg) else {
            let expect = config::models_root().display().to_string();
            *state.engine.write() = EngineSlot::Missing(expect);
            emit_engine_status(&app);
            tray::set_tooltip(&app, "Blurt · 未找到模型，请打开设置查看指引");
            if open_settings_if_missing {
                tray::open_settings(&app);
            }
            return;
        };
        tracing::info!("加载模型：{}", dir.display());
        let t0 = Instant::now();
        match asr::AsrEngine::load(&dir, cfg.num_threads, &cfg.hotwords) {
            Ok(engine) => {
                let engine = std::sync::Arc::new(engine);
                // 先发布 Ready 再热身：用户立刻可以按键说话；若首次识别抢在
                // 热身完成前到来，只是在引擎锁上排队，总耗时不变。
                *state.engine.write() = EngineSlot::Ready(engine.clone());
                emit_engine_status(&app);
                tray::set_tooltip(&app, "Blurt · 就绪（按住 Ctrl+Alt 说话）");
                tracing::info!(
                    "模型就绪，耗时 {:.1}s（后台热身中）",
                    t0.elapsed().as_secs_f64()
                );
                engine.warmup();
            }
            Err(e) => {
                tracing::error!("模型加载失败：{e:#}");
                *state.engine.write() = EngineSlot::Failed(format!("{e:#}"));
                emit_engine_status(&app);
                tray::set_tooltip(&app, "Blurt · 模型加载失败");
            }
        }
    });
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
        Session::Idle => match &*state.engine.read() {
            EngineSlot::Ready(_) => PressAction::Start,
            EngineSlot::Loading => PressAction::Flash("loading", 1400, false),
            EngineSlot::Missing(_) | EngineSlot::Failed(_) => {
                PressAction::Flash("error", 1000, true)
            }
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

/// 按住 Ctrl+Alt 期间又按了第三个键：用户实际是在按别的快捷键（如 Ctrl+Alt+A
/// 截图）。仅当这次按住直接开启的录音尚未松开时静默取消，避免把快捷键操作
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
    let gen = state.gen.load(Ordering::SeqCst);
    hud::emit_state(app, "cancel", None);
    hud::hide_later(app, 220, gen);
    tray::set_tooltip(app, "Blurt · 就绪（按住 Ctrl+Alt 说话）");
}

/* ---------------- 会话流转 ---------------- */

/// 开始录音（调用方持有 session 锁，杜绝并发按键竞态）
fn start_recording(app: &AppHandle, session: &mut Session) {
    let state = app.state::<AppState>();
    let gen = state.gen.fetch_add(1, Ordering::SeqCst) + 1;
    let cfg = state.config.read().clone();

    // HUD 必须立刻出现 —— “它听到我了”
    hud::position_on_active_monitor(app);
    hud::show(app);
    hud::emit_state(app, "listen", None);
    tray::set_tooltip(app, "Blurt · 正在聆听…（Esc 取消）");

    // 静音端点检测（自动停止）：原始 RMS 域乘性噪声门（audio::SilenceGate），
    // 纯环境噪音不算有声；仅对切换模式生效（见 auto_stop_session）。
    // 本底种子来自上次学习结果（感知域持久化，换麦克风时 set_config 已重置），
    // 学习成果每 0.5s 回写，随下次识别后的 save_stats 一并落盘。
    let auto_stop = cfg.auto_stop_secs.clamp(0.0, 10.0);
    let level_app = app.clone();
    let done_app = app.clone();
    let seed = audio::perceptual_to_rms(state.stats.lock().noise_floor.clamp(0.02, 0.4));
    let mut gate = audio::SilenceGate::new(auto_stop, seed);
    let vad_t0 = Instant::now();
    let mut vad_tick = 0u32;
    let rec = audio::start_recording(
        cfg.mic_device.clone(),
        cfg.max_record_secs,
        move |rms| {
            hud::emit_level(&level_app, audio::perceptual_level(rms));
            if gate.update(rms, vad_t0.elapsed().as_secs_f32()) {
                auto_stop_session(&level_app, gen);
            }
            vad_tick += 1;
            if auto_stop > 0.0 && vad_tick % 25 == 0 {
                let mapped = audio::perceptual_level(gate.floor()).clamp(0.02, 0.4);
                level_app.state::<AppState>().stats.lock().noise_floor = mapped;
            }
        },
        move |res| on_audio_ready(&done_app, gen, res),
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
            // 兜底监视 Ctrl+Alt 松开 + 会话期 Esc 取消 + HUD 悬停出 ✕ 按钮
            hotkey::spawn_release_watcher(app, gen);
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

/// 静音端点触发的自动结束（录音线程回调里调用）。
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
            tracing::info!("静音超时，自动结束录音进入识别");
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
        tray::set_tooltip(app, "Blurt · 正在识别…");
        // HUD 保持 listen，等 on_audio_ready 携带音频时长后切 process（含预计耗时）
    } else {
        *session = old;
    }
}

/// 录音线程交付音频（Finish 或超时自动结束时）
pub fn on_audio_ready(app: &AppHandle, gen: u64, res: Result<Vec<f32>, String>) {
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

    let samples = match res {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("录音失败：{e}");
            hud::emit_state(app, "error", None);
            finish_session(app, gen, 1000, "Blurt · 就绪（按住 Ctrl+Alt 说话）");
            return;
        }
    };

    let speech = audio::trim_silence(samples);
    let speech_s = speech.len() as f32 / TARGET_SR_F;
    if speech_s < MIN_SPEECH_S {
        // 没听到有效语音：灰色塌陷提示，而非报错
        hud::emit_state(app, "nothing", None);
        finish_session(app, gen, 900, "Blurt · 就绪（按住 Ctrl+Alt 说话）");
        return;
    }

    // “还要多久”：按历史 RTF 预测识别耗时，交给 HUD 画进度
    let eta_ms = {
        let stats = state.stats.lock();
        ((speech_s as f64 * stats.rtf_ema as f64 * 1.15 + 0.35) * 1000.0).clamp(500.0, 20000.0)
            as u64
    };
    hud::emit_state(app, "process", Some(eta_ms));

    let engine = match &*state.engine.read() {
        EngineSlot::Ready(e) => e.clone(),
        _ => {
            hud::emit_state(app, "error", None);
            finish_session(app, gen, 1000, "Blurt · 引擎不可用");
            return;
        }
    };

    let app = app.clone();
    std::thread::spawn(move || {
        let result = engine.transcribe(&speech);
        let state = app.state::<AppState>();

        // 识别期间可能被 Esc 取消
        if state.gen.load(Ordering::SeqCst) != gen {
            return;
        }

        match result {
            Ok((text, elapsed)) => {
                // 更新 RTF 滑动平均，供下次预测（落盘与广播挪到注入之后：
                // %APPDATA% 写盘可能被实时防护放大到几十毫秒，不该挡住上屏）
                let stats_now = {
                    let mut stats = state.stats.lock();
                    let rtf = (elapsed / speech_s as f64) as f32;
                    stats.rtf_ema = (stats.rtf_ema * 0.7 + rtf * 0.3).clamp(0.02, 2.0);
                    stats.last_ms = Some((elapsed * 1000.0) as u64);
                    *stats
                };
                tracing::info!(
                    "识别完成 {:.2}s（音频 {:.2}s）：{}",
                    elapsed,
                    speech_s,
                    text
                );

                if text.is_empty() {
                    hud::emit_state(&app, "nothing", None);
                    finish_session(&app, gen, 900, "Blurt · 就绪（按住 Ctrl+Alt 说话）");
                } else {
                    let cfg = state.config.read().clone();
                    match inject::inject(&text, &cfg.inject_mode, cfg.type_threshold) {
                        Ok(()) => {
                            hud::emit_state(&app, "success", None);
                            finish_session(&app, gen, 650, "Blurt · 就绪（按住 Ctrl+Alt 说话）");
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
                emit_engine_status(&app);
            }
            Err(e) => {
                tracing::error!("识别失败：{e:#}");
                hud::emit_state(&app, "error", None);
                finish_session(&app, gen, 1100, "Blurt · 识别失败");
            }
        }
    });
}

/// 会话收尾：回 Idle、延时隐藏 HUD、恢复托盘提示（Esc 看门线程见 Idle 自行退出）
fn finish_session(app: &AppHandle, gen: u64, hide_delay_ms: u64, tooltip: &str) {
    let state = app.state::<AppState>();
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
