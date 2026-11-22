//! Push-to-talk voice over the DNS tunnel.
//!
//! Audio is compressed with [Codec 2](https://rowetel.com/codec2.html) — the
//! low-bitrate speech codec the ham/M17 world uses for exactly this — and the
//! coded frames ride ordinary signed envelopes, so voice inherits the same
//! routing, encryption and authenticity as a chat message.
//!
//! ```text
//! 20 ms of 8 kHz PCM (160 samples)
//!     └─ Codec2 3200 ─► 8 bytes    (2400 ─► 6 bytes)
//!         └─ 12 frames per packet ─► 240 ms of speech, ~250 bytes of wire
//!             └─ DLV1 header + frames ─► MsgType::Voice envelope ─► DNS
//! ```
//!
//! One press of the talk button is a *stream*: a packet sequence with a random
//! 16-bit id, starting at packet 0 and marked `last` on the final packet. The
//! receiver decodes continuous PCM, inserting silence for frames the sender
//! skipped (it drops packets that carry nothing but silence) or that were lost
//! on the way.
//!
//! Why not full duplex: a DNS tunnel is a request/response transport with an
//! RTT of tens to hundreds of milliseconds and a per-query payload of ~125
//! bytes after base32, so the walkie-talkie model (speak, then listen) is the
//! one that actually works. Expect roughly a second of buffering between
//! press and sound at the other end.

use codec2::{Codec2, Codec2Mode};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

pub mod wav;

#[cfg(feature = "audio")]
pub mod audio;

/// Codec 2 is narrowband: 8 kHz, the telephone band.
pub const SAMPLE_RATE: u32 = 8000;
/// One Codec 2 frame: 20 ms at 8 kHz.
pub const SAMPLES_PER_FRAME: usize = 160;
/// Duration of one Codec 2 frame in milliseconds.
pub const FRAME_MS: u32 = 20;
/// Header magic; a body that does not start with this is not ours.
pub const MAGIC: &[u8; 4] = b"DLV1";
/// `magic(4) | stream(2) | seq(2) | first_frame(4) | mode(1) | frames(1) | flags(1)`
pub const HEADER_LEN: usize = 15;
/// Upper bound on frames per packet: 48 frames = 960 ms, far beyond any sane
/// `voice_frames_per_packet`, but it bounds what a hostile peer can make us
/// decode in one go.
pub const MAX_FRAMES: u8 = 48;
/// Largest body we will accept for a voice packet.
pub const MAX_PACKET_BYTES: usize = HEADER_LEN + MAX_FRAMES as usize * 8;
/// Flag: this packet ends the stream.
const FLAG_LAST: u8 = 0b0000_0001;
/// Frames quieter than this (RMS of the i16 samples) are treated as silence and
/// are not sent: holding the key in a quiet room costs nothing on the wire.
pub const SILENCE_RMS: f64 = 260.0;

/// Codec 2 bitrates we can carry. `bytes_per_frame` is what matters on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// 48 bits per 20 ms frame (6 bytes).
    Bitrate2400,
    /// 64 bits per 20 ms frame (8 bytes).
    #[default]
    Bitrate3200,
}

impl Mode {
    /// Map a configured bitrate (`config.voice_mode`) to a codec mode.
    pub fn from_bitrate(bitrate: u16) -> Mode {
        match bitrate {
            2400 => Mode::Bitrate2400,
            _ => Mode::Bitrate3200,
        }
    }

    pub fn bitrate(self) -> u16 {
        match self {
            Mode::Bitrate2400 => 2400,
            Mode::Bitrate3200 => 3200,
        }
    }

    pub fn codec2(self) -> Codec2Mode {
        match self {
            Mode::Bitrate2400 => Codec2Mode::MODE_2400,
            Mode::Bitrate3200 => Codec2Mode::MODE_3200,
        }
    }

    /// Coded bytes per 20 ms frame.
    pub fn bytes_per_frame(self) -> usize {
        match self {
            Mode::Bitrate2400 => 6,
            Mode::Bitrate3200 => 8,
        }
    }

