use std::{
    env,
    error::Error,
    fs::{self, File, OpenOptions},
    os::fd::AsRawFd,
    path::PathBuf,
};

/// Process-wide singleton lock for the assistant daemon.
///
/// The lock is advisory and Linux-specific by design because the assistant
/// runtime already uses Unix sockets and XDG_RUNTIME_DIR on its supported
/// development environment. The file itself may remain after a crash; the
/// kernel releases the flock automatically when the owning process exits.
pub struct RuntimeInstanceLock {
    file: File,
}

impl RuntimeInstanceLock {
    pub fn acquire() -> Result<Self, Box<dyn Error>> {
        Self::acquire_at(runtime_lock_path()?)
    }

    fn acquire_at(path: PathBuf) -> Result<Self, Box<dyn Error>> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&path)?;

        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EWOULDBLOCK)
                || error.raw_os_error() == Some(libc::EAGAIN)
            {
                return Err(format!(
                    "Another assistant runtime daemon is already running (lock: {}).",
                    path.display()
                )
                .into());
            }
            return Err(format!(
                "Could not acquire assistant runtime lock {}: {error}",
                path.display()
            )
            .into());
        }

        Ok(Self { file })
    }
}

fn runtime_lock_path() -> Result<PathBuf, Box<dyn Error>> {
    if let Some(value) = env::var_os("ASSISTANT_RUNTIME_LOCK_PATH") {
        return Ok(PathBuf::from(value));
    }

    if let Some(runtime_dir) = env::var_os("XDG_RUNTIME_DIR") {
        return Ok(PathBuf::from(runtime_dir).join("assistant-runtime.lock"));
    }

    let home = env::var_os("HOME").ok_or("HOME environment variable is not set.")?;
    Ok(PathBuf::from(home).join(".cache/assistant/assistant-runtime.lock"))
}

impl Drop for RuntimeInstanceLock {
    fn drop(&mut self) {
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn second_lock_in_same_process_is_rejected() -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let lock_path = directory.path().join("runtime.lock");

        let first = RuntimeInstanceLock::acquire_at(lock_path.clone())?;
        let second = RuntimeInstanceLock::acquire_at(lock_path.clone());

        assert!(second.is_err());
        assert!(fs::metadata(lock_path).is_ok());

        drop(first);
        Ok(())
    }

    #[test]
    fn lock_can_be_reacquired_after_release() -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let lock_path = directory.path().join("runtime.lock");

        let first = RuntimeInstanceLock::acquire_at(lock_path.clone())?;
        drop(first);
        let second = RuntimeInstanceLock::acquire_at(lock_path)?;
        drop(second);
        Ok(())
    }
}
