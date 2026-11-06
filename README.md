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
sudo dnf install libxkbcommon-devel
```

On Bazzite:

```sh
rpm-ostree install libxkbcommon-devel
```

Reboot after installing the package on Bazzite to activate the new deployment.
The development package is only needed to build from source; users of prebuilt binaries need the runtime library and XKB layout data.

## Installation

### Prebuilt

### From sources

```sh
cargo fetch
make build
make install
```

The installer checks for `qdbus` using the system service's `PATH`, which may differ from your shell's `PATH`, and prints the selected executable. The program resolves the executable once at startup using its own `PATH` and logs the selected path.

## Switching statistics

Each successful layout switch is recorded in SQLite, even if replaying the word is later cancelled or fails.
The database is stored at `$XDG_STATE_HOME/true-switcher/statistics.sqlite3`, or `~/.local/state/true-switcher/statistics.sqlite3` when `XDG_STATE_HOME` is unset or empty.

The `switches` table contains `id`, `timestamp` (Unix seconds), `prev` (the previous word as typed), `a` (the corrected word before switching), and `b` (the word in the new layout).
Missing words are stored as empty strings. Records are skipped when the layout or text cannot be determined. Database errors are logged and do not stop the switcher.

## Tests

```sh
cargo test --bin true-switcher
```

Installer command discovery tests use Python's standard library and temporary files:

```sh
python3 -m unittest discover -s installer -p 'test_*.py'
```

The D-Bus tests use a separate `dbus-daemon` and do not change the desktop layout.
An additional test can read the layout from a running KDE session:

```sh
cargo test --bin true-switcher reads_running_kde_layout -- --ignored
```