    fn to_u8(self) -> u8 {
        match self {
            Mode::Bitrate2400 => 1,
            Mode::Bitrate3200 => 2,
        }
    }

    fn from_u8(v: u8) -> Option<Mode> {
        match v {
            1 => Some(Mode::Bitrate2400),
            2 => Some(Mode::Bitrate3200),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Mode::Bitrate2400 => "2400",
            Mode::Bitrate3200 => "3200",
        }
    }
}

/// One voice packet: a run of consecutive Codec 2 frames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    /// Random per-transmission id, so packets of two presses never mix.
    pub stream: u16,
    /// 0-based packet index within the stream (used for ordering/statistics).
    pub seq: u16,
    /// Index of the first frame in this packet within the stream.
    pub first_frame: u32,
    pub mode: Mode,
    /// Number of frames in `data` (0 on a "the stream ended" marker).
    pub frames: u8,
    pub last: bool,
    /// `frames * mode.bytes_per_frame()` coded bytes.
    pub data: Vec<u8>,
}

impl Packet {
    /// Message id for the envelope: deterministic per `(stream, seq)`, so a
    /// retried packet is deduplicated by the seen-cache instead of played twice.
    pub fn msg_id(&self) -> String {
        format!("{:04x}{:04x}", self.stream, self.seq)
    }

    /// Speech duration carried by this packet.
    pub fn duration_ms(&self) -> u32 {
        self.frames as u32 * FRAME_MS
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + self.data.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&self.stream.to_le_bytes());
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.extend_from_slice(&self.first_frame.to_le_bytes());
        out.push(self.mode.to_u8());
        out.push(self.frames);
        out.push(if self.last { FLAG_LAST } else { 0 });
        out.extend_from_slice(&self.data);
        out
    }

    pub fn decode(body: &[u8]) -> Result<Packet, String> {
        if body.len() < HEADER_LEN {
            return Err(format!(
                "voice packet: {} bytes is shorter than the {HEADER_LEN}-byte header",
                body.len()
            ));
        }
        if &body[..4] != MAGIC {
            return Err("voice packet: bad magic".to_string());
        }
        let stream = u16::from_le_bytes([body[4], body[5]]);
        let seq = u16::from_le_bytes([body[6], body[7]]);
        let first_frame = u32::from_le_bytes([body[8], body[9], body[10], body[11]]);
        let mode = Mode::from_u8(body[12])
            .ok_or_else(|| format!("voice packet: bad mode {}", body[12]))?;
        let frames = body[13];
        let flags = body[14];
        let data = &body[HEADER_LEN..];

        if frames > MAX_FRAMES {
            return Err(format!(
                "voice packet: {frames} frames exceeds the {MAX_FRAMES}-frame limit"
            ));
        }
        let want = frames as usize * mode.bytes_per_frame();
        if data.len() != want {
            return Err(format!(
                "voice packet: {} payload bytes, expected {want} for {frames} frames",
                data.len()
            ));
        }
        if frames == 0 && flags & FLAG_LAST == 0 {
            return Err("voice packet: an empty packet must end the stream".to_string());
        }
        Ok(Packet {
            stream,
            seq,
            first_frame,
            mode,
            frames,
            last: flags & FLAG_LAST != 0,
            data: data.to_vec(),
        })
    }
}

/// A body of `frames` silence, used to fill gaps left by dropped packets.
fn rms(samples: &[i16]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples.iter().map(|s| (*s as f64) * (*s as f64)).sum();
    (sum / samples.len() as f64).sqrt()
}

/// Encodes microphone audio into packets.
///
/// Feed it 8 kHz mono i16 samples in any chunk size; it buffers until whole
/// frames exist, packs `frames_per_packet` frames per packet and skips packets
/// that carry nothing but silence. [`Encoder::flush`] emits the tail.
pub struct Encoder {
    mode: Mode,
    codec: Codec2,
    frames_per_packet: u8,
    stream: u16,
    seq: u16,
    /// Samples that do not yet make up a full frame, or that were pushed after
    /// the last whole frame.
    pending: Vec<i16>,
    /// Coded bytes for the frames already collected into the current packet.
    coded: Vec<u8>,
    count: u8,
    /// Index of the next frame to encode.
    next_frame: u32,
    /// Frames in the current packet that were above the silence floor.
    voiced: u8,
    /// Packets dropped because the whole press was silence.
    pub skipped: u32,
}

