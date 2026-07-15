//! Provider-neutral voice transcription and speech synthesis.
#![cfg_attr(test, allow(dead_code))]

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use reqwest::multipart::{Form, Part};
use serde::Deserialize;
use serde_json::json;

pub const OPENAI_API_KEY_ENV: &str = "OPENAI_API_KEY";
pub const TRANSCRIPTION_MODEL: &str = "gpt-4o-transcribe";
pub const SPEECH_MODEL: &str = "gpt-4o-mini-tts";
pub const SPEECH_VOICE: &str = "marin";
pub const MAX_AUDIO_BYTES: usize = 20 * 1024 * 1024;
const MAX_SPEECH_CHARS: usize = 4096;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioClip {
    pub bytes: Vec<u8>,
    pub filename: String,
    pub mime_type: String,
}

pub(crate) type VoiceFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

pub trait VoiceProvider: Send + Sync {
    fn transcribe<'a>(&'a self, clip: AudioClip) -> VoiceFuture<'a, String>;
    fn synthesize<'a>(&'a self, text: &'a str) -> VoiceFuture<'a, AudioClip>;
}

#[derive(Clone)]
pub struct Voice {
    provider: Arc<dyn VoiceProvider>,
}

impl Voice {
    pub fn from_env() -> Option<Self> {
        let key = std::env::var(OPENAI_API_KEY_ENV).ok()?;
        let key = key.trim();
        if key.is_empty() {
            return None;
        }
        Some(Self {
            provider: Arc::new(OpenAiVoice::new(key.to_string())),
        })
    }

    #[cfg(test)]
    pub fn with_provider(provider: Arc<dyn VoiceProvider>) -> Self {
        Self { provider }
    }

    pub async fn transcribe(&self, clip: AudioClip) -> Result<String> {
        if clip.bytes.is_empty() {
            bail!("voice message is empty");
        }
        if clip.bytes.len() > MAX_AUDIO_BYTES {
            bail!("voice message exceeds the 20 MB limit");
        }
        let text = self.provider.transcribe(clip).await?;
        let text = text.trim();
        if text.is_empty() {
            bail!("voice message did not contain recognizable speech");
        }
        Ok(text.to_string())
    }

    pub async fn synthesize(&self, text: &str) -> Result<AudioClip> {
        if text.trim().is_empty() {
            bail!("speech reply is empty");
        }
        if text.chars().count() > MAX_SPEECH_CHARS {
            bail!("speech reply exceeds the 4096 character limit");
        }
        let clip = self.provider.synthesize(text).await?;
        if clip.bytes.is_empty() {
            bail!("speech reply is empty");
        }
        if clip.bytes.len() > MAX_AUDIO_BYTES {
            bail!("speech reply exceeds the 20 MB limit");
        }
        Ok(clip)
    }
}

struct OpenAiVoice {
    api_key: String,
    client: reqwest::Client,
    base_url: String,
}

impl OpenAiVoice {
    fn new(api_key: String) -> Self {
        Self {
            api_key,
            client: reqwest::Client::new(),
            base_url: "https://api.openai.com/v1".to_string(),
        }
    }

    #[cfg(test)]
    fn with_base_url(api_key: String, base_url: String) -> Self {
        Self {
            api_key,
            client: reqwest::Client::new(),
            base_url,
        }
    }
}

