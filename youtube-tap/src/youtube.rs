//! The YouTube tap.
//!
//! Three libraries, each doing the one thing the others cannot. [`ysrs`]
//! (innertube, no API key) turns a request into a *video*: a query is searched,
//! a URL or id is described from the player endpoint. [`ydrs`] fetches that
//! video's streams and streams the best audio one straight into ffmpeg.
//! `yt-dlp` is what the tap falls back to when the innertube scrape cannot
//! answer at all — and that is the only reason it is still here.
//!
//! The fallback keeps its whole job. A query innertube could not resolve has no
//! video id to hand ydrs, so yt-dlp both finds and fetches that one, exactly as
//! it did before any of this existed. A video innertube could name but not
//! describe keeps its id, so ydrs still streams it — a gate on ysrs's player
//! endpoint is not a verdict, and ydrs walks its own client chain against it.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::{Child, ChildStdout, Command};
use tokio_stream::StreamExt;
use url::Url;
use ydrs::{AsyncTarget, DownloadOptions, PlayableVideo};
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

pub struct YouTubeTapHandler {
    downloader: Arc<Downloader>,
    ytdlp_bin: String,
    /// The transport every innertube call goes out on.
    http: reqwest::Client,
    /// The `hl`/`gl` sent with every innertube call. `NONE_LOCALE` sends
    /// neither, so YouTube answers with the locale its own side of the
    /// connection suggests.
    locale: Locale,
}

