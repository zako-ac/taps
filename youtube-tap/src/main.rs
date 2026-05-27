use std::sync::Arc;
use zako3_tap_sdk::{Transport, tap};

pub mod ytdl;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    dotenvy::dotenv().ok();
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    tracing_subscriber::fmt::init();

    let tap_id = std::env::var("YOUTUBE_TAP_ID").unwrap();
    let api_token = std::env::var("YOUTUBE_API_TOKEN").unwrap();
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
        .friendly_name("YouTube Tap")
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
        .run(Arc::new(ytdl::YtdlTapHandler::new().await?))
        .await?;

    Ok(())
}
