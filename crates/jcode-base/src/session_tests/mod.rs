use super::*;
use std::ffi::OsString;
use std::path::PathBuf;

fn lock_env() -> std::sync::MutexGuard<'static, ()> {
    crate::storage::lock_test_env()
}

struct CwdGuard {
    previous: PathBuf,
}

impl CwdGuard {
    fn change_to(path: impl AsRef<std::path::Path>) -> anyhow::Result<Self> {
        let previous = std::env::current_dir()?;
        std::env::set_current_dir(path)?;
        Ok(Self { previous })
    }
}

fn canonical_temp_path(path: &std::path::Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.previous);
    }
}

struct EnvVarGuard {
    key: &'static str,
    prev: Option<OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let prev = std::env::var_os(key);
        crate::env::set_var(key, value);
        Self { key, prev }
    }

    fn remove(key: &'static str) -> Self {
        let prev = std::env::var_os(key);
        crate::env::remove_var(key);
        Self { key, prev }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        if let Some(prev) = &self.prev {
            crate::env::set_var(self.key, prev);
        } else {
            crate::env::remove_var(self.key);
        }
    }
}

#[path = "cases.rs"]
mod cases;
