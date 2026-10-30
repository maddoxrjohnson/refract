//! The Windows operations of a migration: reading NTFS for the snapshot, the Recycle
//! Bin, the `.lnk` at the old location and VS Code's windows. Elsewhere each one
//! fails, so the app still builds for tests on other systems.
//!
//! The Recycle Bin is reached through `IFileOperation` with
//! `FOFX_RECYCLEONDELETE | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI`, so no
//! dialog ever shows. Without a dialog, Windows would delete a folder too large for
//! the bin for good, so two guards stand before that: [`recycle_fit`] checks the
//! drive's bin first (see [`super::flow::fits`]), and a progress sink refuses any
//! delete that Windows does not flag `TSF_DELETE_RECYCLE_IF_POSSIBLE`, which cancels
//! the operation before it starts.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use super::flow::{Fit, Removal, WindowsOps};
use super::vscode::VsCodeWindow;
use super::walk::{RawEntry, ReadError, SourceFs};

/// The parts of a path the shell and NTFS calls need: `\\?\`-prefixed for long paths.
pub fn verbatim(path: &str) -> String {
    if path.starts_with(r"\\?\") {
        path.to_owned()
    } else if let Some(share) = path.strip_prefix(r"\\") {
        format!(r"\\?\UNC\{share}")
    } else {
        format!(r"\\?\{path}")
    }
}

/// The source folder on NTFS, read natively.
pub struct NtfsFs {
    /// The folder's verbatim path.
    root: String,
}

impl NtfsFs {
    /// `windows_path` is the folder's Windows path, such as `C:\dev\aurora-notes`.
    pub fn new(windows_path: &str) -> NtfsFs {
        NtfsFs {
            root: verbatim(windows_path.trim_end_matches('\\')),
        }
    }

    fn path(&self, rel: &str) -> PathBuf {
        if rel.is_empty() {
            PathBuf::from(&self.root)
        } else {
            PathBuf::from(format!("{}\\{}", self.root, rel.replace('/', "\\")))
        }
    }
}

/// Bytes read at a time while hashing.
const CHUNK: usize = 1 << 20;

impl SourceFs for NtfsFs {
    fn list(&self, rel: &str) -> io::Result<Vec<RawEntry>> {
        imp::list(&self.path(rel))
    }

    fn read_link(&self, rel: &str) -> io::Result<String> {
        std::fs::read_link(self.path(rel)).map(|t| t.to_string_lossy().into_owned())
    }

    fn streams(&self, rel: &str) -> io::Result<Vec<(String, u64)>> {
        imp::streams(&self.path(rel))
    }

    fn hash(
        &self,
        rel: &str,
        cancel: &AtomicBool,
        read: &dyn Fn(u64),
    ) -> Result<String, ReadError> {
        use sha2::{Digest, Sha256};
        use std::io::Read;
        use std::sync::atomic::Ordering;

        let unreadable = |err: io::Error| ReadError::Unreadable(err.to_string());
        let mut file = std::fs::File::open(self.path(rel)).map_err(unreadable)?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; CHUNK];
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(ReadError::Cancelled);
            }
            let n = match file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(unreadable(err)),
            };
            hasher.update(&buf[..n]);
            read(n as u64);
        }
        Ok(hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect())
    }

    fn probe(&self, rel: &str) -> Result<(), ReadError> {
        use std::io::Read;
        let unreadable = |err: io::Error| ReadError::Unreadable(err.to_string());
        let mut file = std::fs::File::open(self.path(rel)).map_err(unreadable)?;
        let mut buf = [0u8; 4096];
        file.read(&mut buf).map(|_| ()).map_err(unreadable)
    }

    fn read_text(&self, rel: &str, max: usize) -> io::Result<String> {
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(self.path(rel))?
            .take(max as u64)
            .read_to_end(&mut bytes)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
}

/// [`WindowsOps`] on this machine.
pub struct Os;

impl WindowsOps for Os {
    fn vscode_windows(&self, folder: &str) -> Vec<VsCodeWindow> {
        vscode_windows(folder)
    }

    fn recycle_fit(&self, path: &Path, files: u64, bytes: u64) -> Fit {
        imp::recycle_fit(path, files, bytes)
    }

