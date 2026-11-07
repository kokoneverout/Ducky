use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::{Arc, Mutex};

pub const SAMPLE_RATE: u32 = 16_000;
pub const MAX_SECONDS: u64 = 180;
const MAX_SAMPLES: usize = SAMPLE_RATE as usize * MAX_SECONDS as usize;

#[derive(Default)]
struct Buffer {
    samples: Vec<i16>,
    error: Option<String>,
}

pub struct Recording {
    stream: cpal::Stream,
    buffer: Arc<Mutex<Buffer>>,
}

impl Recording {
    pub fn start() -> Result<Self> {
        let device = cpal::default_host()
            .default_input_device()
            .context("没有可用的默认麦克风，请检查 Windows 麦克风权限")?;
        let supported = device
            .default_input_config()
            .context("读取麦克风格式失败")?;
        let config = supported.config();
        let buffer = Arc::new(Mutex::new(Buffer::default()));
        let stream = match supported.sample_format() {
            cpal::SampleFormat::F32 => build::<f32>(&device, &config, buffer.clone()),
            cpal::SampleFormat::I16 => build::<i16>(&device, &config, buffer.clone()),
            cpal::SampleFormat::U16 => build::<u16>(&device, &config, buffer.clone()),
            other => bail!("麦克风格式暂不支持：{other:?}"),
        }?;
        stream.play().context("启动麦克风失败")?;
        Ok(Self { stream, buffer })
    }

    pub fn finish(self) -> Result<Vec<i16>> {
        drop(self.stream);
        let mut buffer = self.buffer.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(error) = &buffer.error {
            bail!("录音设备出错：{error}");
        }
        Ok(std::mem::take(&mut buffer.samples))
    }
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    buffer: Arc<Mutex<Buffer>>,
) -> Result<cpal::Stream>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    let channels = config.channels as usize;
    let mut resampler = Resampler::new(config.sample_rate.0);
    let errors = buffer.clone();
    device
        .build_input_stream(
            config,
            move |data: &[T], _| {
                let mut buffer = buffer.lock().unwrap_or_else(|e| e.into_inner());
                for frame in data.chunks_exact(channels) {
                    let mono =
                        frame.iter().map(|s| s.to_sample::<f32>()).sum::<f32>() / channels as f32;
                    let mono = if mono.is_finite() { mono } else { 0.0 };
                    resampler.push(mono, &mut buffer.samples);
                }
            },
            move |error| {
                errors.lock().unwrap_or_else(|e| e.into_inner()).error = Some(error.to_string());
            },
            None,
        )
        .context("打开麦克风失败")
}

/// Linear interpolation retains timing across CPAL callback boundaries.
struct Resampler {
    step: f64,
    next: f64,
    index: u64,
    previous: f32,
}

impl Resampler {
    fn new(rate: u32) -> Self {
        Self {
            step: rate as f64 / SAMPLE_RATE as f64,
            next: 0.0,
            index: 0,
            previous: 0.0,
        }
    }

    fn push(&mut self, current: f32, out: &mut Vec<i16>) {
        while self.next <= self.index as f64 && out.len() < MAX_SAMPLES {
            let fraction = (self.next - self.index as f64 + 1.0).clamp(0.0, 1.0) as f32;
            let sample = self.previous + (current - self.previous) * fraction;
            out.push((sample.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16);
            self.next += self.step;
        }
        self.previous = current;
        self.index += 1;
    }
}

pub fn wav(samples: &[i16]) -> Vec<u8> {
    let bytes = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + bytes as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + bytes).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    out.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&bytes.to_le_bytes());
    for sample in samples {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires a real default Windows microphone; captures locally without uploading"]
    fn captures_default_microphone_locally() {
        let recording = Recording::start().unwrap();
        std::thread::sleep(std::time::Duration::from_secs(2));
        let samples = recording.finish().unwrap();
        assert!(
            samples.len() >= SAMPLE_RATE as usize,
            "Microphone delivered less than one second of audio"
        );
        assert!(samples.len() <= SAMPLE_RATE as usize * 3);
    }

    #[test]
    fn converts_device_rates_to_one_second_of_pcm() {
        for rate in [8_000, 16_000, 44_100, 48_000, 96_000] {
            let mut resampler = Resampler::new(rate);
            let mut samples = Vec::new();
            for _ in 0..rate {
                resampler.push(0.5, &mut samples);
            }
            assert!((samples.len() as isize - 16_000).abs() <= 2);
            assert!(samples.iter().all(|s| (*s - 16_384).abs() <= 1));
        }
    }

    #[test]
    fn wav_contains_correct_header_and_pcm() {
        let bytes = wav(&[-123, 456]);
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(&bytes[8..16], b"WAVEfmt ");
        assert_eq!(
            u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
            16_000
        );
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 4);
        assert_eq!(&bytes[44..], &[133, 255, 200, 1]);
    }

    #[test]
    fn recording_buffer_is_bounded() {
        let mut resampler = Resampler::new(SAMPLE_RATE);
        let mut samples = vec![0; MAX_SAMPLES - 1];
        for _ in 0..100 {
            resampler.push(0.0, &mut samples);
        }
        assert_eq!(samples.len(), MAX_SAMPLES);
    }
}
