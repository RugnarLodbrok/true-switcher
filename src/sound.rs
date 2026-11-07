use std::f64::consts::TAU;
use std::sync::mpsc::{self, Sender};
use std::thread;

use libpulse_binding::{sample, stream};
use libpulse_simple_binding::Simple;

const SAMPLE_RATE: u32 = 48_000;
const DURATION_MS: u32 = 90;
const FREQUENCY_HZ: u32 = 880;
const FADE_IN_MS: u32 = 10;
const FADE_OUT_MS: u32 = 25;
const VOLUME: f64 = 0.2;
const MILLISECONDS_PER_SECOND: u32 = 1000;

pub struct Sound {
    sender: Option<Sender<()>>,
}

impl Sound {
    pub fn start(is_enabled: bool) -> Self {
        if !is_enabled {
            return Self { sender: None };
        }
        let (sender, receiver) = mpsc::channel();
        let result = thread::Builder::new().name("sound".into()).spawn(move || {
            let samples = synthesize();
            while receiver.recv().is_ok() {
                if let Err(error) = play(&samples) {
                    eprintln!("true-switcher: sound: {error}");
                }
            }
        });
        if let Err(error) = result {
            eprintln!("true-switcher: cannot start sound worker: {error}");
            return Self { sender: None };
        }
        Self {
            sender: Some(sender),
        }
    }

    pub fn play(&self) {
        if let Some(sender) = &self.sender
            && let Err(error) = sender.send(())
        {
            eprintln!("true-switcher: sound worker unavailable: {error}");
        }
    }
}

fn synthesize() -> Vec<u8> {
    let count = (SAMPLE_RATE * DURATION_MS / MILLISECONDS_PER_SECOND) as usize;
    let fade_in = f64::from(SAMPLE_RATE * FADE_IN_MS) / f64::from(MILLISECONDS_PER_SECOND);
    let fade_out = f64::from(SAMPLE_RATE * FADE_OUT_MS) / f64::from(MILLISECONDS_PER_SECOND);
    let mut bytes = Vec::with_capacity(count * size_of::<i16>());
    for index in 0..count {
        let attack = (index as f64 / fade_in).min(1.0);
        let release = ((count - 1 - index) as f64 / fade_out).min(1.0);
        let phase = TAU * f64::from(FREQUENCY_HZ) * index as f64 / f64::from(SAMPLE_RATE);
        let sample = (phase.sin() * attack.min(release) * VOLUME * f64::from(i16::MAX)) as i16;
        bytes.extend_from_slice(&sample.to_ne_bytes());
    }
    bytes
}

fn play(samples: &[u8]) -> Result<(), libpulse_binding::error::PAErr> {
    let spec = sample::Spec {
        format: sample::Format::S16NE,
        channels: 1,
        rate: SAMPLE_RATE,
    };
    let audio = Simple::new(
        None,
        "true-switcher",
        stream::Direction::Playback,
        None,
        "layout switch",
        &spec,
        None,
        None,
    )?;
    audio.write(samples)?;
    audio.drain()
}
