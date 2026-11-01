mod history;
mod input;

use std::io::{self, Write};
use std::process::ExitCode;

use evdev::EventSummary;

fn run() -> io::Result<()> {
    let events = input::listen()?;
    let stdout = io::stdout();
    let mut output = stdout.lock();
    let mut history = history::History::default();

    for event in events {
        let (path, event) = event?;
        if let EventSummary::Key(_, code, value) = event.destructure() {
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
