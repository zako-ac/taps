use std::{io::Cursor, sync::Arc};
use zako3_tap_sdk::{
    AttachedMetadata, AudioCachePolicy, AudioCacheType, AudioMetadata, AudioMetadataSuccessMessage,
    AudioRequestSuccessMessage, AudioSource, AudioStreamSender, TapError, TapHandler, Transport,
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
    let hub = std::env::var("TAPHUB_ENDPOINT").unwrap_or_else(|_| "api.zako.ac".to_string());
    let server_name = std::env::var("TAPHUB_SERVER_NAME").ok();
    let healthcheck_port = std::env::var("TAP_HEALTHCHECK_PORT").ok().map(|v| {
        v.parse::<u16>()
            .expect("TAP_HEALTHCHECK_PORT must be a valid port number")
    });

    let mut builder = tap()
        .hub(&hub)
        //.cert_pem("cert.pem")
        .tap_id(&tap_id)
        .friendly_name("Wedding TTS Tap")
        .api_token(&api_token)
        .transport(Transport::Protofish3)
        .selection_weight(1.0);

    if let Some(ref sn) = server_name {
        builder = builder.server_name(sn);
    }
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
        let mp3_bytes = include_bytes!("wd.mp3").to_vec();

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
