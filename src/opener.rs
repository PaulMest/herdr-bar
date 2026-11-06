//! Open the bar's popup through the Herdr CLI: what the `open` action runs.
//!
//! Herdr starts manifest commands without a shell, and Windows has no `sh`,
//! so the CLI call lives in the binary and one action works on every platform.

pub const POPUP_ARGS: &[&str] =
    &["plugin", "pane", "open", "--plugin", "paulmest.herdr-bar", "--entrypoint", "bar", "--placement", "popup"];

/// The Herdr binary to shell out to.
///
/// HERDR_BIN_PATH can outlive the binary it names -- upgrading Herdr under a
/// running server leaves the old path dangling -- so an unusable value falls
/// back to whatever `herdr` resolves to on PATH.
pub fn herdr_bin() -> String {
    if let Some(candidate) = std::env::var_os("HERDR_BIN_PATH") {
        if !candidate.is_empty() && is_executable_file(std::path::Path::new(&candidate)) {
            return candidate.to_string_lossy().into_owned();
        }
    }
    "herdr".into()
}

#[cfg(unix)]
pub fn is_executable_file(path: &std::path::Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(meta) = std::fs::metadata(path) else { return false };
    if !meta.is_file() {
        return false;
    }
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else { return false };
    unsafe { libc::access(c_path.as_ptr(), libc::X_OK) == 0 }
}

#[cfg(windows)]
pub fn is_executable_file(path: &std::path::Path) -> bool {
    // os.access(X_OK) on Windows only checks that the file exists.
    std::fs::metadata(path).map(|m| m.is_file()).unwrap_or(false)
}

pub fn popup_command() -> Vec<String> {
    let mut command = vec![herdr_bin()];
    command.extend(POPUP_ARGS.iter().map(|s| s.to_string()));
    command
}

pub fn open_bar() -> i32 {
    let command = popup_command();
    let mut process = std::process::Command::new(&command[0]);
    process.args(&command[1..]);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Replace this process, so nothing lingers behind the popup.
        let error = process.exec();
        eprintln!("herdr-bar: could not run herdr: {}", crate::host::os_error_str(&error));
        2
    }
    #[cfg(not(unix))]
    {
        // No exec on Windows: wait, so the CLI's exit code reaches Herdr.
        match process.status() {
            Ok(status) => status.code().unwrap_or(1),
            Err(error) => {
                eprintln!("herdr-bar: could not run herdr: {}", crate::host::os_error_str(&error));
                2
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{env_lock, EnvVars, TempDir};

    #[test]
    fn open_hands_the_popup_to_the_herdr_cli() {
        // What `--open` execs (POSIX) or runs (Windows); the exec itself would
        // replace the test process.
        let _env = env_lock();
        let temp = TempDir::new();
        let herdr = temp.path().join("herdr");
        std::fs::write(&herdr, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&herdr, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut vars = EnvVars::new();
        vars.set("HERDR_BIN_PATH", &herdr);
        let bin = herdr.to_string_lossy().into_owned();
        let expected: Vec<String> = [bin.as_str(), "plugin", "pane", "open", "--plugin", "paulmest.herdr-bar", "--entrypoint", "bar", "--placement", "popup"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(popup_command(), expected);
    }

    #[test]
    fn stale_bin_path_falls_back_to_path() {
        let _env = env_lock();
        let mut vars = EnvVars::new();
        vars.set("HERDR_BIN_PATH", "/nonexistent/herdr");
        assert_eq!(herdr_bin(), "herdr");
    }
}