impl VoiceProvider for OpenAiVoice {
    fn transcribe<'a>(&'a self, clip: AudioClip) -> VoiceFuture<'a, String> {
        Box::pin(async move {
            let part = Part::bytes(clip.bytes)
                .file_name(clip.filename)
                .mime_str(&clip.mime_type)
                .context("prepare voice message")?;
            let response = self
                .client
                .post(format!("{}/audio/transcriptions", self.base_url))
                .bearer_auth(&self.api_key)
                .timeout(REQUEST_TIMEOUT)
                .multipart(
                    Form::new()
                        .text("model", TRANSCRIPTION_MODEL)
                        .part("file", part),
                )
                .send()
                .await
                .context("OpenAI transcription request failed")?;
            let status = response.status();
            if !status.is_success() {
                bail!("OpenAI transcription returned HTTP {}", status.as_u16());
            }
            let result: Transcription = response
                .json()
                .await
                .context("OpenAI transcription returned invalid JSON")?;
            Ok(result.text)
        })
    }

    fn synthesize<'a>(&'a self, text: &'a str) -> VoiceFuture<'a, AudioClip> {
        Box::pin(async move {
            let mut response = self
                .client
                .post(format!("{}/audio/speech", self.base_url))
                .bearer_auth(&self.api_key)
                .timeout(REQUEST_TIMEOUT)
                .json(&json!({
                    "model": SPEECH_MODEL,
                    "voice": SPEECH_VOICE,
                    "input": text,
                    "instructions": "Speak naturally, clearly, and concisely.",
                    "response_format": "opus"
                }))
                .send()
                .await
                .context("OpenAI speech request failed")?;
            let status = response.status();
            if !status.is_success() {
                bail!("OpenAI speech returned HTTP {}", status.as_u16());
            }
            if response
                .content_length()
                .is_some_and(|size| size > MAX_AUDIO_BYTES as u64)
            {
                bail!("OpenAI speech response exceeds the 20 MB limit");
            }
            let mut bytes = Vec::with_capacity(
                response
                    .content_length()
                    .unwrap_or_default()
                    .min(MAX_AUDIO_BYTES as u64) as usize,
            );
            while let Some(chunk) = response
                .chunk()
                .await
                .context("read OpenAI speech response")?
            {
                if bytes.len().saturating_add(chunk.len()) > MAX_AUDIO_BYTES {
                    bail!("OpenAI speech response exceeds the 20 MB limit");
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(AudioClip {
                bytes,
                filename: "reply.opus".to_string(),
                mime_type: "audio/ogg".to_string(),
            })
        })
    }
}

#[derive(Deserialize)]
struct Transcription {
    text: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread::JoinHandle;

    struct FakeProvider;

    impl VoiceProvider for FakeProvider {
        fn transcribe<'a>(&'a self, _clip: AudioClip) -> VoiceFuture<'a, String> {
            Box::pin(async { Ok("  remind me tomorrow  ".to_string()) })
        }

        fn synthesize<'a>(&'a self, _text: &'a str) -> VoiceFuture<'a, AudioClip> {
            Box::pin(async {
                Ok(AudioClip {
                    bytes: vec![1, 2, 3],
                    filename: "reply.opus".to_string(),
                    mime_type: "audio/ogg".to_string(),
                })
            })
        }
    }

    fn clip(bytes: Vec<u8>) -> AudioClip {
        AudioClip {
            bytes,
            filename: "voice.ogg".to_string(),
            mime_type: "audio/ogg".to_string(),
        }
    }

    fn serve_once(
        status: &str,
        content_type: &str,
        body: Vec<u8>,
    ) -> (String, JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let status = status.to_string();
        let content_type = content_type.to_string();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let count = stream.read(&mut buffer).unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..count]);
                let Some(headers_end) = request.windows(4).position(|window| window == b"\r\n\r\n")
                else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..headers_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or_default();
                if request.len() >= headers_end + 4 + content_length {
                    break;
                }
            }
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(&body).unwrap();
            request
        });
        (format!("http://{address}"), handle)
    }

    #[tokio::test]
    async fn provider_boundary_trims_transcripts_and_returns_opus() {
        let voice = Voice::with_provider(Arc::new(FakeProvider));
        assert_eq!(
            voice.transcribe(clip(vec![1])).await.unwrap(),
            "remind me tomorrow"
        );
        assert_eq!(
            voice.synthesize("done").await.unwrap().mime_type,
            "audio/ogg"
        );
    }

    #[tokio::test]
    async fn provider_boundary_rejects_empty_and_oversized_input() {
        let voice = Voice::with_provider(Arc::new(FakeProvider));
        assert!(voice.transcribe(clip(Vec::new())).await.is_err());
        assert!(voice
            .transcribe(clip(vec![0; MAX_AUDIO_BYTES + 1]))
            .await
            .is_err());
        assert!(voice
            .synthesize(&"x".repeat(MAX_SPEECH_CHARS + 1))
            .await
            .is_err());
    }

    #[test]
    fn defaults_use_current_openai_audio_models() {
        assert_eq!(TRANSCRIPTION_MODEL, "gpt-4o-transcribe");
        assert_eq!(SPEECH_MODEL, "gpt-4o-mini-tts");
    }

    #[tokio::test]
    async fn openai_transcription_http_contract_uses_multipart_and_current_model() {
        let (base_url, request) = serve_once(
            "200 OK",
            "application/json",
            br#"{"text":"contract transcript"}"#.to_vec(),
        );
        let provider = OpenAiVoice::with_base_url("test-key".to_string(), base_url);

        let transcript = provider
            .transcribe(clip(b"audio-bytes".to_vec()))
            .await
            .unwrap();
        let request = request.join().unwrap();
        let request_text = String::from_utf8_lossy(&request);

        assert_eq!(transcript, "contract transcript");
        assert!(request_text.starts_with("POST /audio/transcriptions HTTP/1.1"));
        assert!(request_text
            .to_ascii_lowercase()
            .contains("authorization: bearer test-key"));
        assert!(request_text.contains("name=\"model\""));
        assert!(request_text.contains(TRANSCRIPTION_MODEL));
        assert!(request_text.contains("filename=\"voice.ogg\""));
        assert!(request_text.contains("audio-bytes"));
    }

    #[tokio::test]
    async fn openai_speech_http_contract_requests_opus_and_returns_audio() {
        let (base_url, request) = serve_once("200 OK", "application/octet-stream", vec![9, 8, 7]);
        let provider = OpenAiVoice::with_base_url("test-key".to_string(), base_url);

        let output = provider.synthesize("hello").await.unwrap();
        let request = request.join().unwrap();
        let body_start = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .unwrap()
            + 4;
        let payload: Value = serde_json::from_slice(&request[body_start..]).unwrap();

        assert_eq!(output.bytes, vec![9, 8, 7]);
        assert_eq!(payload["model"], SPEECH_MODEL);
        assert_eq!(payload["voice"], SPEECH_VOICE);
        assert_eq!(payload["response_format"], "opus");
        assert_eq!(payload["input"], "hello");
    }

    #[tokio::test]
    async fn openai_http_errors_are_actionable_without_returning_body_content() {
        let (base_url, request) = serve_once(
            "401 Unauthorized",
            "application/json",
            br#"{"error":"secret upstream detail"}"#.to_vec(),
        );
        let provider = OpenAiVoice::with_base_url("bad-key".to_string(), base_url);

        let error = provider
            .transcribe(clip(b"audio".to_vec()))
            .await
            .unwrap_err()
            .to_string();
        request.join().unwrap();

        assert!(error.contains("HTTP 401"));
        assert!(!error.contains("secret upstream detail"));
        assert!(!error.contains("bad-key"));
    }
}
