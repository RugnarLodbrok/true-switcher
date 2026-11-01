mod environment;
mod history;
mod input;
pub mod output;

use std::io::{self, Write};
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use evdev::{EventSummary, KeyCode};

fn run() -> io::Result<()> {
    let mut keyboard = output::KeyboardOutput::new()?;
    let events = input::listen()?;
    let stdout = io::stdout();
    let mut output = stdout.lock();
    let mut history = history::History::default();
    let desktop = environment::KdePlasma;

    // Give the desktop time to discover the uinput keyboard before accepting Pause.
    thread::sleep(Duration::from_millis(500));
    eprintln!("Ready");

    for event in events {
        let (path, event) = event?;
        if let EventSummary::Key(_, code, value) = event.destructure() {
            if code == KeyCode::KEY_PAUSE {
                if value == 1 {
                    correct_word(history.latest_word(), &mut keyboard, &desktop)?;
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

fn correct_word(
    word: Option<&history::Word>,
    keyboard: &mut output::KeyboardOutput,
    desktop: &environment::KdePlasma,
) -> io::Result<()> {
    if let Some(word) = word {
        keyboard.erase(word.strokes.len() + word.trailing_spaces)?;
    }
    if let Err(error) = desktop.switch_to_next_layout() {
        eprintln!("true-switcher: {error}");
    }
    if let Some(word) = word {
        keyboard.replay(word)?;
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
