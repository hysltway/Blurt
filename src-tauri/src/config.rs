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
    /// 是否开启专属声纹防干扰
    pub voiceprint_enabled: bool,
    /// 声纹相似度阈值，默认 0.60 (范围 0.50 ~ 0.80)
    #[serde(default = "default_voiceprint_threshold")]
    pub voiceprint_threshold: f32,
}

fn default_voiceprint_threshold() -> f32 {
    0.30
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
            voiceprint_enabled: false,
            voiceprint_threshold: default_voiceprint_threshold(),
        }
    }
}

const DOUBAO_LEGACY_CREDENTIAL_TARGET: &str = "Blurt/DoubaoApiKey";
const DOUBAO_KEYS_CREDENTIAL_TARGET: &str = "Blurt/DoubaoApiKeys";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApiKeyEntry {
    pub id: String,
    pub name: String,
    pub key: String,
    pub is_active: bool,
    pub created_at: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApiKeyDto {
    pub id: String,
    pub name: String,
    pub masked_key: String,
    pub is_active: bool,
    pub created_at: String,
}

pub fn mask_api_key(key: &str) -> String {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let chars: Vec<char> = trimmed.chars().collect();
    if chars.len() <= 8 {
        return "••••••••".to_string();
    }
    let prefix_len = if trimmed.starts_with("sk-") { 3 } else { 2 };
    let suffix_len = 4;
    let prefix: String = chars[..prefix_len].iter().collect();
    let suffix: String = chars[chars.len() - suffix_len..].iter().collect();
    format!("{prefix}••••••••{suffix}")
}

#[cfg(windows)]
fn read_credential_raw(target_name: &str) -> anyhow::Result<Option<String>> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::ERROR_NOT_FOUND;
    use windows::Win32::Security::Credentials::{
        CredFree, CredReadW, CREDENTIALW, CRED_TYPE_GENERIC,
    };

    let target: Vec<u16> = target_name
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

#[cfg(windows)]
fn write_credential_raw(target_name: &str, content: &str) -> anyhow::Result<()> {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::ERROR_NOT_FOUND;
    use windows::Win32::Security::Credentials::{
        CredDeleteW, CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
    };

    let content = content.trim();
    let mut target: Vec<u16> = target_name
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    if content.is_empty() {
        if let Err(e) = unsafe { CredDeleteW(PWSTR(target.as_mut_ptr()), CRED_TYPE_GENERIC, None) }
        {
            if e.code() != windows::core::HRESULT::from_win32(ERROR_NOT_FOUND.0) {
                return Err(anyhow::anyhow!("从 Windows 凭据管理器删除失败：{e}"));
            }
        }
        return Ok(());
    }

    let mut username: Vec<u16> = "Blurt".encode_utf16().chain(std::iter::once(0)).collect();
    let mut blob = content.as_bytes().to_vec();
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
fn delete_credential_raw(target_name: &str) -> anyhow::Result<()> {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::ERROR_NOT_FOUND;
    use windows::Win32::Security::Credentials::{CredDeleteW, CRED_TYPE_GENERIC};

    let mut target: Vec<u16> = target_name
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    if let Err(e) = unsafe { CredDeleteW(PWSTR(target.as_mut_ptr()), CRED_TYPE_GENERIC, None) } {
        if e.code() != windows::core::HRESULT::from_win32(ERROR_NOT_FOUND.0) {
            return Err(anyhow::anyhow!("从 Windows 凭据管理器删除失败：{e}"));
        }
    }
    Ok(())
}

#[cfg(windows)]
pub fn load_doubao_api_keys() -> anyhow::Result<Vec<ApiKeyEntry>> {
    if let Some(json_str) = read_credential_raw(DOUBAO_KEYS_CREDENTIAL_TARGET)? {
        if let Ok(mut keys) = serde_json::from_str::<Vec<ApiKeyEntry>>(&json_str) {
            if !keys.is_empty() && !keys.iter().any(|k| k.is_active) {
                keys[0].is_active = true;
            }
            return Ok(keys);
        }
    }

    // 兼容迁移：若尚未创建新凭据集合，尝试读取旧单条凭据
    if let Some(legacy_key) = read_credential_raw(DOUBAO_LEGACY_CREDENTIAL_TARGET)? {
        let trimmed = legacy_key.trim();
        if !trimmed.is_empty() {
            let migrated = vec![ApiKeyEntry {
                id: uuid::Uuid::new_v4().to_string(),
                name: "默认密钥".to_string(),
                key: trimmed.to_string(),
                is_active: true,
                created_at: local_day_key(),
            }];
            let _ = save_doubao_api_keys(&migrated);
            let _ = delete_credential_raw(DOUBAO_LEGACY_CREDENTIAL_TARGET);
            return Ok(migrated);
        }
    }

    Ok(Vec::new())
}

#[cfg(windows)]
pub fn save_doubao_api_keys(keys: &[ApiKeyEntry]) -> anyhow::Result<()> {
    if keys.is_empty() {
        write_credential_raw(DOUBAO_KEYS_CREDENTIAL_TARGET, "")
    } else {
        let json = serde_json::to_string(keys)?;
        write_credential_raw(DOUBAO_KEYS_CREDENTIAL_TARGET, &json)
    }
}

#[cfg(not(windows))]
pub fn load_doubao_api_keys() -> anyhow::Result<Vec<ApiKeyEntry>> {
    Ok(Vec::new())
}

#[cfg(not(windows))]
pub fn save_doubao_api_keys(_keys: &[ApiKeyEntry]) -> anyhow::Result<()> {
    anyhow::bail!("豆包 API Key 凭据存储仅支持 Windows")
}

pub fn load_doubao_api_key() -> anyhow::Result<Option<String>> {
    let keys = load_doubao_api_keys()?;
    let active = keys.iter().find(|k| k.is_active).or_else(|| keys.first());
    Ok(active.map(|k| k.key.clone()))
}

pub fn save_doubao_api_key(api_key: &str) -> anyhow::Result<()> {
    let api_key = api_key.trim();
    if api_key.is_empty() {
        return save_doubao_api_keys(&[]);
    }
    let mut keys = load_doubao_api_keys()?;
    if let Some(active) = keys.iter_mut().find(|k| k.is_active) {
        active.key = api_key.to_string();
    } else if let Some(first) = keys.first_mut() {
        first.key = api_key.to_string();
        first.is_active = true;
    } else {
        keys.push(ApiKeyEntry {
            id: uuid::Uuid::new_v4().to_string(),
            name: "默认密钥".to_string(),
            key: api_key.to_string(),
            is_active: true,
            created_at: local_day_key(),
        });
    }
    save_doubao_api_keys(&keys)
}

pub fn add_doubao_api_key(name: &str, key: &str) -> anyhow::Result<()> {
    let key = key.trim();
    if key.is_empty() {
        anyhow::bail!("API Key 不能为空");
    }
    let mut keys = load_doubao_api_keys()?;
    let name = name.trim();
    let final_name = if name.is_empty() {
        format!("密钥 {}", keys.len() + 1)
    } else {
        name.to_string()
    };

    for k in keys.iter_mut() {
        k.is_active = false;
    }

    keys.push(ApiKeyEntry {
        id: uuid::Uuid::new_v4().to_string(),
        name: final_name,
        key: key.to_string(),
        is_active: true,
        created_at: local_day_key(),
    });

    save_doubao_api_keys(&keys)
}

pub fn select_doubao_api_key(id: &str) -> anyhow::Result<()> {
    let mut keys = load_doubao_api_keys()?;
    let mut found = false;
    for k in keys.iter_mut() {
        if k.id == id {
            k.is_active = true;
            found = true;
        } else {
            k.is_active = false;
        }
    }
    if !found {
        anyhow::bail!("未找到指定的 API Key");
    }
    save_doubao_api_keys(&keys)
}

pub fn delete_doubao_api_key(id: &str) -> anyhow::Result<()> {
    let mut keys = load_doubao_api_keys()?;
    let initial_len = keys.len();
    let was_active = keys
        .iter()
        .find(|k| k.id == id)
        .map(|k| k.is_active)
        .unwrap_or(false);
    keys.retain(|k| k.id != id);
    if keys.len() == initial_len {
        anyhow::bail!("未找到指定的 API Key");
    }
    if was_active && !keys.is_empty() {
        keys[0].is_active = true;
    }
    save_doubao_api_keys(&keys)
}

pub fn list_doubao_api_key_dtos() -> anyhow::Result<Vec<ApiKeyDto>> {
    let keys = load_doubao_api_keys()?;
    Ok(keys
        .into_iter()
        .map(|k| ApiKeyDto {
            id: k.id,
            name: k.name,
            masked_key: mask_api_key(&k.key),
            is_active: k.is_active,
            created_at: k.created_at,
        })
        .collect())
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
    use super::{mask_api_key, ApiKeyEntry, Stats};

    #[test]
    fn test_mask_api_key() {
        assert_eq!(mask_api_key(""), "");
        assert_eq!(mask_api_key("1234567"), "••••••••");
        assert_eq!(mask_api_key("12345678"), "••••••••");
        assert_eq!(mask_api_key("sk-1234567890abcdef"), "sk-••••••••cdef");
        assert_eq!(mask_api_key("ab12345678901234"), "ab••••••••1234");
    }

    #[test]
    fn test_api_key_entry_serialization() {
        let entry = ApiKeyEntry {
            id: "key-1".into(),
            name: "默认密钥".into(),
            key: "sk-test-secret-1234".into(),
            is_active: true,
            created_at: "2026-08-29".into(),
        };
        let json = serde_json::to_string(&vec![entry.clone()]).unwrap();
        let deserialized: Vec<ApiKeyEntry> = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.len(), 1);
        assert_eq!(deserialized[0], entry);
    }

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
