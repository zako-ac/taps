//! The YouTube tap.
//!
//! Two libraries, each covering what the other cannot: [`ysrs`] (scraping
//! innertube directly, no API key) is what turns a request into a *video* — a
//! query is searched, a URL or id is described from the player endpoint — and
//! `yt-dlp` is what fetches the audio.
//!
//! yt-dlp is also the fallback for the first half. The innertube scrape is the
//! part that YouTube can break, so a call that fails, or a page that no longer
//! parses, falls back to handing yt-dlp what it used to be handed (`ytsearch:`
//! for a query, the URL for a URL). A broken scrape then costs the tap its
//! resolution, not its ability to play.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::Command;
use url::Url;
use ysrs::utils::{Locale, NONE_LOCALE, SearchMode};
use ysrs::{Search, Video, VideoState};
use yt_dlp::Downloader;
use yt_dlp::client::deps::Libraries;
use zakofish4_tap::{
    AttachedMetadata, AudioMetadataSuccessMessage, AudioRequestSuccessMessage, AudioSource,
    AudioStreamSender, TapError, TapHandler, encode::decode_and_stream,
};
use zakofish4_tap::{AudioCachePolicy, AudioCacheType, AudioMetadata};

/// How long one innertube call may take before it is given up on, in seconds.
///
/// ysrs builds a 5-second-per-phase client of its own when it is not handed
/// one. The tap hands it this instead, so an innertube call has one budget
/// rather than one per phase, and a slow call lands on the yt-dlp fallback
/// instead of on the user.
const INNERTUBE_TIMEOUT_SECS: u64 = 10;

