//! Full executable paths, read once per process.

use std::collections::HashMap;

use windows::{
    Win32::{
        Foundation::CloseHandle,
        System::Threading::{
            OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
            QueryFullProcessImageNameW,
        },
    },
    core::PWSTR,
};

/// The longest path Windows supports, in UTF-16 units.
const MAX_LONG_PATH: usize = 32_768;

/// A process as the cache keys it: PID plus start time, because Windows reuses PIDs.
type ProcessKey = (u32, u64);

/// Exe paths of the running processes. Each process is looked up once, a failed lookup
/// included, so protected processes cost one `OpenProcess` instead of one per sample.
#[derive(Debug, Default)]
pub(crate) struct ExeCache {
    paths: HashMap<ProcessKey, Option<String>>,
}

impl ExeCache {
    /// Makes the cache hold exactly `running`, calling `lookup` only for processes it has not
    /// seen before.
    pub(crate) fn update<I, F>(&mut self, running: I, mut lookup: F)
    where
        I: IntoIterator<Item = ProcessKey>,
        F: FnMut(u32) -> Option<String>,
    {
        let mut previous = std::mem::take(&mut self.paths);
        self.paths = running
            .into_iter()
            .map(|key| {
                let path = previous.remove(&key).unwrap_or_else(|| lookup(key.0));
                (key, path)
            })
            .collect();
    }

    /// The exe path of the process, when it is cached and could be read.
    pub(crate) fn get(&self, pid: u32, start_time: u64) -> Option<&str> {
        self.paths.get(&(pid, start_time))?.as_deref()
    }
}

/// Reads the full Win32 path of `pid`'s executable.
///
/// Limited query access is enough for `QueryFullProcessImageNameW` and is granted for
/// elevated processes too, unlike the access `GetModuleFileNameExW` needs. Protected
/// processes and the System process still refuse it and return `None`.
pub(crate) fn image_path(pid: u32) -> Option<String> {
    // SAFETY: plain FFI call; the returned handle is closed below.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut buffer = vec![0u16; MAX_LONG_PATH];
    let mut size = MAX_LONG_PATH as u32;
    // SAFETY: `buffer` holds `size` UTF-16 units and outlives the call, and `size` is a valid
    // in/out pointer; on success it holds the path length without the NUL.
    let read = unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut size,
        )
    };
    // SAFETY: `handle` came from `OpenProcess` and is closed exactly once.
    let _ = unsafe { CloseHandle(handle) };
    read.ok()?;
    let path = String::from_utf16_lossy(&buffer[..size as usize]);
    (!path.is_empty()).then_some(path)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    fn fake_lookup(calls: &RefCell<Vec<u32>>) -> impl FnMut(u32) -> Option<String> + '_ {
        move |pid| {
            calls.borrow_mut().push(pid);
            (pid != 4).then(|| format!(r"C:\bin\{pid}.exe"))
        }
    }

    #[test]
    fn test_exe_cache_looks_up_each_process_once() {
        let calls = RefCell::new(Vec::new());
        let mut cache = ExeCache::default();
        cache.update([(10, 1), (11, 2)], fake_lookup(&calls));
        cache.update([(10, 1), (11, 2), (12, 3)], fake_lookup(&calls));
        assert_eq!(*calls.borrow(), [10, 11, 12]);
        assert_eq!(cache.get(12, 3), Some(r"C:\bin\12.exe"));
    }

    #[test]
    fn test_exe_cache_failed_lookup_is_not_retried() {
        let calls = RefCell::new(Vec::new());
        let mut cache = ExeCache::default();
        cache.update([(4, 0)], fake_lookup(&calls));
        cache.update([(4, 0)], fake_lookup(&calls));
        assert_eq!(*calls.borrow(), [4]);
        assert_eq!(cache.get(4, 0), None);
    }

    #[test]
    fn test_exe_cache_drops_exited_processes_and_rereads_reused_pids() {
        let calls = RefCell::new(Vec::new());
        let mut cache = ExeCache::default();
        cache.update([(10, 1), (11, 2)], fake_lookup(&calls));
        cache.update([(10, 5)], fake_lookup(&calls));
        assert_eq!(*calls.borrow(), [10, 11, 10]);
        assert_eq!(cache.get(11, 2), None);
        assert_eq!(cache.get(10, 1), None);
        assert_eq!(cache.get(10, 5), Some(r"C:\bin\10.exe"));
    }

    #[test]
    fn test_image_path_reads_own_executable() {
        let own = std::env::current_exe().unwrap();
        let path = image_path(std::process::id()).expect("own exe path");
        assert!(
            path.eq_ignore_ascii_case(&own.display().to_string()),
            "{path} is not {own:?}"
        );
    }

    #[test]
    fn test_image_path_of_idle_process_is_none() {
        assert_eq!(image_path(0), None);
    }
}
