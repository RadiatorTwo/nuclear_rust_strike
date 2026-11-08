//! The game's music: its own sequencer (the Miles timer callback 0x497d3c) sends General MIDI
//! channel voice messages through AIL_send_channel_voice_message; they play on a software
//! synthesizer (rustysynth) with the game's SoundFont `data\AUD_IMS1.SF2`, mixed into the
//! sound output (`dsound.rs`).
//!
//! Messages are queued and given to the synthesizer when the output asks for the next block
//! (a few ms late at most, as a hardware synthesizer's queue). The synthesizer is made at the
//! output's rate on its first block.

use super::log;
use rustysynth::{SoundFont, Synthesizer, SynthesizerSettings};
use std::sync::{Arc, Mutex, MutexGuard};

struct Midi {
    font: Option<Arc<SoundFont>>,
    synth: Option<Synthesizer>,
    pending: Vec<[u8; 3]>,
    /// AIL_set_sequence_volume: the volume (0..1), where it goes, and by how much a second
    gain: f32,
    target: f32,
    per_s: f32,
    left: Vec<f32>,
    right: Vec<f32>,
}

static MIDI: Mutex<Midi> =
    Mutex::new(Midi { font: None, synth: None, pending: Vec::new(), gain: 1.0, target: 1.0, per_s: 0.0, left: Vec::new(), right: Vec::new() });

fn midi() -> MutexGuard<'static, Midi> {
    MIDI.lock().unwrap_or_else(|e| e.into_inner())
}

/// The synthesizer's own volume: rustysynth's default (0.5) is quiet against the game's sounds.
const MASTER: f32 = 1.0;

/// AIL_midiOutOpen: load the SoundFont (without it the music stays silent).
pub fn open() {
    if midi().font.is_some() {
        return;
    }
    let path = super::host_path("C:\\NUKEPC\\data\\AUD_IMS1.SF2");
    let font = std::fs::File::open(&path)
        .map_err(|e| e.to_string())
        .and_then(|f| SoundFont::new(&mut std::io::BufReader::new(f)).map_err(|e| format!("{e:?}")));
    match font {
        Ok(f) => {
            log!("music: SoundFont {} ({} presets)", path.display(), f.get_presets().len());
            midi().font = Some(Arc::new(f));
        }
        Err(e) => log!("warn: music: cannot load {}: {e}", path.display()),
    }
}

/// AIL_midiOutClose: all notes off.
pub fn close() {
    let mut m = midi();
    m.pending.clear();
    if let Some(s) = &mut m.synth {
        s.reset();
    }
}

/// A channel voice message (status, data 1, data 2).
pub fn send(status: u8, d1: u8, d2: u8) {
    let mut m = midi();
    if m.font.is_some() {
        m.pending.push([status, d1, d2]);
    }
}

/// AIL_set_sequence_volume: 0..127, reached in `ms`.
pub fn volume(v: u32, ms: u32) {
    let mut m = midi();
    m.target = v.min(127) as f32 / 127.0;
    if ms == 0 {
        m.gain = m.target;
        m.per_s = 0.0;
    } else {
        m.per_s = (m.target - m.gain).abs() * 1000.0 / ms as f32;
    }
}

/// Mix the music into `out` (stereo frames at `rate`): called by the output under the mixer's
/// lock.
pub fn render(out: &mut [[f32; 2]], rate: u32) {
    let mut g = midi();
    let m = &mut *g;
    let Some(font) = &m.font else { return };
    if m.synth.as_ref().is_none_or(|s| s.get_sample_rate() != rate as i32) {
        match Synthesizer::new(font, &SynthesizerSettings::new(rate as i32)) {
            Ok(mut s) => {
                s.set_master_volume(MASTER);
                m.synth = Some(s);
            }
            Err(e) => {
                log!("warn: music: synthesizer: {e:?}");
                m.font = None;
                return;
            }
        }
    }
    let synth = m.synth.as_mut().unwrap();
    for [st, d1, d2] in m.pending.drain(..) {
        synth.process_midi_message((st & 15) as i32, (st & 0xf0) as i32, d1 as i32, d2 as i32);
    }
    let n = out.len();
    m.left.resize(n, 0.0);
    m.right.resize(n, 0.0);
    synth.render(&mut m.left, &mut m.right);
    // the volume moves along the block
    let step = m.per_s / rate as f32;
    for (k, o) in out.iter_mut().enumerate() {
        if m.gain != m.target {
            m.gain = if m.gain < m.target { (m.gain + step).min(m.target) } else { (m.gain - step).max(m.target) };
        }
        o[0] += m.left[k] * m.gain;
        o[1] += m.right[k] * m.gain;
    }
}
