use anyhow::{Context, Result};
use crossbeam_channel::{Sender, bounded};
use std::sync::Arc;
use std::thread;
use tokio::sync::oneshot;
use zako3_tap_sdk::TapError;

use super::engine::{Style, TextToSpeech};
use super::wav::wav_bytes;

#[derive(Clone, Copy, Debug)]
pub struct TtsOpts {
    pub total_step: usize,
    pub speed: f32,
    pub silence_duration: f32,
}

pub struct TtsJob {
    pub text: String,
    pub lang: String,
    pub reply: oneshot::Sender<Result<Vec<u8>, TapError>>,
}

pub struct TtsPool {
    tx: Sender<TtsJob>,
}

impl TtsPool {
    pub fn spawn<I>(workers: usize, init: I, opts: TtsOpts) -> Result<Self>
    where
        I: Fn() -> Result<(TextToSpeech, Style)> + Send + Sync + 'static,
    {
        let workers = workers.max(1);
        let (tx, rx) = bounded::<TtsJob>(workers * 4);
        let init = Arc::new(init);

        for worker_id in 0..workers {
            let rx = rx.clone();
            let init = init.clone();
            thread::Builder::new()
                .name(format!("supertonic-tts-{worker_id}"))
                .spawn(move || {
                    let (mut tts, style) = match init() {
                        Ok(pair) => pair,
                        Err(e) => {
                            tracing::error!("worker {worker_id} init failed: {e:?}");
                            return;
                        }
                    };
                    tracing::info!("supertonic worker {worker_id} ready");

                    while let Ok(job) = rx.recv() {
                        let result = (|| -> Result<Vec<u8>, TapError> {
                            let (wav, _dur) = tts
                                .synthesize(
                                    &job.text,
                                    &job.lang,
                                    &style,
                                    opts.total_step,
                                    opts.speed,
                                    opts.silence_duration,
                                )
                                .map_err(|e| TapError::Retriable(e.to_string()))?;
                            wav_bytes(&wav, tts.sample_rate)
                                .map_err(|e| TapError::Retriable(e.to_string()))
                        })();
                        let _ = job.reply.send(result);
                    }
                    tracing::info!("supertonic worker {worker_id} exiting");
                })
                .context("failed to spawn TTS worker thread")?;
        }

        Ok(TtsPool { tx })
    }

    pub async fn synthesize(&self, text: String, lang: String) -> Result<Vec<u8>, TapError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let job = TtsJob {
            text,
            lang,
            reply: reply_tx,
        };

        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || tx.send(job))
            .await
            .map_err(|e| TapError::Retriable(format!("dispatch task join failed: {e}")))?
            .map_err(|_| TapError::Retriable("TTS pool channel closed".into()))?;

        reply_rx
            .await
            .map_err(|_| TapError::Retriable("TTS worker dropped reply channel".into()))?
    }
}
