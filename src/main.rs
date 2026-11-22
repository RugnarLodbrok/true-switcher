mod config;
mod dictionary;
mod focus;
mod history;
mod hold;
mod input;
mod input_events;
mod kde;
mod layout;
mod learned_rules;
mod lifecycle;
pub mod output;
mod sound;
mod statistics;
mod switch_rules;
mod switcher;
#[cfg(test)]
mod test_logging;
mod tray;
mod word;
mod xkb;

use lifecycle::{RESTART_EXIT_CODE, ShutdownReason};
use std::io;
use std::process::ExitCode;
use std::time::Duration;
use switcher::Switcher;

const KEYBOARD_DISCOVERY_DELAY: Duration = Duration::from_millis(500);

fn run() -> io::Result<ShutdownReason> {
    let (sender, events) = input_events::InputEvents::new();
    lifecycle::start_signals(sender.clone())?;
    let (config, config_file, auto_switch) = match prepare_configuration() {
        Ok(prepared) => prepared,
        Err(error) => {
            log::error!("true-switcher: {error}");
            return tray::Tray::run_error(error, sender, events);
        }
    };

    log::set_max_level(config.log_level);
    let _tray = tray::Tray::start(sender.clone(), &config, config_file)?;

    let desktop = kde::KdePlasma::new()?;
    let keyboard = output::KeyboardOutput::new()?;
    let statistics = statistics::Statistics::start();
    let sound = sound::Sound::default();

    let _focus_service = focus::FocusService::start(sender.clone())?;
    let layouts = kde::LayoutTracker::start(sender.clone());

    let mut input = input::Input::open(events, sender, &config, statistics.clone(), auto_switch)?;

    let mut switcher = Switcher {
        desktop,
        keyboard,
        statistics,
        sound,
        layouts,
    };

    match input.wait_for_keyboard_discovery(KEYBOARD_DISCOVERY_DELAY) {
        Ok(()) => log::info!("Ready"),
        Err(lifecycle::ProcessingError::Interrupted) => {}
        Err(lifecycle::ProcessingError::Io(error)) => return Err(error),
    }

    switcher.process_requests(&mut input)
}

fn prepare_configuration() -> Result<
    (
        config::Configuration,
        config::ConfigurationFile,
        switch_rules::AutoSwitchRules,
    ),
    String,
> {
    config::load().and_then(|(config, file)| {
        let learned = learned_rules::LearnedRules::load(file.learned_rules_path())
            .map_err(|error| format!("Configuration: {error}"))?;
        switch_rules::AutoSwitchRules::new(&config, learned)
            .map(|auto_switch| (config, file, auto_switch))
            .map_err(|error| format!("Configuration: {error}"))
    })
}

fn main() -> ExitCode {
    env_logger::Builder::new()
        .filter_level(log::LevelFilter::Trace)
        .init();
    log::set_max_level(log::LevelFilter::Info);
    match run() {
        Ok(ShutdownReason::Stop) => ExitCode::SUCCESS,
        Ok(ShutdownReason::Restart) => {
            log::info!(
                "Restart requested; exiting with status {RESTART_EXIT_CODE}. Automatic restart requires systemd"
            );
            ExitCode::from(RESTART_EXIT_CODE)
        }
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(error) => {
            log::error!("true-switcher: {error}");
            ExitCode::FAILURE
        }
    }
}
