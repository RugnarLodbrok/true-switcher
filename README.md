# True Switcher

Keyboard layout switcher.
 - manual: Press Pause/Break to retype the last word using a different keyboard layout
 - automatic: 
   - Regex-based rules - for user-defined cases
   - dictionary-based heuristics - for general words

## Table of contents

- [Why Another Switcher?](#why-another-switcher)
  - [Comparison with Alternatives](#comparison-with-alternatives)
- [Supported environments](#supported-environments)
- [Requirements](#requirements)
  - [Runtime requirements](#runtime-requirements)
  - [Build requirements](#build-requirements)
- [Installation](#installation)
  - [Prebuilt](#prebuilt)
  - [From sources](#from-sources)
    - [what `make install` does](#what-make-install-does)
    - [systemd integration](#systemd-integration)
- [Tray and configuration](#tray-and-configuration)
  - [config example](#config-example)
- [Automatic switching](#automatic-switching)
  - [Regex rules](#regex-rules)
  - [Dictionary-based switch](#dictionary-based-switch)
    - [config](#config)
    - [implementation details and meaning of thresholds](#implementation-details-and-meaning-of-thresholds)
    - [manual correction and learned-rules](#manual-correction-and-learned-rules)
- [Switching statistics](#switching-statistics)

## Why Another Switcher?

### Comparison with Alternatives

| Alternative | Downsides |
| --- | --- |
| [XNeur](https://manpages.ubuntu.com/manpages/resolute/man1/xneur.1.html) | does not support Wayland |
| [PolterType](https://poltertype.com/) | solid switcher, but custom commands and autoswitch seemed limited to me. No way to bind the switch to PauseBreak. Also switches `.туц` -> `.new`, not `/new` and it is a dealbreaker | 
| [layout-fix](https://github.com/AkuLinker/layout-fix) | doesn't support last word retyping, but has different features|
| [XSwitcher](https://github.com/ds-voix/xswitcher) | probably has bugs, or I couldn't configure it well. Very convoluted way of configuration |
| [GSwitch](https://github.com/arumata/gswitch) | uses noticeable delays for switching. It works unreliably when the delays are set to 0. Slow switching often interferes with fast typing |
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

### Runtime requirements

- Linux with `evdev` and `uinput` support (basically any Linux)
- KDE Plasma on Wayland.
- `systemd` and a `qdbus` executable in the runtime `PATH`. Supported names, in preference order: `qdbus-qt6`, `qdbus6`, `qdbus`. Fedora/Bazzite provides it in `qt6-qttools`.
- `libxkbcommon` and XKB layout data (`xkeyboard-config`)

### Build requirements

- Rust 1.89 or newer and Cargo.
- A C compiler for the bundled SQLite library.
- Development files for `libxkbcommon` and the PulseAudio client libraries.

Install the library dependencies for your distribution.

| Distribution | Install command |
| --- | --- |
| Debian, Ubuntu, Kubuntu | `sudo apt install libxkbcommon-dev libpulse-dev` |
| Fedora | `sudo dnf install libxkbcommon-devel pulseaudio-libs-devel` |
| Bazzite, Fedora Atomic Desktops (including Kinoite) | `rpm-ostree install libxkbcommon-devel pulseaudio-libs-devel` |
| Arch Linux | `sudo pacman -S libxkbcommon libpulse` |
| openSUSE Leap and Tumbleweed | `sudo zypper install libxkbcommon-devel libpulse-devel` |

## Installation

### Prebuilt

N/A

### From sources

```sh
cargo fetch
make build
make install
```

#### what `make install` does

- Creates `~/.config/true-switcher/config.toml`, no overwrite
- Installs the binary in `/usr/local/bin` and tray icons in `/usr/local/share/true-switcher/icons`.
- Installs system and user `systemd` units and a `polkit` rule. See below for details on the systemd units
- Restores the binary's SELinux label when applicable.
- Starts True Switcher and enables it to start automatically in future graphical sessions.
- Provides steps for checking for SELinux permission issues

#### systemd integration

- **`true-switcher.service` (system)**
  - runs True Switcher as your user
  - access to input devices is granted via supplementary group `input`, without adding your user to that group
- **`true-switcher-session.service` (user)** 
  - oneshot
  - ties the system service to your KDE Plasma session.
  - It starts the system service at graphical login and stops it when the graphical session ends.

The installed `polkit` rule allows your user to start, stop, and restart `true-switcher.service` without `sudo`.

## Tray and configuration

 - application config is `~/.config/true-switcher/config.toml`
 - to apply read changes from file the app needs a restart (tray → right click → reload)
 - configuration changes via tray are in-memory, config file is not affected

### config example

These are basic settings; other parts of config are shown in latter sections.

```toml
switch_key = "KEY_PAUSE"     # Required Linux key name; e.g. KEY_PAUSE, KEY_F12, ...
log_level = "info"           # off, error, warn, info, debug, trace; trace logs typed words
manual_switch_sound = false  # Play a sound after a successful manual switch
auto_switch = false          # Enable automatic switching using dictionaries
auto_switch_sound = true     # Play a sound after a successful automatic switch

```
## Automatic switching

There are two distinct ways to auto-switch 
 - regex rules
 - big dictionaries and heuristics

Regex rules are deterministic and have strict priority over dictionary-based switches 


### Regex rules

True-switcher uses Rust `regex` syntax.
An automatic switch is triggered when any pattern matches the last typed word, including the first trailing space.

configuration example

```toml
pattern_switch = false  # Enable automatic switching using regex rules
[[switch_rules]]        # Optional; repeat this block to add more rules
target = "us"           # XKB layout identifier configured in KDE, e.g. us or ru. The layout must be present in KDE 
patterns = [            # Rust regex patterns matched against the converted word.
    '^/[a-z]',          # this rule will switch for example `.ыефке` -> `/start`
    "^cd $",            # rule for bash command `cd`, with trailing space
] 
```

### Dictionary-based switch

True-switcher can trigger automatic switches using word dictionaries and heuristics.
And there are also ways to adjust the behavior. 

- This repo doesn't supply dictionaries; you can find suitable dicts in https://github.com/hermitdave/FrequencyWords
  - Default config will not have configured dictionaries
  - Dictionary file paths should be relative to config location
- Expected dict format: `<word> <frequency>`; for example `apple 12345`
- Dict-based switching is supported only with two layouts and both dictionaries should be supplied 

#### config

```toml
# defines which words will be dropped entirely from dictionaries.
# In this example, pattern drops all words with capitals and words starting with `.`
dict_skip_pattern = '.*\p{Lu}.*'   

# pattern that disables dictionary-based switch. This example protects punctuation. 
# for example `~/` is not in `us`-dict, but `ru`-layout counterpart `Ё.` might be present in `ru`-dictionary.
# this rule protects from auto-switching `~/`->`Ё.`
dict_block_pattern = '[\p{P}\x{0060}]+'   

[[dict]]
target = "us"
file = "dictionaries/en_full.txt"
threshold = 15.0  # autoswitch threshold for `delta score` - see below
# values in the range ~5-20 will work fine. Higher values make switching more conservative

[[dict]]
target = "ru"
file = "dictionaries/ru_full.txt"
threshold = 15.0
```
#### implementation details and meaning of thresholds

- `dict word score` in dictionary - calculated as log(P); P is probability of given word in dictionary, including its frequency
- `word score` - combined score of all words that are prefixed with the given typed word, (log2(ΣP), not Σlog2(P))
- `delta score` = `word score` in alternative layout minus `word score` in given layout. Used for auto-switch decision 

#### manual correction and learned-rules

dictionary-based switch can do undesired switches. To prevent these, there are _learned-rules_ 

- `switch_key` manually corrects the last typed word. Use `Shift` + `switch_key` to also _teach_
  - to undo an unwanted switch
  - to switch word that you want to be switched automatically
- each successful _teach_ adds score to currently typed word (unfinished words saved as-is), a word is also counted if _teach_ happens after trailing spaces.
- score is added to `delta score` (see above) and affects auto-switch decision-making
- learned rules are stored in `~/.config/true-switcher/learned-rules.toml`

```toml
[us]
"```" = 9
"<" = 1
">" = 1
-r = 1
-rf = 1
```

## Switching statistics

Currently, true-switcher writes typing and switching statistics in sqlite. All typing is saved, and passwords too
`~/.local/state/true-switcher/statistics.sqlite3 `

TODO: make it optional