    fn remove(&self, path: &Path, recycle: bool) -> Result<Removal, String> {
        on_com_thread(|| imp::remove(path, recycle))
    }

    fn exists(&self, path: &Path) -> bool {
        match std::fs::symlink_metadata(path) {
            Ok(_) => true,
            Err(err) => err.kind() != io::ErrorKind::NotFound,
        }
    }

    fn create_shortcut(&self, lnk: &Path, target: &str) -> Result<(), String> {
        on_com_thread(|| imp::create_shortcut(lnk, target))
    }
}

/// Runs `f` on a new thread with COM set up for it (single-threaded apartment, as the
/// shell wants), so the caller's thread state never matters.
fn on_com_thread<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let _com = imp::Com::init();
                f()
            })
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    })
}

/// The visible VS Code windows whose title names `folder` (see `vscode`).
pub fn vscode_windows(folder: &str) -> Vec<VsCodeWindow> {
    imp::top_windows()
        .into_iter()
        .filter(|(_, title, exe)| {
            super::vscode::is_vscode_exe(exe) && super::vscode::title_matches(title, folder)
        })
        .map(|(id, title, _)| VsCodeWindow { id, title })
        .collect()
}

/// Asks the window `id` to close, the way its close button does, so VS Code still
/// asks about unsaved files. Only a window that still has `folder` open is asked;
/// `false` when it is gone.
pub fn close_window(id: u64, folder: &str) -> Result<bool, String> {
    if !vscode_windows(folder).iter().any(|w| w.id == id) {
        return Ok(false);
    }
    imp::post_close(id).map(|()| true)
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::io;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use windows::core::{implement, Interface, Ref, BOOL, HRESULT, HSTRING, PCWSTR, PWSTR};
    use windows::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, E_ABORT, HWND, LPARAM, WPARAM};
    use windows::Win32::Storage::FileSystem::{
        FindClose, FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW, FindFirstStreamW,
        FindNextFileW, FindNextStreamW, FindStreamInfoStandard, GetDiskFreeSpaceExW, GetDriveTypeW,
        GetVolumeNameForVolumeMountPointW, FIND_FIRST_EX_LARGE_FETCH, WIN32_FIND_DATAW,
        WIN32_FIND_STREAM_DATA,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, IPersistFile, CLSCTX_ALL,
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
    };
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::Shell::{
        FileOperation, IFileOperation, IFileOperationProgressSink, IFileOperationProgressSink_Impl,
        IShellItem, IShellLinkW, SHCreateItemFromParsingName, SHQueryRecycleBinW, ShellLink,
        FILEOPERATION_FLAGS, FOFX_EARLYFAILURE, FOFX_RECYCLEONDELETE, FOF_NOCONFIRMATION,
        FOF_NOERRORUI, FOF_SILENT, SHQUERYRBINFO, TSF_DELETE_RECYCLE_IF_POSSIBLE,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
        IsWindowVisible, PostMessageW, WM_CLOSE,
    };

    use super::super::flow::{fits, recycle_bytes, Fit, Removal};
    use super::RawEntry;

    const ERROR_FILE_NOT_FOUND: i32 = 2;
    const ERROR_NO_MORE_FILES: i32 = 18;
    const ERROR_HANDLE_EOF: i32 = 38;
    const DRIVE_FIXED: u32 = 3;
    /// 100 ns intervals from 1601 to 1970.
    const FILETIME_UNIX_EPOCH: i64 = 116_444_736_000_000_000;

    /// The Win32 error code inside an `HRESULT_FROM_WIN32` error.
    fn win32_code(err: &windows::core::Error) -> i32 {
        err.code().0 & 0xFFFF
    }

    fn io_error(err: windows::core::Error) -> io::Error {
        io::Error::from_raw_os_error(win32_code(&err))
    }

    fn until_nul(chars: &[u16]) -> &[u16] {
        let end = chars.iter().position(|c| *c == 0).unwrap_or(chars.len());
        &chars[..end]
    }

    pub fn list(dir: &Path) -> io::Result<Vec<RawEntry>> {
        let pattern = HSTRING::from(dir.join("*").as_os_str());
        let mut data = WIN32_FIND_DATAW::default();
        // SAFETY: `data` outlives the call and is the struct FindExInfoBasic fills.
        let found = unsafe {
            FindFirstFileExW(
                &pattern,
                FindExInfoBasic,
                &mut data as *mut _ as *mut c_void,
                FindExSearchNameMatch,
                None,
                FIND_FIRST_EX_LARGE_FETCH,
            )
        };
        let handle = match found {
            Ok(handle) => handle,
            Err(err) if win32_code(&err) == ERROR_FILE_NOT_FOUND => return Ok(Vec::new()),
            Err(err) => return Err(io_error(err)),
        };
        let mut out = Vec::new();
        let result = loop {
            let name = until_nul(&data.cFileName);
            let dot = name == [b'.' as u16] || name == [b'.' as u16, b'.' as u16];
            if !dot {
                let write = (u64::from(data.ftLastWriteTime.dwHighDateTime) << 32)
                    | u64::from(data.ftLastWriteTime.dwLowDateTime);
                out.push(RawEntry {
                    name: name.to_vec(),
                    attributes: data.dwFileAttributes,
                    size: (u64::from(data.nFileSizeHigh) << 32) | u64::from(data.nFileSizeLow),
                    mtime_ns: (write as i64 - FILETIME_UNIX_EPOCH).saturating_mul(100),
                    reparse_tag: data.dwReserved0,
                });
            }
            // SAFETY: `handle` is a live find handle and `data` is its struct.
            match unsafe { FindNextFileW(handle, &mut data) } {
                Ok(()) => {}
                Err(err) if win32_code(&err) == ERROR_NO_MORE_FILES => break Ok(out),
                Err(err) => break Err(io_error(err)),
            }
        };
        // SAFETY: closed once, after the last use.
        let _ = unsafe { FindClose(handle) };
        result
    }

    pub fn streams(path: &Path) -> io::Result<Vec<(String, u64)>> {
        let mut data = WIN32_FIND_STREAM_DATA::default();
        // SAFETY: `data` outlives the call and is the struct FindStreamInfoStandard fills.
        let found = unsafe {
            FindFirstStreamW(
                &HSTRING::from(path.as_os_str()),
                FindStreamInfoStandard,
                &mut data as *mut _ as *mut c_void,
                None,
            )
        };
        let handle = match found {
            Ok(handle) => handle,
            Err(err) if win32_code(&err) == ERROR_HANDLE_EOF => return Ok(Vec::new()),
            Err(err) => return Err(io_error(err)),
        };
        let mut out = Vec::new();
        let result = loop {
            let name = String::from_utf16_lossy(until_nul(&data.cStreamName));
            // `::$DATA` is the file's own content; named ones are `:<name>:$DATA`.
            if let Some(stream) = name
                .strip_prefix(':')
                .and_then(|n| n.strip_suffix(":$DATA"))
                .filter(|n| !n.is_empty())
            {
                out.push((stream.to_owned(), data.StreamSize.max(0) as u64));
            }
            // SAFETY: `handle` is a live stream handle and `data` is its struct.
            match unsafe { FindNextStreamW(handle, &mut data as *mut _ as *mut c_void) } {
                Ok(()) => {}
                Err(err) if win32_code(&err) == ERROR_HANDLE_EOF => break Ok(out),
                Err(err) => break Err(io_error(err)),
            }
        };
        // SAFETY: closed once, after the last use.
        let _ = unsafe { FindClose(handle) };
        result
    }

    /// COM for this thread, undone on drop when this call set it up.
    pub struct Com(bool);

    impl Com {
        pub fn init() -> Com {
            // SAFETY: paired with CoUninitialize in drop when it succeeded.
            let hr =
                unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
            Com(hr.is_ok())
        }
    }

    impl Drop for Com {
        fn drop(&mut self) {
            if self.0 {
                // SAFETY: matches the successful CoInitializeEx on this thread.
                unsafe { CoUninitialize() };
            }
        }
    }

    /// The drive's root, `C:\`, of a drive path.
    fn drive_root(path: &Path) -> Option<String> {
        let text = path.to_str()?;
        let text = text.strip_prefix(r"\\?\").unwrap_or(text);
        let bytes = text.as_bytes();
        (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
            .then(|| format!("{}:\\", &text[..1]))
    }

    fn reg_dword(key: &str, value: &str) -> Option<u32> {
        let mut data = 0u32;
        let mut size = std::mem::size_of::<u32>() as u32;
        // SAFETY: `data` and `size` describe a DWORD buffer that outlives the call.
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                &HSTRING::from(key),
                &HSTRING::from(value),
                RRF_RT_REG_DWORD,
                None,
                Some(&mut data as *mut u32 as *mut c_void),
                Some(&mut size),
            )
        };
        (status == ERROR_SUCCESS).then_some(data)
    }

    /// `{guid}` of the volume mounted at `root`.
    fn volume_guid(root: &str) -> Option<String> {
        let mut name = [0u16; 64];
        // SAFETY: `name` is large enough for `\\?\Volume{GUID}\` and its NUL.
        unsafe { GetVolumeNameForVolumeMountPointW(&HSTRING::from(root), &mut name) }.ok()?;
        let name = String::from_utf16_lossy(until_nul(&name));
        let start = name.find('{')?;
        let end = name.find('}')?;
        Some(name[start..=end].to_owned())
    }

    pub fn recycle_fit(path: &Path, files: u64, bytes: u64) -> Fit {
        let Some(root) = drive_root(path) else {
            return Fit::WontFit("The folder isn't on a drive with a Recycle Bin.".into());
        };
        let root_w = HSTRING::from(root.as_str());
        // SAFETY: plain calls with a NUL-terminated root path.
        if unsafe { GetDriveTypeW(&root_w) } != DRIVE_FIXED {
            return Fit::WontFit("This drive has no Recycle Bin.".into());
        }
        let mut info = SHQUERYRBINFO {
            cbSize: std::mem::size_of::<SHQUERYRBINFO>() as u32,
            ..SHQUERYRBINFO::default()
        };
        // SAFETY: `info` is sized and outlives the call.
        if unsafe { SHQueryRecycleBinW(&root_w, &mut info) }.is_err() {
            return Fit::WontFit("Windows can't reach this drive's Recycle Bin.".into());
        }
        let key = volume_guid(&root).map(|guid| {
            format!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\BitBucket\Volume\{guid}")
        });
        let nuke = key
            .as_deref()
            .and_then(|key| reg_dword(key, "NukeOnDelete"))
            == Some(1);
        let max = key
            .as_deref()
            .and_then(|key| reg_dword(key, "MaxCapacity"))
            .map(|mb| u64::from(mb) * 1024 * 1024);
        let mut total = 0u64;
        // SAFETY: `total` outlives the call.
        let volume = unsafe { GetDiskFreeSpaceExW(&root_w, None, Some(&mut total), None) }
            .ok()
            .map(|()| total);
        fits(recycle_bytes(files, bytes), nuke, max, volume)
    }

    /// Refuses a delete that Windows would not send to the Recycle Bin.
    #[implement(IFileOperationProgressSink)]
    struct RecycleGuard {
        refused: Arc<AtomicBool>,
    }

    #[allow(non_snake_case)]
    impl IFileOperationProgressSink_Impl for RecycleGuard_Impl {
        fn StartOperations(&self) -> windows::core::Result<()> {
            Ok(())
        }
        fn FinishOperations(&self, _: HRESULT) -> windows::core::Result<()> {
            Ok(())
        }
        fn PreRenameItem(
            &self,
            _: u32,
            _: Ref<IShellItem>,
            _: &PCWSTR,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn PostRenameItem(
            &self,
            _: u32,
            _: Ref<IShellItem>,
            _: &PCWSTR,
            _: HRESULT,
            _: Ref<IShellItem>,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn PreMoveItem(
            &self,
            _: u32,
            _: Ref<IShellItem>,
            _: Ref<IShellItem>,
            _: &PCWSTR,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn PostMoveItem(
            &self,
            _: u32,
            _: Ref<IShellItem>,
            _: Ref<IShellItem>,
            _: &PCWSTR,
            _: HRESULT,
            _: Ref<IShellItem>,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn PreCopyItem(
            &self,
            _: u32,
            _: Ref<IShellItem>,
            _: Ref<IShellItem>,
            _: &PCWSTR,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn PostCopyItem(
            &self,
            _: u32,
            _: Ref<IShellItem>,
            _: Ref<IShellItem>,
            _: &PCWSTR,
            _: HRESULT,
            _: Ref<IShellItem>,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn PreDeleteItem(&self, flags: u32, _: Ref<IShellItem>) -> windows::core::Result<()> {
            if flags & TSF_DELETE_RECYCLE_IF_POSSIBLE.0 as u32 == 0 {
                self.refused.store(true, Ordering::SeqCst);
                return Err(E_ABORT.into());
            }
            Ok(())
        }
        fn PostDeleteItem(
            &self,
            _: u32,
            _: Ref<IShellItem>,
            _: HRESULT,
            _: Ref<IShellItem>,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn PreNewItem(&self, _: u32, _: Ref<IShellItem>, _: &PCWSTR) -> windows::core::Result<()> {
            Ok(())
        }
        fn PostNewItem(
            &self,
            _: u32,
            _: Ref<IShellItem>,
            _: &PCWSTR,
            _: &PCWSTR,
            _: u32,
            _: HRESULT,
            _: Ref<IShellItem>,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn UpdateProgress(&self, _: u32, _: u32) -> windows::core::Result<()> {
            Ok(())
        }
        fn ResetTimer(&self) -> windows::core::Result<()> {
            Ok(())
        }
        fn PauseTimer(&self) -> windows::core::Result<()> {
            Ok(())
        }
        fn ResumeTimer(&self) -> windows::core::Result<()> {
            Ok(())
        }
    }

    /// Deletes `path` with `IFileOperation`: to the Recycle Bin when `recycle`, else for
    /// good. Runs on a thread with COM set up.
    pub fn remove(path: &Path, recycle: bool) -> Result<Removal, String> {
        let why = |what: &str, err: windows::core::Error| format!("{what}: {}", err.message());
        let mut flags = FOF_NOCONFIRMATION.0 | FOF_SILENT.0 | FOF_NOERRORUI.0 | FOFX_EARLYFAILURE.0;
        if recycle {
            flags |= FOFX_RECYCLEONDELETE.0;
        }
        let refused = Arc::new(AtomicBool::new(false));
        // SAFETY: COM is set up on this thread; every interface is used while alive.
        unsafe {
            let op: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_ALL)
                .map_err(|e| why("Cannot start the delete", e))?;
            op.SetOperationFlags(FILEOPERATION_FLAGS(flags))
                .map_err(|e| why("Cannot set up the delete", e))?;
            let cookie = if recycle {
                let sink: IFileOperationProgressSink = RecycleGuard {
                    refused: refused.clone(),
                }
                .into();
                Some(
                    op.Advise(&sink)
                        .map_err(|e| why("Cannot watch the delete", e))?,
                )
            } else {
                None
            };
            let item: IShellItem =
                SHCreateItemFromParsingName(&HSTRING::from(path.as_os_str()), None)
                    .map_err(|e| why("Cannot find the folder", e))?;
            op.DeleteItem(&item, None)
                .map_err(|e| why("Cannot set up the delete", e))?;
            let done = op.PerformOperations();
            if let Some(cookie) = cookie {
                let _ = op.Unadvise(cookie);
            }
            if refused.load(Ordering::SeqCst) {
                return Ok(Removal::WontFit(
                    "Windows would delete it for good instead of moving it to the Recycle Bin."
                        .into(),
                ));
            }
            done.map_err(|e| why("The delete failed", e))?;
            if op
                .GetAnyOperationsAborted()
                .map(|b| b.as_bool())
                .unwrap_or(false)
            {
                return Err("The delete was stopped before it finished.".into());
            }
        }
        Ok(Removal::Done)
    }

    /// Writes a `.lnk` at `lnk` that opens `target`. Runs on a thread with COM set up.
    pub fn create_shortcut(lnk: &Path, target: &str) -> Result<(), String> {
        let why =
            |err: windows::core::Error| format!("Cannot create the shortcut: {}", err.message());
        // SAFETY: COM is set up on this thread; every interface is used while alive.
        unsafe {
            let link: IShellLinkW =
                CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).map_err(why)?;
            link.SetPath(&HSTRING::from(target)).map_err(why)?;
            link.SetDescription(&HSTRING::from("Moved to WSL by Refract"))
                .map_err(why)?;
            let file: IPersistFile = link.cast().map_err(why)?;
            file.Save(&HSTRING::from(lnk.as_os_str()), true)
                .map_err(why)?;
        }
        Ok(())
    }

    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: `lparam` is the `Vec` that `top_windows` passes, alive for the call.
        let out = unsafe { &mut *(lparam.0 as *mut Vec<(HWND, String)>) };
        // SAFETY: `hwnd` comes from EnumWindows; the buffer is sized for the title.
        unsafe {
            if IsWindowVisible(hwnd).as_bool() {
                let len = GetWindowTextLengthW(hwnd);
                if len > 0 {
                    let mut buf = vec![0u16; len as usize + 1];
                    let n = GetWindowTextW(hwnd, &mut buf);
                    out.push((hwnd, String::from_utf16_lossy(&buf[..n.max(0) as usize])));
                }
            }
        }
        BOOL::from(true)
    }

    /// The file name of process `pid`'s executable.
    fn exe_name(pid: u32) -> Option<String> {
        // SAFETY: the handle is closed below; the buffer and its size match.
        unsafe {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
            let mut buf = [0u16; 1024];
            let mut size = buf.len() as u32;
            let named = QueryFullProcessImageNameW(
                process,
                PROCESS_NAME_WIN32,
                PWSTR(buf.as_mut_ptr()),
                &mut size,
            );
            let _ = CloseHandle(process);
            named.ok()?;
            let path = String::from_utf16_lossy(&buf[..size as usize]);
            path.rsplit('\\').next().map(str::to_owned)
        }
    }

    /// Visible top-level windows with a title: handle, title and executable name.
    pub fn top_windows() -> Vec<(u64, String, String)> {
        let mut found: Vec<(HWND, String)> = Vec::new();
        // SAFETY: `found` outlives EnumWindows, which calls `collect` synchronously.
        let _ = unsafe { EnumWindows(Some(collect), LPARAM(&mut found as *mut _ as isize)) };
        let mut names = std::collections::HashMap::new();
        found
            .into_iter()
            .map(|(hwnd, title)| {
                let mut pid = 0u32;
                // SAFETY: `pid` outlives the call.
                unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
                let exe = names
                    .entry(pid)
                    .or_insert_with(|| exe_name(pid).unwrap_or_default())
                    .clone();
                (hwnd.0 as usize as u64, title, exe)
            })
            .collect()
    }

    pub fn post_close(id: u64) -> Result<(), String> {
        let hwnd = HWND(id as usize as *mut c_void);
        // SAFETY: posting a message to a window handle is safe even if it went away.
        unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) }
            .map_err(|err| format!("Cannot close the window: {}", err.message()))
    }
}

#[cfg(not(windows))]
mod imp {
    use std::io;
    use std::path::Path;

    use super::super::flow::{Fit, Removal};
    use super::RawEntry;

    fn unsupported() -> io::Error {
        io::Error::new(io::ErrorKind::Unsupported, "only supported on Windows")
    }

    pub fn list(_: &Path) -> io::Result<Vec<RawEntry>> {
        Err(unsupported())
    }

    pub fn streams(_: &Path) -> io::Result<Vec<(String, u64)>> {
        Ok(Vec::new())
    }

    pub struct Com;

    impl Com {
        pub fn init() -> Com {
            Com
        }
    }

    pub fn recycle_fit(_: &Path, _: u64, _: u64) -> Fit {
        Fit::WontFit("Only supported on Windows.".into())
    }

    pub fn remove(_: &Path, _: bool) -> Result<Removal, String> {
        Err("Only supported on Windows.".into())
    }

    pub fn create_shortcut(_: &Path, _: &str) -> Result<(), String> {
        Err("Only supported on Windows.".into())
    }

    pub fn top_windows() -> Vec<(u64, String, String)> {
        Vec::new()
    }

    pub fn post_close(_: u64) -> Result<(), String> {
        Err("Only supported on Windows.".into())
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::migrate::walk::{read_files, walk, ReadMode};
    use std::collections::BTreeSet;

    /// A fresh folder under `%TEMP%` with an invented name, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> TempDir {
            let dir = std::env::temp_dir().join(format!(
                "refract-test-{name}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn verbatim_paths_reach_long_names() {
        assert_eq!(verbatim(r"C:\dev\x"), r"\\?\C:\dev\x");
        assert_eq!(verbatim(r"\\?\C:\dev"), r"\\?\C:\dev");
        assert_eq!(verbatim(r"\\server\share\x"), r"\\?\UNC\server\share\x");
    }

    #[test]
    fn reads_ntfs_with_streams_and_hashes() {
        let dir = TempDir::new("walk");
        std::fs::create_dir_all(dir.0.join("src")).unwrap();
        std::fs::write(dir.0.join("src").join("main.rs"), b"fn main() {}\n").unwrap();
        std::fs::write(dir.0.join("setup.exe"), b"MZ").unwrap();
        std::fs::write(
            format!("{}:Zone.Identifier", dir.0.join("setup.exe").display()),
            b"[ZoneTransfer]\r\nZoneId=3\r\n",
        )
        .unwrap();
        let fs = NtfsFs::new(&dir.0.to_string_lossy());
        let cancel = AtomicBool::new(false);
        let mut walked = walk(&fs, &BTreeSet::new(), &cancel).unwrap();
        let paths: Vec<&str> = walked
            .manifest
            .entries
            .iter()
            .map(|e| e.path.as_str())
            .collect();
        assert_eq!(paths, ["setup.exe", "src", "src/main.rs"]);
        let unreadable = read_files(
            &fs,
            &mut walked.manifest,
            ReadMode::Hash,
            2,
            &cancel,
            &|_| {},
        )
        .unwrap();
        assert!(unreadable.is_empty());
        let main = &walked.manifest.entries[2];
        assert_eq!(main.size, 13);
        assert!(main.mtime_ns > 1_600_000_000_000_000_000);
        use sha2::{Digest, Sha256};
        let want: String = Sha256::digest(b"fn main() {}\n")
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(main.sha256.as_deref(), Some(want.as_str()));
        assert_eq!(walked.manifest.streams.len(), 1);
        assert_eq!(walked.manifest.streams[0].stream, "Zone.Identifier");
        assert_eq!(walked.manifest.streams[0].size, 26);
    }

    #[test]
    fn a_file_opened_without_sharing_is_unreadable() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = TempDir::new("locked");
        let path = dir.0.join("data.db");
        std::fs::write(&path, b"locked").unwrap();
        let _held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&path)
            .unwrap();
        let fs = NtfsFs::new(&dir.0.to_string_lossy());
        assert!(matches!(fs.probe("data.db"), Err(ReadError::Unreadable(_))));
    }

    #[test]
    fn recycles_a_folder_and_leaves_a_shortcut() {
        let dir = TempDir::new("recycle");
        let victim = dir.0.join("reef-notes");
        std::fs::create_dir_all(victim.join("sub")).unwrap();
        std::fs::write(victim.join("sub").join("a.txt"), b"a").unwrap();
        let os = Os;
        assert_eq!(os.recycle_fit(&victim, 1, 1), Fit::Fits);
        assert_eq!(os.remove(&victim, true), Ok(Removal::Done));
        assert!(!os.exists(&victim));
        let lnk = dir.0.join("reef-notes.lnk");
        os.create_shortcut(&lnk, r"\\wsl.localhost\Ubuntu\home\dev\projects\reef-notes")
            .unwrap();
        assert!(os.exists(&lnk));
        // The recycled folder stays in the bin; the test leaves only that behind.
    }

    #[test]
    fn deletes_a_folder_for_good() {
        let dir = TempDir::new("nuke");
        let victim = dir.0.join("reef-scratch");
        std::fs::create_dir_all(&victim).unwrap();
        std::fs::write(victim.join("b.txt"), b"b").unwrap();
        assert_eq!(Os.remove(&victim, false), Ok(Removal::Done));
        assert!(!Os.exists(&victim));
    }

    #[test]
    fn junctions_are_links_and_leave_the_repo() {
        let dir = TempDir::new("junction");
        let target = dir.0.join("elsewhere");
        let repo = dir.0.join("repo");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::create_dir_all(&repo).unwrap();
        let made = std::process::Command::new("cmd.exe")
            .args(["/c", "mklink", "/J"])
            .arg(repo.join("shared"))
            .arg(&target)
            .output()
            .unwrap();
        assert!(made.status.success(), "{made:?}");
        let fs = NtfsFs::new(&repo.to_string_lossy());
        let walked = walk(&fs, &BTreeSet::new(), &AtomicBool::new(false)).unwrap();
        assert_eq!(walked.external_links, ["shared"]);
        assert!(walked.manifest.entries.is_empty());
    }

    #[test]
    fn snapshots_a_real_repo_with_git_exe() {
        use crate::migrate::git::{find_git_exe, GitExe};
        use crate::migrate::snapshot::take_snapshot;
        use refract_core::migrate::manifest::EntryKind;
        use std::sync::Arc;

        let program_files = std::env::var_os("ProgramFiles").map(PathBuf::from);
        let path_var = std::env::var("PATH").ok();
        let Some(exe) = find_git_exe(path_var.as_deref(), program_files.as_deref(), Path::is_file)
        else {
            eprintln!("git.exe not found; skipping");
            return;
        };
        let dir = TempDir::new("repo");
        let git = |args: &[&str]| {
            let out = std::process::Command::new(&exe)
                .args(["-c", "user.name=Dev", "-c", "user.email=dev@example.com"])
                .args(args)
                .current_dir(&dir.0)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "core.autocrlf", "true"]);
        std::fs::write(dir.0.join(".gitignore"), "node_modules/\n.env\n").unwrap();
        std::fs::write(dir.0.join("README.md"), "# Reef notes\r\nline two\r\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "First"]);
        std::fs::write(dir.0.join("README.md"), "# Reef notes\r\nedited\r\n").unwrap();
        std::fs::write(dir.0.join(".env"), "TOKEN=x").unwrap();
        std::fs::write(dir.0.join("todo.txt"), "untracked").unwrap();
        std::fs::create_dir_all(dir.0.join("node_modules").join("left-pad")).unwrap();
        std::fs::write(
            dir.0.join("node_modules").join("left-pad").join("index.js"),
            "x",
        )
        .unwrap();

        let fs = NtfsFs::new(&dir.0.to_string_lossy());
        let cancel = Arc::new(AtomicBool::new(false));
        let runner = GitExe {
            exe: exe.clone(),
            root: dir.0.clone(),
            cancel: cancel.clone(),
        };
        let snapshot = take_snapshot(&fs, &runner, 2, &cancel, &|_| {}).unwrap();
        let entry = |path: &str| {
            snapshot
                .manifest
                .entries
                .iter()
                .find(|e| e.path.as_str() == path)
                .unwrap_or_else(|| panic!("{path} missing"))
                .clone()
        };
        let skipped: Vec<&str> = snapshot
            .manifest
            .skipped
            .iter()
            .map(|s| s.path.as_str())
            .collect();
        assert_eq!(skipped, ["node_modules"]);
        // The clean blob of the edited file: LF, as git would store it.
        let readme = entry("README.md");
        assert_eq!(readme.kind, EntryKind::File);
        let blob = readme.blob.expect("tracked files get a blob");
        let out = std::process::Command::new(&exe)
            .args(["hash-object", "--stdin"])
            .current_dir(&dir.0)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write;
                child
                    .stdin
                    .take()
                    .unwrap()
                    .write_all(b"# Reef notes\nedited\n")?;
                child.wait_with_output()
            })
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), blob);
        assert!(entry(".env").blob.is_none() && entry(".env").sha256.is_some());
        assert!(entry("todo.txt").blob.is_none());
        assert!(entry(".git/HEAD").sha256.is_some());
        let g0 = &snapshot.git[0].snapshot;
        assert!(g0.head.is_some());
        assert!(g0.refs.iter().any(|r| r.name == "refs/heads/main"));
        assert!(g0.config.iter().any(|c| c.key == "core.autocrlf"));
        assert!(g0.fsck.is_empty(), "{:?}", g0.fsck);
        assert!(!snapshot.linux_git);
    }
}
