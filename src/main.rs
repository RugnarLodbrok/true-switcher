mod config;
mod focus;
mod history;
mod hold;
mod input;
mod input_events;
mod kde;
mod layout;
pub mod output;
mod sound;
mod statistics;
mod switch_rules;
mod switcher;
mod tray;
mod word;
mod xkb;

use std::io;
use std::process::ExitCode;
use std::time::Duration;
use switcher::Switcher;

const KEYBOARD_DISCOVERY_DELAY: Duration = Duration::from_millis(500);

fn run() -> io::Result<()> {
    let (config, auto_switch) = match prepare_configuration() {
        Ok(prepared) => prepared,
        Err(error) => {
            log::error!("true-switcher: {error}");
            return tray::Tray::run_error(error);
        }
    };

    log::set_max_level(config.log_level);
    let _tray = tray::Tray::start()?;

    let desktop = kde::KdePlasma::new()?;
    let keyboard = output::KeyboardOutput::new()?;
    let statistics = statistics::Statistics::start();
    let sound = sound::Sound::start(config.manual_switch_sound || config.auto_switch_sound);

    let (sender, events) = input_events::InputEvents::new();
    let _focus_service = focus::FocusService::start(sender.clone())?;
    let layouts = kde::LayoutTracker::start(sender.clone());

    let mut input = input::Input::open(
        events,
        sender,
        config.switch_key,
        statistics.clone(),
        auto_switch,
    )?;

    let mut switcher = Switcher {
        desktop,
        keyboard,
        statistics,
        sound,
        layouts,
    };

    input.wait_for_keyboard_discovery(KEYBOARD_DISCOVERY_DELAY)?;
    log::info!("Ready");

    switcher.process_requests(&mut input, &config)
}

fn prepare_configuration() -> Result<(config::Configuration, switch_rules::AutoSwitchRules), String>
{
    config::load().and_then(|config| {
        switch_rules::AutoSwitchRules::new(&config)
            .map(|auto_switch| (config, auto_switch))
            .map_err(|error| format!("Configuration: {error}"))
    })
}

fn main() -> ExitCode {
    env_logger::Builder::new()
        .filter_level(log::LevelFilter::Trace)
        .init();
    log::set_max_level(log::LevelFilter::Info);
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(error) => {
            log::error!("true-switcher: {error}");
            ExitCode::FAILURE
        }
    }
}
