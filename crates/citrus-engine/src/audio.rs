//! Engine audio: plays `AudioSource` components in Play mode and spatializes
//! them against the active `AudioListener` (falling back to the editor
//! camera). Decoding (.wav/.flac/.mp3) and mixing are handled by `rodio`.
//!
//! The scene is the source of truth; each frame the engine builds a list of
//! [`AudioCue`]s (one per active source, with its world position) and the
//! engine updates sink volumes from the listener distance. Sources start when
//! Play begins (play-on-start) and all sinks stop when Play ends.

use std::collections::HashMap;
use std::io::BufReader;
use std::path::Path;

use citrus_core::{AudioRolloff, AudioSource};
use glam::Vec3;
use rodio::{Decoder, OutputStream, OutputStreamHandle, Sink, Source};

/// A flattened audio source for one frame (decoupled from the component so the
/// scene isn't borrowed while the audio engine runs).
#[derive(Clone)]
pub struct AudioCue {
    pub object: usize,
    pub clip: String,
    pub play_on_start: bool,
    pub looping: bool,
    pub volume: f32,
    pub pitch: f32,
    pub spatial: bool,
    pub min_distance: f32,
    pub max_distance: f32,
    pub rolloff: AudioRolloff,
    pub position: Vec3,
}

impl AudioCue {
    pub fn from_source(object: usize, src: &AudioSource, position: Vec3) -> Self {
        Self {
            object,
            clip: src.clip.clone(),
            play_on_start: src.play_on_start,
            looping: src.looping,
            volume: src.volume,
            pitch: src.pitch,
            spatial: src.spatial,
            min_distance: src.min_distance,
            max_distance: src.max_distance,
            rolloff: src.rolloff,
            position,
        }
    }

    /// Volume multiplier for this cue given the listener distance.
    fn gain(&self, listener: Vec3) -> f32 {
        if !self.spatial {
            return self.volume.max(0.0);
        }
        let dist = self.position.distance(listener);
        let (lo, hi) = (self.min_distance, self.max_distance.max(self.min_distance + 0.01));
        let atten = if dist <= lo {
            1.0
        } else if dist >= hi {
            0.0
        } else {
            let t = (dist - lo) / (hi - lo);
            match self.rolloff {
                AudioRolloff::Linear => 1.0 - t,
                // Normalized inverse-distance: 1 at min, 0 at max.
                AudioRolloff::Logarithmic => {
                    let inv = lo / dist;
                    let inv_max = lo / hi;
                    ((inv - inv_max) / (1.0 - inv_max)).clamp(0.0, 1.0)
                }
            }
        };
        (self.volume * atten).max(0.0)
    }
}

pub struct AudioEngine {
    // Kept alive for the engine's lifetime; dropping it kills all sound.
    _stream: OutputStream,
    handle: OutputStreamHandle,
    /// Object index → its active sink (only populated while playing).
    sinks: HashMap<usize, Sink>,
}

impl AudioEngine {
    /// Open the default output device. Returns None (with a warning) if there
    /// is no audio device, so the editor still runs silently.
    pub fn new() -> Option<Self> {
        match OutputStream::try_default() {
            Ok((stream, handle)) => Some(Self {
                _stream: stream,
                handle,
                sinks: HashMap::new(),
            }),
            Err(e) => {
                tracing::warn!("no audio output device: {e}");
                None
            }
        }
    }

    pub fn stop_all(&mut self) {
        for (_, sink) in self.sinks.drain() {
            sink.stop();
        }
    }

    /// Begin playback: start every play-on-start cue (called when Play starts).
    pub fn start(&mut self, cues: &[AudioCue], listener: Vec3, project_root: &Path) {
        self.stop_all();
        for cue in cues {
            if cue.play_on_start && !cue.clip.is_empty() {
                self.start_cue(cue, listener, project_root);
            }
        }
    }

    fn start_cue(&mut self, cue: &AudioCue, listener: Vec3, project_root: &Path) {
        let path = project_root.join(&cue.clip);
        let file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(e) => {
                tracing::error!("opening audio {}: {e}", path.display());
                return;
            }
        };
        let decoder = match Decoder::new(BufReader::new(file)) {
            Ok(d) => d,
            Err(e) => {
                tracing::error!("decoding audio {}: {e}", path.display());
                return;
            }
        };
        let sink = match Sink::try_new(&self.handle) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("creating audio sink: {e}");
                return;
            }
        };
        sink.set_speed(cue.pitch.max(0.01));
        sink.set_volume(cue.gain(listener));
        if cue.looping {
            sink.append(decoder.repeat_infinite());
        } else {
            sink.append(decoder);
        }
        self.sinks.insert(cue.object, sink);
    }

    /// Per-frame update while playing: drop finished one-shots and refresh
    /// each live sink's volume from the listener distance.
    pub fn update(&mut self, cues: &[AudioCue], listener: Vec3) {
        self.sinks.retain(|_, s| !s.empty());
        for cue in cues {
            if let Some(sink) = self.sinks.get(&cue.object) {
                sink.set_volume(cue.gain(listener));
            }
        }
    }
}
