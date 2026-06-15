//! Spatial voice comms (task 8). Captures the mic, sends mono PCM frames over the
//! net transport, and plays each remote peer back through a **jitter buffer** so
//! it never sounds laggy (late/reordered packets are absorbed by a small
//! pre-buffer; underruns play silence, not stutter). Playback is **spatial**:
//! each peer's voice sink volume falls off with distance from the listener, like
//! the `AudioEngine` does for `AudioSource`s — walk away and you hear them less.
//!
//! Latency-agnostic by design: the jitter buffer decouples network arrival from
//! playback, and missing audio is silence rather than time-stretched "lag" sound.
//! LAN-grade (raw PCM, no codec); Opus + packet-loss concealment are follow-ups.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use glam::Vec3;
use rodio::{OutputStream, OutputStreamHandle, Sink, Source};

use crate::net::{NetSession, VoicePacket};

/// Voice sample rate (mono). 16 kHz is plenty for speech and keeps packets small.
const VOICE_RATE: u32 = 16_000;
/// Samples per sent frame (20 ms @ 16 kHz).
const FRAME: usize = 320;
/// Pre-buffer depth before a peer starts playing (≈120 ms of jitter headroom).
const PREBUFFER: usize = VOICE_RATE as usize / 1000 * 120;
/// Drop the oldest audio if a peer's buffer grows past this (≈600 ms) so a
/// backlog can't add unbounded latency.
const MAX_BUFFER: usize = VOICE_RATE as usize / 1000 * 600;

/// Per-peer playback: a jitter buffer feeding a rodio sink, positioned in world
/// space for distance attenuation.
struct PeerVoice {
    sink: Sink,
    buf: Arc<Mutex<VecDeque<i16>>>,
    next_seq: u32,
    /// Out-of-order frames held until their turn (seq -> samples).
    pending: HashMap<u32, Vec<i16>>,
    pos: Vec3,
}

/// A never-ending rodio source that drains a peer's jitter buffer; silence on
/// underrun. Waits for `PREBUFFER` samples before the first playback.
struct JitterSource {
    buf: Arc<Mutex<VecDeque<i16>>>,
    playing: Arc<AtomicBool>,
}

impl Iterator for JitterSource {
    type Item = f32;
    fn next(&mut self) -> Option<f32> {
        let mut b = self.buf.lock().unwrap();
        if !self.playing.load(Ordering::Relaxed) {
            if b.len() >= PREBUFFER {
                self.playing.store(true, Ordering::Relaxed);
            } else {
                return Some(0.0);
            }
        }
        match b.pop_front() {
            Some(s) => Some(s as f32 / 32768.0),
            None => {
                // Underrun: re-arm the pre-buffer so we don't dribble.
                self.playing.store(false, Ordering::Relaxed);
                Some(0.0)
            }
        }
    }
}

impl Source for JitterSource {
    fn current_frame_len(&self) -> Option<usize> {
        None
    }
    fn channels(&self) -> u16 {
        1
    }
    fn sample_rate(&self) -> u32 {
        VOICE_RATE
    }
    fn total_duration(&self) -> Option<std::time::Duration> {
        None
    }
}

/// Owns mic capture + per-peer playback. Created lazily when voice is first used.
pub struct VoiceChat {
    _in_stream: Option<cpal::Stream>,
    capture: Arc<Mutex<VecDeque<i16>>>,
    _out_stream: OutputStream,
    out_handle: OutputStreamHandle,
    peers: HashMap<u64, PeerVoice>,
    seq: u32,
}

impl VoiceChat {
    /// Initialise audio output + mic capture. Returns None if no output device;
    /// capture is optional (None mic just means you can hear but not speak).
    pub fn new() -> Option<Self> {
        let (stream, handle) = OutputStream::try_default()
            .map_err(|e| tracing::warn!("voice: no audio output: {e}"))
            .ok()?;
        let capture = Arc::new(Mutex::new(VecDeque::new()));
        let in_stream = start_capture(capture.clone());
        Some(Self {
            _in_stream: in_stream,
            capture,
            _out_stream: stream,
            out_handle: handle,
            peers: HashMap::new(),
            seq: 0,
        })
    }

    /// Drain captured mic audio into 20 ms frames and send them while
    /// `transmit` (push-to-talk) is held. Clears the buffer when not transmitting
    /// so released speech doesn't queue up.
    pub fn capture_and_send(&mut self, net: &mut NetSession, transmit: bool) {
        let mut cap = self.capture.lock().unwrap();
        if !transmit {
            cap.clear();
            return;
        }
        while cap.len() >= FRAME {
            let frame: Vec<i16> = cap.drain(..FRAME).collect();
            drop(cap);
            net.send_voice(self.seq, &frame);
            self.seq = self.seq.wrapping_add(1);
            cap = self.capture.lock().unwrap();
        }
    }

