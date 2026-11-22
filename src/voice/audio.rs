//! Microphone capture and speaker playback, plus the two threads that glue
//! them to the node.
//!
//! Everything here is behind the `audio` feature: the headless build (a relay
//! on a VPS, a `--no-default-features` server) must not pull in a sound stack.
//! On a phone the microphone needs `RECORD_AUDIO` granted — the manifest
//! declares it, and a failure to open the device is reported as a normal error
//! rather than a panic.

use super::wav::Resampler;
use super::{Encoder, FRAME_MS, Mode, SAMPLE_RATE};
use crate::config::Config;
use crate::net::Node;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How much audio (in milliseconds) the player wants buffered before it starts.
/// One packet's worth: less and every tunnel hiccup is audible, more and the
/// walkie-talkie feels sluggish.
const DEFAULT_TARGET_MS: u32 = 240;

// ---------------------------------------------------------------------------
// capture
// ---------------------------------------------------------------------------

/// An open microphone. Dropping it stops the stream.
pub struct Capture {
    _stream: cpal::Stream,
    pub device: String,
}

/// Open the default input device and call `on_pcm` with 8 kHz mono chunks.
pub fn open_capture<F>(mut on_pcm: F) -> Result<Capture, String>
where
    F: FnMut(&[i16]) + Send + 'static,
{
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| "no microphone: the system has no default input device".to_string())?;
    let device_name = device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "microphone".to_string());
    let supported = device
        .default_input_config()
        .map_err(|e| format!("no usable input format on {device_name}: {e}"))?;
    let format = supported.sample_format();
    let config = supported.config();
    let channels = config.channels.max(1) as usize;
    let rate = config.sample_rate.max(1);

    macro_rules! build {
        ($t:ty, $convert:expr) => {{
            let mut resampler = Resampler::new(rate, SAMPLE_RATE);
            let mut mono: Vec<i16> = Vec::new();
            let mut out: Vec<i16> = Vec::new();
            device
                .build_input_stream(
                    config,
                    move |data: &[$t], _info| {
                        mono.clear();
                        out.clear();
                        if channels == 1 {
                            mono.extend(data.iter().map(|s| $convert(*s)));
                        } else {
                            for frame in data.chunks_exact(channels) {
                                let sum: i32 = frame.iter().map(|s| $convert(*s) as i32).sum();
                                mono.push((sum / channels as i32) as i16);
                            }
                        }
                        resampler.process(&mono, &mut out);
                        if !out.is_empty() {
                            on_pcm(&out);
                        }
                    },
                    move |e| eprintln!("[!] microphone error: {e}"),
                    None,
                )
                .map_err(|e| format!("cannot open {device_name}: {e}"))
        }};
    }

    let stream = match format {
        cpal::SampleFormat::F32 => build!(f32, |s: f32| { (s.clamp(-1.0, 1.0) * 32767.0) as i16 })?,
        cpal::SampleFormat::I16 => build!(i16, |s: i16| s)?,
        cpal::SampleFormat::U16 => build!(u16, |s: u16| (s as i32 - 32_768) as i16)?,
        cpal::SampleFormat::I32 => build!(i32, |s: i32| (s >> 16) as i16)?,
        other => {
            return Err(format!(
                "{device_name} uses an unsupported sample format ({other:?}); \
                 pick a 16-bit or float device"
            ));
        }
    };
    stream
        .play()
        .map_err(|e| format!("cannot start {device_name}: {e}"))?;
    Ok(Capture {
        _stream: stream,
        device: device_name,
    })
}

// ---------------------------------------------------------------------------
// playback
// ---------------------------------------------------------------------------

/// Buffered 8 kHz audio with a start threshold, so playback begins with a
/// cushion instead of dribbling packet by packet.
struct Shared {
    queue: VecDeque<i16>,
    /// Fractional read position, in 8 kHz samples, from the front of `queue`.
    pos: f64,
    playing: bool,
    underruns: u32,
}

pub struct JitterBuffer {
    shared: Mutex<Shared>,
    /// 8 kHz samples consumed per output sample (`8000 / device rate`).
    step: f64,
    target: usize,
    /// Cap on buffered audio; anything beyond is old news and gets dropped.
    max: usize,
}

impl JitterBuffer {
    fn new(target_ms: u32, device_rate: u32) -> JitterBuffer {
        let target = (SAMPLE_RATE * target_ms / 1000) as usize;
        JitterBuffer {
            shared: Mutex::new(Shared {
                queue: VecDeque::new(),
                pos: 0.0,
                playing: false,
                underruns: 0,
            }),
            step: SAMPLE_RATE as f64 / device_rate.max(1) as f64,
            target,
            // Audio arrives faster than real time whenever a transmission was
            // stored rather than spoken live (a WAV send, or a whole press
            // dumped by one mailbox poll). Dropping the oldest audio then would
            // cut the start of the clip, so the cap is only a runaway guard —
            // a minute of speech, not two seconds of it.
            max: SAMPLE_RATE as usize * 60,
        }
    }

