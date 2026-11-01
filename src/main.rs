mod environment;
mod history;
mod input;
pub mod output;

use std::io::{self, Write};
use std::process::ExitCode;

use evdev::{EventSummary, KeyCode};

fn run() -> io::Result<()> {
    let events = input::listen()?;
    let stdout = io::stdout();
    let mut output = stdout.lock();
    let mut history = history::History::default();
    let desktop = environment::KdePlasma;

    for event in events {
        let (path, event) = event?;
        if let EventSummary::Key(_, code, value) = event.destructure() {
            if code == KeyCode::KEY_PAUSE {
                // Only the initial press switches: releases and repeats do not.
                if value == 1
                    && let Err(error) = desktop.switch_to_next_layout()
                {
                    eprintln!("true-switcher: {error}");
                }
                continue;
            }
            for line in history.process(&path, code, value) {
                writeln!(output, "{line}")?;
                output.flush()?;
            }
        }
    }

    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("true-switcher: {error}");
            ExitCode::FAILURE
        }
    }
}
