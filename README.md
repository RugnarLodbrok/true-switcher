# True Switcher

Keyboard layout switcher.
 - manual: Press Pause/Break to retype the last word using a different keyboard layout
 - automatic: 
   - small number of custom rules in config
   - dictionary prefix matching

## Why Another Switcher?

### Comparison with Alternatives

| Alternative | Downsides |
| --- | --- |
| [XNeur](https://manpages.ubuntu.com/manpages/resolute/man1/xneur.1.html) | does not support wayland |
| [PolterType](https://poltertype.com/) | solid switcher, but custom commands and autoswitch seemed limited to me. No way to bind switch on PauseBreak. Also switches `.туц` -> `.new`, not `/new` and it is a dealbreakr | 
| [layout-fix](https://github.com/AkuLinker/layout-fix) | doesn't support last word retype, but has different features|
| [XSwitcher](https://github.com/ds-voix/xswitcher) | probably has bugs, or I couldn't configure it well. Very convoluted way of configuration |
| [GSwitch](https://github.com/arumata/gswitch) | uses noticeable delays for switching. When delays're configured to 0 works unreliably. Slow switch often interferes with fast typing |
| [Mahou](https://gitea.com/BladeMight/Mahou) | Best of them all, but only for Windows. Inspiration for TrueSwitcher |
| [Mahou3](https://github.com/Sippelin/Mahou3) | continuation of Mahou |

## Supported environments

| Desktop environment | Supported |
| --- | :---: |
| KDE Plasma (Wayland) | ✅ |
| GNOME | ❌ |
| Cinnamon | ❌ |
| Xfce | ❌ |

Everything else is also not supported.

Efforts to improve this coverage are welcomed. 

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

On Bazzite and Fedora Atomic Desktops (including KDE-based Kinoite):

```sh
rpm-ostree install libxkbcommon-devel pulseaudio-libs-devel
```

Reboot after installing these packages to activate the new deployment; see the [rpm-ostree package layering documentation](https://github.com/coreos/rpm-ostree/blob/main/docs/administrator-handbook.md#hybrid-imagepackaging-via-package-layering).
The development packages are only needed to build from source; users of prebuilt binaries need the runtime libraries and XKB layout data.

## Installation

### Prebuilt

todo

### From sources

```sh
cargo fetch
make build
make install
```

The installer checks for `qdbus` using the system service's `PATH`, which may differ from your shell's `PATH`, and prints the selected executable. The program resolves the executable once at startup using its own `PATH` and logs the selected path.
After installation, the installer starts True Switcher and enables it to start automatically in future graphical sessions.

## Tray and configuration

Edit `~/.config/true-switcher/config.toml`, then select `Reload` in the KDE Plasma tray menu to apply changes. Select `Quit` to stop True Switcher.

The tray can toggle dictionary matching, pattern matching, and the two switch sounds immediately. The `Log level` submenu marks the current level and applies your selection immediately. Select `Save to config` to keep these settings after a restart. This replaces the file with the copy loaded by True Switcher, preserving its comments and formatting. To keep manual edits made on disk, use `Reload` before saving from the tray.

Configuration example with default values for optional settings and one example rule:

```toml
switch_key = "KEY_PAUSE"     # Required Linux key name; e.g. KEY_PAUSE, KEY_F12, ...
log_level = "info"           # off, error, warn, info, debug, trace; trace logs typed words
manual_switch_sound = false  # Play a sound after a successful manual switch
pattern_switch = false      # Enable automatic switching using regex rules
auto_switch = false          # Enable automatic switching using dictionaries
auto_switch_sound = true     # Play a sound after a successful automatic switch

[[switch_rules]]             # Optional; repeat this block to add more rules
target = "us"                # XKB layout identifier configured in KDE, e.g. us or ru. The layout must be present in KDE 
patterns = [                 # Rust regex patterns matched against the converted word.
    '^/[a-z]'                # this rule will auto-switch words like `/start` when typed in non-`us` layout. For example `.ыефке` in `ru` 
] 
```

Add the two required dictionaries described in [Dictionary matching](#dictionary-matching) to complete this configuration or the starter config created by the installer. Rules and dictionaries are validated even when both flags are disabled.

`RUST_LOG` does not override `log_level`. Sound playback requires a PulseAudio-compatible server such as `pipewire-pulse`; playback errors are logged without interrupting switching.

## Automatic switching

Both flags default to `false`. Set `pattern_switch = true` to enable regex rules and `auto_switch = true` to enable dictionary matching. Select `Reload` to apply changes. If you previously used `auto_switch = true` for regex rules, use `pattern_switch = true` for them now.

After each typed character and on the first trailing space, the current word is converted to each rule's `target` layout and matched against its `patterns`. The first matching rule corrects the word and selects that layout. For example, `.т` typed in Russian becomes `/n` with this rule. After any correction, automatic switching is blocked for the rest of that word. Rules for the current layout are skipped. Backspace cancels a pending automatic switch.

Patterns use the Rust `regex` syntax. Matching can occur anywhere in the converted word; use `^` to anchor the beginning. `target` must be a single XKB layout identifier, such as `us`, `gb`, or `ru`. Language codes such as `en` are invalid. If KDE has multiple entries with the same identifier, the first entry and its configured variant are used. A separate `variant` field is not supported.

Invalid patterns, invalid layout identifiers, and targets absent from KDE cause startup to fail with a logged error, including when `pattern_switch = false`. `switch_rules` defaults to an empty list. `auto_switch_sound` defaults to `true` and controls the existing switch sound independently of `manual_switch_sound`.

Manual layout changes reset typing history. Punctuation remains part of a word; Space ends a word, while Tab and Enter end the word and reset history.

### Dictionary matching

Configure exactly two `[[dict]]` entries with distinct targets. Set `auto_switch = true` to enable matching. This feature supports up to two KDE layouts. Each file path is relative to `config.toml`. Put one word and its integer frequency on each line, for example `apple 10`.

The optional top-level `dict_skip_pattern` applies to both dictionaries at load time. It skips an entry when the regex matches the entire word field. For example, `'cat'` skips `cat` and keeps `bobcat`. Leave the setting out or set it to `''` to load words of any case. The example below skips words containing an uppercase Unicode letter.

```toml
dict_skip_pattern = '.*\p{Lu}.*'

[[dict]]
target = "us"
file = "dictionaries/en_full.txt"
threshold = 5.0

[[dict]]
target = "ru"
file = "dictionaries/ru_full.txt"
threshold = 7.0
```

The dictionary check runs on each typed character when regex rules are disabled or no rule matches. For each layout, the prefix probability is the combined frequency of words with that prefix divided by the total frequency of accepted entries. The score is `log₂(P(alternative prefix) / P(current prefix))`. A correction is requested when the score strictly exceeds the destination dictionary's `threshold`. A prefix found only in the alternative dictionary always passes. A prefix absent from the alternative dictionary never triggers a correction.

Choose finite positive thresholds in bits. The example values require a likelihood ratio above 32 for switching to `us` and above 128 for switching to `ru`. With `log_level = "trace"`, each typed character logs the score as `dict switch score: +3.43`.

## Switching statistics

Each successful layout switch is recorded in SQLite, even if replaying the word is later cancelled or fails.
The database is stored at `$XDG_STATE_HOME/true-switcher/statistics.sqlite3`, or `~/.local/state/true-switcher/statistics.sqlite3` when `XDG_STATE_HOME` is unset or empty.

The `switches` table contains `id`, `timestamp` (Unix seconds), `prev` (the previous word as typed), `a` (the corrected word before switching), `b` (the word in the new layout), and `mode` (`0` for manual corrections, `1` for pattern switches, `2` for dictionary auto-switches). Existing databases without `mode` gain it automatically; those old records receive `0`. Earlier versions recorded both automatic types as `1`; those records are unchanged and cannot be distinguished by type.
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
