//! Recursive directory change notifications used to trigger index reconciliation.

use std::path::PathBuf;

#[cfg(windows)]
mod platform {
    use std::{
        collections::{HashMap, HashSet},
        os::windows::ffi::OsStrExt,
        path::{Path, PathBuf},
    };

    use windows::Win32::{
        Foundation::{WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT},
        Storage::FileSystem::{
            FILE_NOTIFY_CHANGE, FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME,
            FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE, FindCloseChangeNotification,
            FindFirstChangeNotificationW, FindNextChangeNotification,
        },
        System::Threading::WaitForSingleObject,
    };

    #[derive(Default)]
    pub(super) struct FolderChanges {
        handles: HashMap<PathBuf, windows::Win32::Foundation::HANDLE>,
        failed: HashSet<PathBuf>,
        configured: HashSet<PathBuf>,
    }

    impl FolderChanges {
        pub(super) fn sync_roots(&mut self, roots: &[PathBuf]) -> (bool, bool) {
            let desired: HashSet<_> = roots.iter().cloned().collect();
            let changed = self.configured != desired;
            self.configured.clone_from(&desired);
            self.handles.retain(|path, handle| {
                if desired.contains(path) {
                    true
                } else {
                    unsafe {
                        let _ = FindCloseChangeNotification(*handle);
                    }
                    false
                }
            });
            self.failed.retain(|path| desired.contains(path));

            for root in roots {
                if self.handles.contains_key(root) || self.failed.contains(root) {
                    continue;
                }
                match create_handle(root) {
                    Ok(handle) => {
                        self.handles.insert(root.clone(), handle);
                    }
                    Err(()) => {
                        self.failed.insert(root.clone());
                    }
                }
            }
            (
                changed,
                self.failed.is_empty() && self.handles.len() == desired.len(),
            )
        }

        pub(super) fn poll(&mut self) -> (bool, bool) {
            let mut changed = false;
            let mut unavailable = false;
            let paths: Vec<_> = self.handles.keys().cloned().collect();
            for path in paths {
                let Some(handle) = self.handles.get(&path).copied() else {
                    continue;
                };
                let wait = unsafe { WaitForSingleObject(handle, 0) };
                if wait == WAIT_TIMEOUT {
                    continue;
                }
                if wait == WAIT_OBJECT_0 {
                    changed = true;
                    if unsafe { FindNextChangeNotification(handle) }.is_ok() {
                        continue;
                    }
                } else if wait != WAIT_FAILED {
                    continue;
                }
                if let Some(handle) = self.handles.remove(&path) {
                    unsafe {
                        let _ = FindCloseChangeNotification(handle);
                    }
                }
                self.failed.insert(path);
                unavailable = true;
            }
            (changed, unavailable)
        }

        pub(super) fn retry_failed(&mut self) {
            self.failed.clear();
        }

        pub(super) fn clear(&mut self) {
            for (_, handle) in self.handles.drain() {
                unsafe {
                    let _ = FindCloseChangeNotification(handle);
                }
            }
            self.failed.clear();
            self.configured.clear();
        }
    }

    impl Drop for FolderChanges {
        fn drop(&mut self) {
            for (_, handle) in self.handles.drain() {
                unsafe {
                    let _ = FindCloseChangeNotification(handle);
                }
            }
        }
    }

    fn create_handle(path: &Path) -> Result<windows::Win32::Foundation::HANDLE, ()> {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let filter: FILE_NOTIFY_CHANGE = FILE_NOTIFY_CHANGE_FILE_NAME
            | FILE_NOTIFY_CHANGE_DIR_NAME
            | FILE_NOTIFY_CHANGE_SIZE
            | FILE_NOTIFY_CHANGE_LAST_WRITE;
        unsafe { FindFirstChangeNotificationW(windows::core::PCWSTR(wide.as_ptr()), true, filter) }
            .map_err(|_| ())
    }

    pub(super) use self::FolderChanges as PublicFolderChanges;
}

#[cfg(not(windows))]
mod platform {
    use std::path::PathBuf;

    #[derive(Default)]
    pub(super) struct PublicFolderChanges {
        configured: std::collections::HashSet<PathBuf>,
    }

    impl PublicFolderChanges {
        pub(super) fn sync_roots(&mut self, roots: &[PathBuf]) -> (bool, bool) {
            let desired: std::collections::HashSet<_> = roots.iter().cloned().collect();
            let changed = self.configured != desired;
            self.configured = desired;
            (changed, false)
        }
        pub(super) fn poll(&mut self) -> (bool, bool) {
            (false, true)
        }
        pub(super) fn retry_failed(&mut self) {}
        pub(super) fn clear(&mut self) {
            self.configured.clear();
        }
    }
}

pub struct FolderChanges(platform::PublicFolderChanges);

impl Default for FolderChanges {
    fn default() -> Self {
        Self(platform::PublicFolderChanges::default())
    }
}

impl FolderChanges {
    /// Synchronize recursive watches, returning whether the root set changed
    /// and whether every configured root currently has a watch.
    pub fn sync_roots(&mut self, roots: &[PathBuf]) -> (bool, bool) {
        self.0.sync_roots(roots)
    }

    /// Poll all notification handles without blocking.
    pub fn poll(&mut self) -> (bool, bool) {
        self.0.poll()
    }

    /// Retry roots whose notification handle could not be registered.
    pub fn retry_failed(&mut self) {
        self.0.retry_failed();
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::FolderChanges;
    use std::{
        thread,
        time::{Duration, Instant},
    };

    #[test]
    fn recursive_notification_detects_nested_file_changes() {
        let temp = tempfile::tempdir().unwrap();
        let nested = temp.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        let mut watcher = FolderChanges::default();
        let (changed, available) = watcher.sync_roots(&[temp.path().to_path_buf()]);
        assert!(changed);
        assert!(available);

        std::fs::write(nested.join("created.txt"), "event").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let (changed, unavailable) = watcher.poll();
            assert!(!unavailable);
            if changed {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "directory notification timed out"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn retrying_an_unavailable_watch_does_not_report_root_configuration_change() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing");
        let mut watcher = FolderChanges::default();
        let (changed, available) = watcher.sync_roots(&[missing.clone()]);
        assert!(changed);
        assert!(!available);

        watcher.retry_failed();
        let (changed, available) = watcher.sync_roots(&[missing]);
        assert!(!changed);
        assert!(!available);
    }
}
