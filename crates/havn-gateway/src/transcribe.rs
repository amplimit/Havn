//! Voice notes and audio files, transcribed by the gateway (issue #164),
//! for every channel and the web chat. A file received into an agent's
//! inbox that is audio is transcribed with `[transcription]`, and the
//! transcript is kept next to it (`<file>.txt`), as a document's text is;
//! a message that carries the file brings the agent the transcript.
//!
//! Two ways, set in config.toml:
//!
//! ```toml
//! [transcription]
//! kind        = "endpoint"                  # an OpenAI-compatible /audio/transcriptions
//! base_url    = "https://api.openai.com/v1" # default; Groq, a local whisper.cpp server...
//! model       = "gpt-4o-mini-transcribe"    # default
//! api_key_env = "OPENAI_API_KEY"            # default; read from the gateway's environment
//!
//! # or a listed model that takes audio, on the agent owner's credentials:
//! # kind  = "model"
//! # model = "openrouter:google/gemini-3.8-flash"
//! ```

use serde::{Deserialize, Serialize};

use crate::llm_proxy::{self, AnthropicMessage, AnthropicRequest, Billing};

/// How audio is transcribed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Transcription {
    /// An OpenAI-compatible `/audio/transcriptions` endpoint.
    Endpoint {
        #[serde(default = "default_base_url")]
        base_url: String,
        #[serde(default = "default_model")]
        model: String,
        /// The environment variable holding the endpoint's API key; none
        /// is sent when it is unset or empty (a local server).
        #[serde(default = "default_key_env")]
        api_key_env: String,
    },
    /// A model of `[[llm.models]]` that takes audio, asked for a
    /// transcript on the agent owner's credentials and budget.
    Model { model: String },
}

fn default_base_url() -> String {
    "https://api.openai.com/v1".into()
}

fn default_model() -> String {
    "gpt-4o-mini-transcribe".into()
}

fn default_key_env() -> String {
    "OPENAI_API_KEY".into()
}

/// Largest audio file transcribed.
pub const MAX_AUDIO_BYTES: usize = 20 * 1024 * 1024;
/// Most of a transcript kept, in characters.
pub const MAX_TRANSCRIPT_CHARS: usize = 20_000;
/// How long a transcription may take: within what a channel adapter waits
/// for its upload to be answered.
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);
/// Transcriptions running at once; more wait their turn.
static RUNNING: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

/// The audio format of a file with media type `media_type` and name
/// `name`, as the endpoints and models name it; `None` when it is not
/// audio this transcribes.
pub fn audio_format(media_type: &str, name: &str) -> Option<&'static str> {
    let media_type = media_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let by_type = match media_type.as_str() {
        "audio/ogg" | "audio/opus" | "application/ogg" => Some("ogg"),
        "audio/mpeg" | "audio/mp3" => Some("mp3"),
        "audio/mp4" | "audio/m4a" | "audio/x-m4a" | "audio/aac" => Some("m4a"),
        "audio/wav" | "audio/x-wav" | "audio/wave" => Some("wav"),
        "audio/webm" => Some("webm"),
        "audio/flac" | "audio/x-flac" => Some("flac"),
        _ => None,
    };
    by_type.or_else(|| {
        let extension = name.rsplit_once('.')?.1.to_ascii_lowercase();
        match extension.as_str() {
            "ogg" | "oga" | "opus" => Some("ogg"),
            "mp3" => Some("mp3"),
            "m4a" | "aac" => Some("m4a"),
            "wav" => Some("wav"),
            "weba" => Some("webm"),
            "flac" => Some("flac"),
            _ => None,
        }
    })
}

/// The media type of audio `format`, for a model's audio block.
fn media_type(format: &str) -> &'static str {
    match format {
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "wav" => "audio/wav",
        "webm" => "audio/webm",
        "flac" => "audio/flac",
        _ => "audio/ogg",
    }
}

/// What a model call needs, for [`Transcription::Model`].
pub struct ModelCall<'a> {
    pub db: &'a sqlx::SqlitePool,
    pub keyring: Option<&'a crate::keyring::KeyRing>,
    pub llm: &'a crate::config::LlmConfig,
    pub models: &'a crate::model_catalog::ModelCatalog,
    pub billing: Billing,
    /// The models the agent's policy allows.
    pub allowed: &'a [String],
}

/// Transcribe `bytes`, audio in `format`: the transcript, cut
/// to [`MAX_TRANSCRIPT_CHARS`]; why not, for a log.
pub async fn transcribe(
    how: &Transcription,
    http: &reqwest::Client,
    call: ModelCall<'_>,
    bytes: Vec<u8>,
    format: &str,
) -> Result<String, String> {
    if bytes.len() > MAX_AUDIO_BYTES {
        return Err(format!(
            "the file is over {} MB",
            MAX_AUDIO_BYTES / (1024 * 1024)
        ));
    }
    // The name it is sent with says its format, whatever it was sent as.
    let file = format!("audio.{format}");
    // Waiting for a turn counts within the limit too.
    let work = async {
        let _turn = RUNNING
            .acquire()
            .await
            .map_err(|e| format!("transcription is closed: {e}"))?;
        match how {
            Transcription::Endpoint {
                base_url,
                model,
                api_key_env,
            } => endpoint(http, base_url, model, api_key_env, bytes, &file).await,
            Transcription::Model { model } => by_model(http, call, model, bytes, format).await,
        }
    };
    let text = tokio::time::timeout(TIMEOUT, work)
        .await
        .map_err(|_| format!("transcription took longer than {}s", TIMEOUT.as_secs()))??;
    let text = text.trim();
    if text.is_empty() {
        return Err("the transcript is empty".into());
    }
    Ok(text.chars().take(MAX_TRANSCRIPT_CHARS).collect())
}

