use std::io;
use std::process::Command;

/// KDE integration stays independent of input devices and word history.
pub struct KdePlasma;

impl KdePlasma {
    /// Let KDE select the next layout in its configured order, including wraparound.
    pub fn switch_to_next_layout(&self) -> io::Result<()> {
        let output = Command::new("qdbus")
            .args([
                "org.kde.keyboard",
                "/Layouts",
                "org.kde.KeyboardLayouts.switchToNextLayout",
            ])
            .output()
            .map_err(|error| io::Error::new(error.kind(), format!("cannot run qdbus: {error}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let message = if stderr.trim().is_empty() {
                stdout.trim()
            } else {
                stderr.trim()
            };
            return Err(io::Error::other(format!(
                "KDE layout switch failed ({}): {message}",
                output.status
            )));
        }

        Ok(())
    }
}