impl Encoder {
    pub fn new(mode: Mode, frames_per_packet: u8) -> Encoder {
        let id = crate::random_bytes(2);
        let stream = u16::from_le_bytes([id[0], id[1]]);
        Encoder {
            mode,
            codec: Codec2::new(mode.codec2()),
            frames_per_packet: frames_per_packet.clamp(1, MAX_FRAMES),
            stream,
            seq: 0,
            pending: Vec::new(),
            coded: Vec::new(),
            count: 0,
            next_frame: 0,
            voiced: 0,
            skipped: 0,
        }
    }

    pub fn stream(&self) -> u16 {
        self.stream
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Push any number of 8 kHz mono samples; returns the packets that became
    /// complete, in order.
    pub fn push(&mut self, samples: &[i16]) -> Vec<Packet> {
        self.pending.extend_from_slice(samples);
        let mut out = Vec::new();
        while self.pending.len() >= SAMPLES_PER_FRAME {
            let frame: Vec<i16> = self.pending.drain(..SAMPLES_PER_FRAME).collect();
            self.encode_frame(&frame);
            if self.count >= self.frames_per_packet
                && let Some(p) = self.take_packet(false)
            {
                out.push(p);
            }
        }
        out
    }

    /// Encode the tail (padding the last partial frame with silence) and emit a
    /// packet flagged `last`. Always returns a packet when anything at all was
    /// recorded, so the receiver learns the transmission ended.
    pub fn flush(&mut self) -> Option<Packet> {
        if !self.pending.is_empty() {
            let mut frame: Vec<i16> = std::mem::take(&mut self.pending);
            frame.resize(SAMPLES_PER_FRAME, 0);
            self.encode_frame(&frame);
        }
        if self.count == 0 {
            // Nothing recorded (or the whole press was silence): a marker-only
            // final packet tells the far end to play out its buffer.
            return Some(Packet {
                stream: self.stream,
                seq: self.seq,
                first_frame: self.next_frame,
                mode: self.mode,
                frames: 0,
                last: true,
                data: Vec::new(),
            });
        }
        self.take_packet(true)
    }

    fn encode_frame(&mut self, frame: &[i16]) {
        let per = self.mode.bytes_per_frame();
        let mut coded = vec![0u8; per];
        self.codec.encode(&mut coded, frame);
        if rms(frame) >= SILENCE_RMS {
            self.voiced += 1;
        }
        self.coded.extend_from_slice(&coded);
        self.count += 1;
        self.next_frame += 1;
    }

    fn take_packet(&mut self, last: bool) -> Option<Packet> {
        let frames = self.count;
        let data = std::mem::take(&mut self.coded);
        let first_frame = self.next_frame - frames as u32;
        let voiced = std::mem::take(&mut self.voiced);
        self.count = 0;

        // Silence inside a press is not worth a query; the receiver fills the
        // gap. A silent *final* packet still goes out: it ends the stream.
        if voiced == 0 && !last {
            self.skipped += 1;
            return None;
        }
        let packet = Packet {
            stream: self.stream,
            seq: self.seq,
            first_frame,
            mode: self.mode,
            frames,
            last,
            data,
        };
        self.seq = self.seq.wrapping_add(1);
        Some(packet)
    }
}

/// What a decoded packet produced: PCM ready to play and stream bookkeeping.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Chunk {
    /// 8 kHz mono samples, already in order.
    pub pcm: Vec<i16>,
    /// This packet started a new transmission (a different `stream` id).
    pub new_stream: bool,
    /// The sender said this was the last packet.
    pub finished: bool,
}

