use std::{
    io::{self, Write},
    path::Path,
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    thread,
    time::{Duration, Instant},
};

use crate::dictionary::{DictionaryLoadObserver, DictionaryLoadProgress};

const FRAME_INTERVAL: Duration = Duration::from_millis(100);
const BAR_WIDTH: usize = 20;
const SPINNER_FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const CLEAR_LINE: &str = "\r\x1b[2K";

enum Frame {
    Reading { filename: String, percent: u64 },
    BuildingIndex(String),
}

pub(super) fn during_load<T>(
    is_visible: bool,
    load: impl FnOnce(Option<&mut DictionaryLoadObserver<'_>>) -> T,
) -> T {
    if !is_visible {
        return load(None);
    }
    thread::scope(|scope| {
        let (sender, receiver) = mpsc::channel();
        scope.spawn(|| draw_frames(receiver, io::stderr()));
        let mut last_reading = None;
        load(Some(&mut |path, progress| {
            let frame = match progress {
                DictionaryLoadProgress::Reading {
                    bytes_read,
                    total_bytes,
                } => {
                    let percent = percentage(bytes_read, total_bytes);
                    let now = Instant::now();
                    if last_reading.is_some_and(|(previous, drawn_at)| {
                        previous == percent || (percent < 100 && now - drawn_at < FRAME_INTERVAL)
                    }) {
                        return;
                    }
                    last_reading = Some((percent, now));
                    Frame::Reading {
                        filename: filename(path),
                        percent,
                    }
                }
                DictionaryLoadProgress::BuildingIndex => {
                    last_reading = None;
                    Frame::BuildingIndex(filename(path))
                }
            };
            let _ = sender.send(frame);
        }))
    })
}

fn filename(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .escape_debug()
        .to_string()
}

fn percentage(bytes_read: u64, total_bytes: u64) -> u64 {
    if total_bytes == 0 {
        return 0;
    }
    ((u128::from(bytes_read.min(total_bytes)) * 100) / u128::from(total_bytes)) as u64
}

fn draw_frames(receiver: Receiver<Frame>, mut output: impl Write) -> io::Result<()> {
    let mut building = None;
    let mut is_visible = false;
    loop {
        let next = if building.is_some() {
            receiver.recv_timeout(FRAME_INTERVAL)
        } else {
            receiver.recv().map_err(|_| RecvTimeoutError::Disconnected)
        };
        let text = match next {
            Ok(Frame::Reading { filename, percent }) => {
                building = None;
                let filled = percent as usize * BAR_WIDTH / 100;
                let bar = "█".repeat(filled) + &"░".repeat(BAR_WIDTH - filled);
                format!("Reading {filename} [{bar}] {percent:>3}%")
            }
            Ok(Frame::BuildingIndex(filename)) => {
                let text = spinner(&filename, 0);
                building = Some((filename, 0));
                text
            }
            Err(RecvTimeoutError::Timeout) => {
                let (filename, frame) = building.as_mut().expect("spinner is running");
                *frame = (*frame + 1) % SPINNER_FRAMES.len();
                spinner(filename, *frame)
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };
        write!(output, "{CLEAR_LINE}{text}")?;
        output.flush()?;
        is_visible = true;
    }
    if is_visible {
        write!(output, "{CLEAR_LINE}")?;
        output.flush()?;
    }
    Ok(())
}

fn spinner(filename: &str, frame: usize) -> String {
    format!("{} Building index for {filename}…", SPINNER_FRAMES[frame])
}
