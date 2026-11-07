# True Switcher

Keyboard layout switcher. Press Pause/Break to retype the last word using a different keyboard layout
Automatic layout switching is not supported for now.

## Why Another Switcher?

### Comparison with Alternatives

| Alternative | Downsides |
| --- | --- |
| [XNeur](https://manpages.ubuntu.com/manpages/resolute/man1/xneur.1.html) | does not support wayland |
| [PolterType](https://poltertype.com/) | solid switcher, but custom commands and autoswitch seemed limited to me. No way to bind switch on PauseBreak. Also switches `.туц` -> `.new`, not `/new` and it is really bad | 
| [layout-fix](https://github.com/AkuLinker/layout-fix) | doesn't support last word retype, but has different features|
| [XSwitcher](https://github.com/ds-voix/xswitcher) | probably has bugs, or I couldn't configure it well. Very convoluted way of configuration |
| [GSwitch](https://github.com/arumata/gswitch) | uses noticeable delays for switching. When delays're configured to 0 works unreliably. Slow switch often interferes with fast typing |
| [Mahou](https://gitea.com/BladeMight/Mahou) | Best of them all, but only for Windows. Inspiration for TrueSwitcher |
| [Mahou3](https://github.com/Sippelin/Mahou3) | continuation of Mahou |

## Requirements

### Runtime

- Linux with `evdev` and `uinput` support
- KDE Plasma on Wayland.
- `systemd` and a `qdbus` executable in the runtime `PATH`. Supported names, in preference order: `qdbus-qt6`, `qdbus6`, `qdbus`. Fedora/Bazzite provides it in `qt6-qttools`.
- `libxkbcommon` and XKB layout data (`xkeyboard-config`)

### Build

- Rust 1.89 or newer and Cargo.
- A C compiler for the bundled SQLite library.
- Development files for `libxkbcommon` (`libxkbcommon-devel` on Fedora/Bazzite).

On Fedora:

```sh
sudo dnf install libxkbcommon-devel pulseaudio-libs-devel
```

On Bazzite:

```sh
rpm-ostree install libxkbcommon-devel pulseaudio-libs-devel
```

Reboot after installing the package on Bazzite to activate the new deployment.
The development packages are only needed to build from source; users of prebuilt binaries need the runtime libraries and XKB layout data.

## Installation

### Prebuilt

### From sources

```sh
cargo fetch
make build
make install
```

The installer checks for `qdbus` using the system service's `PATH`, which may differ from your shell's `PATH`, and prints the selected executable. The program resolves the executable once at startup using its own `PATH` and logs the selected path.
After installation, the installer starts True Switcher and enables it to start automatically in future graphical sessions.

## Tray and configuration

True Switcher shows an icon in the KDE Plasma system tray while the process is running.
Its menu contains `Reload`, which restarts the system service `true-switcher.service` without waiting for the restart to finish. The icon briefly disappears and the new process reads `~/.config/true-switcher/config.toml` again.
`Quit` stops the system service.

If the configuration cannot be read, or `switch_key` is missing or invalid, the program uses `KEY_PAUSE` and sends a desktop notification. A failed restart request is also logged and reported in a notification.

To play a short sound after a successful manual switch, set the following in `~/.config/true-switcher/config.toml` and select Reload in the tray menu:

```toml
manual_switch_sound = false
```

Change `false` to `true` to enable sound. When the option is missing, sound is disabled. Playback errors are logged and do not interrupt switching. Linux playback requires a PulseAudio-compatible server such as `pipewire-pulse`.
The tray registers again when Plasma's tray service becomes available or changes its D-Bus owner.

The installer copies the SVG from `icon/` to `/usr/local/share/true-switcher/icons/` and allows `restart` for this service in its Polkit rule. Runs from the source tree can use `icon/` directly; `Reload` always targets the installed system service.
Use `make update-installed` to rebuild and update the installed binary, stopping and starting the system service around the update. This does not update the installed icon, service units or Polkit rule.

## Automatic switching

Automatic switching is disabled by default. Add these settings to `~/.config/true-switcher/config.toml` and select `Reload`:

```toml
auto_switch = true
auto_switch_sound = true

[[switch_rules]]
target = "us"
patterns = ['^/[a-z]']
```

After each typed character, the current word is converted to each rule's `target` layout and matched against its `patterns`. The first matching rule corrects the word and selects that layout, without waiting for a separator. For example, `.т` typed in Russian becomes `/n` with this rule. The word can switch again when later characters match another target. Rules for the current layout are skipped; Backspace does not trigger matching.

Patterns use the Rust `regex` syntax. Matching can occur anywhere in the converted word; use `^` to anchor the beginning. `target` must be a single XKB layout identifier, such as `us`, `gb`, or `ru`. Language codes such as `en` are invalid. If KDE has multiple entries with the same identifier, the first entry and its configured variant are used. A separate `variant` field is not supported.

Invalid patterns, invalid layout identifiers, and targets absent from KDE cause startup to fail with a logged error, including when `auto_switch = false`. `switch_rules` defaults to an empty list. `auto_switch_sound` defaults to `true` and controls the existing switch sound independently of `manual_switch_sound`.

Manual layout changes reset typing history. Punctuation remains part of a word; Space ends a word, while Tab and Enter end the word and reset history.

## Switching statistics

Each successful layout switch is recorded in SQLite, even if replaying the word is later cancelled or fails.
The database is stored at `$XDG_STATE_HOME/true-switcher/statistics.sqlite3`, or `~/.local/state/true-switcher/statistics.sqlite3` when `XDG_STATE_HOME` is unset or empty.

The `switches` table contains `id`, `timestamp` (Unix seconds), `prev` (the previous word as typed), `a` (the corrected word before switching), `b` (the word in the new layout), and `mode` (`0` for manual corrections, `1` for automatic corrections). Existing databases gain `mode` automatically; old records receive `0`.
Missing words are stored as empty strings. Records are skipped when the layout or text cannot be determined. Database errors are logged and do not stop the switcher.

The same database stores typing frequencies in `typing_words`, with `word` (`TEXT PRIMARY KEY`) and `count` (`INTEGER NOT NULL`).
Each word is counted once when it leaves the typing history, including when the whole history is reset. The first occurrence creates a counter of 1; subsequent occurrences increment it.
Corrections update the word kept in history, so only its final version is counted. Words fully erased with Backspace are excluded.
Words are converted to lowercase, preserving punctuation: `Привет` and `привет` share a counter, while `привет,` has a separate counter.
Words that cannot be converted to text are skipped with a log message. Both tables are written by the existing statistics worker thread.

## Tests

```sh
cargo test --bin true-switcher
```

Installer tests cover command discovery and the tray icon's installation, removal and rollback using Python's standard library, temporary files and mocked system commands:

```sh
python3 -m unittest discover -s installer -p 'test_*.py'
```

The D-Bus tests use a separate `dbus-daemon` and do not change the desktop layout.
An additional test can read the layout from a running KDE session:

```sh
cargo test --bin true-switcher reads_running_kde_layout -- --ignored
```
