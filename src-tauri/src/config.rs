//! 配置与持久化：%APPDATA%\Blurt\{config.json, state.json, logs, vad}

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

fn default_hotkey() -> String {
    "Ctrl+Alt".into()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// 全局语音快捷键，例如 Ctrl+Alt 或 Ctrl+Shift+K
    #[serde(default = "default_hotkey")]
    pub hotkey: String,
    /// 注入方式：auto | type | paste
    pub inject_mode: String,
    /// auto 模式下，长度 ≤ 该值用模拟键入，否则用剪贴板粘贴
    pub type_threshold: usize,
    /// 麦克风设备名，None = 系统默认
    pub mic_device: Option<String>,
    /// 热词，逗号分隔
    pub hotwords: String,
    /// 最长录音秒数，超时自动开始识别
    pub max_record_secs: u64,
    /// 切换模式下说完后静音多少秒自动结束识别，0 = 关闭
    pub auto_stop_secs: f32,
    /// 开机自启动
    pub autostart: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            hotkey: default_hotkey(),
            inject_mode: "auto".into(),
            type_threshold: 20,
            mic_device: None,
            hotwords: String::new(),
            max_record_secs: 120,
            auto_stop_secs: 2.0,
            autostart: false,
        }
    }
}

const DOUBAO_CREDENTIAL_TARGET: &str = "Blurt/DoubaoApiKey";

/// API Key 只存入 Windows 凭据管理器，不进入 config.json。
#[cfg(windows)]
pub fn save_doubao_api_key(api_key: &str) -> anyhow::Result<()> {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::ERROR_NOT_FOUND;
    use windows::Win32::Security::Credentials::{
        CredDeleteW, CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
    };

    let api_key = api_key.trim();
    let mut target: Vec<u16> = DOUBAO_CREDENTIAL_TARGET
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    if api_key.is_empty() {
        if let Err(e) = unsafe { CredDeleteW(PWSTR(target.as_mut_ptr()), CRED_TYPE_GENERIC, None) }
        {
            if e.code() != windows::core::HRESULT::from_win32(ERROR_NOT_FOUND.0) {
                return Err(anyhow::anyhow!("从 Windows 凭据管理器删除失败：{e}"));
            }
        }
        return Ok(());
    }

    let mut username: Vec<u16> = "Blurt".encode_utf16().chain(std::iter::once(0)).collect();
    let mut blob = api_key.as_bytes().to_vec();
    let credential = CREDENTIALW {
        Type: CRED_TYPE_GENERIC,
        TargetName: PWSTR(target.as_mut_ptr()),
        CredentialBlobSize: blob.len().try_into()?,
        CredentialBlob: blob.as_mut_ptr(),
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        UserName: PWSTR(username.as_mut_ptr()),
        ..Default::default()
    };
    unsafe { CredWriteW(&credential, 0) }
        .map_err(|e| anyhow::anyhow!("写入 Windows 凭据管理器失败：{e}"))
}

#[cfg(windows)]
pub fn load_doubao_api_key() -> anyhow::Result<Option<String>> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::ERROR_NOT_FOUND;
    use windows::Win32::Security::Credentials::{
        CredFree, CredReadW, CREDENTIALW, CRED_TYPE_GENERIC,
    };

    let target: Vec<u16> = DOUBAO_CREDENTIAL_TARGET
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut credential: *mut CREDENTIALW = std::ptr::null_mut();
    let result = unsafe {
        CredReadW(
            PCWSTR(target.as_ptr()),
            CRED_TYPE_GENERIC,
            None,
            &mut credential,
        )
    };
    if let Err(e) = result {
        if e.code() == windows::core::HRESULT::from_win32(ERROR_NOT_FOUND.0) {
            return Ok(None);
        }
        return Err(anyhow::anyhow!("读取 Windows 凭据管理器失败：{e}"));
    }

    if credential.is_null() {
        return Ok(None);
    }
    let bytes = unsafe {
        let credential_ref = &*credential;
        let len = credential_ref.CredentialBlobSize as usize;
        let bytes = if len == 0 {
            Ok(Vec::new())
        } else if credential_ref.CredentialBlob.is_null() {
            Err(anyhow::anyhow!("Windows 凭据内容无效"))
        } else {
            Ok(std::slice::from_raw_parts(credential_ref.CredentialBlob, len).to_vec())
        };
        CredFree(credential.cast());
        bytes
    };
    let value =
        String::from_utf8(bytes?).map_err(|_| anyhow::anyhow!("Windows 凭据内容不是 UTF-8"))?;
    Ok((!value.trim().is_empty()).then_some(value))
}

#[cfg(not(windows))]
pub fn save_doubao_api_key(_api_key: &str) -> anyhow::Result<()> {
    anyhow::bail!("豆包 API Key 凭据存储仅支持 Windows")
}

