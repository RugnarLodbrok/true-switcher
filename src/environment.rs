use std::env;
use std::ffi::OsStr;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

const QDBUS_NAMES: [&str; 3] = ["qdbus-qt6", "qdbus6", "qdbus"];
const EXECUTABLE_BITS: u32 = 0o111;

/// KDE integration stays independent of input devices and word history.
pub struct KdePlasma {
    qdbus: PathBuf,
}

impl KdePlasma {
    pub fn new() -> io::Result<Self> {
        let path = env::var_os("PATH").unwrap_or_default();
        let qdbus = find_qdbus(&path)?;
        eprintln!("qdbus executable: {}", qdbus.display());
        Ok(Self { qdbus })
    }

    /// Let KDE select the next layout in its configured order, including wraparound.
    pub fn switch_to_next_layout(&self) -> io::Result<()> {
        self.call_layout_method("org.kde.KeyboardLayouts.switchToNextLayout", &[])
    }

    pub fn switch_to_layout(&self, name: &str) -> io::Result<()> {
        let index = crate::layout::layout_index(name)?.to_string();
        self.call_layout_method("org.kde.KeyboardLayouts.setLayout", &[&index])
    }

    fn call_layout_method(&self, method: &str, arguments: &[&str]) -> io::Result<()> {
        let output = Command::new(&self.qdbus)
            .args(["org.kde.keyboard", "/Layouts", method])
            .args(arguments)
            .output()
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("cannot run {}: {error}", self.qdbus.display()),
                )
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let message = if stderr.trim().is_empty() {
                stdout.trim()
            } else {
                stderr.trim()
            };
            return Err(io::Error::other(format!(
                "KDE layout switch via {} failed ({}): {message}",
                self.qdbus.display(),
                output.status
            )));
        }

        if String::from_utf8_lossy(&output.stdout).trim() == "false" {
            return Err(io::Error::other("KDE rejected the layout switch"));
        }

        Ok(())
    }
}

fn find_qdbus(path: &OsStr) -> io::Result<PathBuf> {
    for name in QDBUS_NAMES {
        for directory in env::split_paths(path) {
            let candidate = directory.join(name);
            if let Ok(metadata) = candidate.metadata()
                && metadata.is_file()
                && metadata.permissions().mode() & EXECUTABLE_BITS != 0
            {
                return std::path::absolute(candidate);
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "no qdbus executable found; tried {} in PATH={}",
            QDBUS_NAMES.join(", "),
            path.to_string_lossy()
        ),
    ))
}

#[cfg(test)]
mod tests;
