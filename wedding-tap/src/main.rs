use std::{io::Cursor, sync::Arc};
use zakofish4_tap::{
    AttachedMetadata, AudioCachePolicy, AudioCacheType, AudioMetadata, AudioMetadataSuccessMessage,
    AudioRequestSuccessMessage, AudioSource, AudioStreamSender, TapError, TapHandler,
    encode::decode_and_stream, tap,
};

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    dotenvy::dotenv().ok();
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    tracing_subscriber::fmt::init();

    let tap_id = std::env::var("WEDDING_TAP_ID").unwrap();
    let api_token = std::env::var("WEDDING_API_TOKEN").unwrap();
    // One WebSocket to HQ, carrying control only — the audio goes straight to
    // whichever sink HQ nominates, over UDP. The default is the in-cluster
    // address; set this to wss://zako.ac/gateway to run a tap from outside.
    let gateway = std::env::var("ZAKO_GATEWAY_URL")
        .unwrap_or_else(|_| "ws://zako3-hq:8080/gateway".to_string());
    let healthcheck_port = std::env::var("TAP_HEALTHCHECK_PORT").ok().map(|v| {
        v.parse::<u16>()
            .expect("TAP_HEALTHCHECK_PORT must be a valid port number")
    });

    let mut builder = tap()
        .hub(&gateway)
        .tap_id(&tap_id)
        .friendly_name("Wedding TTS Tap")
        .api_token(&api_token)
        .selection_weight(1.0);

    if let Some(port) = healthcheck_port {
        builder = builder.healthcheck_port(port);
    }

    builder.run(Arc::new(WeddingTapHandler)).await?;

    Ok(())
}

pub struct WeddingTapHandler;

#[async_trait::async_trait]
impl TapHandler for WeddingTapHandler {
    async fn handle_audio_metadata_request(
        &self,
        source: AudioSource,
    ) -> Result<AudioMetadataSuccessMessage, TapError> {
        Ok(AudioMetadataSuccessMessage {
            metadatas: vec![AudioMetadata::Title(source.as_str().to_string())],
            cache: AudioCachePolicy {
                cache_type: AudioCacheType::ARHash,
                ttl_seconds: Some(1029_u32), // TTS output is deterministic — cache forever
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

        // Query "c" always plays cursed; otherwise 1/4 random chance
        let cursed = text.trim() == "c";
        let re = text.trim() == "r";
        let mp3_bytes = if cursed {
            include_bytes!("wdcursed.mp3").to_vec()
        } else if re {
            include_bytes!("wdr.ogg").to_vec()
        }
        else{
            include_bytes!("wd.mp3").to_vec()
        };

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