#[cfg(not(windows))]
pub fn load_doubao_api_key() -> anyhow::Result<Option<String>> {
    Ok(None)
}

pub fn app_dir() -> PathBuf {
    let d = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Blurt");
    let _ = fs::create_dir_all(&d);
    d
}

pub fn logs_dir() -> PathBuf {
    let d = app_dir().join("logs");
    let _ = fs::create_dir_all(&d);
    d
}

fn config_path() -> PathBuf {
    app_dir().join("config.json")
}

fn state_path() -> PathBuf {
    app_dir().join("state.json")
}

pub fn load() -> Config {
    match fs::read_to_string(config_path()) {
        Ok(s) => {
            let mut config: Config = serde_json::from_str(&s).unwrap_or_default();
            config.hotkey =
                crate::hotkey::canonicalize(&config.hotkey).unwrap_or_else(|_| default_hotkey());
            // Rewrite once to remove fields from versions that supported local ASR.
            let _ = save(&config);
            config
        }
        Err(_) => {
            let c = Config::default();
            let _ = save(&c);
            c
        }
    }
}

pub fn save(c: &Config) -> anyhow::Result<()> {
    fs::write(config_path(), serde_json::to_string_pretty(c)?)?;
    Ok(())
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DailyUsage {
    pub date: String,
    pub audio_secs: f64,
    pub chars: u64,
    pub sessions: u64,
}

/// 运行期统计（使用概览与降噪本底复用），跨会话持久化。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Stats {
    /// 学习到的环境噪声本底（设备与环境不变则可复用，免去每次会话重新学习；
    /// 换麦克风时重置，会话内仍自适应微调）
    pub noise_floor: f32,
    pub total_audio_secs: f64,
    pub total_chars: u64,
    pub daily_usage: Vec<DailyUsage>,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            noise_floor: 0.05,
            total_audio_secs: 0.0,
            total_chars: 0,
            daily_usage: Vec::new(),
        }
    }
}

impl Stats {
    pub fn record_usage(&mut self, audio_secs: f32, text: &str) {
        self.record_usage_for_day(&local_day_key(), audio_secs, text);
    }

    fn record_usage_for_day(&mut self, date: &str, audio_secs: f32, text: &str) {
        let chars = text.chars().filter(|c| !c.is_whitespace()).count() as u64;
        if chars == 0 {
            return;
        }

        let audio_secs = f64::from(audio_secs.max(0.0));
        self.total_audio_secs += audio_secs;
        self.total_chars += chars;

        if let Some(day) = self.daily_usage.iter_mut().find(|day| day.date == date) {
            day.audio_secs += audio_secs;
            day.chars += chars;
            day.sessions += 1;
        } else {
            self.daily_usage.push(DailyUsage {
                date: date.to_string(),
                audio_secs,
                chars,
                sessions: 1,
            });
            self.daily_usage.sort_by(|a, b| a.date.cmp(&b.date));
        }
    }
}

#[cfg(windows)]
fn local_day_key() -> String {
    use windows::Win32::System::SystemInformation::GetLocalTime;

    let now = unsafe { GetLocalTime() };
    format!("{:04}-{:02}-{:02}", now.wYear, now.wMonth, now.wDay)
}

#[cfg(not(windows))]
fn local_day_key() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let days = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        / 86_400;
    format!("day-{days}")
}

pub fn load_stats() -> Stats {
    fs::read_to_string(state_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_stats(s: &Stats) {
    if let Ok(j) = serde_json::to_string(s) {
        let _ = fs::write(state_path(), j);
    }
}

#[cfg(test)]
mod tests {
    use super::Stats;

    #[test]
    fn legacy_stats_files_receive_usage_defaults() {
        let stats: Stats = serde_json::from_str(r#"{"noise_floor":0.12}"#).unwrap();
        assert_eq!(stats.noise_floor, 0.12);
        assert_eq!(stats.total_audio_secs, 0.0);
        assert_eq!(stats.total_chars, 0);
        assert!(stats.daily_usage.is_empty());
    }

    #[test]
    fn usage_aggregates_by_day_and_ignores_whitespace() {
        let mut stats = Stats::default();
        stats.record_usage_for_day("2026-07-29", 12.5, "hello 世界");
        stats.record_usage_for_day("2026-07-29", 7.5, " 再见 ");
        stats.record_usage_for_day("2026-07-30", 3.0, "   \n");

        assert_eq!(stats.total_audio_secs, 20.0);
        assert_eq!(stats.total_chars, 9);
        assert_eq!(stats.daily_usage.len(), 1);
        assert_eq!(stats.daily_usage[0].sessions, 2);
        assert_eq!(stats.daily_usage[0].chars, 9);
    }
}
