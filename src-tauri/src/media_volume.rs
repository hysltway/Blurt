//! Windows playback-volume ducking for voice input sessions.
//!
//! The COM endpoint is owned by one worker thread so rapid start/stop events can
//! interrupt an in-flight fade without racing an older fade back over the user.

use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

const DUCK_RATIO: f32 = 0.5;
const FADE_DURATION: Duration = Duration::from_millis(320);
const FADE_STEP: Duration = Duration::from_millis(16);

#[derive(Clone, Copy, Debug)]
enum Command {
    Duck,
    Restore,
}

pub struct PlaybackDucker {
    tx: Sender<Command>,
}

impl PlaybackDucker {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        if let Err(error) = std::thread::Builder::new()
            .name("blurt-volume-ducker".into())
            .spawn(move || run_worker(rx))
        {
            tracing::warn!("无法启动播放音量控制线程：{error}");
        }
        Self { tx }
    }

    pub fn duck(&self) {
        let _ = self.tx.send(Command::Duck);
    }

    pub fn restore(&self) {
        let _ = self.tx.send(Command::Restore);
    }
}

#[cfg(windows)]
fn run_worker(rx: Receiver<Command>) {
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

    if let Err(error) = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok() {
        tracing::warn!("无法初始化播放音量控制：{error}");
        return;
    }

    run_windows_worker(&rx);
    unsafe { CoUninitialize() };
}

#[cfg(not(windows))]
fn run_worker(rx: Receiver<Command>) {
    while rx.recv().is_ok() {}
}

#[cfg(windows)]
fn run_windows_worker(rx: &Receiver<Command>) {
    use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;

    let mut endpoint: Option<IAudioEndpointVolume> = None;
    let mut initial_volume: Option<f32> = None;
    let mut pending = None;

    loop {
        let command = match pending.take() {
            Some(command) => command,
            None => match rx.recv() {
                Ok(command) => command,
                Err(_) => {
                    if let (Some(endpoint), Some(initial)) = (&endpoint, initial_volume) {
                        let _ = set_volume(endpoint, initial);
                    }
                    break;
                }
            },
        };

        match command {
            Command::Duck => {
                if endpoint.is_none() {
                    match default_playback_endpoint() {
                        Ok(value) => endpoint = Some(value),
                        Err(error) => {
                            tracing::warn!("无法访问默认播放设备，跳过音量淡出：{error}");
                            continue;
                        }
                    }
                }
                let Some(active_endpoint) = endpoint.as_ref() else {
                    continue;
                };
                if initial_volume.is_none() {
                    match get_volume(active_endpoint) {
                        Ok(volume) => initial_volume = Some(volume),
                        Err(error) => {
                            tracing::warn!("无法读取当前播放音量，跳过音量淡出：{error}");
                            endpoint = None;
                            continue;
                        }
                    }
                }
                if let Some(initial) = initial_volume {
                    match fade_to(active_endpoint, initial * DUCK_RATIO, rx) {
                        Ok(interrupted_by) => pending = interrupted_by,
                        Err(error) => {
                            tracing::warn!("播放音量淡出失败：{error}");
                            endpoint = None;
                            initial_volume = None;
                        }
                    }
                }
            }
            Command::Restore => {
                let (Some(active_endpoint), Some(initial)) = (endpoint.as_ref(), initial_volume)
                else {
                    continue;
                };
                match fade_to(active_endpoint, initial, rx) {
                    Ok(Some(interrupted_by)) => pending = Some(interrupted_by),
                    Ok(None) => {
                        endpoint = None;
                        initial_volume = None;
                    }
                    Err(error) => {
                        tracing::warn!("播放音量恢复失败：{error}");
                        endpoint = None;
                        initial_volume = None;
                    }
                }
            }
        }
    }
}

#[cfg(windows)]
fn default_playback_endpoint(
) -> windows::core::Result<windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume> {
    use windows::Win32::Media::Audio::{
        eMultimedia, eRender, IMMDeviceEnumerator, MMDeviceEnumerator,
    };
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia)?;
        device.Activate(CLSCTX_ALL, None)
    }
}

#[cfg(windows)]
fn get_volume(
    endpoint: &windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume,
) -> windows::core::Result<f32> {
    unsafe { endpoint.GetMasterVolumeLevelScalar() }
}

#[cfg(windows)]
fn set_volume(
    endpoint: &windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume,
    level: f32,
) -> windows::core::Result<()> {
    unsafe { endpoint.SetMasterVolumeLevelScalar(level.clamp(0.0, 1.0), std::ptr::null()) }
}

#[cfg(windows)]
fn fade_to(
    endpoint: &windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume,
    target: f32,
    rx: &Receiver<Command>,
) -> windows::core::Result<Option<Command>> {
    let from = get_volume(endpoint)?;
    let steps = (FADE_DURATION.as_millis() / FADE_STEP.as_millis()).max(1) as u32;
    for step in 1..=steps {
        if let Ok(command) = rx.recv_timeout(FADE_STEP) {
            return Ok(Some(command));
        }
        let progress = step as f32 / steps as f32;
        let eased = ease_in_out_cubic(progress);
        set_volume(endpoint, from + (target - from) * eased)?;
    }
    Ok(None)
}

fn ease_in_out_cubic(value: f32) -> f32 {
    if value < 0.5 {
        4.0 * value * value * value
    } else {
        1.0 - (-2.0 * value + 2.0).powi(3) / 2.0
    }
}

#[cfg(test)]
mod tests {
    use super::ease_in_out_cubic;

    #[test]
    fn fade_curve_has_stable_endpoints_and_midpoint() {
        assert_eq!(ease_in_out_cubic(0.0), 0.0);
        assert_eq!(ease_in_out_cubic(0.5), 0.5);
        assert_eq!(ease_in_out_cubic(1.0), 1.0);
        assert!(ease_in_out_cubic(0.25) < 0.25);
        assert!(ease_in_out_cubic(0.75) > 0.75);
    }
}