/// Turns packets — possibly reordered, duplicated, batched or missing — into
/// continuous PCM.
///
/// A DNS mailbox hands blobs over one poll reply at a time, so a transmission
/// routinely arrives out of order and in two or three batches. The stream
/// therefore buffers decoded frames by frame index and only ever emits a
/// contiguous run: audio plays as soon as it is in order, a hole is held for
/// `grace` (a late packet still fills it) and only then becomes silence. That is
/// why a lost packet costs a gap instead of the rest of the sentence.
pub struct Stream {
    codec: Option<(Mode, Codec2)>,
    stream: Option<u16>,
    /// Decoded frames waiting for their neighbours, by frame index.
    pending: BTreeMap<u32, Vec<i16>>,
    /// Index of the next frame to hand out.
    next_out: u32,
    /// Exclusive end of the stream, once the `last` packet has arrived.
    last_frame: Option<u32>,
    saw_last: bool,
    /// When the current hole (a missing frame with data queued behind it)
    /// started. `None` when nothing is missing.
    hole_since: Option<Instant>,
    /// How long a hole is held open for a late packet.
    pub grace: Duration,
    pub played_frames: u32,
    /// Frames of silence inserted because packets were lost or skipped.
    pub filled_frames: u32,
    /// Packets or frames dropped: duplicates, or too late to play.
    pub dropped: u32,
    pub packets: u32,
}

impl Default for Stream {
    fn default() -> Self {
        Stream::new()
    }
}

impl Stream {
    pub fn new() -> Stream {
        Stream {
            codec: None,
            stream: None,
            pending: BTreeMap::new(),
            next_out: 0,
            last_frame: None,
            saw_last: false,
            hole_since: None,
            grace: Duration::from_millis(1200),
            played_frames: 0,
            filled_frames: 0,
            dropped: 0,
            packets: 0,
        }
    }

    /// `true` while a transmission is in progress (started, not yet finished).
    pub fn active(&self) -> bool {
        self.stream.is_some() && !self.finished()
    }

    pub fn current(&self) -> Option<u16> {
        self.stream
    }

    /// `true` once the last packet has arrived *and* every frame before it has
    /// been played or filled.
    pub fn finished(&self) -> bool {
        self.saw_last && self.last_frame.is_some_and(|end| end <= self.next_out)
    }

    /// Buffered frames that are waiting for a neighbour.
    pub fn queued_frames(&self) -> usize {
        self.pending.len()
    }

    /// Apply one packet; the returned PCM is the contiguous audio that can be
    /// played now (possibly nothing).
    pub fn push(&mut self, p: &Packet, now: Instant) -> Chunk {
        let mut chunk = Chunk::default();

        if self.stream != Some(p.stream) {
            // A new press: the codec state of the old one is meaningless.
            chunk.new_stream = true;
            self.stream = Some(p.stream);
            self.codec = None;
            self.pending.clear();
            self.next_out = p.first_frame;
            self.last_frame = None;
            self.saw_last = false;
            self.hole_since = None;
        }
        self.packets += 1;

        if self.codec.as_ref().map(|c| c.0) != Some(p.mode) {
            self.codec = Some((p.mode, Codec2::new(p.mode.codec2())));
        }

        if p.frames > 0 {
            let per = p.mode.bytes_per_frame();
            if let Some((_, codec)) = self.codec.as_mut() {
                let mut pcm = vec![0i16; SAMPLES_PER_FRAME];
                for (i, frame) in p.data.chunks_exact(per).enumerate() {
                    let index = p.first_frame + i as u32;
                    if index < self.next_out {
                        // Already played (duplicate, or too late): the audio is
                        // gone, but the rest of the packet may still be early.
                        self.dropped += 1;
                        continue;
                    }
                    codec.decode(&mut pcm, frame);
                    self.pending.insert(index, pcm.clone());
                }
            }
        }

        if p.last {
            self.saw_last = true;
            self.last_frame = Some(p.first_frame + p.frames as u32);
        }

        chunk.pcm = self.drain(now);
        chunk.finished = self.finished();
        chunk
    }

