//! 豆包流式语音识别 1.0：录音期间实时发送 16kHz/16bit/mono PCM。

use anyhow::{anyhow, bail, Context, Result};
use crossbeam_channel::RecvTimeoutError;
use crossbeam_channel::{unbounded, Receiver, Sender};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::time::{Duration, Instant};
use tungstenite::client::IntoClientRequest;
use tungstenite::http::HeaderValue;
use tungstenite::{connect, Message};
use uuid::Uuid;

const ENDPOINT: &str = "wss://openspeech.bytedance.com/api/v3/sauc/bigmodel_async";
const RESOURCE_ID: &str = "volc.seedasr.sauc.duration";
const AUDIO_CHUNK_SAMPLES: usize = 3200; // 200ms @ 16kHz，文档推荐值
const RESULT_TIMEOUT: Duration = Duration::from_secs(45);
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

enum Command {
    Audio(Vec<f32>),
    Finish,
    Abort,
}

#[derive(Clone)]
pub struct AudioSender {
    tx: Sender<Command>,
}

impl AudioSender {
    pub fn push(&self, samples: &[f32]) {
        if !samples.is_empty() {
            let _ = self.tx.send(Command::Audio(samples.to_vec()));
        }
    }
}

pub struct Stream {
    tx: Option<Sender<Command>>,
    result_rx: Receiver<Result<String, String>>,
}

impl Stream {
    pub fn start(api_key: String, hotwords: String) -> Self {
        let (tx, rx) = unbounded();
        let (result_tx, result_rx) = unbounded();
        std::thread::Builder::new()
            .name("blurt-doubao-asr".into())
            .spawn(move || {
                let result = run_stream(&api_key, &hotwords, rx).map_err(|e| format!("{e:#}"));
                let _ = result_tx.send(result);
            })
            .expect("创建豆包识别线程失败");
        Self {
            tx: Some(tx),
            result_rx,
        }
    }

    pub fn audio_sender(&self) -> AudioSender {
        AudioSender {
            tx: self.tx.as_ref().expect("豆包流已结束").clone(),
        }
    }

    pub fn finish(self) -> Result<(String, f64)> {
        self.finish_with_timeout(RESULT_TIMEOUT)
    }

    /// 发送一小段静音，验证网络连通性与 API Key；不会产生识别文本。
    pub fn check(api_key: String) -> Result<()> {
        let stream = Self::start(api_key, String::new());
        stream.audio_sender().push(&[0.0; AUDIO_CHUNK_SAMPLES]);
        stream.finish_with_timeout(PROBE_TIMEOUT).map(|_| ())
    }

    fn finish_with_timeout(mut self, timeout: Duration) -> Result<(String, f64)> {
        let started = Instant::now();
        let tx = self.tx.take().context("豆包流已结束")?;
        let finish_sent = tx.send(Command::Finish).is_ok();
        drop(tx);
        let text = self
            .result_rx
            .recv_timeout(timeout)
            .map_err(|e| match e {
                RecvTimeoutError::Timeout => anyhow!("等待豆包服务响应超时"),
                RecvTimeoutError::Disconnected if !finish_sent => anyhow!("豆包识别线程已退出"),
                RecvTimeoutError::Disconnected => anyhow!("豆包识别线程异常退出"),
            })?
            .map_err(anyhow::Error::msg)?;
        Ok((text, started.elapsed().as_secs_f64()))
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Command::Abort);
        }
    }
}

fn run_stream(api_key: &str, hotwords: &str, rx: Receiver<Command>) -> Result<String> {
    // Tauri 的依赖树可能同时包含多种 Rustls provider，显式选择 ring。
    let _ = rustls::crypto::ring::default_provider().install_default();
    let request_id = Uuid::new_v4().to_string();
    let mut request = ENDPOINT.into_client_request()?;
    let headers = request.headers_mut();
    headers.insert("X-Api-Key", HeaderValue::from_str(api_key)?);
    headers.insert("X-Api-Resource-Id", HeaderValue::from_static(RESOURCE_ID));
    headers.insert("X-Api-Request-Id", HeaderValue::from_str(&request_id)?);
    headers.insert("X-Api-Connect-Id", HeaderValue::from_str(&request_id)?);
    headers.insert("X-Api-Sequence", HeaderValue::from_static("-1"));

    let (mut socket, response) = connect(request).context("连接豆包语音服务失败")?;
    if let Some(log_id) = response.headers().get("X-Tt-Logid") {
        tracing::info!("豆包连接成功，logid={}", log_id.to_str().unwrap_or("?"));
    }
    socket.send(Message::Binary(full_request(hotwords)?.into()))?;

    let mut pcm = Vec::<i16>::with_capacity(AUDIO_CHUNK_SAMPLES * 2);
    loop {
        match rx.recv() {
            Ok(Command::Audio(samples)) => {
                pcm.extend(samples.into_iter().map(float_to_i16));
                while pcm.len() >= AUDIO_CHUNK_SAMPLES {
                    let frame = audio_request(&pcm[..AUDIO_CHUNK_SAMPLES], false)?;
                    socket.send(Message::Binary(frame.into()))?;
                    pcm.drain(..AUDIO_CHUNK_SAMPLES);
                }
            }
            Ok(Command::Finish) => {
                socket.send(Message::Binary(audio_request(&pcm, true)?.into()))?;
                break;
            }
            Ok(Command::Abort) | Err(_) => {
                let _ = socket.close(None);
                bail!("识别已取消");
            }
        }
    }

    let mut latest = String::new();
    loop {
        match socket.read().context("读取豆包识别响应失败")? {
            Message::Binary(data) => {
                let response = parse_server_frame(&data)?;
                if let Some(text) = response.text {
                    latest = text;
                }
                if response.is_final {
                    let _ = socket.close(None);
                    return Ok(apply_replacements(latest.trim(), hotwords));
                }
            }
            Message::Text(text) => {
                if let Ok(value) = serde_json::from_str::<Value>(&text) {
                    if let Some(result) = value.pointer("/result/text").and_then(Value::as_str) {
                        latest = result.to_string();
                    }
                }
            }
            Message::Ping(payload) => socket.send(Message::Pong(payload))?,
            Message::Close(_) => bail!("豆包在返回最终结果前关闭了连接"),
            _ => {}
        }
    }
}

