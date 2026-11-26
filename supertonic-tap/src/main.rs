use std::path::PathBuf;
use std::sync::Arc;
use zako3_tap_sdk::{Transport, tap};

pub mod supertonic;
pub mod tts;

use tts::{TtsOpts, TtsPool, load_text_to_speech, load_voice_style};

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    dotenvy::dotenv().ok();
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    // Explicit filter, not `tracing_subscriber::fmt::init()`.
    //
    // `fmt::init()` installs an `EnvFilter` whenever the `env-filter` feature is
    // in the dependency graph, and an `EnvFilter` built from an unset `RUST_LOG`
    // passes nothing below ERROR. This binary pulls `zako3-telemetry` in through
    // the tap SDK, which enables that feature — so the plain `init()` silently
    // dropped every INFO line, including the one line per request this tap
    // exists to emit, while the pod looked perfectly healthy.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let tap_id = std::env::var("SUPERTONIC_TAP_ID").expect("SUPERTONIC_TAP_ID is required");
    let api_token =
        std::env::var("SUPERTONIC_API_TOKEN").expect("SUPERTONIC_API_TOKEN is required");
    let hub = std::env::var("TAPHUB_ENDPOINT").unwrap_or_else(|_| "api.zako.ac".to_string());
    let server_name = std::env::var("TAPHUB_SERVER_NAME").ok();
    let healthcheck_port = std::env::var("TAP_HEALTHCHECK_PORT").ok().map(|v| {
        v.parse::<u16>()
            .expect("TAP_HEALTHCHECK_PORT must be a valid port number")
    });

    let model_dir = std::env::var("SUPERTONIC_MODEL_DIR")
        .unwrap_or_else(|_| "./assets/supertonic-3".to_string());
    let voice_style_path = std::env::var("SUPERTONIC_VOICE_STYLE")
        .unwrap_or_else(|_| format!("{model_dir}/voice_styles/M1.json"));
    let lang = std::env::var("SUPERTONIC_LANG").unwrap_or_else(|_| "en".to_string());
    let total_step: usize = std::env::var("SUPERTONIC_TOTAL_STEP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);
    let speed: f32 = std::env::var("SUPERTONIC_SPEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1.05);
    let silence_duration: f32 = std::env::var("SUPERTONIC_SILENCE_DUR")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.3);
    let workers: usize = std::env::var("SUPERTONIC_WORKERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2);
    let hf_repo = std::env::var("SUPERTONIC_HF_REPO")
        .unwrap_or_else(|_| "https://huggingface.co/Supertone/supertonic-3".to_string());

    let model_path = PathBuf::from(&model_dir);
    tts::assets::ensure_assets(&model_path, &hf_repo)?;

    let onnx_dir_for_init = model_path.join("onnx").to_string_lossy().into_owned();
    let voice_style_for_init = voice_style_path.clone();
    let pool = Arc::new(TtsPool::spawn(
        workers,
        move |id| {
            let span = tracing::info_span!("worker", worker_id = id);
            let _enter = span.enter();
            let tts = load_text_to_speech(&onnx_dir_for_init)?;
            let style = load_voice_style(std::slice::from_ref(&voice_style_for_init), false)?;
            Ok((tts, style))
        },
        TtsOpts {
            total_step,
            speed,
            silence_duration,
        },
    )?);

    let mut builder = tap()
        .hub(&hub)
        .tap_id(&tap_id)
        .friendly_name("Supertonic TTS Tap")
        .api_token(&api_token)
        .transport(Transport::Protofish3)
        .selection_weight(1.0);

    if let Some(ref sn) = server_name {
        builder = builder.server_name(sn);
    }
    if let Some(port) = healthcheck_port {
        builder = builder.healthcheck_port(port);
    }

    builder
        .run(Arc::new(supertonic::SupertonicTapHandler { pool, lang }))
        .await?;

    Ok(())
}
