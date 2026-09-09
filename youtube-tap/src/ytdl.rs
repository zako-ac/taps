use std::io::Cursor;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use url::Url;
use yt_dlp::Downloader;
use yt_dlp::client::deps::Libraries;
use zakofish4_tap::{
    AttachedMetadata, AudioMetadataSuccessMessage, AudioRequestSuccessMessage, AudioSource,
    AudioStreamSender, TapError, TapHandler, encode::decode_and_stream,
};
use zakofish4_tap::{AudioCachePolicy, AudioCacheType, AudioMetadata};

pub struct YtdlTapHandler {
    downloader: Arc<Downloader>,
    ytdlp_bin: String,
}

impl YtdlTapHandler {
    pub async fn new() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let ytdlp_bin =
            std::env::var("YTDLP_BIN").unwrap_or_else(|_| "/usr/local/bin/yt-dlp".to_string());
        let libraries = Libraries::new(
            std::path::PathBuf::from(&ytdlp_bin),
            std::path::PathBuf::from("ffmpeg"),
        );
        let downloader = Downloader::builder(libraries, "/tmp/ytdl-tap")
            .build()
            .await?;
        Ok(Self {
            downloader: Arc::new(downloader),
            ytdlp_bin,
        })
    }
}

struct ARSResolutionResult {
    url: String,
    is_search: bool,
}

/// Resolves an AudioSource to a yt-dlp-compatible string.
/// - `youtu.be/<id>` → `https://youtu.be/<id>`
/// - `*.youtube.com/watch?v=<id>` → `https://www.youtube.com/watch?v=<id>`
/// - anything else → `ytsearch:<string>`
fn resolve_ars(ars: &str) -> ARSResolutionResult {
    let s = ars.to_string();
    if let Ok(url) = Url::parse(&s) {
        let host = url.host_str().unwrap_or("");
        // youtu.be/ID
        if host == "youtu.be" {
            let id = url.path().trim_start_matches('/');
            if !id.is_empty() {
                return ARSResolutionResult {
                    url: format!("https://youtu.be/{id}"),
                    is_search: false,
                };
            }
        }
        // *.youtube.com/shorts/ID
        if (host == "youtube.com" || host.ends_with(".youtube.com"))
            && url.path().starts_with("/shorts/")
        {
            let id = url.path().trim_start_matches("/shorts/");
            if !id.is_empty() {
                return ARSResolutionResult {
                    url: format!("https://www.youtube.com/watch?v={id}"),
                    is_search: false,
                };
            }
        }
        // *.youtube.com/watch?v=ID  (www, music, m, etc.)
        if (host == "youtube.com" || host.ends_with(".youtube.com")) && url.path() == "/watch" {
            if let Some(v) = url
                .query_pairs()
                .find(|(k, _)| k == "v")
                .map(|(_, v)| v.into_owned())
            {
                if !v.is_empty() {
                    return ARSResolutionResult {
                        url: format!("https://www.youtube.com/watch?v={v}"),
                        is_search: false,
                    };
                }
            }
        }
    }
    // Not a recognizable YouTube URL — treat as search query
    ARSResolutionResult {
        url: format!("ytsearch:{s}"),
        is_search: true,
    }
}

#[async_trait::async_trait]
impl TapHandler for YtdlTapHandler {
    async fn handle_audio_metadata_request(
        &self,
        source: AudioSource,
    ) -> Result<AudioMetadataSuccessMessage, TapError> {
        let r = resolve_ars(source.as_str());
        let url = r.url;
        tracing::info!(url, "fetching metadata");

        let video = self
            .downloader
            .fetch_video_infos(&url)
            .await
            .map_err(|e| TapError::Retriable(e.to_string()))?;

        let mut metadatas = vec![AudioMetadata::Title(video.title.clone())];
        if let Some(channel) = &video.channel {
            metadatas.push(AudioMetadata::Artist(channel.clone()));
        }

        if !r.is_search {
            metadatas.push(AudioMetadata::Url(url));
        }

        Ok(AudioMetadataSuccessMessage {
            metadatas,
            cache: AudioCachePolicy {
                cache_type: AudioCacheType::ARHash,
                // 1 month
                ttl_seconds: Some(30 * 24 * 3600),
            },
        })
    }

    async fn handle_audio_request(
        &self,
        source: AudioSource,
        stream: AudioStreamSender,
    ) -> Result<AudioRequestSuccessMessage, TapError> {
        let r = resolve_ars(source.as_str());
        let url = r.url;
        tracing::info!(url, "received audio request");

        let video = self
            .downloader
            .fetch_video_infos(&url)
            .await
            .map_err(|e| TapError::Retriable(e.to_string()))?;

        let duration_secs = video.duration.map(|d| d as f32);

        let ytdlp_bin = self.ytdlp_bin.clone();
        tokio::spawn(async move {
            // Spawn yt-dlp — outputs native format (WebM/Opus) to stdout
            let mut ytdlp = match Command::new(&ytdlp_bin)
                .args(["--no-playlist", "-f", "bestaudio", "-o", "-", &url])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!("failed to spawn yt-dlp: {e}");
                    return;
                }
            };

            let mut ytdlp_out = ytdlp.stdout.take().unwrap();

            // Read all bytes from yt-dlp
            let mut audio_bytes = Vec::new();
            match ytdlp_out.read_to_end(&mut audio_bytes).await {
                Ok(_) => {
                    tracing::info!("downloaded {} bytes", audio_bytes.len());
                }
                Err(e) => {
                    tracing::error!("failed to read yt-dlp output: {e}");
                    return;
                }
            }

            let _ = ytdlp.wait().await;

            // Use SDK's ffmpeg pipeline: WebM/Opus → OGG/Opus
            let cursor = Cursor::new(audio_bytes);
            if let Err(e) = decode_and_stream(cursor, stream).await {
                tracing::error!("decode_and_stream failed: {e}");
            }
        });

        Ok(AudioRequestSuccessMessage {
            cache: AudioCachePolicy {
                cache_type: AudioCacheType::ARHash,
                ttl_seconds: Some(300),
            },
            duration_secs,
            metadatas: AttachedMetadata::UseCached,
        })
    }
}