async fn endpoint(
    http: &reqwest::Client,
    base_url: &str,
    model: &str,
    api_key_env: &str,
    bytes: Vec<u8>,
    name: &str,
) -> Result<String, String> {
    let url = format!("{}/audio/transcriptions", base_url.trim_end_matches('/'));
    let form = reqwest::multipart::Form::new()
        .text("model", model.to_string())
        .part(
            "file",
            reqwest::multipart::Part::bytes(bytes).file_name(name.to_string()),
        );
    let mut request = http.post(&url).timeout(TIMEOUT).multipart(form);
    if let Ok(key) = std::env::var(api_key_env)
        && !key.is_empty()
    {
        request = request.bearer_auth(key);
    }
    let response = request
        .send()
        .await
        .map_err(|e| format!("the transcription endpoint could not be reached: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        let body: String = body.chars().take(300).collect();
        return Err(format!(
            "the transcription endpoint answered {status}: {body}"
        ));
    }
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("the transcription endpoint's answer is not JSON: {e}"))?;
    body.get("text")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "the transcription endpoint's answer has no text".into())
}

/// What a model is asked.
const ASK: &str = "Transcribe this audio word for word, in the language spoken. Reply with \
                   the transcript only: no quotes, notes or translation. If nothing is said, \
                   reply with nothing.";

async fn by_model(
    http: &reqwest::Client,
    call: ModelCall<'_>,
    model: &str,
    bytes: Vec<u8>,
    format: &str,
) -> Result<String, String> {
    let entry = call.models.resolve(model)?.clone();
    // What each API takes (issue #164): Anthropic no audio, OpenAI's own
    // `input_audio` only WAV and MP3; an endpoint, what its API takes.
    match call
        .llm
        .endpoints
        .api_of(entry.provider())
        .unwrap_or_default()
    {
        "anthropic" => {
            return Err(format!(
                "{} takes no audio: use an OpenRouter or Gemini model that does",
                entry.id
            ));
        }
        "openai" if !matches!(format, "wav" | "mp3") => {
            return Err(format!(
                "{} takes WAV and MP3 audio only, not {format}: use an OpenRouter or Gemini model",
                entry.id
            ));
        }
        _ => {}
    }
    if !llm_proxy::model_allowed(call.allowed, &entry.id) {
        return Err(format!(
            "the transcription model {:?} is not allowed by this agent's policy",
            entry.id
        ));
    }
    let keyring = call
        .keyring
        .ok_or("credential encryption is not configured")?;
    let request = AnthropicRequest {
        model: entry.model().to_string(),
        max_tokens: 4096,
        reasoning: None,
        messages: vec![AnthropicMessage {
            role: "user".into(),
            content: serde_json::json!([
                {"type": "text", "text": ASK},
                {"type": "audio", "source": {
                    "type": "base64",
                    "media_type": media_type(format),
                    "data": llm_proxy::base64(&bytes),
                }},
            ]),
        }],
        system: None,
        temperature: None,
        tools: None,
        system_stable: 0,
    };
    let answer = llm_proxy::complete_with_provider(
        call.db,
        http,
        keyring,
        &call.billing,
        entry.provider(),
        entry.max_output_tokens,
        &request,
        call.llm,
    )
    .await
    .map_err(|e| format!("the transcription model failed: {e}"))?;
    Ok(answer
        .content
        .as_array()
        .into_iter()
        .flatten()
        .filter(|b| b.get("type").and_then(serde_json::Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(serde_json::Value::as_str))
        .collect::<Vec<_>>()
        .join(""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_is_known_by_type_or_name() {
        assert_eq!(audio_format("audio/ogg", "voice.ogg"), Some("ogg"));
        assert_eq!(audio_format("audio/ogg; codecs=opus", "x"), Some("ogg"));
        assert_eq!(
            audio_format("application/octet-stream", "memo.M4A"),
            Some("m4a")
        );
        assert_eq!(audio_format("audio/mpeg", "song"), Some("mp3"));
        assert_eq!(audio_format("video/mp4", "clip.mp4"), None);
        assert_eq!(audio_format("application/pdf", "a.pdf"), None);
    }

    #[test]
    fn the_config_reads_both_kinds() {
        let endpoint: Transcription = toml::from_str("kind = \"endpoint\"\n").expect("endpoint");
        assert_eq!(
            endpoint,
            Transcription::Endpoint {
                base_url: "https://api.openai.com/v1".into(),
                model: "gpt-4o-mini-transcribe".into(),
                api_key_env: "OPENAI_API_KEY".into(),
            }
        );
        let model: Transcription =
            toml::from_str("kind = \"model\"\nmodel = \"openrouter:google/gemini-3.8-flash\"\n")
                .expect("model");
        assert_eq!(
            model,
            Transcription::Model {
                model: "openrouter:google/gemini-3.8-flash".into()
            }
        );
        assert!(toml::from_str::<Transcription>("kind = \"model\"\n").is_err());
        assert!(toml::from_str::<Transcription>("kind = \"whisper\"\n").is_err());
        assert!(
            toml::from_str::<Transcription>("kind = \"endpoint\"\napi_key = \"sk\"\n").is_err(),
            "a key goes in the environment, not the file"
        );
    }
}
