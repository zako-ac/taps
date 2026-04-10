use std::sync::Arc;
use zako3_tap_sdk::tap;

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
    let hub = std::env::var("TAPHUB_ENDPOINT").unwrap_or_else(|_| "api.zako.ac".to_string());
    let server_name = std::env::var("TAPHUB_SERVER_NAME").ok();

    let mut builder = tap()
        //.cert_pem("cert.pem")
        .hub(&hub)
        .tap_id(&tap_id)
        .friendly_name("Papago TTS Tap")
        .api_token(&api_token)
        .selection_weight(1.0);

    if let Some(ref sn) = server_name {
        builder = builder.server_name(sn);
    }

    builder.run(Arc::new(papago::PapagoTapHandler)).await?;

    Ok(())
}
