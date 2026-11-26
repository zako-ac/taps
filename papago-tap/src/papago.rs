use base64::{Engine, engine::general_purpose::STANDARD};
use std::io::Cursor;
use zakofish4_tap::{
    AttachedMetadata, AudioMetadataSuccessMessage, AudioRequestSuccessMessage, AudioSource,
    AudioStreamSender, TapError, TapHandler, encode::decode_and_stream,
};
use zakofish4_tap::{AudioCachePolicy, AudioCacheType, AudioMetadata};

const MAKEURL: &str = "https://papago.naver.com/apis/tts/makeID";
const HMAC_KEY: &str = "v1.9.3_3bdf0438a8";

fn generate_token(timestamp_ms: u64) -> (String, String) {
    let uuid = uuid::Uuid::new_v4().to_string();
    let plain = format!("{uuid}\n{MAKEURL}\n{timestamp_ms}");

    // HMAC-MD5: (key XOR opad) || MD5((key XOR ipad) || message)
    let key = HMAC_KEY.as_bytes();
    let data = plain.as_bytes();

    let block_size = 64;
    let mut ipad = vec![0x36; block_size];
    let mut opad = vec![0x5c; block_size];

    for i in 0..key.len() {
        ipad[i] ^= key[i];
        opad[i] ^= key[i];
    }

    let mut inner = ipad;
    inner.extend_from_slice(data);
    let inner_hash = md5::compute(&inner);

    let mut outer = opad;
    outer.extend_from_slice(&inner_hash.0);
    let result = md5::compute(&outer);

    let sig = STANDARD.encode(result.0);
    let token = format!("PPG {uuid}:{sig}");
    (token, timestamp_ms.to_string())
}

async fn get_voice_id(text: &str, speaker: &str) -> Result<String, TapError> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let (token, ts_str) = generate_token(ts);

    let body = format!(
        "alpha=0&pitch=0&speaker={}&speed=0&text={}",
        speaker,
        urlencoding::encode(text)
    );

    let res = reqwest::Client::new()
        .post(MAKEURL)
        .header(
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:121.0) Gecko/20100101 Firefox/121.0",
        )
        .header("Accept", "application/json")
        .header("Accept-Language", "en")
        .header(
            "Content-Type",
            "application/x-www-form-urlencoded; charset=UTF-8",
        )
        .header("Authorization", token)
        .header("Timestamp", ts_str)
        .header("Pragma", "no-cache")
        .header("Cache-Control", "no-cache")
        .body(body)
        .send()
        .await
        .map_err(|e| TapError::Retriable(e.to_string()))?;

    if !res.status().is_success() {
        let status = res.status().as_u16();
        let body = res.text().await.unwrap_or_default();
        return Err(TapError::Retriable(format!(
            "makeID failed {status}: {body}"
        )));
    }

    let text = res
        .text()
        .await
        .map_err(|e| TapError::Retriable(e.to_string()))?;
    let data: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| TapError::Retriable(format!("JSON parse error: {e}")))?;
    data["id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| TapError::Retriable("missing id in response".into()))
}

async fn get_voice(id: &str) -> Result<bytes::Bytes, TapError> {
    reqwest::get(format!("https://papago.naver.com/apis/tts/{id}"))
        .await
        .map_err(|e| TapError::Retriable(e.to_string()))?
        .bytes()
        .await
        .map_err(|e| TapError::Retriable(e.to_string()))
}

pub struct PapagoTapHandler;

#[async_trait::async_trait]
impl TapHandler for PapagoTapHandler {
    async fn handle_audio_metadata_request(
        &self,
        source: AudioSource,
    ) -> Result<AudioMetadataSuccessMessage, TapError> {
        Ok(AudioMetadataSuccessMessage {
            metadatas: vec![AudioMetadata::Title(source.as_str().to_string())],
            cache: AudioCachePolicy {
                cache_type: AudioCacheType::ARHash,
                ttl_seconds: None,
            },
        })
    }

    async fn handle_audio_request(
        &self,
        source: AudioSource,
        stream: AudioStreamSender,
    ) -> Result<AudioRequestSuccessMessage, TapError> {
        let text = source.as_str().to_string();
        tracing::info!(text, "fetching Papago TTS audio");

        let id = get_voice_id(&text, "kyuri").await?;
        let mp3_bytes = get_voice(&id).await?;

        tokio::spawn(async move {
            let cursor = Cursor::new(mp3_bytes.to_vec());
            if let Err(e) = decode_and_stream(cursor, stream).await {
                tracing::error!("decode_and_stream failed: {e}");
            }
            tracing::info!("finished streaming Papago TTS audio"); //TODO remove
        });

        Ok(AudioRequestSuccessMessage {
            cache: AudioCachePolicy {
                cache_type: AudioCacheType::ARHash,
                ttl_seconds: None,
            },
            duration_secs: None,
            metadatas: AttachedMetadata::UseCached,
        })
    }
}