    /// Queue decoded speech. The player starts once `target` is buffered.
    pub fn push(&self, pcm: &[i16]) {
        let mut s = self.shared.lock().unwrap();
        s.queue.extend(pcm.iter().copied());
        // Way behind: drop the oldest audio rather than play it late forever.
        while s.queue.len() > self.max {
            s.queue.pop_front();
        }
    }

    /// Drop whatever is buffered (a new transmission is starting).
    pub fn flush(&self) {
        let mut s = self.shared.lock().unwrap();
        s.queue.clear();
        s.pos = 0.0;
        s.playing = false;
    }

    pub fn playing(&self) -> bool {
        self.shared.lock().unwrap().playing
    }

    pub fn buffered_ms(&self) -> u32 {
        let s = self.shared.lock().unwrap();
        (s.queue.len() as u64 * 1000 / SAMPLE_RATE as u64) as u32
    }

    /// Produce exactly `n` device samples: `true` when real audio was produced
    /// rather than padding.
    fn pull(&self, n: usize, out: &mut Vec<f32>) -> bool {
        let mut s = self.shared.lock().unwrap();
        if !s.playing {
            if s.queue.len() < self.target {
                out.resize(n, 0.0);
                return false;
            }
            s.playing = true;
            s.pos = 0.0;
        }
        let mut real = 0usize;
        for _ in 0..n {
            if s.queue.len() < 2 {
                // Ran dry mid-callback: silence, and wait for the next refill.
                out.push(0.0);
                s.playing = false;
                s.pos = 0.0;
                s.underruns += 1;
                continue;
            }
            let i = s.pos as usize;
            let frac = (s.pos - i as f64) as f32;
            let a = s.queue.get(i).copied().unwrap_or(0) as f32;
            let b = s.queue.get(i + 1).copied().unwrap_or(a as i16) as f32;
            out.push((a + (b - a) * frac) / 32768.0);
            real += 1;
            s.pos += self.step;
            let consumed = s.pos as usize;
            if consumed > 0 {
                let drop = consumed.min(s.queue.len());
                s.queue.drain(..drop);
                s.pos -= drop as f64;
                if s.queue.is_empty() {
                    s.pos = 0.0;
                }
            }
        }
        real > 0
    }
}

/// A handle to the player's buffer, cheap to clone and safe to send to the
/// decoding thread.
#[derive(Clone)]
pub struct PlayerHandle(Arc<JitterBuffer>);

impl PlayerHandle {
    pub fn push(&self, pcm: &[i16]) {
        self.0.push(pcm);
    }
    pub fn flush(&self) {
        self.0.flush();
    }
    pub fn playing(&self) -> bool {
        self.0.playing()
    }
    pub fn buffered_ms(&self) -> u32 {
        self.0.buffered_ms()
    }
}

/// An open speaker. Dropping it stops the stream.
pub struct Player {
    _stream: cpal::Stream,
    buffer: Arc<JitterBuffer>,
    pub device: String,
    pub rate: u32,
}

impl Player {
    pub fn open(target_ms: u32) -> Result<Player, String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| "no speaker: the system has no default output device".to_string())?;
        let device_name = device
            .description()
            .map(|d| d.name().to_string())
            .unwrap_or_else(|_| "speaker".to_string());
        let supported = device
            .default_output_config()
            .map_err(|e| format!("no usable output format on {device_name}: {e}"))?;
        let format = supported.sample_format();
        let config = supported.config();
        let channels = config.channels.max(1) as usize;
        let rate = config.sample_rate.max(1);
        let buffer = Arc::new(JitterBuffer::new(target_ms, rate));

        macro_rules! build {
            ($t:ty, $convert:expr) => {{
                let buffer = buffer.clone();
                let mut scratch: Vec<f32> = Vec::new();
                device
                    .build_output_stream(
                        config,
                        move |data: &mut [$t], _info| {
                            scratch.clear();
                            let frames = data.len() / channels;
                            buffer.pull(frames, &mut scratch);
                            for (i, frame) in data.chunks_exact_mut(channels).enumerate() {
                                let v = scratch.get(i).copied().unwrap_or(0.0);
                                for slot in frame.iter_mut() {
                                    *slot = $convert(v);
                                }
                            }
                        },
                        move |e| eprintln!("[!] speaker error: {e}"),
                        None,
                    )
                    .map_err(|e| format!("cannot open {device_name}: {e}"))
            }};
        }

        let stream = match format {
            cpal::SampleFormat::F32 => build!(f32, |v: f32| v)?,
            cpal::SampleFormat::I16 => build!(i16, |v: f32| (v * 32767.0) as i16)?,
            cpal::SampleFormat::U16 => build!(u16, |v: f32| (v * 32767.0 + 32768.0)
                .clamp(0.0, 65535.0)
                as u16)?,
            cpal::SampleFormat::I32 => build!(i32, |v: f32| (v * 2147483520.0) as i32)?,
            other => {
                return Err(format!(
                    "{device_name} uses an unsupported sample format ({other:?})"
                ));
            }
        };
        stream
            .play()
            .map_err(|e| format!("cannot start {device_name}: {e}"))?;
        Ok(Player {
            _stream: stream,
            buffer,
            device: device_name,
            rate,
        })
    }

    pub fn handle(&self) -> PlayerHandle {
        PlayerHandle(self.buffer.clone())
    }
}

