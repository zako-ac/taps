use std::io::Cursor;
use zakofish4_tap::{
    AttachedMetadata, AudioMetadataSuccessMessage, AudioRequestSuccessMessage, AudioSource,
    AudioStreamSender, TapError, TapHandler, encode::decode_and_stream,
};
use zakofish4_tap::{AudioCachePolicy, AudioCacheType, AudioMetadata};

pub struct GttsTapHandler;

#[async_trait::async_trait]
impl TapHandler for GttsTapHandler {
    async fn handle_audio_metadata_request(
        &self,
        source: AudioSource,
    ) -> Result<AudioMetadataSuccessMessage, TapError> {
        Ok(AudioMetadataSuccessMessage {
            metadatas: vec![AudioMetadata::Title(source.as_str().to_string())],
            cache: AudioCachePolicy {
                cache_type: AudioCacheType::ARHash,
                ttl_seconds: None, // TTS output is deterministic — cache forever
            },
        })
    }

    async fn handle_audio_request(
        &self,
        source: AudioSource,
        stream: AudioStreamSender,
    ) -> Result<AudioRequestSuccessMessage, TapError> {
        let text = source.as_str().to_string();
        let url = tts_urls::google_translate::url(&text, "ko");
        tracing::info!(url, "fetching Google TTS audio");

        // Download MP3 bytes
        let mp3_bytes = reqwest::get(&url)
            .await
            .map_err(|e| TapError::Retriable(e.to_string()))?
            .bytes()
            .await
            .map_err(|e| TapError::Retriable(e.to_string()))?;

        tokio::spawn(async move {
            // Use SDK's ffmpeg pipeline: MP3 → OGG/Opus
            let cursor = Cursor::new(mp3_bytes.to_vec());
            if let Err(e) = decode_and_stream(cursor, stream).await {
                tracing::error!("decode_and_stream failed: {e}");
            }
        });

        Ok(AudioRequestSuccessMessage {
            cache: AudioCachePolicy {
                cache_type: AudioCacheType::ARHash,
                ttl_seconds: None,
            },
            duration_secs: None, // Google TTS doesn't provide duration
            metadatas: AttachedMetadata::UseCached,
        })
    }
}
