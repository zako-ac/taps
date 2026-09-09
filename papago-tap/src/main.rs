use std::sync::Arc;
use zakofish4_tap::tap;

pub mod papago;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    dotenvy::dotenv().ok();
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    tracing_subscriber::fmt::init();

    let tap_id = std::env::var("PAPAGO_TAP_ID").expect("PAPAGO_TAP_ID env var is required");
    let api_token =
        std::env::var("PAPAGO_API_TOKEN").expect("PAPAGO_API_TOKEN env var is required");
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
        .friendly_name("Papago TTS Tap")
        .api_token(&api_token)
        .selection_weight(1.0);

    if let Some(port) = healthcheck_port {
        builder = builder.healthcheck_port(port);
    }

    builder.run(Arc::new(papago::PapagoTapHandler)).await?;

    Ok(())
}
