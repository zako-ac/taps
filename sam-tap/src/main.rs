use std::sync::Arc;
use zako3_tap_sdk::tap;

pub mod sam;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    dotenvy::dotenv().ok();
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    tracing_subscriber::fmt::init();

    let tap_id = std::env::var("SAM_TAP_ID").unwrap();
    let api_token = std::env::var("SAM_API_TOKEN").unwrap();
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
        .friendly_name("SAM TTS Tap")
        .api_token(&api_token)
        .selection_weight(1.0);

    if let Some(ref sn) = server_name {
        builder = builder.server_name(sn);
    }
    if let Some(port) = healthcheck_port {
        builder = builder.healthcheck_port(port);
    }

    builder.run(Arc::new(sam::SamTapHandler)).await?;

    Ok(())
}
