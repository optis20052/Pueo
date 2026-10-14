//! Preserve the existing XDG storage layout on Linux and macOS.
use std::path::PathBuf;

#[cfg(test)]
thread_local! { static TEST_BASE: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) }; }
#[cfg(test)]
pub fn set_test_base(path: PathBuf) {
    TEST_BASE.with(|base| *base.borrow_mut() = Some(path));
}
fn base(variable: &str, fallback: &str) -> PathBuf {
    #[cfg(test)]
    if let Some(path) = TEST_BASE.with(|base| base.borrow().clone()) {
        return path.join(variable);
    }

    std::env::var_os(variable)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            dirs::home_dir()
                .expect("home directory unavailable")
                .join(fallback)
        })
}
pub fn config_base() -> PathBuf {
    base("XDG_CONFIG_HOME", ".config")
}
pub fn data_base() -> PathBuf {
    base("XDG_DATA_HOME", ".local/share")
}

/// Private storage for askpass scripts and temporary SSH keys.
pub fn runtime_dir() -> anyhow::Result<PathBuf> {
    use std::sync::OnceLock;
    static DIRECTORY: OnceLock<tempfile::TempDir> = OnceLock::new();
    if let Some(dir) = DIRECTORY.get() {
        return Ok(dir.path().into());
    }
    let dir = tempfile::Builder::new().prefix("pueo-ssh-").tempdir()?;
    let _ = DIRECTORY.set(dir);
    Ok(DIRECTORY
        .get()
        .expect("runtime directory initialized")
        .path()
        .into())
}
