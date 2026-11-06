use std::error::Error;
use std::f64::consts::TAU;
use std::fs;
use std::io;

use libpulse_binding::{sample, stream};
use libpulse_simple_binding::Simple;
use serde::Deserialize;

const CONFIG_PATH: &str = "sound-debug.toml";
const SAMPLE_RATE: u32 = 48_000;
const MAX_DURATION_MS: u32 = 10_000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    duration_ms: u32,
    frequency_hz: u32,
    fade_in_ms: u32,
    fade_out_ms: u32,
    volume: f64,
}

impl Settings {
    fn validate(&self) -> Result<(), io::Error> {
        if self.duration_ms == 0
            || self.duration_ms > MAX_DURATION_MS
            || self.frequency_hz == 0
            || self.frequency_hz >= SAMPLE_RATE / 2
            || self.fade_in_ms > self.duration_ms
            || self.fade_out_ms > self.duration_ms
            || !(0.0..=1.0).contains(&self.volume)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid sound parameters",
            ));
        }
        Ok(())
    }
}

fn synthesize(settings: &Settings) -> Vec<u8> {
    let count = (u64::from(SAMPLE_RATE) * u64::from(settings.duration_ms) / 1000) as usize;
    let fade_in = f64::from(settings.fade_in_ms) * f64::from(SAMPLE_RATE) / 1000.0;
    let fade_out = f64::from(settings.fade_out_ms) * f64::from(SAMPLE_RATE) / 1000.0;
    let mut bytes = Vec::with_capacity(count * size_of::<i16>());
    for index in 0..count {
        let attack = if fade_in > 0.0 {
            (index as f64 / fade_in).min(1.0)
        } else {
            1.0
        };
        let release = if fade_out > 0.0 {
            ((count - 1 - index) as f64 / fade_out).min(1.0)
        } else {
            1.0
        };
        let phase = TAU * f64::from(settings.frequency_hz) * index as f64 / f64::from(SAMPLE_RATE);
        let sample =
            (phase.sin() * attack.min(release) * settings.volume * f64::from(i16::MAX)) as i16;
        bytes.extend_from_slice(&sample.to_ne_bytes());
    }
    bytes
}

fn run() -> Result<(), Box<dyn Error>> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| CONFIG_PATH.into());
    let settings: Settings = toml::from_str(&fs::read_to_string(&path)?)?;
    settings.validate()?;
    let spec = sample::Spec {
        format: sample::Format::S16NE,
        channels: 1,
        rate: SAMPLE_RATE,
    };
    let audio = Simple::new(
        None,
        "true-switcher-sound-debug",
        stream::Direction::Playback,
        None,
        "debug beep",
        &spec,
        None,
        None,
    )?;
    audio.write(&synthesize(&settings))?;
    audio.drain()?;
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("sound-debug: {error}");
        std::process::exit(1);
    }
}
