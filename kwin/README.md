# KWin focus tracking

The switcher provides `org.true_switcher.Focus.FocusChanged` on the session
D-Bus. KWin must load the script in `true-switcher-focus` for active-window
changes to reach the switcher.

Copy `kwin/true-switcher-focus` to
`~/.local/share/kwin/scripts/true-switcher-focus`, then enable **True Switcher
Focus Tracking** in System Settings → Window Management → KWin Scripts. The
switcher must be running before the script reports focus changes.
