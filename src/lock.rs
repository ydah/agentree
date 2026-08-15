use std::{
    fs::{File, OpenOptions},
    path::Path,
};

use crate::domain::{AppError, ErrorKind};

pub struct FileLock {
    file: File,
    path: std::path::PathBuf,
}

impl FileLock {
    pub fn acquire(path: &Path) -> Result<Self, AppError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(path)?;
        #[cfg(unix)]
        {
            let result = unsafe {
                libc::flock(
                    std::os::fd::AsRawFd::as_raw_fd(&file),
                    libc::LOCK_EX | libc::LOCK_NB,
                )
            };
            if result != 0 {
                return Err(AppError::diagnostic(
                    "AGT-0601",
                    format!("lock is busy: {}", path.display()),
                    ErrorKind::LockConflict,
                ));
            }
        }
        Ok(Self {
            file,
            path: path.to_path_buf(),
        })
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::flock(std::os::fd::AsRawFd::as_raw_fd(&self.file), libc::LOCK_UN);
        }
        let _ = &self.path;
    }
}
