//! Voice messages to text: audio that comes with a message (a QQ voice
//! message, an email attachment, a Web UI upload) is transcribed by a model
//! with audio input once, when it arrives, and the transcript goes into the
//! message text, so the model and the history only ever see text.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use serde_json::{Value, json};

use crate::attachments::Attachment;
use crate::config::ModelConfig;

/// Largest audio file sent for transcription (about 10 minutes of speech).
pub const MAX_AUDIO_BYTES: usize = 10 * 1024 * 1024;
/// Most audio files transcribed per message.
pub const MAX_AUDIO: usize = 3;

const PROMPT: &str = "Transcribe this voice message word for word, in the language it is spoken in. \
     Reply with only the transcript, no notes or quotes. If nothing is said, reply with: (silence)";

pub struct Transcriber {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    pub model: String,
    /// Set by the user rather than falling back to the main model.
    chosen: bool,
}

impl Transcriber {
    /// Uses `audio_model`, else the main model.
    pub fn new(model: &ModelConfig, audio_model: Option<&str>, api_key: &str) -> Result<Self> {
        let chosen = audio_model.map(str::trim).filter(|m| !m.is_empty());
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(model.request_timeout_secs.max(120)))
            .build()
            .context("cannot build HTTP client for transcription")?;
        Ok(Self {
            http,
            base_url: model.base_url.trim_end_matches('/').to_owned(),
            api_key: api_key.to_owned(),
            model: chosen.unwrap_or(&model.model).to_owned(),
            chosen: chosen.is_some(),
        })
    }

    pub async fn transcribe(&self, data: &[u8], format: &str) -> Result<String> {
        let body = json!({
            "model": self.model,
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": PROMPT},
                {"type": "input_audio", "input_audio": {
                    "data": base64::engine::general_purpose::STANDARD.encode(data),
                    "format": format,
                }},
            ]}],
            "stream": false,
        });
        let response = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .header("X-Title", "OpenClaw")
            .json(&body)
            .send()
            .await
            .context("transcription request failed")?;
        let status = response.status();
        let text = response.text().await.context("transcription interrupted")?;
        let body: Option<Value> = serde_json::from_str(&text).ok();
        let error = body
            .as_ref()
            .and_then(|b| b.pointer("/error/message"))
            .and_then(Value::as_str);
        if !status.is_success() || error.is_some() {
            let message = error
                .map(str::to_owned)
                .unwrap_or_else(|| text.chars().take(300).collect());
            bail!("{} answered HTTP {status}: {message}", self.model);
        }
        let transcript = body
            .as_ref()
            .and_then(|b| b.pointer("/choices/0/message/content"))
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        if transcript.is_empty() {
            bail!("{} returned no transcript", self.model);
        }
        Ok(transcript.to_owned())
    }

    /// Notes for the message text, one per audio file in `files`: its
    /// transcript, or why there is none.
    pub async fn notes(&self, workspace: &Path, files: &[Attachment]) -> Vec<String> {
        let mut notes = Vec::new();
        for (index, file) in files.iter().filter(|f| is_audio(&f.mime)).enumerate() {
            if index >= MAX_AUDIO {
                notes.push(not_transcribed(
                    file,
                    "too many voice messages in one message",
                ));
                continue;
            }
            notes.push(match self.note(workspace, file).await {
                Ok(transcript) => transcribed(file, &transcript),
                Err(err) => {
                    let mut why = format!("{err:#}");
                    if !self.chosen {
                        why.push_str(
                            "; if the main model cannot take audio, set agent.audio_model to one that can",
                        );
                    }
                    eprintln!("voice: cannot transcribe {}: {why}", file.path);
                    not_transcribed(file, &why)
                }
            });
        }
        notes
    }

    async fn note(&self, workspace: &Path, file: &Attachment) -> Result<String> {
        let format = audio_format(&file.mime, &file.name).with_context(|| {
            format!(
                "{} audio is not supported (wav, mp3, ogg, m4a, aac, flac, aiff are)",
                file.mime
            )
        })?;
        if file.bytes as usize > MAX_AUDIO_BYTES {
            bail!("longer than the {} MB limit", MAX_AUDIO_BYTES / 1024 / 1024);
        }
        let data = std::fs::read(workspace.join(&file.path))
            .with_context(|| format!("cannot read {}", file.path))?;
        self.transcribe(&data, format).await
    }
}

pub fn is_audio(mime: &str) -> bool {
    mime.starts_with("audio/")
}

/// The `input_audio` format for a file, `None` when providers take none
/// (QQ's SILK and AMR, for instance).
pub fn audio_format(mime: &str, name: &str) -> Option<&'static str> {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    Some(match (mime, ext.as_str()) {
        ("audio/wav" | "audio/x-wav" | "audio/wave" | "audio/vnd.wave", _) | (_, "wav") => "wav",
        ("audio/mpeg" | "audio/mp3", _) | (_, "mp3") => "mp3",
        ("audio/ogg" | "audio/opus", _) | (_, "ogg" | "oga" | "opus") => "ogg",
        ("audio/mp4" | "audio/x-m4a" | "audio/m4a", _) | (_, "m4a") => "m4a",
        ("audio/aac", _) | (_, "aac") => "aac",
        ("audio/flac" | "audio/x-flac", _) | (_, "flac") => "flac",
        ("audio/aiff" | "audio/x-aiff", _) | (_, "aif" | "aiff") => "aiff",
        _ => return None,
    })
}