    /// Emit whatever is contiguous, and fill holes that have waited longer than
    /// `grace` with silence. Call this on a timer (the receiver does, every
    /// 100 ms) so a stalled hole cannot hold the stream open forever.
    pub fn tick(&mut self, now: Instant) -> Vec<i16> {
        let Some(since) = self.hole_since else {
            return Vec::new();
        };
        if now.duration_since(since) < self.grace {
            return Vec::new();
        }
        let mut out = Vec::new();
        // Everything queued behind the hole has waited just as long, so fill
        // every run of missing frames, not only the first one.
        for _ in 0..MAX_FILL_RUNS {
            let Some(&next) = self.pending.keys().next() else {
                break;
            };
            while self.next_out < next {
                out.extend_from_slice(&[0i16; SAMPLES_PER_FRAME]);
                self.filled_frames += 1;
                self.next_out += 1;
            }
            self.hole_since = None;
            out.extend(self.drain(now));
            if self.hole_since.is_none() {
                break;
            }
            self.hole_since = Some(since);
        }
        out
    }

    /// Pull contiguous frames off the buffer.
    fn drain(&mut self, now: Instant) -> Vec<i16> {
        let mut out = Vec::new();
        loop {
            if let Some(frame) = self.pending.remove(&self.next_out) {
                out.extend_from_slice(&frame);
                self.next_out += 1;
                self.played_frames += 1;
                self.hole_since = None;
                continue;
            }
            // Nothing queued at all: wait quietly, this is not a hole yet.
            if self.pending.is_empty() {
                self.hole_since = None;
                break;
            }
            // Everything queued is past `next_out`: a hole. Hold it for `grace`.
            if self.hole_since.is_none() {
                self.hole_since = Some(now);
            }
            break;
        }
        out
    }
}

/// Where decoded speech goes: the speaker, in practice.
pub type PcmSink = std::sync::Arc<dyn Fn(&[i16]) + Send + Sync>;

/// What the receiver does with audio: push it somewhere, and optionally drop
/// whatever is still buffered when a new transmission starts.
#[derive(Clone)]
pub struct Sink {
    pub push: PcmSink,
    pub flush: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
}

/// How long a transmission may go quiet before the receiver plays out what it
/// has and calls it done (a packet carrying the `last` flag was lost).
const STALL: Duration = Duration::from_secs(5);
/// Bound on how many separate holes one [`Stream::tick`] will fill, so a burst
/// of loss cannot turn into a long loop of silence.
const MAX_FILL_RUNS: usize = 64;

/// Decodes inbound voice packets (and optionally writes each transmission to a
/// WAV file).
///
/// Keeping this value alive keeps the thread alive. When the node is dropped its
/// voice sink goes with it, the channel closes and the thread returns.
pub struct Receiver {
    _thread: Option<std::thread::JoinHandle<()>>,
}

impl std::fmt::Debug for Receiver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("voice::Receiver")
    }
}

fn save_and_log(
    node: &std::sync::Arc<crate::net::Node>,
    from: &str,
    captured: &mut Vec<i16>,
    save_dir: &Option<String>,
    stream: &Stream,
) {
    let ms = captured.len() as u64 * 1000 / SAMPLE_RATE as u64;
    node.log(format!(
        "[<] voice from {from}: {:.1} s{}",
        ms as f32 / 1000.0,
        if stream.filled_frames > 0 || stream.dropped > 0 {
            format!(
                " ({} frame(s) of silence filled, {} frame(s) dropped)",
                stream.filled_frames, stream.dropped
            )
        } else {
            String::new()
        }
    ));
    if let Some(dir) = save_dir {
        let dir = std::path::Path::new(dir);
        let _ = std::fs::create_dir_all(dir);
        let path = dir.join(format!(
            "voice-{:06x}-{}.wav",
            stream.current().unwrap_or(0),
            crate::db::now_secs()
        ));
        match std::fs::write(&path, wav::write(captured, SAMPLE_RATE)) {
            Ok(()) => node.log(format!("[+] saved {}", path.display())),
            Err(e) => node.log(format!("[!] cannot write {}: {e}", path.display())),
        }
    }
    captured.clear();
}

