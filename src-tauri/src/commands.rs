//! 设置页 / HUD 调用的 Tauri 命令。

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::app::AppState;
use crate::config::{self, Config};

#[tauri::command]
pub fn get_config(state: State<'_, AppState>) -> Config {
    state.config.read().clone()
}

#[tauri::command]
pub fn set_config(
    app: AppHandle,
    state: State<'_, AppState>,
    config: Config,
) -> Result<(), String> {
    let old = state.config.read().clone();

    // 开机自启
    if config.autostart != old.autostart {
        crate::autostart::set_enabled(&app, config.autostart)?;
        crate::tray::sync_autostart(config.autostart);
    }

    // 换麦克风：缓存的噪声本底不再适用，重置后重新学习
    if config.mic_device != old.mic_device {
        let mut stats = state.stats.lock();
        stats.noise_floor = config::Stats::default().noise_floor;
        config::save_stats(&stats);
    }

    *state.config.write() = config.clone();
    config::save(&config).map_err(|e| format!("保存配置失败：{e}"))?;

    // 引擎相关变更 → 后台重载
    if config.recognition_mode != old.recognition_mode
        || config.num_threads != old.num_threads
        || config.hotwords != old.hotwords
        || config.model_dir != old.model_dir
    {
        crate::app::spawn_engine_load(&app, false);
    }
    Ok(())
}

#[tauri::command]
pub fn doubao_api_key_status() -> serde_json::Value {
    match config::load_doubao_api_key() {
        Ok(key) => serde_json::json!({ "configured": key.is_some(), "error": null }),
        Err(e) => serde_json::json!({ "configured": false, "error": format!("{e:#}") }),
    }
}

#[tauri::command]
pub fn set_doubao_api_key(app: AppHandle, api_key: String) -> Result<(), String> {
    config::save_doubao_api_key(&api_key).map_err(|e| format!("保存 API Key 失败：{e:#}"))?;
    crate::app::emit_engine_status(&app);
    if app.state::<AppState>().config.read().recognition_mode == "doubao" {
        if api_key.trim().is_empty() {
            crate::tray::set_tooltip(&app, "Blurt · 请在设置中配置豆包 API Key");
        } else {
            crate::tray::set_tooltip(&app, "Blurt · 就绪（豆包 API）");
        }
    }
    Ok(())
}

#[tauri::command]
pub fn list_input_devices() -> Vec<String> {
    crate::audio::list_input_devices()
}

#[tauri::command]
pub fn engine_status(app: AppHandle) -> serde_json::Value {
    crate::app::engine_status_dto(&app)
}

#[tauri::command]
pub fn reload_engine(app: AppHandle) {
    crate::app::spawn_engine_load(&app, false);
}

#[tauri::command]
pub fn open_model_dir(state: State<'_, AppState>) {
    let cfg = state.config.read().clone();
    let dir = config::resolve_model_dir(&cfg).unwrap_or_else(config::models_root);
    let _ = std::process::Command::new("explorer.exe").arg(dir).spawn();
}

#[tauri::command]
pub fn open_log_dir() {
    let _ = std::process::Command::new("explorer.exe")
        .arg(config::logs_dir())
        .spawn();
}

#[tauri::command]
pub fn copy_text(text: String) -> Result<(), String> {
    let mut cb = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    cb.set_text(text).map_err(|e| e.to_string())
}

/// HUD ✕ 按钮：取消当前会话（与 Esc 同路径）
#[tauri::command]
pub fn cancel_session(app: AppHandle) {
    crate::app::esc_pressed(&app);
}

/// HUD 启动时读取缓存的环境噪声本底（跨重启复用，免去每次学习过程）
#[tauri::command]
pub fn get_noise_floor(state: State<'_, AppState>) -> f32 {
    state.stats.lock().noise_floor
}

fn pick_test_wav(dir: &Path) -> Option<PathBuf> {
    let tw = dir.join("test_wavs");
    for n in ["codeswitch.wav", "zh.wav", "en.wav"] {
        let p = tw.join(n);
        if p.is_file() {
            return Some(p);
        }
    }
    std::fs::read_dir(&tw).ok()?.flatten().find_map(|e| {
        let p = e.path();
        (p.extension()? == "wav").then_some(p)
    })
}

/// 一键测速：依次以不同线程数 加载→识别 同一段音频，量化线程数收益。
/// 进度经 `bench:progress` / `bench:result` / `bench:done` 事件推送。
#[tauri::command]
pub fn bench_threads(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    if state.benching.swap(true, Ordering::SeqCst) {
        return Err("测速已在进行中".into());
    }
    let cfg = state.config.read().clone();
    let Some(dir) = config::resolve_model_dir(&cfg) else {
        state.benching.store(false, Ordering::SeqCst);
        return Err("未找到模型，无法测速".into());
    };
    let Some(wav) = pick_test_wav(&dir) else {
        state.benching.store(false, Ordering::SeqCst);
        return Err("模型目录缺少 test_wavs 测试音频".into());
    };

    std::thread::spawn(move || {
        let done = |app: &AppHandle| {
            app.state::<AppState>()
                .benching
                .store(false, Ordering::SeqCst);
        };
        let samples = match crate::audio::read_wav_16k_mono(&wav.to_string_lossy()) {
            Ok(s) => s,
            Err(e) => {
                let _ = app.emit("bench:done", serde_json::json!({ "best": 0, "error": e }));
                done(&app);
                return;
            }
        };
        let dur_s = samples.len() as f64 / crate::audio::TARGET_SR as f64;
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        let mut cands: Vec<usize> = [2usize, 4, 8, 12]
            .iter()
            .copied()
            .filter(|&t| t <= cores)
            .collect();
        if cands.is_empty() {
            cands.push(cores.max(1));
        }
        let total = cands.len();
        let mut results: Vec<(usize, f64)> = vec![];
        for (i, &t) in cands.iter().enumerate() {
            let _ = app.emit(
                "bench:progress",
                serde_json::json!({ "threads": t, "idx": i + 1, "total": total }),
            );
            match crate::asr::bench_once(&dir, t, &samples) {
                Ok(ms) => {
                    tracing::info!("测速 {t} 线程：{ms:.0}ms (RTF {:.3})", ms / 1000.0 / dur_s);
                    results.push((t, ms));
                    let _ = app.emit(
                        "bench:result",
                        serde_json::json!({ "threads": t, "ms": ms, "rtf": ms / 1000.0 / dur_s }),
                    );
                }
                Err(e) => {
                    tracing::error!("测速 {t} 线程失败：{e:#}");
                    let _ = app.emit(
                        "bench:result",
                        serde_json::json!({ "threads": t, "error": format!("{e:#}") }),
                    );
                }
            }
        }
        let best = results
            .iter()
            .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
            .map(|r| r.0)
            .unwrap_or(0);
        let _ = app.emit("bench:done", serde_json::json!({ "best": best }));
        done(&app);
    });
    Ok(())
}