/// The note for a transcript QQ (or a model) made.
pub fn transcribed(file: &Attachment, transcript: &str) -> String {
    format!("[Voice message {}, transcribed: {transcript}]", file.path)
}

fn not_transcribed(file: &Attachment, why: &str) -> String {
    format!(
        "[Voice message {} could not be transcribed: {why}]",
        file.path
    )
}

/// The note for a QQ voice message QQ already transcribed.
pub fn qq_transcript(transcript: &str) -> String {
    format!("[Voice message, transcribed by QQ: {}]", transcript.trim())
}

/// `text` followed by the voice notes.
pub fn with_notes(text: &str, notes: &[String]) -> String {
    if notes.is_empty() {
        return text.to_owned();
    }
    let notes = notes.join("\n");
    if text.trim().is_empty() {
        notes
    } else {
        format!("{text}\n\n{notes}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audio(name: &str, mime: &str, bytes: u64) -> Attachment {
        Attachment {
            name: name.into(),
            mime: mime.into(),
            path: format!("inbox/{name}"),
            bytes,
        }
    }

    #[test]
    fn formats_come_from_type_or_name() {
        assert_eq!(audio_format("audio/wav", "x"), Some("wav"));
        assert_eq!(audio_format("audio/mpeg", "x"), Some("mp3"));
        assert_eq!(
            audio_format("application/octet-stream", "v.M4A"),
            Some("m4a")
        );
        assert_eq!(audio_format("audio/ogg", "v.oga"), Some("ogg"));
        assert_eq!(audio_format("audio/amr", "v.amr"), None);
        assert_eq!(audio_format("audio/silk", "v.silk"), None);
    }

    #[test]
    fn notes_join_the_text() {
        assert_eq!(with_notes("hi", &[]), "hi");
        assert_eq!(with_notes("", &["[a]".into()]), "[a]");
        assert_eq!(
            with_notes("hi", &["[a]".into(), "[b]".into()]),
            "hi\n\n[a]\n[b]"
        );
    }

    /// A stand-in for OpenRouter that checks the request and answers.
    async fn server(
        answer: Value,
        status: u16,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<Value>>>) {
        use axum::{Json, Router, routing::post};
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        let app = Router::new().route(
            "/chat/completions",
            post(move |Json(body): Json<Value>| {
                let log = log.clone();
                let answer = answer.clone();
                async move {
                    log.lock().unwrap().push(body);
                    (
                        axum::http::StatusCode::from_u16(status).unwrap(),
                        Json(answer),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), seen)
    }

    fn model(base_url: &str) -> ModelConfig {
        ModelConfig {
            base_url: base_url.into(),
            model: "main/model".into(),
            ..ModelConfig::default()
        }
    }

    #[tokio::test]
    async fn audio_files_are_transcribed_into_notes() {
        let (url, seen) = server(
            json!({"choices": [{"message": {"content": " 明天早上八点叫我起床 \n"}}]}),
            200,
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("inbox")).unwrap();
        std::fs::write(dir.path().join("inbox/v.wav"), b"RIFF....WAVE").unwrap();
        let files = [
            audio("v.wav", "audio/wav", 12),
            audio("p.png", "image/png", 3),
            audio("v.amr", "audio/amr", 5),
        ];
        let transcriber = Transcriber::new(&model(&url), Some(" audio/model "), "k").unwrap();
        let notes = transcriber.notes(dir.path(), &files).await;
        assert_eq!(
            notes,
            [
                "[Voice message inbox/v.wav, transcribed: 明天早上八点叫我起床]".to_owned(),
                "[Voice message inbox/v.amr could not be transcribed: audio/amr audio is not supported (wav, mp3, ogg, m4a, aac, flac, aiff are)]".to_owned(),
            ]
        );
        let request = seen.lock().unwrap()[0].clone();
        assert_eq!(request["model"], "audio/model");
        let part = &request["messages"][0]["content"][1];
        assert_eq!(part["type"], "input_audio");
        assert_eq!(part["input_audio"]["format"], "wav");
        assert_eq!(
            part["input_audio"]["data"],
            base64::engine::general_purpose::STANDARD.encode(b"RIFF....WAVE")
        );
    }

    #[tokio::test]
    async fn a_model_without_audio_input_is_explained() {
        let (url, _) = server(
            json!({"error": {"message": "No endpoints found that support input audio"}}),
            404,
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("inbox")).unwrap();
        std::fs::write(dir.path().join("inbox/v.mp3"), b"ID3").unwrap();
        let transcriber = Transcriber::new(&model(&url), None, "k").unwrap();
        assert_eq!(transcriber.model, "main/model");
        let notes = transcriber
            .notes(dir.path(), &[audio("v.mp3", "audio/mpeg", 3)])
            .await;
        assert!(
            notes[0].contains("No endpoints found that support input audio"),
            "{notes:?}"
        );
        assert!(notes[0].contains("agent.audio_model"), "{notes:?}");
    }
}
