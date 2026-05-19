use anyhow::Result;
use hound::{SampleFormat, WavSpec, WavWriter};
use std::io::Cursor;

pub fn wav_bytes(audio: &[f32], sample_rate: i32) -> Result<Vec<u8>> {
    let spec = WavSpec {
        channels: 1,
        sample_rate: sample_rate as u32,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };

    let mut buf = Cursor::new(Vec::<u8>::new());
    {
        let mut writer = WavWriter::new(&mut buf, spec)?;
        for &sample in audio {
            let clamped = sample.clamp(-1.0, 1.0);
            let val = (clamped * 32767.0) as i16;
            writer.write_sample(val)?;
        }
        writer.finalize()?;
    }

    Ok(buf.into_inner())
}