pub struct YtdlTapHandler {
    downloader: Arc<Downloader>,
    ytdlp_bin: String,
    /// The transport every innertube call goes out on.
    http: reqwest::Client,
    /// The `hl`/`gl` sent with every innertube call. `NONE_LOCALE` sends
    /// neither, so YouTube answers with the locale its own side of the
    /// connection suggests.
    locale: Locale,
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
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(INNERTUBE_TIMEOUT_SECS))
            .build()?;

        Ok(Self {
            downloader: Arc::new(downloader),
            ytdlp_bin,
            http,
            locale: NONE_LOCALE,
        })
    }

    /// The video a request names, searching YouTube when it names a query.
    ///
    /// Every path ends in a [`Resolved`]: the innertube one where it can answer
    /// and the yt-dlp one where it cannot.
    async fn resolve(&self, ars: &str) -> Result<Resolved, TapError> {
        match resolve_ars(ars) {
            ArsResolution::Video(id) => self.resolve_video(&id).await,
            ArsResolution::Search(query) => match self.search_video(&query).await {
                Ok(id) => self.resolve_video(&id).await,
                Err(error) => {
                    // The search is the new part; failing it must cost us the
                    // search, not the request.
                    tracing::warn!(
                        query,
                        %error,
                        "youtube search failed, handing the query to yt-dlp"
                    );
                    self.ytdlp_resolved(format!("ytsearch:{query}")).await
                }
            },
        }
    }

    /// Describe a video innertube named, falling back to yt-dlp when a gate —
    /// rather than the video itself — is what answered.
    async fn resolve_video(&self, id: &str) -> Result<Resolved, TapError> {
        let url = watch_url(id);

        match self.innertube_video(id).await {
            Ok(info) => Ok(Resolved {
                download_url: url,
                info,
            }),
            Err(InnertubeMiss::Gone(reason)) => Err(TapError::Permanent(reason)),
            Err(InnertubeMiss::Uncertain(reason)) => {
                tracing::warn!(
                    id,
                    reason,
                    "innertube could not describe the video, asking yt-dlp"
                );
                self.ytdlp_resolved(url).await
            }
        }
    }

    /// Search YouTube and take the first video result.
    ///
    /// `SearchMode::Video` keeps channels and playlists out of the answer — an
    /// audio request cannot play either of them.
    async fn search_video(&self, query: &str) -> Result<String, TapError> {
        let search = Search::asearch(
            query,
            Some(SearchMode::Video),
            self.locale,
            Some(&self.http),
        )
        .await
        .map_err(|error| TapError::Retriable(format!("youtube search failed: {error}")))?;

        search
            .first_video()
            .map(|video| video.id.clone())
            .ok_or_else(|| TapError::Permanent(format!("no video matches {query:?}")))
    }

    /// Describe a video from the innertube player endpoint — no API key, no
    /// download, and a verdict for the videos that cannot be played at all.
    async fn innertube_video(&self, id: &str) -> Result<VideoInfo, InnertubeMiss> {
        let (video, state) = Video::aget(id, self.locale, Some(&self.http))
            .await
            .map_err(|error| InnertubeMiss::Uncertain(error.to_string()))?;

        let Some(video) = video else {
            return Err(match state {
                // Not a verdict but a gate: the anonymous player endpoint is
                // what answered, and yt-dlp's own clients may still get through.
                VideoState::BotDetection
                | VideoState::LoginRequired
                | VideoState::Unavailable
                | VideoState::RecordingUnavailable => {
                    InnertubeMiss::Uncertain(format!("the player endpoint answered {state}"))
                }
                // Gone for good — no downloader brings these back.
                _ => InnertubeMiss::Gone(format!("the video is not available ({state})")),
            });
        };

        Ok(VideoInfo {
            title: Some(video.title.clone()),
            channel: (!video.channel_name.is_empty()).then(|| video.channel_name.clone()),
            duration_secs: (video.length_seconds > 0).then_some(video.length_seconds as f32),
            thumbnail: video
                .get_highest_res_thumbnail()
                .map(|thumbnail| thumbnail.url.clone()),
        })
    }

    /// Describe whatever yt-dlp would download, and hand back what to download.
    ///
    /// This is the path a request took before innertube was in the picture: it
    /// resolves a `ytsearch:` query as well as a URL, and it is yt-dlp's own
    /// answer that is spoken when innertube had none.
    async fn ytdlp_resolved(&self, download_url: String) -> Result<Resolved, TapError> {
        let video = self
            .downloader
            .fetch_video_infos(&download_url)
            .await
            .map_err(|error| TapError::Retriable(error.to_string()))?;

        Ok(Resolved {
            info: VideoInfo {
                title: Some(video.title.clone()),
                channel: video.channel.clone(),
                duration_secs: video.duration.map(|duration| duration as f32),
                // yt-dlp's video infos carry no thumbnail this tap uses; the
                // innertube path is where the artwork comes from.
                thumbnail: None,
            },
            download_url,
        })
    }
}

/// What a request string names.
enum ArsResolution {
    /// A YouTube video the string pointed at.
    Video(String),
    /// A free-text query to search YouTube for.
    Search(String),
}

/// Resolves an AudioSource to what it names.
/// - `youtu.be/<id>` → that video
/// - `*.youtube.com/watch?v=<id>` → that video
/// - `*.youtube.com/shorts/<id>` → that video
/// - anything else → a query to search for
fn resolve_ars(ars: &str) -> ArsResolution {
    if let Ok(url) = Url::parse(ars) {
        let host = url.host_str().unwrap_or("");
        let is_youtube = host == "youtube.com" || host.ends_with(".youtube.com");

        let id = if host == "youtu.be" {
            url.path().trim_start_matches('/').to_string()
        } else if is_youtube && url.path().starts_with("/shorts/") {
            url.path().trim_start_matches("/shorts/").to_string()
        } else if is_youtube && url.path() == "/watch" {
            url.query_pairs()
                .find(|(key, _)| key == "v")
                .map(|(_, value)| value.into_owned())
                .unwrap_or_default()
        } else {
            String::new()
        };

        if !id.is_empty() {
            return ArsResolution::Video(id);
        }
    }

    ArsResolution::Search(ars.to_string())
}

/// The canonical watch URL of a video id.
fn watch_url(id: &str) -> String {
    format!("https://www.youtube.com/watch?v={id}")
}

