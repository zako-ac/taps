use std::process::Stdio;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio_stream::StreamExt;
use url::Url;
use yt_dlp::Downloader;
use yt_dlp::client::deps::Libraries;
use zakofish4_tap::{
    AttachedMetadata, AudioMetadataSuccessMessage, AudioRequestSuccessMessage, AudioSource,
    AudioStreamSender, TapError, TapHandler,
};
use zakofish4_tap::{AudioCachePolicy, AudioCacheType, AudioMetadata};

/// Shortest audio cache lifetime this tap will ask for, in seconds.
const AUDIO_CACHE_TTL_FLOOR_SECS: u64 = 300;

/// How long a cache entry must outlive the track it holds, in seconds.
const AUDIO_CACHE_TTL_MARGIN_SECS: u64 = 300;

/// How many 20 ms frames short of the video's own duration a stream may end
/// before it is worth saying so.
const SHORTFALL_TOLERANCE_FRAMES: u64 = 50;

/// Frames a 20 ms framing implies for a track this long.
fn expected_frames(duration_secs: Option<f32>) -> Option<u64> {
    duration_secs.map(|d| (d.max(0.0) as f64 * 50.0) as u64)
}

/// How long the audio cache entry may live, in seconds.
///
/// HQ stamps `expire_at = now + ttl` when the request is *made* and commits the
/// entry only when the *transfer ends*, so a TTL shorter than the track itself
/// produces an entry that is already expired the moment it exists: every play
/// then misses the cache and downloads the whole video again. The track's own
/// length plus a margin is the smallest TTL that can ever be hit.
fn audio_cache_ttl_secs(duration_secs: Option<f32>) -> u32 {
    let track = duration_secs.unwrap_or(0.0).max(0.0).ceil() as u64;
    (track + AUDIO_CACHE_TTL_MARGIN_SECS)
        .max(AUDIO_CACHE_TTL_FLOOR_SECS)
        .min(u32::MAX as u64) as u32
}

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

/// yt-dlp → ffmpeg → Opus frames, pushed out as they are produced.
///
/// Nothing here waits for the download: yt-dlp writes to its stdout while
/// ffmpeg demuxes it and hands out 20 ms packets, which go straight to the
/// sender. Reading the download to the end first (which this used to do) delays
/// the first frame by the whole download and leaves the sender's realtime pacer
/// that far behind — it then delivers the backlog as fast as the socket
/// accepts, and that burst is what overruns a sink holding a few seconds of
/// audio. On a long track the result is a stutter every few seconds; on a short
/// one it is invisible.
async fn stream_audio(
    ytdlp_bin: &str,
    url: &str,
    stream: AudioStreamSender,
    duration_secs: Option<f32>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut ytdlp = Command::new(ytdlp_bin)
        .args([
            "--no-playlist",
            "-f",
            "bestaudio",
            // The media goes into a pipe, so a stalled read must not hang the
            // transfer for good and a transient failure must not truncate it.
            "--socket-timeout",
            "15",
            "--retries",
            "10",
            "--fragment-retries",
            "10",
            "-o",
            "-",
            url,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut ytdlp_out = ytdlp.stdout.take().expect("stdout was piped");

    // WebM/Opus (or whatever `bestaudio` picks) → OGG/Opus, 20 ms a packet.
    let mut ffmpeg = Command::new("ffmpeg")
        .args([
            "-v",
            "quiet",
            "-i",
            "pipe:0",
            "-vn",
            "-c:a",
            "libopus",
            // Pinned rather than left to the default: the frame index below is
            // turned into a timestamp by multiplying by 20 ms, and under
            // protofish4 that timestamp is the receiver's buffer key rather
            // than an opaque prefix, so a different frame duration would be
            // audible.
            "-frame_duration",
            "20",
            "-f",
            "ogg",
            "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;

    let mut ffmpeg_in = ffmpeg.stdin.take().expect("stdin was piped");
    let ffmpeg_out = ffmpeg.stdout.take().expect("stdout was piped");

    // Fed from its own task: ffmpeg will not drain stdin while we are not
    // reading stdout, so copying inline would deadlock on anything larger than
    // a pipe buffer. Closing stdin at the end is the end of the input — without
    // it ffmpeg waits for more instead of flushing the tail of the track.
    let pump = tokio::spawn(async move {
        let copied = tokio::io::copy(&mut ytdlp_out, &mut ffmpeg_in).await;
        let _ = ffmpeg_in.shutdown().await;
        copied
    });

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
                if !stream.send_opus_frame(frame_index, data).await {
                    // The sink is gone. Nothing to report — the runtime already
                    // knows, and it owns telling the hub.
                    break;
                }
                frame_index += 1;
            }
            Err(e) => {
                tracing::warn!(%e, "ogg packet read error");
                break;
            }
        }
    }

    match pump.await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => tracing::warn!(%e, "failed to pipe yt-dlp into ffmpeg"),
        Err(e) => tracing::warn!(%e, "the yt-dlp pump task failed"),
    }

    // The download's own outcome only exists here, once the stream has run:
    // a non-zero exit is a truncated track, which used to be silent.
    match ytdlp.wait().await {
        Ok(status) if status.success() => {
            tracing::info!(frames = frame_index, "audio finished");
        }
        Ok(status) => tracing::warn!(
            code = ?status.code(),
            frames = frame_index,
            "yt-dlp exited with an error; the track may be truncated"
        ),
        Err(e) => tracing::warn!(%e, "failed to reap yt-dlp"),
    }

    if let Some(expected) = expected_frames(duration_secs)
        && frame_index + SHORTFALL_TOLERANCE_FRAMES < expected
    {
        tracing::warn!(
            frames = frame_index,
            expected,
            "stream ended short of the video's own duration"
        );
    }

    Ok(())
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
        let cache = AudioCachePolicy {
            cache_type: AudioCacheType::ARHash,
            ttl_seconds: Some(audio_cache_ttl_secs(duration_secs)),
        };

        let ytdlp_bin = self.ytdlp_bin.clone();
        tokio::spawn(async move {
            if let Err(e) = stream_audio(&ytdlp_bin, &url, stream, duration_secs).await {
                tracing::error!(%url, "audio stream failed: {e}");
            }
        });

        Ok(AudioRequestSuccessMessage {
            cache,
            duration_secs,
            metadatas: AttachedMetadata::UseCached,
        })
    }
}
