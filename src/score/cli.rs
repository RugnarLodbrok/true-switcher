use std::{
    ffi::OsString,
    io::{self, BufRead, IsTerminal, Write},
    os::fd::{AsRawFd, RawFd},
    process::ExitCode,
};

use clap::{Arg, ArgAction, Command, value_parser};
use rustline::{Config, Rustline, RustlineError};

use crate::{config, kde};

use super::{ScoreSession, progress};

pub(crate) const COMMAND: &str = "score";
const LEARNED_RULES: &str = "learned-rules";
const THRESHOLD: &str = "threshold";
const PROMPT: &str = "> ";
const END_OK: &str = "END OK";
const END_ERROR: &str = "END ERROR";

const AFTER_HELP: &str = "\
Request format:
  FROM TO WORD [INDEX ...]
  Use the configured layout names for FROM and TO.
  Append one zero-based choice index per ambiguous character, from left to right.

Examples:
  ru us куищще
  us ru bkb
  us ru //tmp 1 0

Output:
  One score row per prefix, then one for the first space (shown as ␣).
  With terminal input, the prompt is \"> \".
  With input from a file or pipe, each response ends with END OK or END ERROR.
  Error details go to stderr.

Exit status:
  0  Help shown or normal completion.
  1  Startup failure or a request error when reading from a file or pipe.
  2  Invalid command-line arguments.

  Request errors are reported immediately. Processing continues until EOF.";

#[derive(Debug, PartialEq)]
struct Options {
    is_learned_enabled: bool,
    threshold: Option<f64>,
}

impl Options {
    fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Self, clap::Error> {
        let matches =
            command().try_get_matches_from(std::iter::once(COMMAND.into()).chain(args))?;
        Ok(Self {
            is_learned_enabled: *matches.get_one(LEARNED_RULES).expect("boolean default"),
            threshold: matches.get_one(THRESHOLD).copied(),
        })
    }
}

fn command() -> Command {
    Command::new(COMMAND)
        .bin_name("true-switcher score")
        .about("Score words against the configured dictionaries in a single session.\nRead requests from stdin until EOF.")
        .arg(
            Arg::new(LEARNED_RULES)
                .short('l')
                .long(LEARNED_RULES)
                .action(ArgAction::Set)
                .value_name("true|false")
                .value_parser(value_parser!(bool))
                .default_value("true")
                .help("Use learned rules and show the Learned column"),
        )
        .arg(
            Arg::new(THRESHOLD)
                .short('t')
                .long(THRESHOLD)
                .value_name("NUMBER")
                .allow_negative_numbers(true)
                .value_parser(parse_threshold)
                .help("Override the target dictionary's threshold for this session. Must be finite and positive."),
        )
        .after_help(AFTER_HELP)
}

fn parse_threshold(value: &str) -> Result<f64, &'static str> {
    value
        .parse::<f64>()
        .ok()
        .filter(|threshold| threshold.is_finite() && *threshold > 0.0)
        .ok_or("threshold must be a finite positive number")
}

pub(crate) fn run(args: impl IntoIterator<Item = OsString>) -> ExitCode {
    let options = match Options::parse(args) {
        Ok(options) => options,
        Err(error) => error.exit(),
    };
    match run_with_options(options) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("score: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run_with_options(options: Options) -> Result<ExitCode, String> {
    let stdin = io::stdin();
    let stderr = io::stderr();
    let is_interactive = stdin.is_terminal();
    let is_stderr_terminal = stderr.is_terminal();
    let (config, config_file) = config::load()?;
    let kde_path = kde::config_path().map_err(|error| error.to_string())?;
    let session = progress::during_load(is_interactive && is_stderr_terminal, |observer| {
        ScoreSession::from_config(
            &config,
            &config_file,
            &kde_path,
            options.is_learned_enabled,
            observer,
        )
    })
    .map_err(|error| error.to_string())?;
    let mut editor = (is_interactive && is_stderr_terminal).then(|| {
        Rustline::with_config(Config {
            history_max_size: usize::MAX,
            enable_hints: false,
            enable_completion: false,
            enable_multiline: false,
            highlight_brackets: false,
            ..Config::default()
        })
    });
    let mut input = stdin.lock();
    run_session(
        &session,
        options.threshold,
        is_interactive,
        |line, diagnostics| {
            if let Some(editor) = &mut editor {
                return read_edited_request(editor, stdin.as_raw_fd(), stderr.as_raw_fd(), line);
            }
            if is_interactive {
                write!(diagnostics, "{PROMPT}")?;
                diagnostics.flush()?;
            }
            input.read_until(b'\n', line).map(|read| read != 0)
        },
        io::stdout(),
        stderr.lock(),
    )
    .map_err(|error| error.to_string())
}

fn read_edited_request(
    editor: &mut Rustline,
    input: RawFd,
    output: RawFd,
    line: &mut Vec<u8>,
) -> io::Result<bool> {
    loop {
        // Rustline writes working edits back into browsed entries. Keep the submitted lines.
        let history = editor.history().clone();
        let request = editor.readline_raw(PROMPT, "", input, output);
        editor.set_history(history);
        match request {
            Ok(request) => {
                line.extend_from_slice(request.as_bytes());
                if !request.trim().is_empty() {
                    editor.history_mut().push(request);
                }
                return Ok(true);
            }
            Err(RustlineError::Interrupted) => continue,
            Err(RustlineError::Eof) => return Ok(false),
            Err(error) => return Err(io::Error::other(error)),
        }
    }
}

fn run_session<D: Write>(
    session: &ScoreSession,
    threshold: Option<f64>,
    is_interactive: bool,
    mut read_request: impl FnMut(&mut Vec<u8>, &mut D) -> io::Result<bool>,
    mut output: impl Write,
    mut diagnostics: D,
) -> io::Result<ExitCode> {
    writeln!(diagnostics, "Dictionaries loaded.")?;
    diagnostics.flush()?;
    let mut line = Vec::new();
    let mut had_request_error = false;
    loop {
        line.clear();
        if !read_request(&mut line, &mut diagnostics)? {
            break;
        }
        let request = std::str::from_utf8(&line);
        if request.is_ok_and(|text| text.trim().is_empty()) {
            continue;
        }
        let trace = request
            .map_err(|error| format!("Request must be valid UTF-8: {error}"))
            .and_then(|text| session.trace(text, threshold));
        let marker = match trace {
            Ok(table) => {
                output.write_all(table.as_bytes())?;
                END_OK
            }
            Err(error) => {
                had_request_error = true;
                writeln!(diagnostics, "{error}")?;
                diagnostics.flush()?;
                END_ERROR
            }
        };
        if !is_interactive {
            writeln!(output, "{marker}")?;
        }
        output.flush()?;
    }
    Ok(ExitCode::from(u8::from(
        !is_interactive && had_request_error,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_short_and_long_flags_and_uses_safe_defaults() {
        let parse = |args: &[&str]| Options::parse(args.iter().map(OsString::from)).unwrap();
        assert_eq!(
            parse(&[]),
            Options {
                is_learned_enabled: true,
                threshold: None
            }
        );
        let expected = Options {
            is_learned_enabled: false,
            threshold: Some(2.5),
        };
        assert_eq!(parse(&["-l", "false", "-t", "2.5"]), expected);
        assert_eq!(
            parse(&["--learned-rules", "false", "--threshold", "2.5"]),
            expected
        );
    }
}