/// A video, as far as it could be described.
#[derive(Debug, Default)]
struct VideoInfo {
    title: Option<String>,
    channel: Option<String>,
    duration_secs: Option<f32>,
    thumbnail: Option<String>,
}

/// A request, resolved into something yt-dlp can download and something the hub
/// can show.
#[derive(Debug)]
struct Resolved {
    /// What yt-dlp is handed: a watch URL, or a `ytsearch:` query when only the
    /// fallback could name the request.
    download_url: String,
    info: VideoInfo,
}

/// Why innertube did not describe a video.
enum InnertubeMiss {
    /// A verdict rather than a gate: the video is gone, and no downloader gets
    /// it back.
    Gone(String),
    /// A gate, or a call that did not go through. yt-dlp may still do better,
    /// so the caller falls back to it rather than failing the request.
    Uncertain(String),
}

#[async_trait::async_trait]
impl TapHandler for YtdlTapHandler {
    async fn handle_audio_metadata_request(
        &self,
        source: AudioSource,
    ) -> Result<AudioMetadataSuccessMessage, TapError> {
        let resolved = self.resolve(source.as_str()).await?;
        tracing::info!(url = %resolved.download_url, "resolved metadata request");

        let mut metadatas = Vec::new();
        if let Some(title) = &resolved.info.title {
            metadatas.push(AudioMetadata::Title(title.clone()));
        }
        if let Some(channel) = &resolved.info.channel {
            metadatas.push(AudioMetadata::Artist(channel.clone()));
        }
        if let Some(thumbnail) = &resolved.info.thumbnail {
            metadatas.push(AudioMetadata::ImageUrl(thumbnail.clone()));
        }
        // A query resolves to one concrete video now, so its URL is worth
        // attaching for a query too. Only the yt-dlp fallback has nothing to
        // name — there the video is still resolved by yt-dlp at play time.
        if !resolved.download_url.starts_with("ytsearch:") {
            metadatas.push(AudioMetadata::Url(resolved.download_url.clone()));
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
        let resolved = self.resolve(source.as_str()).await?;
        let url = resolved.download_url;
        let duration_secs = resolved.info.duration_secs;
        tracing::info!(url, "received audio request");

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
            let cursor = std::io::Cursor::new(audio_bytes);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The video id a request string names, or `None` when it is a query.
    fn video(ars: &str) -> Option<String> {
        match resolve_ars(ars) {
            ArsResolution::Video(id) => Some(id),
            ArsResolution::Search(_) => None,
        }
    }

    #[test]
    fn youtube_urls_resolve_to_their_video() {
        assert_eq!(
            video("https://youtu.be/6sFi_F3DJX4").as_deref(),
            Some("6sFi_F3DJX4")
        );
        assert_eq!(
            video("https://www.youtube.com/watch?v=6sFi_F3DJX4").as_deref(),
            Some("6sFi_F3DJX4")
        );
        // a watch URL on any youtube.com host, with the rest of the query kept
        assert_eq!(
            video("https://music.youtube.com/watch?v=6sFi_F3DJX4&list=RD6sFi_F3DJX4").as_deref(),
            Some("6sFi_F3DJX4")
        );
        assert_eq!(
            video("https://www.youtube.com/shorts/6sFi_F3DJX4").as_deref(),
            Some("6sFi_F3DJX4")
        );
    }

    #[test]
    fn anything_else_is_a_query() {
        assert!(matches!(
            resolve_ars("아이유 밤편지"),
            ArsResolution::Search(_)
        ));
        assert!(matches!(
            resolve_ars("https://example.com/watch?v=6sFi_F3DJX4"),
            ArsResolution::Search(_)
        ));
        // a youtu.be URL with no id in it is not a video
        assert!(matches!(
            resolve_ars("https://youtu.be/"),
            ArsResolution::Search(_)
        ));
        // a bare id is a query, exactly as it was before innertube
        assert!(matches!(
            resolve_ars("6sFi_F3DJX4"),
            ArsResolution::Search(_)
        ));
    }
}
