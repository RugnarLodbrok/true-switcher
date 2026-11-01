mod input;

use std::io::{self, Write};
use std::process::ExitCode;

use evdev::EventSummary;

fn run() -> io::Result<()> {
    let events = input::listen()?;
    let stdout = io::stdout();
    let mut output = stdout.lock();

    for event in events {
        let (path, event) = event?;
        if let EventSummary::Key(_, code, value) = event.destructure() {
            let action = match value {
                0 => "release",
                1 => "press",
                2 => "repeat",
                _ => continue,
            };
            writeln!(
                output,
                "{} code={} key={code:?} action={action}",
                path.display(),
                code.0,
            )?;
            output.flush()?;
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
