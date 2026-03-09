use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;

pub(super) struct FileLockGuard {
    _in_process: tokio::sync::OwnedMutexGuard<()>,
    _file: std::fs::File,
}

#[cfg(unix)]
fn lock_file_exclusive_blocking(path: &Path) -> Result<std::fs::File, io::Error> {
    use std::os::unix::io::AsRawFd;

    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(file)
}

#[cfg(all(not(unix), not(windows)))]
fn lock_file_exclusive_blocking(path: &Path) -> Result<std::fs::File, io::Error> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;

    Ok(file)
}

#[cfg(windows)]
fn lock_file_exclusive_blocking(path: &Path) -> Result<std::fs::File, io::Error> {
    use std::os::windows::fs::OpenOptionsExt;

    const ERROR_SHARING_VIOLATION: i32 = 32;
    const ERROR_LOCK_VIOLATION: i32 = 33;

    loop {
        match OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .share_mode(0)
            .open(path)
        {
            Ok(file) => return Ok(file),
            Err(err)
                if matches!(
                    err.raw_os_error(),
                    Some(ERROR_SHARING_VIOLATION) | Some(ERROR_LOCK_VIOLATION)
                ) =>
            {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(err) => return Err(err),
        }
    }
}

pub(super) async fn lock_file_exclusive(path: &Path) -> Result<FileLockGuard, io::Error> {
    static IN_PROCESS_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> =
        OnceLock::new();

    let mutex = {
        let locks = IN_PROCESS_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
        let mut locks = locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        locks
            .entry(path.to_path_buf())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    };
    let in_process = mutex.lock_owned().await;

    let path = path.to_path_buf();
    let file = tokio::task::spawn_blocking(move || lock_file_exclusive_blocking(&path))
        .await
        .map_err(|err| io::Error::other(err.to_string()))??;

    Ok(FileLockGuard {
        _in_process: in_process,
        _file: file,
    })
}