    /// Feed received voice packets into per-peer jitter buffers (reordered by
    /// seq). `positions` maps a peer to its world position for spatialization.
    pub fn receive(&mut self, packets: Vec<VoicePacket>, positions: &HashMap<u64, Vec3>) {
        for p in packets {
            let handle = &self.out_handle;
            let peer = self.peers.entry(p.from).or_insert_with(|| {
                let buf = Arc::new(Mutex::new(VecDeque::new()));
                let sink = Sink::try_new(handle).ok();
                if let Some(sink) = &sink {
                    sink.append(JitterSource {
                        buf: buf.clone(),
                        playing: Arc::new(AtomicBool::new(false)),
                    });
                }
                PeerVoice {
                    sink: sink.unwrap_or_else(|| Sink::new_idle().0),
                    buf,
                    next_seq: p.seq,
                    pending: HashMap::new(),
                    pos: Vec3::ZERO,
                }
            });
            if let Some(pos) = positions.get(&p.from) {
                peer.pos = *pos;
            }
            // Reorder: stash, then flush in-order frames into the buffer.
            peer.pending.insert(p.seq, p.samples);
            while let Some(samples) = peer.pending.remove(&peer.next_seq) {
                let mut b = peer.buf.lock().unwrap();
                b.extend(samples);
                while b.len() > MAX_BUFFER {
                    b.pop_front();
                }
                peer.next_seq = peer.next_seq.wrapping_add(1);
            }
            // Drop very stale out-of-order frames so `pending` can't leak.
            peer.pending.retain(|s, _| s.wrapping_sub(peer.next_seq) < 256);
        }
    }

    /// Update spatial volumes from the listener position (distance falloff).
    pub fn update(&mut self, listener: Vec3, range: f32) {
        for peer in self.peers.values() {
            let d = peer.pos.distance(listener);
            let gain = if peer.pos == Vec3::ZERO {
                1.0 // unpositioned peer: non-spatial
            } else {
                (1.0 - d / range).clamp(0.0, 1.0).powi(2)
            };
            peer.sink.set_volume(gain);
        }
    }
}

/// Open the default mic and stream mono 16 kHz samples into `capture`.
fn start_capture(capture: Arc<Mutex<VecDeque<i16>>>) -> Option<cpal::Stream> {
    let host = cpal::default_host();
    let device = host.default_input_device()?;
    let config = device.default_input_config().ok()?;
    let src_rate = config.sample_rate().0;
    let channels = config.channels() as usize;
    let err = |e| tracing::warn!("voice capture error: {e}");
    let cap = capture.clone();
    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => device
            .build_input_stream(
                &config.into(),
                move |data: &[f32], _: &_| push_mono(&cap, data, channels, src_rate),
                err,
                None,
            )
            .ok()?,
        cpal::SampleFormat::I16 => device
            .build_input_stream(
                &config.into(),
                move |data: &[i16], _: &_| {
                    let f: Vec<f32> = data.iter().map(|s| *s as f32 / 32768.0).collect();
                    push_mono(&cap, &f, channels, src_rate);
                },
                err,
                None,
            )
            .ok()?,
        _ => return None,
    };
    if let Err(e) = stream.play() {
        tracing::warn!("voice: mic start failed: {e}");
        return None;
    }
    tracing::info!("voice: mic capture started ({src_rate} Hz, {channels} ch)");
    Some(stream)
}

/// Downmix to mono and resample `src_rate` -> 16 kHz (linear), pushing i16.
fn push_mono(capture: &Arc<Mutex<VecDeque<i16>>>, data: &[f32], channels: usize, src_rate: u32) {
    if channels == 0 {
        return;
    }
    let step = src_rate as f32 / VOICE_RATE as f32;
    let frames = data.len() / channels;
    let mut buf = capture.lock().unwrap();
    let mut pos = 0.0f32;
    while (pos as usize) < frames {
        let i = pos as usize;
        // Average channels for a mono sample.
        let mut sum = 0.0;
        for c in 0..channels {
            sum += data[i * channels + c];
        }
        let mono = (sum / channels as f32).clamp(-1.0, 1.0);
        buf.push_back((mono * 32767.0) as i16);
        pos += step;
    }
    // Bound latency if playback isn't draining the capture side.
    while buf.len() > MAX_BUFFER {
        buf.pop_front();
    }
}
