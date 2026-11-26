use std::io::Cursor;
use std::sync::Arc;
use std::time::{Duration, Instant};
use zakofish4_tap::{
    AttachedMetadata, AudioCachePolicy, AudioCacheType, AudioMetadata, AudioMetadataSuccessMessage,
    AudioRequestSuccessMessage, AudioSource, AudioStreamSender, ProbeResult, TapError, TapHandler,
    encode::decode_and_stream,
};

use crate::tts::TtsPool;

/// The phrase a probe synthesizes.
///
/// Short on purpose: the probe runs every 30 seconds per connection (the hub's
/// `ProbeConfig::default`), on the same pool real requests use, so it must cost
/// roughly nothing. "ok" is still a full pass of the pipeline — script, model,
/// WAV encode — which is what makes the answer worth anything; a probe that
/// skipped the work would answer "healthy" for a wedged engine too.
const PROBE_TEXT: &str = "ok";

/// A probe that has not answered in this long is a failed probe, not a slow
/// one. The hub gives up at 20s (`probe_timeout`), so answering after that is
/// answering into a closed question.
const PROBE_DEADLINE: Duration = Duration::from_secs(15);

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
        // The metadata request is the first half of every message, so this is
        // where "did the tap receive it at all" is answered. The SDK logs the
        // receipt too; this line is the one that proves the tap's own handler
        // was reached, which it has to be if a metadata request is to succeed.
        tracing::info!(source = source.as_str(), "handling audio metadata request");

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

    /// Prove the synthesis pipeline still works, without a sink and without the
    /// network.
    ///
    /// This is the answer the pf3 path never had: the tap held its control
    /// connection, answered heartbeats and answered `AudioRequestSuccess`, and
    /// a wedged synthesis engine was invisible to everything except a silent
    /// listener. The probe runs the same pool real requests run and throws the
    /// audio away.
    async fn probe(&self) -> ProbeResult {
        let started = Instant::now();
        match tokio::time::timeout(
            PROBE_DEADLINE,
            self.pool
                .synthesize(PROBE_TEXT.to_string(), self.lang.clone()),
        )
        .await
        {
            Ok(Ok(bytes)) if bytes.is_empty() => ProbeResult::Failed {
                reason: "probe synthesized 0 bytes".to_string(),
            },
            Ok(Ok(bytes)) => {
                let time_to_first_sample_ms = started.elapsed().as_millis() as u64;
                tracing::debug!(
                    bytes = bytes.len(),
                    time_to_first_sample_ms,
                    "probe synthesized"
                );
                ProbeResult::Ready {
                    time_to_first_sample_ms,
                }
            }
            Ok(Err(e)) => ProbeResult::Failed {
                reason: e.to_string(),
            },
            Err(_) => ProbeResult::Failed {
                reason: format!("probe synthesis exceeded {PROBE_DEADLINE:?}"),
            },
        }
    }
}