fn full_request(hotwords: &str) -> Result<Vec<u8>> {
    let words: Vec<Value> = hotword_items(hotwords)
        .into_iter()
        .filter_map(|item| match item.split_once("=>") {
            Some(_) => None,
            None => Some(json!({ "word": item })),
        })
        .collect();
    let mut request = json!({
        "user": { "uid": "blurt-desktop" },
        "audio": {
            "format": "pcm",
            "codec": "raw",
            "rate": 16000,
            "bits": 16,
            "channel": 1
        },
        "request": {
            "model_name": "bigmodel",
            "enable_itn": true,
            "enable_punc": true,
            "enable_ddc": true,
            "enable_nonstream": true,
            "enable_lid": true,
            "show_utterances": true,
            "result_type": "full"
        }
    });
    if !words.is_empty() {
        request["request"]["corpus"] = json!({
            "context": serde_json::to_string(&json!({ "hotwords": words }))?
        });
    }
    encode_frame([0x11, 0x10, 0x11, 0x00], &serde_json::to_vec(&request)?)
}

fn audio_request(samples: &[i16], final_packet: bool) -> Result<Vec<u8>> {
    let mut pcm = Vec::with_capacity(samples.len() * 2);
    for sample in samples {
        pcm.extend_from_slice(&sample.to_le_bytes());
    }
    encode_frame(
        [0x11, if final_packet { 0x22 } else { 0x20 }, 0x01, 0x00],
        &pcm,
    )
}

fn encode_frame(header: [u8; 4], payload: &[u8]) -> Result<Vec<u8>> {
    let compressed = gzip(payload)?;
    let mut frame = Vec::with_capacity(8 + compressed.len());
    frame.extend_from_slice(&header);
    frame.extend_from_slice(&(compressed.len() as u32).to_be_bytes());
    frame.extend_from_slice(&compressed);
    Ok(frame)
}

fn gzip(payload: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::with_capacity(payload.len()), Compression::fast());
    encoder.write_all(payload)?;
    Ok(encoder.finish()?)
}

#[derive(Debug, PartialEq, Eq)]
struct ServerResponse {
    text: Option<String>,
    is_final: bool,
}

fn parse_server_frame(frame: &[u8]) -> Result<ServerResponse> {
    if frame.len() < 4 {
        bail!("豆包响应头不完整");
    }
    let header_size = (frame[0] as usize & 0x0f) * 4;
    if header_size < 4 || frame.len() < header_size {
        bail!("豆包响应头长度无效");
    }
    let message_type = frame[1] >> 4;
    let flags = frame[1] & 0x0f;
    let compression = frame[2] & 0x0f;
    let mut offset = header_size;

    match message_type {
        0x9 => {
            let sequence = if flags & 0x1 != 0 {
                let value = read_i32(frame, offset)?;
                offset += 4;
                Some(value)
            } else {
                None
            };
            let size = read_u32(frame, offset)? as usize;
            offset += 4;
            let payload = read_payload(frame, offset, size, compression)?;
            let value: Value = serde_json::from_slice(&payload).with_context(|| {
                format!("豆包返回了无效 JSON：{}", String::from_utf8_lossy(&payload))
            })?;
            let text = value
                .pointer("/result/text")
                .and_then(Value::as_str)
                .map(str::to_string);
            Ok(ServerResponse {
                text,
                is_final: flags & 0x2 != 0 || sequence.is_some_and(|value| value < 0),
            })
        }
        0xf => {
            let code = read_u32(frame, offset)?;
            offset += 4;
            let size = read_u32(frame, offset)? as usize;
            offset += 4;
            let payload = read_payload(frame, offset, size, compression)?;
            bail!(
                "豆包识别失败（{code}）：{}",
                String::from_utf8_lossy(&payload)
            )
        }
        _ => Ok(ServerResponse {
            text: None,
            is_final: false,
        }),
    }
}

