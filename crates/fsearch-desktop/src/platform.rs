//! Native file actions performed outside the GUI update loop.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::Arc;

const ACTION_QUEUE_CAPACITY: usize = 8;

/// A user-requested action on an original search-result path.
pub enum Action {
    Open(PathBuf),
    Reveal(PathBuf),
}

/// An action's completion; errors can be displayed without blocking the GUI.
pub struct ActionResult {
    pub error: Option<String>,
}

/// One bounded action queue serviced by a dedicated standard-library thread.
pub struct PlatformWorker {
    pub events: Receiver<ActionResult>,
    commands: SyncSender<Action>,
    active: Arc<AtomicBool>,
}

impl PlatformWorker {
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> io::Result<Self> {
        Self::with_executor(wake, execute_action)
    }

    fn with_executor(
        wake: impl Fn() + Send + Sync + 'static,
        execute: impl Fn(Action) -> Result<(), String> + Send + 'static,
    ) -> io::Result<Self> {
        let (commands, requests) = mpsc::sync_channel(ACTION_QUEUE_CAPACITY);
        let (completed, events) = mpsc::sync_channel(ACTION_QUEUE_CAPACITY);
        let active = Arc::new(AtomicBool::new(true));
        let worker_active = Arc::clone(&active);

        // The handle is deliberately detached: closing the GUI must not wait
        // for a system shell API or external desktop opener to finish.
        std::thread::Builder::new()
            .name("fsearch-file-actions".into())
            .spawn(move || {
                while let Ok(action) = requests.recv() {
                    if !worker_active.load(Ordering::Acquire) {
                        break;
                    }
                    let error = execute(action).err();
                    if !worker_active.load(Ordering::Acquire)
                        || completed.send(ActionResult { error }).is_err()
                    {
                        break;
                    }
                    wake();
                }
            })?;

        Ok(Self {
            events,
            commands,
            active,
        })
    }

    /// Queue an action immediately; a busy queue is a visible, recoverable error.
    pub fn send(&self, action: Action) -> Result<(), String> {
        self.commands.try_send(action).map_err(|error| match error {
            TrySendError::Full(_) => "File action queue is busy; try again shortly".into(),
            TrySendError::Disconnected(_) => "File action worker is unavailable".into(),
        })
    }
}

impl Drop for PlatformWorker {
    fn drop(&mut self) {
        // Dropping the sender/receiver wakes channel waits; queued actions are
        // discarded. An action already executing is allowed to finish.
        self.active.store(false, Ordering::Release);
    }
}

fn execute_action(action: Action) -> Result<(), String> {
    let (path, reveal) = match action {
        Action::Open(path) => (path, false),
        Action::Reveal(path) => (path, true),
    };
    let operation = if reveal { "reveal" } else { "open" };
    let result = absolute_existing_path(&path).and_then(|absolute| {
        if reveal {
            system::reveal(&absolute)
        } else {
            system::open(&absolute)
        }
    });
    result.map_err(|error| format!("Cannot {operation} '{}': {error}", path.display()))
}

fn absolute_existing_path(path: &Path) -> io::Result<PathBuf> {
    if path.as_os_str().is_empty() || path.as_os_str().as_encoded_bytes().contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "The path is empty or contains a NUL character",
        ));
    }
    // Resolve relative/drive-relative paths on this worker; keep native path
    // data and avoid canonicalization, which would alter link identity.
    let absolute = std::path::absolute(path)?;
    // Following metadata here deliberately rejects deleted paths and broken
    // links before requesting an action from the desktop shell.
    std::fs::metadata(&absolute)?;
    Ok(absolute)
}

/// Read an optional installed CJK font during startup, outside frame updates.
/// No font is bundled, downloaded, or copied into the project.
pub fn load_cjk_font() -> Option<Vec<u8>> {
    cjk_font_candidates().into_iter().find_map(|path| {
        std::fs::read(path)
            .ok()
            .filter(|contents| !contents.is_empty())
    })
}

