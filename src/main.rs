mod config;
mod dictionary;
mod focus;
mod history;
mod hold;
mod input;
mod input_events;
mod kde;
mod key_codes;
mod layout;
mod learned_rules;
mod lifecycle;
pub mod output;
mod punctuation;
mod score;
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
use std::ffi::OsStr;
use std::io;
use std::process::ExitCode;
use std::time::Duration;
use switcher::Switcher;

const KEYBOARD_DISCOVERY_DELAY: Duration = Duration::from_millis(500);

fn run() -> io::Result<ShutdownReason> {
    let (sender, events) = input_events::InputEvents::new();
    lifecycle::start_signals(sender.clone())?;
    let (mut config, config_file, auto_switch) = match prepare_configuration() {
        Ok(prepared) => prepared,
        Err(error) => {
            log::error!("true-switcher: {error}");
            return tray::Tray::run_error(error, sender, events);
        }
    };

    let is_dictionary_pair_loaded = auto_switch.dictionaries.is_pair_loaded();
    let warning = disable_autoswitch_without_pair(&mut config, is_dictionary_pair_loaded);
    if let Some(warning) = warning {
        log::warn!("true-switcher: {warning}");
    }
    log::set_max_level(config.log_level);
    let _tray = tray::Tray::start(
        sender.clone(),
        &config,
        config_file,
        is_dictionary_pair_loaded,
        warning,
    )?;

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

fn disable_autoswitch_without_pair(
    config: &mut config::Configuration,
    is_dictionary_pair_loaded: bool,
) -> Option<&'static str> {
    if config.auto_switch && !is_dictionary_pair_loaded {
        config.auto_switch = false;
        Some("Dictionary auto-switch disabled: two current dictionaries are required")
    } else {
        None
    }
}

fn prepare_configuration() -> Result<
    (
        config::Configuration,
        config::ConfigurationFile,
        switch_rules::AutoSwitch,
    ),
    String,
> {
    config::load().and_then(|(config, file)| {
        let learned = learned_rules::LearnedRules::load(file.learned_rules_path())
            .map_err(|error| format!("Configuration: {error}"))?;
        switch_rules::AutoSwitch::new(&config, file.directory(), learned)
            .map(|auto_switch| (config, file, auto_switch))
            .map_err(|error| format!("Configuration: {error}"))
    })
}

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() == Some(OsStr::new(score::COMMAND)) {
        return score::run(args);
    }

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

#[cfg(test)]
mod tests {
    #[test]
    fn startup_without_a_pair_uses_effective_settings_without_saving() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let contents = "switch_key = 'KEY_PAUSE'\nauto_switch = true\npattern_switch = true\n";
        std::fs::write(&path, contents).unwrap();
        let (mut config, _) = crate::config::load_from(&path).unwrap();
        assert!(super::disable_autoswitch_without_pair(&mut config, true).is_none());
        let auto_switch =
            super::switch_rules::AutoSwitch::new(&config, directory.path(), Default::default())
                .unwrap();
        assert!(
            super::disable_autoswitch_without_pair(
                &mut config,
                auto_switch.dictionaries.is_pair_loaded()
            )
            .is_some()
        );
        let settings = crate::config::RuntimeSettings::from(&config);
        assert!(!settings.is_auto_switch);
        assert!(settings.is_pattern_switch);
        assert_eq!(std::fs::read_to_string(path).unwrap(), contents);
    }
}
