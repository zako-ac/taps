pub mod assets;
pub mod engine;
pub mod pool;
pub mod text;
pub mod wav;

pub use engine::{Style, TextToSpeech, load_text_to_speech, load_voice_style};
pub use pool::{TtsOpts, TtsPool};