// ---------------------------------------------------------------------------
// transmitting
// ---------------------------------------------------------------------------

/// A press of the talk button: the microphone is open and the encoder thread is
/// sending packets as they fill up.
pub struct Recorder {
    capture: Option<Capture>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
    pub stream: u16,
}

impl Recorder {
    /// Open the microphone and start encoding/sending in the background.
    pub fn start(
        node: Arc<Node>,
        to: String,
        mode: Mode,
        frames_per_packet: u8,
    ) -> Result<Recorder, String> {
        let (tx, rx) = mpsc::channel::<Vec<i16>>();
        let capture = open_capture(move |pcm| {
            // If the worker is gone the send just fails; nothing to do here.
            let _ = tx.send(pcm.to_vec());
        })?;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let mut encoder = Encoder::new(mode, frames_per_packet);
        let stream = encoder.stream();

        let worker = std::thread::spawn(move || {
            let mut stats = TalkStats::default();
            // First packet goes out 240 ms into the press, not at the end, so
            // the other side hears you while you are still talking.
            loop {
                match rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(chunk) => send_packets(&node, &to, &mut encoder, &chunk, &mut stats),
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
                if flag.load(Ordering::SeqCst) {
                    break;
                }
            }
            // Whatever is left (and the `last` marker) still goes out.
            while let Ok(chunk) = rx.try_recv() {
                send_packets(&node, &to, &mut encoder, &chunk, &mut stats);
            }
            if let Some(last) = encoder.flush() {
                stats.packets += 1;
                stats.frames += last.frames as u32;
                match node.send_voice(&to, &last) {
                    Ok(q) => stats.queries += q,
                    Err(e) => {
                        stats.errors += 1;
                        node.log(format!("[!] voice tail -> {to} failed: {e}"));
                    }
                }
            }
            stats.skipped = encoder.skipped;
            let ms = stats.frames * FRAME_MS;
            node.log(format!(
                "[>] voice -> {to}: {:.1} s in {} packet(s), {} DNS quer{} ({} silent skipped{})",
                ms as f32 / 1000.0,
                stats.packets,
                stats.queries,
                if stats.queries == 1 { "y" } else { "ies" },
                stats.skipped,
                if stats.errors > 0 {
                    format!(", {} failed", stats.errors)
                } else {
                    String::new()
                }
            ));
        });

        Ok(Recorder {
            capture: Some(capture),
            stop,
            worker: Some(worker),
            stream,
        })
    }

    /// Release the microphone and let the encoder thread flush its tail.
    ///
    /// Deliberately does not join: the final send can take a second on a slow
    /// tunnel and the caller is usually a UI thread.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.capture = None;
        self.worker = None;
    }

    pub fn device(&self) -> &str {
        self.capture
            .as_ref()
            .map(|c| c.device.as_str())
            .unwrap_or("")
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.capture = None;
    }
}

#[derive(Default)]
struct TalkStats {
    packets: usize,
    queries: usize,
    frames: u32,
    skipped: u32,
    errors: usize,
}