/// Start decoding whatever the node's voice sink receives.
///
/// `save_dir` writes `<dir>/voice-<stream>-<unix>.wav` per transmission, which
/// is how a headless box (or a test) hears anything.
pub fn start_receiver(
    node: &std::sync::Arc<crate::net::Node>,
    sink: Option<Sink>,
    save_dir: Option<String>,
) -> Receiver {
    let (tx, rx) = std::sync::mpsc::channel::<(String, Packet)>();
    node.set_voice_sink(tx);
    let node = node.clone();
    // A mailbox poll delivers a batch at a time, so a hole is usually a packet
    // that is one poll behind: hold holes at least a poll interval plus slack.
    let grace = Duration::from_millis(700 + node.cfg.poll_interval_secs.max(1) * 1000);

    let thread = std::thread::spawn(move || {
        let mut stream = Stream::new();
        stream.grace = grace;
        let mut captured: Vec<i16> = Vec::new();
        let mut from = String::new();
        let mut last_packet = Instant::now();

        loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok((sender, packet)) => {
                    last_packet = Instant::now();
                    from = sender;
                    let chunk = stream.push(&packet, Instant::now());
                    if node.cfg.verbose {
                        node.log(format!(
                            "[<] voice pkt #{:04x}/{:<3} frames {}+{} -> {} samples{}",
                            packet.stream,
                            packet.seq,
                            packet.first_frame,
                            packet.frames,
                            chunk.pcm.len() / SAMPLES_PER_FRAME,
                            if packet.last { " (last)" } else { "" }
                        ));
                    }
                    if chunk.new_stream {
                        // A new press: the previous capture belongs to the
                        // previous transmission, and the speaker starts fresh.
                        captured.clear();
                        if let Some(flush) = sink.as_ref().and_then(|s| s.flush.clone()) {
                            flush();
                        }
                        node.log(format!(
                            "[<] voice from {from} (#{:04x}, codec2 {})",
                            packet.stream,
                            packet.mode.label()
                        ));
                    }
                    if !chunk.pcm.is_empty() {
                        captured.extend_from_slice(&chunk.pcm);
                        if let Some(sink) = &sink {
                            (sink.push)(&chunk.pcm);
                        }
                    }
                    if chunk.finished {
                        save_and_log(&node, &from, &mut captured, &save_dir, &stream);
                        stream = Stream::new();
                        stream.grace = grace;
                        continue;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }

            // Play holes that have waited long enough, and close out a stream
            // whose `last` packet never made it.
            let pcm = stream.tick(Instant::now());
            if !pcm.is_empty() {
                captured.extend_from_slice(&pcm);
                if let Some(sink) = &sink {
                    (sink.push)(&pcm);
                }
            }
            if stream.finished() {
                save_and_log(&node, &from, &mut captured, &save_dir, &stream);
                stream = Stream::new();
                stream.grace = grace;
            } else if stream.active() && last_packet.elapsed() >= STALL {
                node.log("[~] voice stream went quiet; playing what arrived".to_string());
                save_and_log(&node, &from, &mut captured, &save_dir, &stream);
                stream = Stream::new();
                stream.grace = grace;
            }
        }
    });

    Receiver {
        _thread: Some(thread),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 440 Hz "speech" — loud enough to count as voiced, cheap to generate.
    fn tone(ms: u32, amplitude: i16) -> Vec<i16> {
        let n = (SAMPLE_RATE * ms / 1000) as usize;
        (0..n)
            .map(|i| {
                let t = i as f64 / SAMPLE_RATE as f64;
                (amplitude as f64 * (2.0 * std::f64::consts::PI * 440.0 * t).sin()) as i16
            })
            .collect()
    }

    fn packet(stream: u16, seq: u16, first_frame: u32, frames: u8, last: bool) -> Packet {
        Packet {
            stream,
            seq,
            first_frame,
            mode: Mode::Bitrate3200,
            frames,
            last,
            data: vec![0xA5; frames as usize * 8],
        }
    }

    #[test]
    fn packets_round_trip() {
        let p = Packet {
            stream: 0xbeef,
            seq: 3,
            first_frame: 36,
            mode: Mode::Bitrate2400,
            frames: 12,
            last: true,
            data: (0..72).map(|b| b as u8).collect(),
        };
        let back = Packet::decode(&p.encode()).unwrap();
        assert_eq!(back, p);
        assert_eq!(back.msg_id(), "beef0003");
        assert_eq!(back.duration_ms(), 240);
    }

    #[test]
    fn bad_packets_are_rejected() {
        let good = packet(1, 0, 0, 2, false);
        assert!(Packet::decode(&good.encode()).is_ok());

        // Truncated payload.
        let mut body = good.encode();
        body.pop();
        assert!(Packet::decode(&body).is_err());

        // Bad magic.
        let mut body = good.encode();
        body[0] = b'X';
        assert!(Packet::decode(&body).is_err());

        // Unknown mode.
        let mut body = good.encode();
        body[12] = 9;
        assert!(Packet::decode(&body).is_err());

        // Too many frames.
        let mut body = good.encode();
        body[13] = MAX_FRAMES + 1;
        assert!(Packet::decode(&body).is_err());

        // An empty packet that does not end the stream.
        let empty = packet(1, 0, 0, 0, false);
        assert!(Packet::decode(&empty.encode()).is_err());
        let empty = packet(1, 0, 0, 0, true);
        assert!(Packet::decode(&empty.encode()).is_ok());
    }

    #[test]
    fn the_encoder_packs_whole_frames_and_flushes_the_tail() {
        let mut enc = Encoder::new(Mode::Bitrate3200, 4);
        // 3.5 frames of audio: one full packet (4 frames) plus a partial frame.
        let audio = tone(20 * 3 + 10, 6000);
        let packets = enc.push(&audio);
        assert_eq!(packets.len(), 0, "3.5 frames is not a full packet yet");

        let packets = enc.push(&tone(20, 6000));
        assert_eq!(packets.len(), 1, "4 frames complete a packet");
        assert_eq!(packets[0].frames, 4);
        assert_eq!(packets[0].data.len(), 4 * 8);
        assert_eq!(packets[0].first_frame, 0);
        assert!(!packets[0].last);

        let tail = enc.flush().expect("a flush always emits");
        assert!(tail.last);
        assert_eq!(tail.seq, 1);
        assert_eq!(tail.first_frame, 4);
        assert_eq!(tail.frames, 1, "the partial frame is padded, not dropped");
        assert_eq!(tail.data.len(), 8);
    }

    #[test]
    fn silence_is_not_worth_a_query_but_the_end_still_arrives() {
        let mut enc = Encoder::new(Mode::Bitrate3200, 2);
        let quiet = vec![0i16; SAMPLES_PER_FRAME * 4];
        assert!(enc.push(&quiet).is_empty());
        let tail = enc
            .flush()
            .expect("the receiver must learn the press ended");
        assert!(tail.last);
        assert_eq!(tail.frames, 0);
        assert!(tail.data.is_empty());
        assert_eq!(enc.skipped, 2);
        assert_eq!(tail.first_frame, 4, "frame numbering still advances");
    }

    #[test]
    fn a_packet_round_trips_through_the_codec() {
        // Encode one press of tone, decode it back: same frame count, and the
        // vocoder output is not silent (it will not be sample-exact — that is
        // what a vocoder is).
        let mut enc = Encoder::new(Mode::Bitrate3200, 2);
        let mut packets = enc.push(&tone(40, 8000));
        packets.extend(enc.flush());
        assert_eq!(packets.len(), 2);

        let mut stream = Stream::new();
        let mut pcm = Vec::new();
        let mut finished = false;
        for p in &packets {
            let now = Instant::now();
            let chunk = stream.push(&Packet::decode(&p.encode()).unwrap(), now);
            pcm.extend(chunk.pcm);
            finished |= chunk.finished;
        }
        assert!(finished, "the tail packet must end the stream");
        assert_eq!(
            pcm.len(),
            2 * SAMPLES_PER_FRAME,
            "the marker packet carries no audio of its own"
        );
        let energy: f64 = pcm.iter().map(|s| (*s as f64).abs()).sum::<f64>() / pcm.len() as f64;
        assert!(
            energy > 100.0,
            "decoded audio should have real energy, got {energy}"
        );
    }

    #[test]
    fn reordered_packets_still_play_in_order() {
        // A mailbox poll hands over a batch, and batches do not preserve the
        // order packets were sent in: 0, 2, 1, 3 must still play as 0..4.
        let mut stream = Stream::new();
        let now = Instant::now();
        let mut pcm = Vec::new();
        for (seq, first) in [(0u16, 0u32), (2, 4), (1, 2)] {
            let chunk = stream.push(&packet(7, seq, first, 2, false), now);
            pcm.extend(chunk.pcm);
            assert!(!chunk.finished);
        }
        assert_eq!(
            pcm.len(),
            6 * SAMPLES_PER_FRAME,
            "all three packets are contiguous once packet 1 arrives"
        );
        assert_eq!(stream.filled_frames, 0, "nothing was missing");
        assert_eq!(stream.dropped, 0, "nothing was too late");

        let last = stream.push(&packet(7, 3, 6, 1, true), now);
        assert!(last.finished);
        assert!(!stream.active());
    }

    #[test]
    fn a_missing_packet_becomes_silence_after_the_grace_window() {
        // Packets 0 and 2 arrive, 1 is lost. The hole is held for `grace` so a
        // late packet could still fill it, then it plays as silence.
        let mut stream = Stream::new();
        stream.grace = Duration::from_millis(50);
        let start = Instant::now();
        stream.push(&packet(7, 0, 0, 2, false), start);
        let third = stream.push(&packet(7, 2, 4, 2, true), start);
        assert!(third.pcm.is_empty(), "frames 2..4 are missing: hold them");
        assert!(!third.finished, "the stream cannot end with a hole open");

        assert!(stream.tick(start + Duration::from_millis(10)).is_empty());
        let filled = stream.tick(start + Duration::from_millis(60));
        // Frames 2 and 3 as silence, then the two frames that were waiting.
        assert_eq!(filled.len(), 4 * SAMPLES_PER_FRAME);
        assert!(filled[..2 * SAMPLES_PER_FRAME].iter().all(|s| *s == 0));
        assert_eq!(stream.filled_frames, 2);
        assert!(stream.finished(), "with the hole filled the stream is over");
    }

    #[test]
    fn a_late_packet_fills_its_hole_instead_of_becoming_silence() {
        let mut stream = Stream::new();
        stream.grace = Duration::from_millis(500);
        let start = Instant::now();
        stream.push(&packet(7, 0, 0, 2, false), start);
        stream.push(&packet(7, 2, 4, 2, true), start);
        // Packet 1 turns up 100 ms later, still inside the grace window.
        let late = stream.push(
            &packet(7, 1, 2, 2, false),
            start + Duration::from_millis(100),
        );
        assert_eq!(
            late.pcm.len(),
            4 * SAMPLES_PER_FRAME,
            "the hole and the queued packet play together"
        );
        assert_eq!(stream.filled_frames, 0);
        assert_eq!(stream.dropped, 0);
        assert!(stream.finished());
    }

    #[test]
    fn a_new_stream_resets_the_player() {
        let mut stream = Stream::new();
        let now = Instant::now();
        stream.push(&packet(1, 0, 0, 2, false), now);
        let second = stream.push(&packet(2, 0, 0, 2, true), now);
        assert!(second.new_stream, "a different stream id is a new press");
        assert_eq!(second.pcm.len(), 2 * SAMPLES_PER_FRAME);
        assert!(second.finished);
    }
}