fn read_payload(frame: &[u8], offset: usize, size: usize, compression: u8) -> Result<Vec<u8>> {
    let end = offset.checked_add(size).context("豆包响应长度溢出")?;
    let bytes = frame.get(offset..end).context("豆包响应负载不完整")?;
    if compression != 1 {
        return Ok(bytes.to_vec());
    }
    let mut decoder = GzDecoder::new(bytes);
    let mut decoded = Vec::new();
    decoder.read_to_end(&mut decoded)?;
    Ok(decoded)
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let value: [u8; 4] = bytes
        .get(offset..offset + 4)
        .context("豆包响应整数不完整")?
        .try_into()?;
    Ok(u32::from_be_bytes(value))
}

fn read_i32(bytes: &[u8], offset: usize) -> Result<i32> {
    Ok(read_u32(bytes, offset)? as i32)
}

fn float_to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16
}

fn hotword_items(hotwords: &str) -> Vec<&str> {
    hotwords
        .split([',', '，', '\n'])
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .collect()
}

fn replace_case_insensitive(haystack: &str, needle: &str, replacement: &str) -> String {
    if needle.is_empty() {
        return haystack.to_string();
    }
    let char_indices: Vec<(usize, char)> = haystack.char_indices().collect();
    let needle_chars: Vec<char> = needle.chars().collect();
    let needle_len = needle_chars.len();

    let mut result = String::with_capacity(haystack.len());
    let mut last_end = 0;
    let mut i = 0;

    while i < char_indices.len() {
        let (byte_start, _) = char_indices[i];
        if i + needle_len <= char_indices.len() {
            let matches = needle_chars.iter().enumerate().all(|(offset, &nc)| {
                let (_, hc) = char_indices[i + offset];
                hc.to_lowercase().eq(nc.to_lowercase())
            });
            if matches {
                result.push_str(&haystack[last_end..byte_start]);
                result.push_str(replacement);
                let match_end_byte = if i + needle_len < char_indices.len() {
                    char_indices[i + needle_len].0
                } else {
                    haystack.len()
                };
                last_end = match_end_byte;
                i += needle_len;
                continue;
            }
        }
        i += 1;
    }
    result.push_str(&haystack[last_end..]);
    result
}

fn apply_replacements(text: &str, hotwords: &str) -> String {
    let mut result = text.to_string();
    for item in hotword_items(hotwords) {
        let Some((from, to)) = item.split_once("=>") else {
            continue;
        };
        let from = from.trim();
        let to = to.trim();
        if from.is_empty() || to.is_empty() {
            continue;
        }
        result = replace_case_insensitive(&result, from, to);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_documented_request_headers() {
        let full = full_request("Rust,cloud=>Claude").unwrap();
        assert_eq!(&full[..4], &[0x11, 0x10, 0x11, 0x00]);
        let final_audio = audio_request(&[1, -2], true).unwrap();
        assert_eq!(&final_audio[..4], &[0x11, 0x22, 0x01, 0x00]);
    }

    #[test]
    fn parses_gzipped_final_response() {
        let payload = gzip(br#"{"result":{"text":"hello"}}"#).unwrap();
        let mut frame = vec![0x11, 0x93, 0x11, 0x00];
        frame.extend_from_slice(&(-3i32).to_be_bytes());
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(&payload);
        assert_eq!(
            parse_server_frame(&frame).unwrap(),
            ServerResponse {
                text: Some("hello".into()),
                is_final: true
            }
        );
    }

    #[test]
    fn applies_configured_replacements_case_insensitively() {
        assert_eq!(
            apply_replacements("open cloud now", "Rust, cloud=>Claude"),
            "open Claude now"
        );
        assert_eq!(
            apply_replacements("Use Cloud Code and Torrey", "cloud=>Claude, torrey=>Tauri"),
            "Use Claude Code and Tauri"
        );
        assert_eq!(
            apply_replacements("测试热刺哭", "热刺哭=>热词库"),
            "测试热词库"
        );
    }

    #[test]
    #[ignore = "requires a live Doubao account and test WAV"]
    fn live_stream_recognizes_fixture() {
        let api_key = std::env::var("BLURT_DOUBAO_API_KEY").expect("缺少 API Key 环境变量");
        let wav = std::env::var("BLURT_DOUBAO_TEST_WAV").expect("缺少测试 WAV 环境变量");
        let samples = crate::audio::read_wav_16k_mono(&wav).unwrap();
        let stream = Stream::start(api_key, String::new());
        let sender = stream.audio_sender();
        for chunk in samples.chunks(AUDIO_CHUNK_SAMPLES) {
            sender.push(chunk);
            std::thread::sleep(Duration::from_millis(200));
        }
        drop(sender);
        let (text, _) = stream.finish().unwrap();
        assert!(!text.is_empty(), "豆包没有返回识别文本");
        println!("豆包识别结果：{text}");
    }
}
