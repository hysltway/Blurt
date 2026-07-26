//! 无界面自检：blurt --selftest [wav路径]
//! 验证 模型加载 → wav 读取 → 重采样 → 识别 全链路，并打印耗时与 RTF。

use std::time::Instant;

use crate::{audio, config};

#[cfg(windows)]
fn attach_console() {
    use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

pub fn run(wav: Option<String>) -> i32 {
    #[cfg(windows)]
    attach_console();
    println!();
    println!("=== Blurt 自检 ===");

    let cfg = config::load();
    let Some(model_dir) = config::resolve_model_dir(&cfg) else {
        eprintln!("✗ 未找到模型目录。请先运行 scripts/get-model.ps1");
        return 2;
    };
    println!("模型目录: {}", model_dir.display());

    let wav_path = wav.or_else(|| {
        let tw = model_dir.join("test_wavs");
        for name in ["codeswitch.wav", "zh.wav", "en.wav"] {
            let p = tw.join(name);
            if p.is_file() {
                return Some(p.to_string_lossy().into_owned());
            }
        }
        std::fs::read_dir(&tw).ok()?.flatten().find_map(|e| {
            let p = e.path();
            (p.extension()? == "wav").then(|| p.to_string_lossy().into_owned())
        })
    });
    let Some(wav_path) = wav_path else {
        eprintln!("✗ 没有可用的测试 wav");
        return 2;
    };
    println!("测试音频: {wav_path}");

    let samples = match audio::read_wav_16k_mono(&wav_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("✗ {e}");
            return 2;
        }
    };
    let dur = samples.len() as f32 / audio::TARGET_SR as f32;
    println!("音频时长: {dur:.2}s");

    let t0 = Instant::now();
    let engine = match crate::asr::AsrEngine::load(&model_dir, cfg.num_threads, &cfg.hotwords) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("✗ 模型加载失败：{e:#}");
            return 3;
        }
    };
    println!("模型加载: {:.2}s", t0.elapsed().as_secs_f64());

    // 预热一次，再计时正式识别
    engine.warmup();

    let trimmed = audio::trim_silence(&samples);
    let use_samples = if trimmed.is_empty() {
        &samples
    } else {
        &trimmed
    };
    match engine.transcribe(use_samples) {
        Ok((text, elapsed)) => {
            println!("识别耗时: {elapsed:.2}s   RTF: {:.3}", elapsed / dur as f64);
            println!("识别结果: {text}");
            if text.trim().is_empty() {
                eprintln!("✗ 结果为空");
                return 4;
            }
            println!("✓ 自检通过");
            0
        }
        Err(e) => {
            eprintln!("✗ 识别失败：{e:#}");
            4
        }
    }
}
