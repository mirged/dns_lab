//! Just enough WAV to move audio in and out of the codec.
//!
//! PCM (8/16/24/32-bit) and 32-bit float files are read, downmixed to mono and
//! resampled to 8 kHz; writing produces a 16-bit mono file. This keeps the
//! phoneless paths testable — `dns_lab voice send --wav clip.wav` needs no
//! microphone and `voice listen --save dir` needs no speaker.

use super::SAMPLE_RATE;

/// A decoded WAV file: interleaved i16 samples.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wav {
    pub rate: u32,
    pub channels: u16,
    pub samples: Vec<i16>,
}

impl Wav {
    /// Frames (one sample per channel) in the file.
    pub fn frames(&self) -> usize {
        if self.channels == 0 {
            0
        } else {
            self.samples.len() / self.channels as usize
        }
    }

    pub fn duration_ms(&self) -> u32 {
        if self.rate == 0 {
            0
        } else {
            (self.frames() as u64 * 1000 / self.rate as u64) as u32
        }
    }
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// Parse a RIFF/WAVE file. Only the first `fmt ` and `data` chunks are used.
pub fn read(bytes: &[u8]) -> Result<Wav, String> {
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a RIFF/WAVE file".to_string());
    }
    let mut pos = 12;
    let mut fmt: Option<(u16, u16, u32, u16)> = None; // tag, channels, rate, bits
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let len = u32_at(bytes, pos + 4) as usize;
        let body_start = pos + 8;
        let body_end = body_start.saturating_add(len).min(bytes.len());
        let body = &bytes[body_start..body_end];
        match id {
            b"fmt " if body.len() >= 16 => {
                let tag = u16_at(body, 0);
                let channels = u16_at(body, 2);
                let rate = u32_at(body, 4);
                // WAVE_FORMAT_EXTENSIBLE: the real format is in the sub-format guid.
                let tag = if tag == 0xFFFE && body.len() >= 26 {
                    u16_at(body, 24)
                } else {
                    tag
                };
                fmt = Some((tag, channels, rate, u16_at(body, 14)));
            }
            b"data" => {
                let Some((tag, channels, rate, bits)) = fmt else {
                    return Err("WAV data chunk before fmt chunk".to_string());
                };
                if channels == 0 || rate == 0 {
                    return Err("WAV fmt chunk has no channels or no rate".to_string());
                }
                let samples = decode_pcm(tag, body, bits, channels)?;
                return Ok(Wav {
                    rate,
                    channels,
                    samples,
                });
            }
            _ => {}
        }
        pos = body_start + len + (len & 1);
    }
    Err("no data chunk in the WAV file".to_string())
}

fn decode_pcm(tag: u16, body: &[u8], bits: u16, channels: u16) -> Result<Vec<i16>, String> {
    let frame_bytes = (bits as usize / 8) * channels as usize;
    if frame_bytes == 0 {
        return Err(format!("WAV has {bits} bits per sample"));
    }
    let usable = body.len() - body.len() % frame_bytes;
    let mut out = Vec::with_capacity(usable / (bits as usize / 8));
    match (tag, bits) {
        (1, 8) => out.extend(body[..usable].iter().map(|b| (*b as i16 - 128) << 8)),
        (1, 16) => {
            let (frames, _) = body[..usable].as_chunks::<2>();
            out.extend(frames.iter().map(|c| i16::from_le_bytes(*c)));
        }
        (1, 24) => {
            // 24 -> 16 bits is a shift: keep the top two bytes of each sample.
            let (frames, _) = body[..usable].as_chunks::<3>();
            out.extend(frames.iter().map(|c| i16::from_le_bytes([c[1], c[2]])));
        }
        (1, 32) => {
            let (frames, _) = body[..usable].as_chunks::<4>();
            out.extend(frames.iter().map(|c| (i32::from_le_bytes(*c) >> 16) as i16));
        }
        (3, 32) => {
            let (frames, _) = body[..usable].as_chunks::<4>();
            out.extend(frames.iter().map(|c| {
                let f = f32::from_le_bytes(*c).clamp(-1.0, 1.0);
                (f * 32767.0) as i16
            }));
        }
        (3, 64) => {
            let (frames, _) = body[..usable].as_chunks::<8>();
            out.extend(frames.iter().map(|c| {
                let f = f64::from_le_bytes(*c).clamp(-1.0, 1.0);
                (f * 32767.0) as i16
            }));
        }
        (tag, bits) => return Err(format!("unsupported WAV format {tag} at {bits} bits")),
    }
    Ok(out)
}

/// A 16-bit mono WAV file at `rate`.
pub fn write(samples: &[i16], rate: u32) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + samples.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes()); // byte rate
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// Streaming linear-interpolation resampler.
///
/// Keeps its phase between calls, so feeding it arbitrary chunk sizes produces
/// the same audio as one big call — which is what both the microphone path (device
/// rate -> 8 kHz) and the speaker path (8 kHz -> device rate) need.
#[derive(Debug, Clone)]
pub struct Resampler {
    step: f64,
    /// Position of the next output sample, in source samples, relative to the
    /// start of the next input chunk.
    pos: f64,
    /// Last sample of the previous chunk, for interpolation across boundaries.
    last: f32,
    started: bool,
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Resampler {
        let step = if to == 0 {
            1.0
        } else {
            from.max(1) as f64 / to.max(1) as f64
        };
        Resampler {
            step,
            pos: 0.0,
            last: 0.0,
            started: false,
        }
    }

