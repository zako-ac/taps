use std::io::Cursor;
use std::sync::Arc;
use zako3_tap_sdk::{
    AttachedMetadata, AudioCachePolicy, AudioCacheType, AudioMetadata, AudioMetadataSuccessMessage,
    AudioRequestSuccessMessage, AudioSource, AudioStreamSender, TapError, TapHandler,
    encode::decode_and_stream,
};

use crate::tts::TtsPool;

pub struct SupertonicTapHandler {
    pub pool: Arc<TtsPool>,
    pub lang: String,
}

#[async_trait::async_trait]
impl TapHandler for SupertonicTapHandler {
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
        let lang = self.lang.clone();
        tracing::info!(text, lang, "synthesizing with Supertonic TTS");

        let wav_bytes = self.pool.synthesize(text, lang).await?;

        tracing::info!("synthesized {} bytes of audio", wav_bytes.len());

        tokio::spawn(async move {
            let cursor = Cursor::new(wav_bytes);
            if let Err(e) = decode_and_stream(cursor, stream).await {
                tracing::error!("decode_and_stream failed: {e}");
            }
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
