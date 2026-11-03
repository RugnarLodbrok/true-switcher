mod environment;
mod history;
mod input;
pub mod output;

use std::io;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

fn run() -> io::Result<()> {
    let mut keyboard = output::KeyboardOutput::new()?;
    let mut input = input::Input::open()?;
    let desktop = environment::KdePlasma;

    // Give the desktop time to discover the uinput keyboard before accepting Pause.
    thread::sleep(Duration::from_millis(500));
    eprintln!("Ready");

    loop {
        input.wait_for_request()?;
        input.enter()?;
        let word = input.latest_word().cloned();
        let is_cancelled = AtomicBool::new(false);
        thread::scope(|scope| -> io::Result<()> {
            let worker =
                scope.spawn(|| correct_word(word.as_ref(), &mut keyboard, &desktop, &is_cancelled));
            let mut read_error = None;
            while !worker.is_finished() {
                if let Err(error) = input.read_captured() {
                    is_cancelled.store(true, Ordering::Relaxed);
                    read_error = Some(error);
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
            let result = worker
                .join()
                .map_err(|_| io::Error::other("correction thread panicked"))?;
            if let Some(error) = read_error {
                return Err(error);
            }
            result
        })?;
        input.leave()?;
    }
}

fn correct_word(
    word: Option<&history::Word>,
    keyboard: &mut output::KeyboardOutput,
    desktop: &environment::KdePlasma,
    is_cancelled: &AtomicBool,
) -> io::Result<()> {
    if is_cancelled.load(Ordering::Relaxed) {
        return Ok(());
    }
    eprintln!("SYNTHETIC erase");
    if let Some(word) = word {
        keyboard.erase(word.strokes.len() + word.trailing_spaces)?;
    }
    if is_cancelled.load(Ordering::Relaxed) {
        return Ok(());
    }
    desktop.switch_to_next_layout()?;
    if is_cancelled.load(Ordering::Relaxed) {
        return Ok(());
    }
    eprintln!("SYNTHETIC replay");
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