    /// Output samples produced by `n` input samples, on average.
    pub fn step(&self) -> f64 {
        self.step
    }

    fn sample_at(&self, input: &[i16], i: isize) -> f32 {
        if i < 0 {
            return if self.started {
                self.last
            } else {
                input[0] as f32
            };
        }
        match input.get(i as usize) {
            Some(v) => *v as f32,
            None => *input.last().unwrap_or(&0) as f32,
        }
    }

    /// Resample `input`, appending to `out`.
    pub fn process(&mut self, input: &[i16], out: &mut Vec<i16>) {
        if input.is_empty() {
            return;
        }
        let len = input.len() as f64;
        while self.pos < len {
            let i = self.pos.floor();
            let frac = (self.pos - i) as f32;
            let a = self.sample_at(input, i as isize);
            let b = self.sample_at(input, i as isize + 1);
            let v = a + (b - a) * frac;
            out.push(v.round().clamp(-32768.0, 32767.0) as i16);
            self.pos += self.step;
        }
        self.pos -= len;
        if self.pos < 0.0 {
            self.pos = 0.0;
        }
        self.last = *input.last().unwrap_or(&0) as f32;
        self.started = true;
    }
}

/// Downmix to mono and resample to [`SAMPLE_RATE`] in one step.
pub fn to_mono_8k(w: &Wav) -> Vec<i16> {
    let ch = w.channels.max(1) as usize;
    let mut mono = Vec::with_capacity(w.frames());
    if ch == 1 {
        mono.extend_from_slice(&w.samples);
    } else {
        for frame in w.samples.chunks_exact(ch) {
            let sum: i32 = frame.iter().map(|s| *s as i32).sum();
            mono.push((sum / ch as i32) as i16);
        }
    }
    if w.rate == SAMPLE_RATE {
        return mono;
    }
    let mut out =
        Vec::with_capacity(mono.len() * SAMPLE_RATE as usize / w.rate.max(1) as usize + 2);
    Resampler::new(w.rate, SAMPLE_RATE).process(&mono, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm16(samples: &[i16], rate: u32, channels: u16) -> Vec<u8> {
        let data_len = (samples.len() * 2) as u32;
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        let byte_rate = rate * channels as u32 * 2;
        out.extend_from_slice(&byte_rate.to_le_bytes());
        out.extend_from_slice(&(channels * 2).to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        for s in samples {
            out.extend_from_slice(&s.to_le_bytes());
        }
        out
    }

    #[test]
    fn writes_and_reads_16_bit_mono() {
        let samples: Vec<i16> = (0..800).map(|i| ((i * 37) % 2000 - 1000) as i16).collect();
        let bytes = write(&samples, SAMPLE_RATE);
        let back = read(&bytes).unwrap();
        assert_eq!(back.rate, SAMPLE_RATE);
        assert_eq!(back.channels, 1);
        assert_eq!(back.samples, samples);
        assert_eq!(back.frames(), 800);
        assert_eq!(back.duration_ms(), 100);
    }

    #[test]
    fn reads_stereo_and_downmixes() {
        // Left channel +1000, right channel -1000 -> mono ~0.
        let mut interleaved = Vec::new();
        for _ in 0..400 {
            interleaved.push(1000i16);
            interleaved.push(-1000i16);
        }
        let wav = read(&pcm16(&interleaved, 8000, 2)).unwrap();
        assert_eq!(wav.channels, 2);
        assert_eq!(wav.frames(), 400);
        assert!(to_mono_8k(&wav).iter().all(|s| s.abs() <= 1));
    }

    #[test]
    fn resamples_48k_to_8k_and_back_to_the_same_length() {
        let mut up = Resampler::new(8000, 48000);
        let mut out = Vec::new();
        up.process(&vec![1000i16; 800], &mut out);
        assert!(
            (out.len() as i64 - 4800).abs() <= 2,
            "8k -> 48k of 800 samples should be ~4800, got {}",
            out.len()
        );

        let mut down = Resampler::new(48000, 8000);
        let mut back = Vec::new();
        down.process(&out, &mut back);
        assert!(
            (back.len() as i64 - 800).abs() <= 2,
            "round trip should return ~800 samples, got {}",
            back.len()
        );
        // A constant signal stays constant through linear interpolation.
        assert!(back.iter().all(|s| (*s - 1000).abs() <= 1));
    }

    #[test]
    fn chunked_resampling_matches_one_shot() {
        let input: Vec<i16> = (0..1000)
            .map(|i| ((i as f32 * 0.1).sin() * 8000.0) as i16)
            .collect();
        let mut one = Vec::new();
        Resampler::new(48000, 8000).process(&input, &mut one);

        let mut many = Vec::new();
        let mut r = Resampler::new(48000, 8000);
        for chunk in input.chunks(137) {
            r.process(chunk, &mut many);
        }
        assert_eq!(one.len(), many.len(), "chunking must not change the count");
        assert_eq!(one, many, "chunking must not change the audio");
    }

    #[test]
    fn rejects_things_that_are_not_wav() {
        assert!(read(b"not a wav file at all").is_err());
        assert!(read(&[]).is_err());
    }
}
