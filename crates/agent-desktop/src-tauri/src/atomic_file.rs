use std::path::Path;

pub(crate) fn replace(temporary: &Path, target: &Path) -> std::io::Result<()> {
    #[cfg(not(windows))]
    {
        std::fs::rename(temporary, target)
    }
    #[cfg(windows)]
    {
        if !target.exists() {
            return std::fs::rename(temporary, target);
        }
        use std::os::windows::ffi::OsStrExt;
        use std::ptr;
        use windows_sys::Win32::Storage::FileSystem::{REPLACEFILE_WRITE_THROUGH, ReplaceFileW};

        let target = target
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let temporary = temporary
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        for delay_ms in [0, 10, 25, 50] {
            if delay_ms > 0 {
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            }
            let replaced = unsafe {
                ReplaceFileW(
                    target.as_ptr(),
                    temporary.as_ptr(),
                    ptr::null(),
                    REPLACEFILE_WRITE_THROUGH,
                    ptr::null(),
                    ptr::null(),
                )
            };
            if replaced != 0 {
                return Ok(());
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(32) {
                return Err(error);
            }
        }

        overwrite_locked_target(temporary, target)
    }
}

#[cfg(any(windows, test))]
fn overwrite_locked_target(temporary: &Path, target: &Path) -> std::io::Result<()> {
    use std::io::Write;

    let replacement = std::fs::read(temporary)?;
    let original = std::fs::read(target)?;
    let overwrite = |bytes: &[u8]| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(target)?;
        file.write_all(bytes)?;
        file.sync_all()
    };
    if let Err(error) = overwrite(&replacement) {
        let _ = overwrite(&original);
        return Err(error);
    }
    let _ = std::fs::remove_file(temporary);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn locked_target_fallback_overwrites_synced_content_and_removes_temporary_file() {
        let suffix = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "morrow-atomic-file-{}-{suffix}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("create test directory");
        let target = root.join("desktop.json");
        let temporary = root.join(".desktop.json.tmp");
        std::fs::write(&target, b"old state").expect("write target");
        std::fs::write(&temporary, b"new state").expect("write temporary file");

        overwrite_locked_target(&temporary, &target).expect("overwrite target");

        assert_eq!(std::fs::read(&target).expect("read target"), b"new state");
        assert!(!temporary.exists());
        let _ = std::fs::remove_dir_all(root);
    }
}