impl YouTubeTapHandler {
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
                    // search, not the request. With no id to hand ydrs, yt-dlp
                    // both finds this one and fetches it.
                    tracing::warn!(
                        query,
                        %error,
                        "youtube search failed, handing the query to yt-dlp"
                    );
                    let target = format!("ytsearch:{query}");
                    let info = self.ytdlp_info(&target).await?;
                    Ok(Resolved {
                        download: Download::Ytdlp { target },
                        info,
                    })
                }
            },
        }
    }

    /// Describe a video innertube named, falling back to yt-dlp's description
    /// when a gate — rather than the video itself — is what answered.
    async fn resolve_video(&self, id: &str) -> Result<Resolved, TapError> {
        let download = Download::Innertube { id: id.to_string() };

        match self.innertube_video(id).await {
            Ok(info) => Ok(Resolved { download, info }),
            Err(InnertubeMiss::Gone(reason)) => Err(TapError::Permanent(reason)),
            Err(InnertubeMiss::Uncertain(reason)) => {
                tracing::warn!(
                    id,
                    reason,
                    "innertube could not describe the video, asking yt-dlp"
                );
                let info = self.ytdlp_info(&watch_url(id)).await?;
                Ok(Resolved { download, info })
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
                // what answered, and ydrs's own clients may still get through.
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
    async fn ytdlp_info(&self, target: &str) -> Result<VideoInfo, TapError> {
        let video = self
            .downloader
            .fetch_video_infos(target)
            .await
            .map_err(|error| TapError::Retriable(error.to_string()))?;

        Ok(VideoInfo {
            title: Some(video.title.clone()),
            channel: video.channel.clone(),
            duration_secs: video.duration.map(|duration| duration as f32),
            // yt-dlp's video infos carry no thumbnail this tap uses; the
            // innertube path is where the artwork comes from.
            thumbnail: None,
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

/// What to play, and what plays it.
#[derive(Debug)]
enum Download {
    /// The video innertube named — ydrs streams its best audio into ffmpeg.
    Innertube { id: String },
    /// What yt-dlp is handed when innertube could not name the video at all: a
    /// `ytsearch:` query, which yt-dlp resolves and downloads itself.
    Ytdlp { target: String },
}

/// A request, resolved into something that can be played and something the hub
/// can show.
#[derive(Debug)]
struct Resolved {
    download: Download,
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

/// Where decoded Opus frames go — the tap's audio sink, or a test's tally.
#[async_trait::async_trait]
trait FrameSink: Send {
    /// Push one 20 ms frame; `false` once the sink has gone away.
    async fn send_frame(&self, frame_index: u64, data: bytes::Bytes) -> bool;
}

#[async_trait::async_trait]
impl FrameSink for AudioStreamSender {
    async fn send_frame(&self, frame_index: u64, data: bytes::Bytes) -> bool {
        self.send_opus_frame(frame_index, data).await
    }
}

/// Stream a video's best audio-only stream through ffmpeg and into the sink.
///
/// Nothing waits for the whole track. ydrs writes into ffmpeg's stdin in
/// adaptive blocks while the frames ffmpeg produces are read off its stdout and
/// pushed on, so the first frame is out as soon as the first bytes are — and a
/// five-minute track never has to exist anywhere in one piece.
///
/// Streaming into a pipe gives up one thing a file would have given us: when a
/// transfer dies mid-download and the retry is answered with the whole body
/// instead of the range that resumes it, ydrs wants to rewind the target and
/// start over, and a pipe cannot be rewound. That is a mid-download connection
/// drop *and* a server that ignores `Range` at once; the failure is reported
/// rather than swallowed.
async fn stream_with_ydrs<S: FrameSink>(id: &str, sink: S) -> Result<(), TapError> {
    let video = PlayableVideo::afetch(id).await.map_err(|error| {
        TapError::Retriable(format!("ydrs could not fetch the streams: {error}"))
    })?;

    let format = video
        .best_audio()
        .ok_or_else(|| {
            TapError::Permanent(format!(
                "no audio-only stream for {} (client {}); live and manifest-only \
                 streams are out of ydrs's scope",
                video.video_id, video.client
            ))
        })?
        .clone();
    let itag = format.itag;

    let mut ffmpeg = spawn_ffmpeg()?;
    let ffmpeg_in = ffmpeg.stdin.take().expect("ffmpeg stdin was piped");
    let ffmpeg_out = ffmpeg.stdout.take().expect("ffmpeg stdout was piped");

    // The download runs in a task of its own: ffmpeg will not drain its stdin
    // while nobody is reading its stdout, so the two halves have to move at the
    // same time or neither moves at all.
    let download = tokio::spawn(async move {
        format
            .adownload(AsyncTarget::stream(ffmpeg_in), &DownloadOptions::default())
            .await
            .map_err(|error| TapError::Retriable(format!("ydrs download failed: {error}")))
    });

    let (frames, sink_gone) = forward_opus_frames(ffmpeg_out, &sink).await;
    let _ = ffmpeg.wait().await;

    // The listener left. The runtime has already seen the sink go and owns
    // telling the hub; the transfer is simply abandoned.
    if sink_gone {
        download.abort();
        tracing::info!(itag, frames, "the sink went away mid-track");
        return Ok(());
    }

    let download = download
        .await
        .map_err(|error| TapError::Retriable(format!("the download task failed: {error}")))?;

    match &download {
        Ok(result) => tracing::info!(
            itag,
            bytes = result.bytes_written,
            seconds = result.elapsed_seconds,
            frames,
            "ydrs streamed a track"
        ),
        Err(error) => tracing::warn!(itag, frames, %error, "the track ended early"),
    }

    download.map(|_| ())
}

/// ffmpeg, reading an encoded stream on stdin and writing Ogg/Opus to stdout.
///
/// The arguments are the SDK's `decode_and_stream` arguments, down to the
/// pinned 20 ms frame duration: the frame index becomes a timestamp by
/// multiplying by 20 ms, and under Zako4 that timestamp is the receiver's
/// jitter-buffer key, so a different frame duration would be audible rather
/// than a harmless mismatch.
fn spawn_ffmpeg() -> Result<Child, TapError> {
    Command::new("ffmpeg")
        .args([
            "-v",
            "quiet",
            "-i",
            "pipe:0",
            "-vn",
            "-c:a",
            "libopus",
            "-frame_duration",
            "20",
            "-f",
            "ogg",
            "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        // A track abandoned mid-way must not leave ffmpeg behind writing into a
        // pipe nobody will read again.
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| TapError::Retriable(format!("failed to spawn ffmpeg: {error}")))
}

/// Push ffmpeg's Opus frames into the sink as they arrive.
///
/// The SDK's `decode_and_stream` loop, written against a pipe instead of a
/// `Cursor<Vec<u8>>` — the helper a tap is normally handed takes the whole file,
/// which is exactly what streaming must not do. Framing, header skipping and
/// the timestamp rule are the SDK's.
///
/// Returns how many frames went out, and whether the sink went away first.
async fn forward_opus_frames<S: FrameSink>(ffmpeg_out: ChildStdout, sink: &S) -> (u64, bool) {
    let mut packets = ogg::reading::async_api::PacketReader::new(ffmpeg_out);
    let mut frame_index = 0u64;

    while let Some(result) = packets.next().await {
        match result {
            Ok(packet) => {
                // Header packets, not audio: counting them would shift every
                // timestamp that follows.
                if packet.data.starts_with(b"OpusHead") || packet.data.starts_with(b"OpusTags") {
                    continue;
                }
                let data = bytes::Bytes::copy_from_slice(&packet.data);
                if !sink.send_frame(frame_index, data).await {
                    return (frame_index, true);
                }
                frame_index += 1;
            }
            Err(error) => {
                tracing::warn!(%error, "ogg packet read error");
                break;
            }
        }
    }

    (frame_index, false)
}

/// Download whatever yt-dlp resolved — the fallback path — and push its Opus
/// frames to the sink.
///
/// This one still buffers: it is the path that existed before any of this, and
/// it is here for one request shape (a query innertube could not search) rather
/// than for every track.
async fn stream_with_ytdlp(
    ytdlp_bin: &str,
    target: &str,
    stream: AudioStreamSender,
) -> Result<(), TapError> {
    // Spawn yt-dlp — outputs native format (WebM/Opus) to stdout
    let mut ytdlp = Command::new(ytdlp_bin)
        .args(["--no-playlist", "-f", "bestaudio", "-o", "-", target])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| TapError::Retriable(format!("failed to spawn yt-dlp: {error}")))?;

    let mut ytdlp_out = ytdlp.stdout.take().expect("stdout was piped");

    // Read all bytes from yt-dlp
    let mut audio_bytes = Vec::new();
    ytdlp_out
        .read_to_end(&mut audio_bytes)
        .await
        .map_err(|error| TapError::Retriable(format!("failed to read yt-dlp output: {error}")))?;

    let _ = ytdlp.wait().await;
    tracing::info!(bytes = audio_bytes.len(), "yt-dlp downloaded");

    decode_and_stream(std::io::Cursor::new(audio_bytes), stream)
        .await
        .map_err(|error| TapError::Retriable(error.to_string()))
}

#[async_trait::async_trait]
impl TapHandler for YouTubeTapHandler {
    async fn handle_audio_metadata_request(
        &self,
        source: AudioSource,
    ) -> Result<AudioMetadataSuccessMessage, TapError> {
        let resolved = self.resolve(source.as_str()).await?;
        tracing::info!(download = ?resolved.download, "resolved metadata request");

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
        if let Download::Innertube { id } = &resolved.download {
            metadatas.push(AudioMetadata::Url(watch_url(id)));
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
        let duration_secs = resolved.info.duration_secs;
        let download = resolved.download;
        tracing::info!(?download, "received audio request");

        let ytdlp_bin = self.ytdlp_bin.clone();
        tokio::spawn(async move {
            let result = match download {
                Download::Innertube { id } => stream_with_ydrs(&id, stream).await,
                Download::Ytdlp { target } => stream_with_ytdlp(&ytdlp_bin, &target, stream).await,
            };

            if let Err(error) = result {
                // The transfer is over either way: the sink is gone or the
                // fetch failed, and the SDK owns reporting the outcome to the
                // hub. What is worth saying here is why.
                tracing::error!(%error, "streaming failed");
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
    use std::sync::atomic::{AtomicU64, Ordering};

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

    /// A sink that counts frames instead of sending them anywhere.
    #[derive(Default, Clone)]
    struct Tally {
        frames: Arc<AtomicU64>,
        bytes: Arc<AtomicU64>,
    }

    #[async_trait::async_trait]
    impl FrameSink for Tally {
        async fn send_frame(&self, _frame_index: u64, data: bytes::Bytes) -> bool {
            self.frames.fetch_add(1, Ordering::Relaxed);
            self.bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
            true
        }
    }

    /// Live check of the whole primary path — fetch, stream, decode — against
    /// real YouTube. Not run by default: `cargo test -p youtube-tap
    /// --bin youtube-tap live_stream -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore = "streams from YouTube over the network, and needs ffmpeg"]
    async fn live_streams_a_track_through_ffmpeg() {
        let tally = Tally::default();
        stream_with_ydrs("YE7VzlLtp-4", tally.clone())
            .await
            .expect("the track should stream");

        let frames = tally.frames.load(Ordering::Relaxed);
        let bytes = tally.bytes.load(Ordering::Relaxed);
        println!("{frames} frames, {bytes} bytes of Opus");
        // Big Buck Bunny's audio track runs 9m57s, which is ~29 850 frames of
        // 20 ms. Well over a thousand means the pipe worked end to end.
        assert!(frames > 1_000, "only {frames} frames came out");
    }
}