fn send_packets(
    node: &Arc<Node>,
    to: &str,
    encoder: &mut Encoder,
    chunk: &[i16],
    stats: &mut TalkStats,
) {
    for packet in encoder.push(chunk) {
        stats.packets += 1;
        stats.frames += packet.frames as u32;
        match node.send_voice(to, &packet) {
            Ok(q) => stats.queries += q,
            Err(e) => {
                stats.errors += 1;
                node.log(format!(
                    "[!] voice packet {} -> {to} failed: {e}",
                    packet.seq
                ));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// receiving
// ---------------------------------------------------------------------------

/// A speaker plus the decoder thread feeding it.
pub struct Listen {
    /// `None` when there is no usable output device but a save directory was
    /// given: the audio still lands on disk.
    pub player: Option<Player>,
    pub receiver: crate::voice::Receiver,
}

impl Listen {
    /// `true` while speech is actually coming out of the speaker.
    pub fn playing(&self) -> bool {
        self.player
            .as_ref()
            .map(|p| p.handle().playing())
            .unwrap_or(false)
    }

    /// Milliseconds of decoded audio waiting to be played.
    pub fn buffered_ms(&self) -> u32 {
        self.player
            .as_ref()
            .map(|p| p.handle().buffered_ms())
            .unwrap_or(0)
    }
}

/// Open the speaker (best effort) and start decoding inbound packets into it.
///
/// Returns an error only when there is neither a speaker nor a `save_dir`: in
/// that case there is nothing to do with the audio at all.
pub fn listen(node: &Arc<Node>, cfg: &Config, save_dir: Option<String>) -> Result<Listen, String> {
    let mut player = None;
    let buffer_ms = if cfg.voice_buffer_ms == 0 {
        DEFAULT_TARGET_MS
    } else {
        cfg.voice_buffer_ms
    };
    let sink = match Player::open(buffer_ms) {
        Ok(p) => {
            node.log(format!(
                "[+] voice playback: {} at {} Hz ({} ms buffer)",
                p.device, p.rate, buffer_ms
            ));
            let handle = p.handle();
            let flush_handle = handle.clone();
            let sink = crate::voice::Sink {
                push: handle.sink(),
                flush: Some(std::sync::Arc::new(move || flush_handle.flush())),
            };
            player = Some(p);
            Some(sink)
        }
        Err(e) => {
            if save_dir.is_none() {
                return Err(e);
            }
            node.log(format!("[~] no speaker, saving voice to disk instead: {e}"));
            None
        }
    };
    let receiver = crate::voice::start_receiver(node, sink, save_dir);
    Ok(Listen { player, receiver })
}

impl PlayerHandle {
    /// A sink closure for [`crate::voice::start_receiver`].
    pub fn sink(&self) -> crate::voice::PcmSink {
        let handle = self.clone();
        std::sync::Arc::new(move |pcm: &[i16]| handle.push(pcm))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_buffer_waits_for_a_cushion_before_it_plays() {
        // 240 ms target at 48 kHz = 1920 source samples of 8 kHz audio.
        let buffer = JitterBuffer::new(240, 48_000);
        buffer.push(&[1000i16; 100]);
        let mut out = Vec::new();
        assert!(
            !buffer.pull(480, &mut out),
            "100 samples is far below the target: it must stay silent"
        );
        assert_eq!(out.len(), 480);
        assert!(out.iter().all(|v| *v == 0.0));

        buffer.push(&[1000i16; 4000]);
        out.clear();
        assert!(buffer.pull(960, &mut out), "past the target it plays");
        assert!(buffer.playing());
    }

    #[test]
    fn a_second_of_audio_plays_out_and_then_stops() {
        let buffer = JitterBuffer::new(240, 48_000);
        // 1 s of 8 kHz audio: 8000 samples, i.e. ~48000 samples at 48 kHz.
        buffer.push(&[16384i16; 8000]);
        let mut played = 0usize;
        for _ in 0..10 {
            let mut out = Vec::new();
            buffer.pull(4_800, &mut out);
            played += out.iter().filter(|v| **v > 0.0).count();
        }
        assert!(
            (44_000..=48_000).contains(&played),
            "expected roughly one second of audio, got {played} samples"
        );

        // Drained: silence, not a loop.
        let mut out = Vec::new();
        buffer.pull(4_800, &mut out);
        assert!(out.iter().all(|v| *v == 0.0));
        assert!(!buffer.playing());
    }

    #[test]
    fn a_stored_clip_is_kept_whole_but_a_runaway_stream_is_capped() {
        // A transmission delivered in one burst (a WAV send, or a mailbox dump)
        // arrives far faster than real time; it must still play from the start.
        let buffer = JitterBuffer::new(240, 8_000);
        buffer.push(&vec![1000i16; SAMPLE_RATE as usize * 10]);
        assert!(
            buffer.buffered_ms() >= 9_900,
            "a ten-second clip must be kept, got {} ms",
            buffer.buffered_ms()
        );

        // The cap is only a guard against a stream that never stops.
        buffer.push(&vec![1000i16; SAMPLE_RATE as usize * 90]);
        assert!(
            buffer.buffered_ms() <= 60_010,
            "buffered {} ms exceeds the cap",
            buffer.buffered_ms()
        );
    }

    #[test]
    fn the_encoder_batches_before_the_worker_sends() {
        // The worker sends whole packets, so a press must produce exactly one
        // packet per `frames_per_packet` frames plus a tail.
        let mut encoder = Encoder::new(Mode::Bitrate3200, 12);
        let mut packets = encoder.push(&vec![6000i16; 160 * 12]);
        packets.extend(encoder.flush());
        assert_eq!(packets.len(), 2);
        assert_eq!(packets[0].frames, 12);
        assert_eq!(packets[0].data.len(), 96);
        assert!(packets[1].last);
    }
}