#[cfg(windows)]
fn cjk_font_candidates() -> Vec<PathBuf> {
    let windows_directory = std::env::var_os("WINDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let fonts = windows_directory.join("Fonts");
    ["msyh.ttc", "simhei.ttf", "simsun.ttc"]
        .into_iter()
        .map(|name| fonts.join(name))
        .collect()
}

#[cfg(target_os = "macos")]
fn cjk_font_candidates() -> Vec<PathBuf> {
    [
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/Supplemental/Songti.ttc",
        "/System/Library/Fonts/STHeiti Medium.ttc",
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect()
}

#[cfg(target_os = "linux")]
fn cjk_font_candidates() -> Vec<PathBuf> {
    [
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
        "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect()
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
fn cjk_font_candidates() -> Vec<PathBuf> {
    Vec::new()
}

#[cfg(windows)]
mod system {
    use super::*;
    use std::ffi::c_void;
    use std::marker::PhantomData;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;
    use std::rc::Rc;

    #[link(name = "shell32")]
    unsafe extern "system" {
        #[link_name = "ShellExecuteW"]
        fn shell_execute(
            window: *mut c_void,
            operation: *const u16,
            file: *const u16,
            parameters: *const u16,
            directory: *const u16,
            show_command: i32,
        ) -> *mut c_void;
        #[link_name = "ILCreateFromPathW"]
        fn create_pidl(path: *const u16) -> *mut c_void;
        #[link_name = "ILFree"]
        fn free_pidl(pidl: *mut c_void);
        #[link_name = "SHOpenFolderAndSelectItems"]
        fn open_folder_and_select(
            folder: *const c_void,
            item_count: u32,
            items: *const *const c_void,
            flags: u32,
        ) -> i32;
    }

    #[link(name = "ole32")]
    unsafe extern "system" {
        #[link_name = "CoInitializeEx"]
        fn initialize_com(reserved: *mut c_void, flags: u32) -> i32;
        #[link_name = "CoUninitialize"]
        fn uninitialize_com();
    }

    // COM initialization must be balanced on the same thread, including S_FALSE.
    struct ComApartment(PhantomData<Rc<()>>);

    impl ComApartment {
        fn new() -> io::Result<Self> {
            const COINIT_APARTMENTTHREADED: u32 = 0x2;
            const COINIT_DISABLE_OLE1DDE: u32 = 0x4;
            // SAFETY: the reserved pointer is NULL, flags are valid, and this
            // guard never leaves the current worker thread.
            let result = unsafe {
                initialize_com(
                    ptr::null_mut(),
                    COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE,
                )
            };
            if result < 0 {
                Err(io::Error::other(format!(
                    "COM initialization failed (HRESULT 0x{:08X})",
                    result as u32
                )))
            } else {
                Ok(Self(PhantomData))
            }
        }
    }

    impl Drop for ComApartment {
        fn drop(&mut self) {
            // SAFETY: creation succeeded on this same thread; each successful
            // initialization is balanced by exactly this one call.
            unsafe { uninitialize_com() };
        }
    }

    struct ItemIdList(*mut c_void);

    impl Drop for ItemIdList {
        fn drop(&mut self) {
            // SAFETY: this non-NULL PIDL came from ILCreateFromPathW and is
            // released once after the selection API has finished using it.
            unsafe { free_pidl(self.0) };
        }
    }

    fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        if wide.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "The path contains a NUL character",
            ));
        }
        wide.push(0);
        Ok(wide)
    }

    pub(super) fn open(path: &Path) -> io::Result<()> {
        let _apartment = ComApartment::new()?;
        let wide = wide_path(path)?;
        let verb = [b'o' as u16, b'p' as u16, b'e' as u16, b'n' as u16, 0];
        // SAFETY: both strings are NUL-terminated and alive for this call; all
        // optional pointers are NULL. No path is interpolated into shell text.
        let result = unsafe {
            shell_execute(
                ptr::null_mut(),
                verb.as_ptr(),
                wide.as_ptr(),
                ptr::null(),
                ptr::null(),
                1, // SW_SHOWNORMAL
            )
        } as isize;
        if result > 32 {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "Windows could not open the item (ShellExecuteW code {result})"
            )))
        }
    }

    pub(super) fn reveal(path: &Path) -> io::Result<()> {
        let _apartment = ComApartment::new()?;
        let wide = wide_path(path)?;
        // ILCreateFromPathW documents a MAX_PATH limit including the NUL.
        // Reject explicitly rather than silently truncating a long path.
        if wide.len() > 260 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Windows item selection supports at most 259 UTF-16 path units",
            ));
        }
        // SAFETY: wide is a valid, live NUL-terminated native path string.
        let raw_pidl = unsafe { create_pidl(wide.as_ptr()) };
        if raw_pidl.is_null() {
            return Err(io::Error::other(
                "Windows could not resolve this item for selection",
            ));
        }
        let pidl = ItemIdList(raw_pidl);
        // SAFETY: PIDL ownership remains with the guard throughout this call.
        // Per the API contract, cidl=0 makes the full PIDL a single selected
        // item; its parent folder is opened without any child PIDL array.
        let result = unsafe { open_folder_and_select(pidl.0, 0, ptr::null(), 0) };
        if result < 0 {
            Err(io::Error::other(format!(
                "Windows item selection failed (HRESULT 0x{:08X})",
                result as u32
            )))
        } else {
            Ok(())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;

        #[test]
        fn native_path_is_one_unchanged_wide_argument() -> io::Result<()> {
            let path = Path::new(r"C:\资料\comma, space [draft] $report.txt");
            let wide = wide_path(path)?;
            assert_eq!(wide.last(), Some(&0));
            let round_trip = OsString::from_wide(&wide[..wide.len() - 1]);
            assert_eq!(round_trip.as_os_str(), path.as_os_str());
            Ok(())
        }

        #[test]
        fn wide_paths_reject_interior_nul() {
            assert_eq!(
                wide_path(Path::new("a\0b")).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod system {
    use super::*;
    use std::process::{Command, Stdio};

    fn command_for(path: &Path, reveal: bool) -> Command {
        #[cfg(target_os = "linux")]
        {
            let mut command = Command::new("xdg-open");
            // Linux has no universal item-selection API. Reveal opens the
            // parent directory; a root directory is its own fallback.
            let target = if reveal {
                path.parent().unwrap_or(path)
            } else {
                path
            };
            command.arg(target);
            command
        }
        #[cfg(target_os = "macos")]
        {
            let mut command = Command::new("/usr/bin/open");
            if reveal {
                command.arg("-R");
            }
            command.arg(path);
            command
        }
    }

    fn run_command(mut command: Command) -> io::Result<()> {
        // Waiting occurs only on the action worker. Arguments remain separate
        // native strings; shell metacharacters have no special interpretation.
        let status = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "Desktop file opener failed with {status}"
            )))
        }
    }

    pub(super) fn open(path: &Path) -> io::Result<()> {
        run_command(command_for(path, false))
    }

    pub(super) fn reveal(path: &Path) -> io::Result<()> {
        run_command(command_for(path, true))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::ffi::OsStr;

        #[test]
        fn open_preserves_metacharacters_in_one_argument() {
            let path = Path::new("/tmp/资料 comma, space $(test) [draft].txt");
            let command = command_for(path, false);
            assert_eq!(command.get_args().collect::<Vec<_>>(), [path.as_os_str()]);
            #[cfg(target_os = "linux")]
            assert_eq!(command.get_program(), OsStr::new("xdg-open"));
            #[cfg(target_os = "macos")]
            assert_eq!(command.get_program(), OsStr::new("/usr/bin/open"));
        }

        #[test]
        fn reveal_uses_platform_arguments_without_a_shell() {
            let path = Path::new("/tmp/资料 comma, space $(test).txt");
            let command = command_for(path, true);
            #[cfg(target_os = "linux")]
            assert_eq!(command.get_args().collect::<Vec<_>>(), [OsStr::new("/tmp")]);
            #[cfg(target_os = "macos")]
            assert_eq!(
                command.get_args().collect::<Vec<_>>(),
                [OsStr::new("-R"), path.as_os_str()]
            );
        }
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod system {
    use super::*;

    pub(super) fn open(_path: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "File actions are not implemented for this operating system",
        ))
    }

    pub(super) fn reveal(path: &Path) -> io::Result<()> {
        open(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn missing_path() -> io::Result<PathBuf> {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos();
        Ok(std::env::temp_dir().join(format!(
            "fsearch-action-missing-{}-{unique}",
            std::process::id()
        )))
    }

    #[test]
    fn relative_path_is_resolved_on_the_worker() -> io::Result<()> {
        let resolved = absolute_existing_path(Path::new("."))?;
        assert!(resolved.is_absolute());
        assert_eq!(
            resolved.canonicalize()?,
            std::env::current_dir()?.canonicalize()?
        );
        Ok(())
    }

    #[test]
    fn missing_and_invalid_paths_return_errors_without_launching() -> io::Result<()> {
        assert_eq!(
            absolute_existing_path(&missing_path()?).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        for path in [Path::new(""), Path::new("a\0b")] {
            assert_eq!(
                absolute_existing_path(path).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
        Ok(())
    }

    #[test]
    fn missing_path_action_reports_completion_and_wakes_gui() -> Result<(), Box<dyn Error>> {
        let (wake_sender, wakes) = mpsc::channel();
        let worker = PlatformWorker::new(move || {
            let _ = wake_sender.send(());
        })?;
        let path = missing_path()?;
        worker.send(Action::Open(path.clone()))?;
        let result = worker.events.recv_timeout(Duration::from_secs(5))?;
        let error = result.error.ok_or("expected a missing-path error")?;
        assert!(error.contains("Cannot open"));
        assert!(error.contains(path.file_name().unwrap().to_str().unwrap()));
        wakes.recv_timeout(Duration::from_secs(5))?;
        Ok(())
    }

    #[test]
    fn bounded_queue_does_not_wait_for_an_executing_action() -> Result<(), Box<dyn Error>> {
        let (entered_sender, entered) = mpsc::channel();
        let (release_sender, release) = mpsc::channel();
        let (finished_sender, finished) = mpsc::channel();
        let worker = PlatformWorker::with_executor(
            || {},
            move |_| {
                let _ = entered_sender.send(());
                release
                    .recv()
                    .map_err(|error| format!("test gate disconnected: {error}"))?;
                let _ = finished_sender.send(());
                Ok(())
            },
        )?;
        worker.send(Action::Reveal(PathBuf::from("first")))?;
        entered.recv_timeout(Duration::from_secs(5))?;
        for _ in 0..ACTION_QUEUE_CAPACITY {
            worker.send(Action::Open(PathBuf::from("queued")))?;
        }
        assert!(worker
            .send(Action::Open(PathBuf::from("overflow")))
            .is_err());
        // This returns while the executor is still blocked. Shutdown does not
        // execute the queued requests, and it never joins the worker.
        drop(worker);
        release_sender.send(())?;
        finished.recv_timeout(Duration::from_secs(5))?;
        Ok(())
    }
}
