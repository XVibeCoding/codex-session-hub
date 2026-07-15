use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessIdentity {
    pub pid: u32,
    pub name: String,
    pub path: Option<String>,
    pub started_at: Option<String>,
    pub parent_pid: Option<u32>,
    pub session_id: Option<u32>,
    pub verified: bool,
    pub is_current: bool,
    pub is_ancestor: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockingProcess {
    pub identity: ProcessIdentity,
    pub application_type: String,
    pub application_root_pid: Option<u32>,
    pub service_name: Option<String>,
    pub restartable: bool,
    pub close_allowed: bool,
    pub close_reason: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloseProcessResult {
    pub pid: u32,
    pub mode: String,
    pub requested: bool,
    pub exited: bool,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationLockStatus {
    pub state: String,
    pub path: String,
    pub owner_pid: Option<u32>,
    pub owner_started_at: Option<String>,
    pub command: Option<String>,
    pub age_seconds: Option<u64>,
}

#[cfg(windows)]
mod imp {
    use super::*;
    use serde::{Deserialize, Serialize};
    use sha2::{Digest, Sha256};
    use std::{
        collections::{HashMap, HashSet},
        ffi::OsStr,
        fs::{self, File},
        io::{Read, Seek, SeekFrom, Write},
        mem::size_of,
        os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::FromRawHandle},
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };
    use windows::{
        core::{BOOL, HRESULT, PCWSTR, PWSTR},
        Win32::{
            Foundation::{
                CloseHandle, ERROR_ACCESS_DENIED, ERROR_LOCK_VIOLATION, ERROR_MORE_DATA,
                ERROR_SUCCESS, FILETIME, HANDLE, HWND, LPARAM, WAIT_OBJECT_0, WPARAM,
            },
            Storage::FileSystem::{
                CreateFileW, LockFileEx, MoveFileExW, UnlockFileEx, FILE_ATTRIBUTE_NORMAL,
                FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ, FILE_SHARE_WRITE,
                LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, MOVEFILE_REPLACE_EXISTING,
                MOVEFILE_WRITE_THROUGH, OPEN_ALWAYS,
            },
            System::{
                Diagnostics::ToolHelp::{
                    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
                    TH32CS_SNAPPROCESS,
                },
                RemoteDesktop::ProcessIdToSessionId,
                RestartManager::{
                    RmConsole, RmCritical, RmEndSession, RmExplorer, RmGetList, RmMainWindow,
                    RmOtherWindow, RmRegisterResources, RmService, RmShutdown, RmStartSession,
                    RM_APP_TYPE, RM_PROCESS_INFO, RM_UNIQUE_PROCESS,
                },
                Threading::{
                    GetCurrentProcessId, GetProcessTimes, OpenProcess, QueryFullProcessImageNameW,
                    TerminateProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
                    PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
                },
                IO::OVERLAPPED,
            },
            UI::WindowsAndMessaging::{
                EnumWindows, GetWindow, GetWindowThreadProcessId, IsWindowVisible,
                SendMessageTimeoutW, GW_OWNER, SMTO_ABORTIFHUNG, SMTO_BLOCK, WM_CLOSE,
            },
        },
    };

    const LOCK_OFFSET_HIGH: u32 = 1;
    const PROCESS_CLOSE_TIMEOUT_MS: u32 = 8_000;
    const PROCESS_FORCE_TIMEOUT_MS: u32 = 5_000;
    const WINDOW_CLOSE_TIMEOUT_MS: u32 = 2_000;
    const DATABASE_RELEASE_TIMEOUT_MS: u32 = 2_000;
    const RESTART_MANAGER_GRACEFUL_FLAGS: u32 = 0;
    const RESTART_MANAGER_FORCE_FLAGS: u32 = 0x1;
    const ERROR_FAIL_NOACTION_REBOOT_CODE: u32 = 350;

    fn explorer_compatible_path(path: &Path) -> PathBuf {
        let value = path.to_string_lossy();
        if let Some(rest) = value.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = value.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
        path.to_path_buf()
    }

    pub fn open_exclusive_file(path: &Path) -> Result<File, String> {
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(path)
            .map_err(|error| {
                format!(
                    "cannot acquire exclusive file handle ({}): {error}",
                    path.display()
                )
            })
    }

    pub fn open_provider_config_guard(path: &Path) -> Result<File, String> {
        fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ.0)
            .open(path)
            .map_err(|error| {
                format!(
                    "failed to lock {} against provider changes: {error}",
                    path.display()
                )
            })
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct LockMetadata {
        pid: u32,
        started_at: Option<u64>,
        command: String,
        created_at: u64,
    }

    struct OwnedHandle(HANDLE);

    impl OwnedHandle {
        fn raw(&self) -> HANDLE {
            self.0
        }
    }

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if !self.0.is_invalid() {
                unsafe {
                    let _ = CloseHandle(self.0);
                }
            }
        }
    }

    struct RestartManagerSession(u32);

    impl RestartManagerSession {
        fn start() -> Result<Self, String> {
            let mut handle = 0_u32;
            let mut key = [0_u16; 33];
            let result = unsafe { RmStartSession(&mut handle, Some(0), PWSTR(key.as_mut_ptr())) };
            if result != ERROR_SUCCESS {
                return Err(format!("RmStartSession failed: {}", result.0));
            }
            Ok(Self(handle))
        }
    }

    impl Drop for RestartManagerSession {
        fn drop(&mut self) {
            unsafe {
                let _ = RmEndSession(self.0);
            }
        }
    }

    pub struct OperationLock {
        file: Option<File>,
        path: PathBuf,
        overlapped: OVERLAPPED,
    }

    impl OperationLock {
        pub fn path(&self) -> &Path {
            &self.path
        }

        pub fn release(mut self) -> Result<(), String> {
            self.release_inner()
        }

        fn release_inner(&mut self) -> Result<(), String> {
            let Some(mut file) = self.file.take() else {
                return Ok(());
            };
            let clear_result = (|| {
                file.set_len(0).map_err(|error| error.to_string())?;
                file.seek(SeekFrom::Start(0))
                    .map_err(|error| error.to_string())?;
                file.sync_all().map_err(|error| error.to_string())
            })();
            let unlock_result = unsafe {
                UnlockFileEx(
                    HANDLE(file.as_raw_handle()),
                    None,
                    1,
                    0,
                    &mut self.overlapped,
                )
            }
            .map_err(|error| error.to_string());
            drop(file);
            clear_result.and(unlock_result)
        }
    }

    impl Drop for OperationLock {
        fn drop(&mut self) {
            let _ = self.release_inner();
        }
    }

    use std::os::windows::io::AsRawHandle;

    fn wide(value: &OsStr) -> Vec<u16> {
        value.encode_wide().chain(Some(0)).collect()
    }

    fn wide_string(value: &[u16]) -> String {
        let length = value
            .iter()
            .position(|item| *item == 0)
            .unwrap_or(value.len());
        String::from_utf16_lossy(&value[..length])
    }

    fn filetime_value(value: FILETIME) -> u64 {
        ((value.dwHighDateTime as u64) << 32) | value.dwLowDateTime as u64
    }

    fn lock_overlapped() -> OVERLAPPED {
        let mut overlapped = OVERLAPPED::default();
        overlapped.Anonymous.Anonymous.OffsetHigh = LOCK_OFFSET_HIGH;
        overlapped
    }

    fn open_lock_file(path: &Path) -> Result<File, String> {
        let path_wide = wide(path.as_os_str());
        let handle = unsafe {
            CreateFileW(
                PCWSTR(path_wide.as_ptr()),
                FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_ALWAYS,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        }
        .map_err(|error| format!("cannot open operation lock: {error}"))?;
        Ok(unsafe { File::from_raw_handle(handle.0 as _) })
    }

    fn try_lock_file(file: &File, overlapped: &mut OVERLAPPED) -> Result<(), windows::core::Error> {
        unsafe {
            LockFileEx(
                HANDLE(file.as_raw_handle()),
                LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                None,
                1,
                0,
                overlapped,
            )
        }
    }

    fn lock_violation(error: &windows::core::Error) -> bool {
        error.code() == HRESULT::from_win32(ERROR_LOCK_VIOLATION.0)
    }

    fn now_unix() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    fn home_hash(home: &Path) -> String {
        let canonical = fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
        let normalized = canonical.to_string_lossy().to_ascii_lowercase();
        let digest = Sha256::digest(normalized.as_bytes());
        digest[..16]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    pub fn operation_lock_path(home: &Path) -> Result<PathBuf, String> {
        let local_app_data = std::env::var_os("LOCALAPPDATA")
            .ok_or_else(|| "LOCALAPPDATA is not available".to_string())?;
        Ok(PathBuf::from(local_app_data)
            .join("Codex Provider Hub")
            .join("locks")
            .join(format!("{}.lck", home_hash(home))))
    }

    pub fn atomic_replace_file(source: &Path, target: &Path) -> Result<(), String> {
        let source = wide(source.as_os_str());
        let target = wide(target.as_os_str());
        unsafe {
            MoveFileExW(
                PCWSTR(source.as_ptr()),
                PCWSTR(target.as_ptr()),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        }
        .map_err(|error| format!("atomic file replacement failed: {error}"))
    }

    fn current_started_at() -> Option<u64> {
        query_process(unsafe { GetCurrentProcessId() }, None)
            .ok()
            .map(|(_, started)| started)
    }

    pub fn acquire_operation_lock(home: &Path, command: &str) -> Result<OperationLock, String> {
        let path = operation_lock_path(home)?;
        let parent = path
            .parent()
            .ok_or_else(|| "operation lock path has no parent".to_string())?;
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        let mut file = open_lock_file(&path)?;
        let mut overlapped = lock_overlapped();
        try_lock_file(&file, &mut overlapped).map_err(|error| {
            if lock_violation(&error) {
                "another Provider Hub operation is already active".to_string()
            } else {
                format!("cannot acquire operation lock: {error}")
            }
        })?;
        let metadata = LockMetadata {
            pid: unsafe { GetCurrentProcessId() },
            started_at: current_started_at(),
            command: command.to_string(),
            created_at: now_unix(),
        };
        let write_result = (|| {
            file.set_len(0).map_err(|error| error.to_string())?;
            file.seek(SeekFrom::Start(0))
                .map_err(|error| error.to_string())?;
            serde_json::to_writer(&mut file, &metadata).map_err(|error| error.to_string())?;
            file.flush().map_err(|error| error.to_string())?;
            file.sync_all().map_err(|error| error.to_string())
        })();
        if let Err(error) = write_result {
            unsafe {
                let _ = UnlockFileEx(HANDLE(file.as_raw_handle()), None, 1, 0, &mut overlapped);
            }
            return Err(format!("cannot write operation lock metadata: {error}"));
        }
        Ok(OperationLock {
            file: Some(file),
            path,
            overlapped,
        })
    }

    pub fn inspect_operation_lock(home: &Path) -> Result<OperationLockStatus, String> {
        let path = operation_lock_path(home)?;
        if !path.is_file() {
            return Ok(OperationLockStatus {
                state: "clear".into(),
                path: path.to_string_lossy().to_string(),
                owner_pid: None,
                owner_started_at: None,
                command: None,
                age_seconds: None,
            });
        }
        let mut file = open_lock_file(&path)?;
        let mut overlapped = lock_overlapped();
        match try_lock_file(&file, &mut overlapped) {
            Ok(()) => {
                unsafe {
                    let _ = UnlockFileEx(HANDLE(file.as_raw_handle()), None, 1, 0, &mut overlapped);
                }
                Ok(OperationLockStatus {
                    state: "clear".into(),
                    path: path.to_string_lossy().to_string(),
                    owner_pid: None,
                    owner_started_at: None,
                    command: None,
                    age_seconds: None,
                })
            }
            Err(error) if lock_violation(&error) => {
                file.seek(SeekFrom::Start(0))
                    .map_err(|error| error.to_string())?;
                let mut contents = Vec::new();
                file.read_to_end(&mut contents)
                    .map_err(|error| error.to_string())?;
                let metadata = serde_json::from_slice::<LockMetadata>(&contents).ok();
                Ok(OperationLockStatus {
                    state: "active".into(),
                    path: path.to_string_lossy().to_string(),
                    owner_pid: metadata.as_ref().map(|value| value.pid),
                    owner_started_at: metadata
                        .as_ref()
                        .and_then(|value| value.started_at)
                        .map(|value| value.to_string()),
                    command: metadata.as_ref().map(|value| value.command.clone()),
                    age_seconds: metadata
                        .as_ref()
                        .map(|value| now_unix().saturating_sub(value.created_at)),
                })
            }
            Err(error) => Err(format!("cannot inspect operation lock: {error}")),
        }
    }

    fn resource_paths(home: &Path) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        for relative in ["state_5.sqlite", "sqlite/codex-dev.db"] {
            let database = home.join(relative);
            for candidate in [
                database.clone(),
                PathBuf::from(format!("{}-wal", database.to_string_lossy())),
                PathBuf::from(format!("{}-shm", database.to_string_lossy())),
            ] {
                if candidate.is_file() {
                    paths.push(fs::canonicalize(&candidate).unwrap_or(candidate));
                }
            }
        }
        paths.sort();
        paths.dedup();
        paths
    }

    fn restart_manager_processes(home: &Path) -> Result<Vec<RM_PROCESS_INFO>, String> {
        let paths = resource_paths(home);
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let session = RestartManagerSession::start()?;
        let path_buffers = paths
            .iter()
            .map(|path| wide(path.as_os_str()))
            .collect::<Vec<_>>();
        let path_pointers = path_buffers
            .iter()
            .map(|path| PCWSTR(path.as_ptr()))
            .collect::<Vec<_>>();
        let register = unsafe { RmRegisterResources(session.0, Some(&path_pointers), None, None) };
        if register != ERROR_SUCCESS {
            return Err(format!("RmRegisterResources failed: {}", register.0));
        }

        let mut needed = 0_u32;
        let mut count = 0_u32;
        let mut reboot_reasons = 0_u32;
        let initial = unsafe {
            RmGetList(
                session.0,
                &mut needed,
                &mut count,
                None,
                &mut reboot_reasons,
            )
        };
        if initial == ERROR_SUCCESS {
            return Ok(Vec::new());
        }
        if initial != ERROR_MORE_DATA {
            return Err(format!("RmGetList failed: {}", initial.0));
        }
        for _ in 0..4 {
            let mut processes = vec![RM_PROCESS_INFO::default(); needed as usize];
            count = processes.len() as u32;
            let result = unsafe {
                RmGetList(
                    session.0,
                    &mut needed,
                    &mut count,
                    Some(processes.as_mut_ptr()),
                    &mut reboot_reasons,
                )
            };
            if result == ERROR_SUCCESS {
                processes.truncate(count as usize);
                return Ok(processes);
            }
            if result != ERROR_MORE_DATA {
                return Err(format!("RmGetList failed: {}", result.0));
            }
        }
        Err("RmGetList changed repeatedly while enumerating processes".into())
    }

    fn process_table() -> HashMap<u32, (u32, String)> {
        let mut rows = HashMap::new();
        let snapshot = match unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) } {
            Ok(handle) => OwnedHandle(handle),
            Err(_) => return rows,
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if unsafe { Process32FirstW(snapshot.raw(), &mut entry) }.is_err() {
            return rows;
        }
        loop {
            rows.insert(
                entry.th32ProcessID,
                (entry.th32ParentProcessID, wide_string(&entry.szExeFile)),
            );
            if unsafe { Process32NextW(snapshot.raw(), &mut entry) }.is_err() {
                break;
            }
        }
        rows
    }

    fn ancestor_pids(table: &HashMap<u32, (u32, String)>) -> HashSet<u32> {
        let mut ancestors = HashSet::new();
        let mut current = unsafe { GetCurrentProcessId() };
        let mut child_started_at = current_started_at();
        for _ in 0..64 {
            let Some((parent, _)) = table.get(&current) else {
                break;
            };
            if *parent == 0 {
                break;
            }
            let parent_started_at = query_process(*parent, None)
                .ok()
                .map(|(_, started_at)| started_at);
            if parent_started_at.is_none()
                || child_started_at
                    .zip(parent_started_at)
                    .is_some_and(|(child, parent)| parent >= child)
                || !ancestors.insert(*parent)
            {
                break;
            }
            current = *parent;
            child_started_at = parent_started_at;
        }
        ancestors
    }

    fn query_process(pid: u32, expected_started_at: Option<u64>) -> Result<(String, u64), String> {
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                false,
                pid,
            )
        }
        .map(OwnedHandle)
        .map_err(|error| error.to_string())?;
        query_process_handle(handle.raw(), expected_started_at)
    }

    fn query_process_handle(
        handle: HANDLE,
        expected_started_at: Option<u64>,
    ) -> Result<(String, u64), String> {
        let mut created = FILETIME::default();
        let mut exited = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) }
            .map_err(|error| error.to_string())?;
        let started_at = filetime_value(created);
        if expected_started_at.is_some_and(|expected| expected != started_at) {
            return Err("process identity changed".into());
        }
        let mut path = vec![0_u16; 32_768];
        let mut size = path.len() as u32;
        unsafe {
            QueryFullProcessImageNameW(
                handle,
                Default::default(),
                PWSTR(path.as_mut_ptr()),
                &mut size,
            )
        }
        .map_err(|error| error.to_string())?;
        path.truncate(size as usize);
        Ok((String::from_utf16_lossy(&path), started_at))
    }

    fn current_session_id() -> Option<u32> {
        let mut session = 0_u32;
        unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) }
            .ok()
            .map(|_| session)
    }

    fn app_type_name(value: RM_APP_TYPE) -> &'static str {
        if value == RmMainWindow {
            "main-window"
        } else if value == RmOtherWindow {
            "other-window"
        } else if value == RmService {
            "service"
        } else if value == RmExplorer {
            "explorer"
        } else if value == RmConsole {
            "console"
        } else if value == RmCritical {
            "critical"
        } else {
            "unknown"
        }
    }

    fn allowlisted_name(name: &str) -> bool {
        matches!(
            name.to_ascii_lowercase().as_str(),
            "chatgpt.exe"
                | "codex.exe"
                | "codexprovidersync.exe"
                | "codex-plus-plus.exe"
                | "codex-plus-plus-manager.exe"
                | "codexpilot.exe"
                | "codexpilot-manager.exe"
        )
    }

    pub fn blocking_processes(home: &Path) -> Result<Vec<BlockingProcess>, String> {
        let rm_processes = restart_manager_processes(home)?;
        let table = process_table();
        let ancestors = ancestor_pids(&table);
        let current_pid = unsafe { GetCurrentProcessId() };
        let current_path = std::env::current_exe()
            .ok()
            .map(|path| path.to_string_lossy().to_ascii_lowercase());
        let current_session = current_session_id();
        let mut result = Vec::new();
        for process in rm_processes {
            let pid = process.Process.dwProcessId;
            let rm_started_at = filetime_value(process.Process.ProcessStartTime);
            let queried = query_process(pid, Some(rm_started_at));
            let (path, started_at, verified) = match queried {
                Ok((path, started_at)) => (Some(path), Some(started_at.to_string()), true),
                Err(_) => (None, Some(rm_started_at.to_string()), false),
            };
            let table_row = table.get(&pid);
            let name = path
                .as_ref()
                .and_then(|value| Path::new(value).file_name())
                .map(|value| value.to_string_lossy().to_string())
                .or_else(|| table_row.map(|(_, name)| name.clone()))
                .unwrap_or_else(|| wide_string(&process.strAppName));
            let is_current = pid == current_pid;
            let is_ancestor = ancestors.contains(&pid);
            let same_executable = path.as_ref().is_some_and(|path| {
                current_path
                    .as_ref()
                    .is_some_and(|current| path.to_ascii_lowercase() == *current)
            });
            let same_session =
                current_session.is_some_and(|session| process.TSSessionId == session);
            let protected_type = process.ApplicationType == RmService
                || process.ApplicationType == RmCritical
                || process.ApplicationType == RmExplorer;
            let close_reason = if is_current || same_executable {
                "Provider Hub processes are never closed"
            } else if is_ancestor {
                "ancestor processes are protected"
            } else if protected_type {
                "services, critical processes, and Explorer are protected"
            } else if !same_session {
                "process belongs to another Windows session"
            } else if !verified {
                "process path and start time could not be verified"
            } else if !allowlisted_name(&name) {
                "process is not in the Codex close allowlist"
            } else {
                "verified SQLite owner"
            };
            let close_allowed = close_reason == "verified SQLite owner";
            result.push(BlockingProcess {
                identity: ProcessIdentity {
                    pid,
                    name,
                    path,
                    started_at,
                    parent_pid: table_row.map(|(parent, _)| *parent),
                    session_id: Some(process.TSSessionId),
                    verified,
                    is_current,
                    is_ancestor,
                },
                application_type: app_type_name(process.ApplicationType).into(),
                application_root_pid: None,
                service_name: match wide_string(&process.strServiceShortName) {
                    value if value.is_empty() => None,
                    value => Some(value),
                },
                restartable: process.bRestartable.as_bool(),
                close_allowed,
                close_reason: close_reason.into(),
            });
        }
        result.sort_by(|left, right| {
            left.identity
                .name
                .cmp(&right.identity.name)
                .then(left.identity.pid.cmp(&right.identity.pid))
        });
        result.dedup_by_key(|process| process.identity.pid);
        for process in &mut result {
            process.application_root_pid = verified_application_family(process)
                .ok()
                .and_then(|family| application_root_pid(&family));
        }
        Ok(result)
    }

    #[derive(Debug, Clone)]
    struct VerifiedProcess {
        pid: u32,
        parent_pid: u32,
        name: String,
        path: String,
        started_at: u64,
        session_id: u32,
    }

    fn process_session_id(pid: u32) -> Option<u32> {
        let mut session = 0_u32;
        unsafe { ProcessIdToSessionId(pid, &mut session) }
            .ok()
            .map(|_| session)
    }

    fn verified_process(pid: u32, table: &HashMap<u32, (u32, String)>) -> Option<VerifiedProcess> {
        let (parent_pid, table_name) = table.get(&pid)?;
        let (path, started_at) = query_process(pid, None).ok()?;
        let name = Path::new(&path)
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_else(|| table_name.clone());
        if !allowlisted_name(&name) {
            return None;
        }
        Some(VerifiedProcess {
            pid,
            parent_pid: *parent_pid,
            name,
            path,
            started_at,
            session_id: process_session_id(pid)?,
        })
    }

    fn known_host_child(parent_name: &str, child_name: &str) -> bool {
        matches!(
            (
                parent_name.to_ascii_lowercase().as_str(),
                child_name.to_ascii_lowercase().as_str(),
            ),
            ("chatgpt.exe", "codex.exe")
                | ("codex-plus-plus-manager.exe", "codex-plus-plus.exe")
                | ("codex-plus-plus-manager.exe", "codexprovidersync.exe")
                | ("codexpilot-manager.exe", "codexpilot.exe")
        )
    }

    fn child_is_in_host_install(parent_path: &str, child_path: &str) -> bool {
        let Some(parent_dir) = Path::new(parent_path).parent() else {
            return false;
        };
        let parent_dir = parent_dir
            .to_string_lossy()
            .trim_end_matches(['\\', '/'])
            .to_ascii_lowercase();
        let child_path = child_path.to_ascii_lowercase();
        child_path == parent_dir
            || child_path
                .strip_prefix(&parent_dir)
                .is_some_and(|suffix| suffix.starts_with('\\') || suffix.starts_with('/'))
    }

    fn same_application_edge(parent: &VerifiedProcess, child: &VerifiedProcess) -> bool {
        parent.pid == child.parent_pid
            && parent.session_id == child.session_id
            && parent.started_at <= child.started_at
            && (parent.path.eq_ignore_ascii_case(&child.path)
                || (known_host_child(&parent.name, &child.name)
                    && child_is_in_host_install(&parent.path, &child.path)))
    }

    fn verified_application_family(
        process: &BlockingProcess,
    ) -> Result<Vec<VerifiedProcess>, String> {
        let expected_path = process
            .identity
            .path
            .as_deref()
            .ok_or_else(|| "process path is unavailable".to_string())?;
        let expected_started_at = process
            .identity
            .started_at
            .as_deref()
            .ok_or_else(|| "process start time is unavailable".to_string())?
            .parse::<u64>()
            .map_err(|_| "process start time is invalid".to_string())?;
        let table = process_table();
        let protected = ancestor_pids(&table);
        let current_pid = unsafe { GetCurrentProcessId() };
        let target = verified_process(process.identity.pid, &table)
            .ok_or_else(|| "process identity can no longer be verified".to_string())?;
        if target.started_at != expected_started_at
            || !target.path.eq_ignore_ascii_case(expected_path)
        {
            return Err("process identity changed".into());
        }

        let mut root = target.clone();
        for _ in 0..64 {
            if root.parent_pid == 0
                || root.parent_pid == current_pid
                || protected.contains(&root.parent_pid)
            {
                break;
            }
            let Some(parent) = verified_process(root.parent_pid, &table) else {
                break;
            };
            if !same_application_edge(&parent, &root) {
                break;
            }
            root = parent;
        }

        let mut family = vec![root.clone()];
        let mut seen = HashSet::from([root.pid]);
        let mut cursor = 0;
        while cursor < family.len() {
            let parent = family[cursor].clone();
            cursor += 1;
            for (pid, (parent_pid, _)) in table.iter() {
                if seen.contains(pid) || *pid == current_pid || protected.contains(pid) {
                    continue;
                }
                if *parent_pid != parent.pid {
                    continue;
                }
                let Some(child) = verified_process(*pid, &table) else {
                    continue;
                };
                if same_application_edge(&parent, &child) {
                    seen.insert(*pid);
                    family.push(child);
                }
            }
        }
        family.sort_by_key(|candidate| candidate.pid);
        Ok(family)
    }

    fn application_root_pid(processes: &[VerifiedProcess]) -> Option<u32> {
        let family_pids = processes
            .iter()
            .map(|process| process.pid)
            .collect::<HashSet<_>>();
        processes
            .iter()
            .find(|process| !family_pids.contains(&process.parent_pid))
            .map(|process| process.pid)
    }

    fn restart_manager_targets(
        processes: &[VerifiedProcess],
        sqlite_owner_pids: &HashSet<u32>,
    ) -> Vec<VerifiedProcess> {
        let root_pid = application_root_pid(processes);
        let mut targets = processes
            .iter()
            .filter(|process| {
                Some(process.pid) == root_pid || sqlite_owner_pids.contains(&process.pid)
            })
            .cloned()
            .collect::<Vec<_>>();
        targets.sort_by_key(|process| process.pid);
        targets.dedup_by_key(|process| process.pid);
        targets
    }

    struct WindowSearch {
        pids: HashSet<u32>,
        windows: Vec<HWND>,
    }

    unsafe extern "system" fn enum_windows_for_process(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let search = unsafe { &mut *(lparam.0 as *mut WindowSearch) };
        let mut pid = 0_u32;
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
        }
        if search.pids.contains(&pid)
            && unsafe { IsWindowVisible(hwnd).as_bool() }
            && unsafe { GetWindow(hwnd, GW_OWNER).is_err() }
        {
            search.windows.push(hwnd);
        }
        BOOL(1)
    }

    fn restart_manager_shutdown(processes: &[VerifiedProcess], force: bool) -> Result<(), String> {
        if processes.is_empty() {
            return Err("no verified application processes are available".into());
        }
        let applications = processes
            .iter()
            .map(|process| RM_UNIQUE_PROCESS {
                dwProcessId: process.pid,
                ProcessStartTime: FILETIME {
                    dwLowDateTime: process.started_at as u32,
                    dwHighDateTime: (process.started_at >> 32) as u32,
                },
            })
            .collect::<Vec<_>>();
        let session = RestartManagerSession::start()?;
        let register = unsafe { RmRegisterResources(session.0, None, Some(&applications), None) };
        if register != ERROR_SUCCESS {
            return Err(format!("RmRegisterResources failed: {}", register.0));
        }
        // RmShutdownOnlyRegistered means every process must have called
        // RegisterApplicationRestart. It is not a process allowlist and causes
        // ordinary desktop hosts such as ChatGPT to be left running.
        let flags = if force {
            RESTART_MANAGER_FORCE_FLAGS
        } else {
            RESTART_MANAGER_GRACEFUL_FLAGS
        };
        let shutdown = unsafe { RmShutdown(session.0, flags, None) };
        if shutdown != ERROR_SUCCESS {
            if shutdown.0 == ERROR_FAIL_NOACTION_REBOOT_CODE {
                return Err(
                    "Restart Manager could not close the registered application without a reboot"
                        .into(),
                );
            }
            return Err(format!("RmShutdown failed: {}", shutdown.0));
        }
        Ok(())
    }

    fn verified_family_handles(
        processes: &[VerifiedProcess],
        target_pid: u32,
        terminate: bool,
    ) -> Result<Vec<(VerifiedProcess, OwnedHandle)>, String> {
        let family_pids = processes
            .iter()
            .map(|process| process.pid)
            .collect::<HashSet<_>>();
        let mut handles = Vec::new();
        for process in processes {
            let required = process.pid == target_pid || !family_pids.contains(&process.parent_pid);
            let access = PROCESS_QUERY_LIMITED_INFORMATION
                | PROCESS_SYNCHRONIZE
                | if terminate {
                    PROCESS_TERMINATE
                } else {
                    Default::default()
                };
            let verified = (|| {
                let handle = unsafe {
                    OpenProcess(access, false, process.pid)
                }
                .map(OwnedHandle)
                .map_err(|error| {
                    if terminate
                        && error.code() == HRESULT::from_win32(ERROR_ACCESS_DENIED.0)
                    {
                        format!(
                            "access denied while preparing to terminate PID {}; only restart Provider Hub as administrator if that process is elevated",
                            process.pid
                        )
                    } else {
                        error.to_string()
                    }
                })?;
                let (path, _) = query_process_handle(handle.raw(), Some(process.started_at))?;
                if !path.eq_ignore_ascii_case(&process.path) {
                    return Err(format!(
                        "application-group process changed: PID {}",
                        process.pid
                    ));
                }
                Ok(handle)
            })();
            match verified {
                Ok(handle) => handles.push((process.clone(), handle)),
                Err(error) if required => return Err(error),
                Err(error)
                    if terminate
                        && query_process(process.pid, Some(process.started_at)).is_ok() =>
                {
                    return Err(error)
                }
                Err(_) => {
                    // Electron renderer/helper children can exit between the
                    // process snapshot and handle acquisition. They are not a
                    // reason to abandon a verified root + SQLite-owner close.
                }
            }
        }
        Ok(handles)
    }

    fn family_exited(handles: &[(VerifiedProcess, OwnedHandle)], timeout_ms: u32) -> bool {
        let deadline = Instant::now() + Duration::from_millis(u64::from(timeout_ms));
        loop {
            if handles.iter().all(|(_, handle)| {
                (unsafe { WaitForSingleObject(handle.raw(), 0) }) == WAIT_OBJECT_0
            }) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn family_termination_order(processes: &[VerifiedProcess]) -> Vec<usize> {
        let family_pids = processes
            .iter()
            .map(|process| process.pid)
            .collect::<HashSet<_>>();
        let mut order = (0..processes.len()).collect::<Vec<_>>();
        order.sort_by_key(|index| {
            let process = &processes[*index];
            (family_pids.contains(&process.parent_pid), process.pid)
        });
        order
    }

    fn terminate_verified_family(
        handles: &[(VerifiedProcess, OwnedHandle)],
    ) -> Result<usize, String> {
        let processes = handles
            .iter()
            .map(|(process, _)| process.clone())
            .collect::<Vec<_>>();
        let mut requested = 0_usize;
        for index in family_termination_order(&processes) {
            let (process, handle) = &handles[index];
            if unsafe { WaitForSingleObject(handle.raw(), 0) } == WAIT_OBJECT_0 {
                continue;
            }
            if let Err(error) = unsafe { TerminateProcess(handle.raw(), 1) } {
                if unsafe { WaitForSingleObject(handle.raw(), 0) } == WAIT_OBJECT_0 {
                    continue;
                }
                return Err(format!(
                    "failed to terminate verified application process PID {}: {error}",
                    process.pid
                ));
            }
            requested += 1;
        }
        Ok(requested)
    }

    fn same_application_blocker(blocker: &BlockingProcess, family: &[VerifiedProcess]) -> bool {
        family.iter().any(|process| {
            blocker.identity.session_id == Some(process.session_id)
                && blocker.identity.path.as_deref().map_or_else(
                    || blocker.identity.name.eq_ignore_ascii_case(&process.name),
                    |path| path.eq_ignore_ascii_case(&process.path),
                )
        })
    }

    fn wait_for_database_release(
        home: &Path,
        family: &[VerifiedProcess],
        timeout_ms: u32,
    ) -> Result<bool, String> {
        let deadline = Instant::now() + Duration::from_millis(u64::from(timeout_ms));
        loop {
            let still_blocked = blocking_processes(home)?
                .iter()
                .any(|blocker| same_application_blocker(blocker, family));
            if !still_blocked {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    fn already_exited_result(identity: &ProcessIdentity, mode: &str) -> Option<CloseProcessResult> {
        let table = process_table();
        if table.contains_key(&identity.pid) {
            return None;
        }
        Some(CloseProcessResult {
            pid: identity.pid,
            mode: mode.into(),
            requested: false,
            exited: true,
            message: "process already exited".into(),
        })
    }

    pub fn close_process(
        home: &Path,
        requested_identity: &ProcessIdentity,
        force: bool,
    ) -> CloseProcessResult {
        let mode = if force { "force" } else { "graceful" };
        let failure = |message: String| CloseProcessResult {
            pid: requested_identity.pid,
            mode: mode.into(),
            requested: false,
            exited: false,
            message,
        };
        let current_processes = match blocking_processes(home) {
            Ok(processes) => processes,
            Err(error) => {
                return already_exited_result(requested_identity, mode)
                    .unwrap_or_else(|| failure(error))
            }
        };
        let Some(process) = current_processes
            .iter()
            .find(|process| {
                process.identity.pid == requested_identity.pid
                    && process.identity.started_at == requested_identity.started_at
                    && process
                        .identity
                        .path
                        .as_deref()
                        .zip(requested_identity.path.as_deref())
                        .is_some_and(|(current, requested)| current.eq_ignore_ascii_case(requested))
            })
            .cloned()
        else {
            if let Some(result) = already_exited_result(requested_identity, mode) {
                return result;
            }
            return failure("process no longer matches the verified SQLite owner".into());
        };
        let family = match verified_application_family(&process) {
            Ok(family) => family,
            Err(error) => return failure(error),
        };
        let application_root_pid = process.application_root_pid.unwrap_or(process.identity.pid);
        let sqlite_owner_pids = current_processes
            .iter()
            .filter(|candidate| {
                candidate.close_allowed
                    && candidate
                        .application_root_pid
                        .unwrap_or(candidate.identity.pid)
                        == application_root_pid
            })
            .map(|candidate| candidate.identity.pid)
            .collect::<HashSet<_>>();
        let tracked_processes = restart_manager_targets(&family, &sqlite_owner_pids);
        let family_handles =
            match verified_family_handles(&tracked_processes, process.identity.pid, force) {
                Ok(handles) => handles,
                Err(error) => {
                    return already_exited_result(requested_identity, mode)
                        .unwrap_or_else(|| failure(error))
                }
            };
        if force {
            let restart_targets = restart_manager_targets(&family, &sqlite_owner_pids);
            let restart_manager_requested =
                restart_manager_shutdown(&restart_targets, true).is_ok();
            if restart_manager_requested
                && family_exited(&family_handles, PROCESS_FORCE_TIMEOUT_MS / 2)
                && wait_for_database_release(home, &family, DATABASE_RELEASE_TIMEOUT_MS)
                    .unwrap_or(false)
            {
                return CloseProcessResult {
                    pid: process.identity.pid,
                    mode: mode.into(),
                    requested: true,
                    exited: true,
                    message: format!(
                        "SQLite ownership released by Windows Restart Manager ({} verified root/owner process(es))",
                        tracked_processes.len()
                    ),
                };
            }
            let terminated = match terminate_verified_family(&family_handles) {
                Ok(terminated) => terminated,
                Err(error) => return failure(error),
            };
            let tracked_exited = family_exited(&family_handles, PROCESS_FORCE_TIMEOUT_MS);
            let released = tracked_exited
                && wait_for_database_release(home, &family, DATABASE_RELEASE_TIMEOUT_MS)
                    .unwrap_or(false);
            return CloseProcessResult {
                pid: process.identity.pid,
                mode: mode.into(),
                requested: restart_manager_requested || terminated > 0,
                exited: released,
                message: if released {
                    format!(
                        "SQLite ownership released after terminating {} verified root/owner process(es)",
                        tracked_processes.len()
                    )
                } else if tracked_exited {
                    "verified root/owner processes exited, but a matching SQLite owner reappeared; refresh and retry".into()
                } else {
                    "root/owner termination was requested, but at least one verified process is still running".into()
                },
            };
        }
        let mut search = WindowSearch {
            pids: family.iter().map(|candidate| candidate.pid).collect(),
            windows: Vec::new(),
        };
        if let Err(error) = unsafe {
            EnumWindows(
                Some(enum_windows_for_process),
                LPARAM(&mut search as *mut WindowSearch as isize),
            )
        } {
            return failure(error.to_string());
        }
        let mut requested = false;
        let mut window_requests = 0_usize;
        for window in search.windows {
            let delivered = unsafe {
                SendMessageTimeoutW(
                    window,
                    WM_CLOSE,
                    WPARAM(0),
                    LPARAM(0),
                    SMTO_ABORTIFHUNG | SMTO_BLOCK,
                    WINDOW_CLOSE_TIMEOUT_MS,
                    None,
                )
            };
            if delivered.0 != 0 {
                requested = true;
                window_requests += 1;
            }
        }
        if requested && family_exited(&family_handles, PROCESS_CLOSE_TIMEOUT_MS / 2) {
            let released = wait_for_database_release(home, &family, DATABASE_RELEASE_TIMEOUT_MS)
                .unwrap_or(false);
            return CloseProcessResult {
                pid: process.identity.pid,
                mode: mode.into(),
                requested: true,
                exited: released,
                message: if !released {
                    "the tracked application processes exited after WM_CLOSE, but SQLite ownership reappeared".into()
                } else if tracked_processes.len() > 1 {
                    format!(
                        "SQLite ownership released after WM_CLOSE ({window_requests} window(s), {} verified root/owner process(es))",
                        tracked_processes.len()
                    )
                } else {
                    "SQLite ownership released after WM_CLOSE".into()
                },
            };
        }
        let remaining_family = family_handles
            .iter()
            .filter(|(_, handle)| {
                (unsafe { WaitForSingleObject(handle.raw(), 0) }) != WAIT_OBJECT_0
            })
            .map(|(process, _)| process.clone())
            .collect::<Vec<_>>();
        if remaining_family.is_empty() {
            let released = wait_for_database_release(home, &family, DATABASE_RELEASE_TIMEOUT_MS)
                .unwrap_or(false);
            return CloseProcessResult {
                pid: process.identity.pid,
                mode: mode.into(),
                requested,
                exited: released,
                message: if released {
                    format!(
                        "SQLite ownership released after WM_CLOSE ({window_requests} window(s), {} verified root/owner process(es))",
                        tracked_processes.len()
                    )
                } else {
                    "the tracked application processes exited, but SQLite ownership reappeared"
                        .into()
                },
            };
        }
        let restart_manager = restart_manager_shutdown(&remaining_family, false);
        match &restart_manager {
            Ok(()) => requested = true,
            Err(error) if !requested => {
                return failure(format!("no graceful close path succeeded: {error}"));
            }
            Err(_) => {}
        }
        let tracked_exited = family_exited(&family_handles, PROCESS_CLOSE_TIMEOUT_MS);
        let released = tracked_exited
            && wait_for_database_release(home, &family, DATABASE_RELEASE_TIMEOUT_MS)
                .unwrap_or(false);
        CloseProcessResult {
            pid: process.identity.pid,
            mode: mode.into(),
            requested,
            exited: released,
            message: if released {
                if tracked_processes.len() > 1 {
                    format!(
                        "SQLite ownership released after Restart Manager request ({} verified root/owner process(es))",
                        tracked_processes.len()
                    )
                } else {
                    "SQLite ownership released after Restart Manager request".into()
                }
            } else if tracked_exited {
                "the verified root/owner processes exited, but SQLite ownership reappeared".into()
            } else {
                match restart_manager {
                    Ok(()) if window_requests > 0 => format!(
                        "graceful close reached {window_requests} window(s) and Restart Manager, but the application group is still running"
                    ),
                    Ok(()) => "Restart Manager accepted the graceful close request, but the application group is still running".into(),
                    Err(error) => format!(
                        "WM_CLOSE reached {window_requests} window(s), but the application group is still running and Restart Manager failed: {error}"
                    ),
                }
            },
        }
    }

    pub fn reopen_codex(executable_path: &Path) -> Result<String, String> {
        use std::os::windows::process::CommandExt;

        let metadata = std::fs::symlink_metadata(executable_path).map_err(|error| {
            format!(
                "cannot reopen Codex ({}): {error}",
                executable_path.display()
            )
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(format!(
                "refusing to launch a non-regular executable: {}",
                executable_path.display()
            ));
        }
        let canonical =
            std::fs::canonicalize(executable_path).map_err(|error| error.to_string())?;
        let filename = canonical
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        let launch_path = if filename.eq_ignore_ascii_case("chatgpt.exe") {
            canonical
        } else if filename.eq_ignore_ascii_case("codex.exe") {
            let desktop_host = canonical
                .parent()
                .and_then(Path::parent)
                .map(|app| app.join("ChatGPT.exe"));
            match desktop_host {
                Some(path) if path.is_file() => path,
                _ => canonical,
            }
        } else {
            return Err(format!(
                "refusing to launch an executable outside the Codex allowlist: {filename}"
            ));
        };

        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::process::Command::new(&launch_path)
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map_err(|error| format!("cannot reopen Codex ({}): {error}", launch_path.display()))?;
        Ok(launch_path.to_string_lossy().to_string())
    }

    pub fn open_folder(folder_path: &Path) -> Result<String, String> {
        use std::os::windows::process::CommandExt;

        let canonical = std::fs::canonicalize(folder_path).map_err(|error| {
            format!(
                "cannot open project folder ({}): {error}",
                folder_path.display()
            )
        })?;
        if !canonical.is_dir() {
            return Err(format!(
                "project folder no longer exists: {}",
                canonical.display()
            ));
        }

        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let shell_path = explorer_compatible_path(&canonical);
        std::process::Command::new("explorer.exe")
            .arg(&shell_path)
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map_err(|error| {
                format!(
                    "cannot open project folder ({}): {error}",
                    canonical.display()
                )
            })?;
        Ok(canonical.to_string_lossy().to_string())
    }

    pub fn reveal_file(file_path: &Path) -> Result<String, String> {
        use std::os::windows::process::CommandExt;

        let canonical = std::fs::canonicalize(file_path).map_err(|error| {
            format!(
                "cannot locate rollout file ({}): {error}",
                file_path.display()
            )
        })?;
        if !canonical.is_file() {
            return Err(format!(
                "rollout file no longer exists: {}",
                canonical.display()
            ));
        }

        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let shell_path = explorer_compatible_path(&canonical);
        std::process::Command::new("explorer.exe")
            .arg(format!("/select,{}", shell_path.display()))
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map_err(|error| {
                format!(
                    "cannot locate rollout file ({}): {error}",
                    canonical.display()
                )
            })?;
        Ok(canonical.to_string_lossy().to_string())
    }

    #[cfg(test)]
    mod close_tests {
        use super::*;

        fn process(
            pid: u32,
            parent_pid: u32,
            name: &str,
            path: &str,
            started_at: u64,
        ) -> VerifiedProcess {
            VerifiedProcess {
                pid,
                parent_pid,
                name: name.into(),
                path: path.into(),
                started_at,
                session_id: 1,
            }
        }

        fn blocker(pid: u32, name: &str, path: &str) -> BlockingProcess {
            BlockingProcess {
                identity: ProcessIdentity {
                    pid,
                    name: name.into(),
                    path: Some(path.into()),
                    started_at: Some("999".into()),
                    parent_pid: None,
                    session_id: Some(1),
                    verified: true,
                    is_current: false,
                    is_ancestor: false,
                },
                application_type: "main-window".into(),
                application_root_pid: Some(pid),
                service_name: None,
                restartable: false,
                close_allowed: true,
                close_reason: "verified SQLite owner".into(),
            }
        }

        #[test]
        fn codex_child_is_part_of_its_verified_chatgpt_install() {
            let parent = process(
                10,
                1,
                "ChatGPT.exe",
                r"C:\Program Files\WindowsApps\OpenAI.Codex\app\ChatGPT.exe",
                100,
            );
            let child = process(
                11,
                10,
                "codex.exe",
                r"C:\Program Files\WindowsApps\OpenAI.Codex\app\resources\codex.exe",
                101,
            );
            assert!(same_application_edge(&parent, &child));
        }

        #[test]
        fn host_child_pair_from_another_install_is_rejected() {
            let parent = process(
                10,
                1,
                "ChatGPT.exe",
                r"C:\Program Files\WindowsApps\OpenAI.Codex\app\ChatGPT.exe",
                100,
            );
            let child = process(11, 10, "codex.exe", r"C:\Users\me\bin\codex.exe", 101);
            assert!(!same_application_edge(&parent, &child));
        }

        #[test]
        fn unrelated_allowlisted_sibling_is_rejected() {
            let parent = process(
                10,
                1,
                "ChatGPT.exe",
                r"C:\Program Files\WindowsApps\OpenAI.Codex\app\ChatGPT.exe",
                100,
            );
            let sibling = process(
                11,
                9,
                "ChatGPT.exe",
                r"C:\Program Files\WindowsApps\OpenAI.Codex\app\ChatGPT.exe",
                101,
            );
            assert!(!same_application_edge(&parent, &sibling));
        }

        #[test]
        fn existing_pid_is_not_misreported_as_already_exited() {
            let identity = ProcessIdentity {
                pid: unsafe { GetCurrentProcessId() },
                name: "codex.exe".into(),
                path: Some("C:\\placeholder\\codex.exe".into()),
                started_at: Some("1".into()),
                parent_pid: None,
                session_id: process_session_id(unsafe { GetCurrentProcessId() }),
                verified: true,
                is_current: true,
                is_ancestor: false,
            };
            assert!(already_exited_result(&identity, "graceful").is_none());
        }

        #[test]
        fn restart_manager_graceful_shutdown_does_not_require_restart_registration() {
            assert_eq!(RESTART_MANAGER_GRACEFUL_FLAGS, 0);
        }

        #[test]
        fn force_termination_places_the_host_before_its_children() {
            let child = process(
                11,
                10,
                "codex.exe",
                r"C:\Program Files\WindowsApps\OpenAI.Codex\app\resources\codex.exe",
                101,
            );
            let host = process(
                10,
                1,
                "ChatGPT.exe",
                r"C:\Program Files\WindowsApps\OpenAI.Codex\app\ChatGPT.exe",
                100,
            );
            let renderer = process(
                12,
                10,
                "ChatGPT.exe",
                r"C:\Program Files\WindowsApps\OpenAI.Codex\app\ChatGPT.exe",
                102,
            );
            let family = vec![child, renderer, host];
            let order = family_termination_order(&family);
            assert_eq!(family[order[0]].pid, 10);
        }

        #[test]
        fn restart_manager_only_receives_the_host_and_sqlite_owner() {
            let host = process(
                10,
                1,
                "ChatGPT.exe",
                r"C:\Program Files\WindowsApps\OpenAI.Codex\app\ChatGPT.exe",
                100,
            );
            let owner = process(
                11,
                10,
                "codex.exe",
                r"C:\Program Files\WindowsApps\OpenAI.Codex\app\resources\codex.exe",
                101,
            );
            let renderer = process(
                12,
                10,
                "ChatGPT.exe",
                r"C:\Program Files\WindowsApps\OpenAI.Codex\app\ChatGPT.exe",
                102,
            );
            let targets = restart_manager_targets(&[host, owner, renderer], &HashSet::from([11]));
            assert_eq!(
                targets
                    .iter()
                    .map(|process| process.pid)
                    .collect::<Vec<_>>(),
                vec![10, 11]
            );
        }

        #[test]
        fn respawned_sqlite_owner_is_matched_by_verified_install_path() {
            let owner_path = r"C:\Program Files\WindowsApps\OpenAI.Codex\app\resources\codex.exe";
            let family = vec![process(11, 10, "codex.exe", owner_path, 101)];
            assert!(same_application_blocker(
                &blocker(99, "codex.exe", owner_path),
                &family
            ));
            assert!(!same_application_blocker(
                &blocker(99, "codex.exe", r"C:\Other\codex.exe"),
                &family
            ));
        }

        #[test]
        fn restart_manager_force_flag_is_explicit() {
            assert_eq!(RESTART_MANAGER_FORCE_FLAGS, 0x1);
        }

        #[test]
        fn explorer_paths_drop_the_extended_length_prefix() {
            assert_eq!(
                explorer_compatible_path(Path::new(r"\\?\C:\Users\xuan\.codex")),
                PathBuf::from(r"C:\Users\xuan\.codex")
            );
            assert_eq!(
                explorer_compatible_path(Path::new(r"\\?\UNC\server\share\file.jsonl")),
                PathBuf::from(r"\\server\share\file.jsonl")
            );
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;

    pub fn open_exclusive_file(_path: &Path) -> Result<std::fs::File, String> {
        Err("exclusive file guards are only available on Windows".into())
    }

    pub fn open_provider_config_guard(path: &Path) -> Result<std::fs::File, String> {
        std::fs::File::open(path).map_err(|error| error.to_string())
    }

    pub struct OperationLock;

    impl OperationLock {
        pub fn path(&self) -> &Path {
            Path::new("")
        }

        pub fn release(self) -> Result<(), String> {
            Err("operation locking is only available on Windows".into())
        }
    }

    pub fn operation_lock_path(_home: &Path) -> Result<PathBuf, String> {
        Err("operation locking is only available on Windows".into())
    }

    pub fn atomic_replace_file(source: &Path, target: &Path) -> Result<(), String> {
        std::fs::rename(source, target).map_err(|error| error.to_string())
    }

    pub fn acquire_operation_lock(_home: &Path, _command: &str) -> Result<OperationLock, String> {
        Err("operation locking is only available on Windows".into())
    }

    pub fn inspect_operation_lock(_home: &Path) -> Result<OperationLockStatus, String> {
        Ok(OperationLockStatus {
            state: "unsupported".into(),
            path: String::new(),
            owner_pid: None,
            owner_started_at: None,
            command: None,
            age_seconds: None,
        })
    }

    pub fn blocking_processes(_home: &Path) -> Result<Vec<BlockingProcess>, String> {
        Err("native process inspection is only available on Windows".into())
    }

    pub fn close_process(
        _home: &Path,
        requested_identity: &ProcessIdentity,
        force: bool,
    ) -> CloseProcessResult {
        CloseProcessResult {
            pid: requested_identity.pid,
            mode: if force { "force" } else { "graceful" }.into(),
            requested: false,
            exited: false,
            message: "native process closing is only available on Windows".into(),
        }
    }

    pub fn reopen_codex(_executable_path: &Path) -> Result<String, String> {
        Err("reopening Codex is only available on Windows".into())
    }

    pub fn open_folder(folder_path: &Path) -> Result<String, String> {
        let canonical = std::fs::canonicalize(folder_path).map_err(|error| {
            format!(
                "cannot open project folder ({}): {error}",
                folder_path.display()
            )
        })?;
        if !canonical.is_dir() {
            return Err(format!(
                "project folder no longer exists: {}",
                canonical.display()
            ));
        }

        #[cfg(target_os = "macos")]
        let mut command = std::process::Command::new("open");
        #[cfg(all(unix, not(target_os = "macos")))]
        let mut command = std::process::Command::new("xdg-open");

        command.arg(&canonical).spawn().map_err(|error| {
            format!(
                "cannot open project folder ({}): {error}",
                canonical.display()
            )
        })?;
        Ok(canonical.to_string_lossy().to_string())
    }

    pub fn reveal_file(file_path: &Path) -> Result<String, String> {
        let canonical = std::fs::canonicalize(file_path).map_err(|error| {
            format!(
                "cannot locate rollout file ({}): {error}",
                file_path.display()
            )
        })?;
        if !canonical.is_file() {
            return Err(format!(
                "rollout file no longer exists: {}",
                canonical.display()
            ));
        }
        let parent = canonical
            .parent()
            .ok_or_else(|| "rollout file has no parent directory".to_string())?;

        #[cfg(target_os = "macos")]
        let mut command = std::process::Command::new("open");
        #[cfg(all(unix, not(target_os = "macos")))]
        let mut command = std::process::Command::new("xdg-open");

        command.arg(parent).spawn().map_err(|error| {
            format!(
                "cannot locate rollout file ({}): {error}",
                canonical.display()
            )
        })?;
        Ok(canonical.to_string_lossy().to_string())
    }
}

pub use imp::{
    acquire_operation_lock, atomic_replace_file, blocking_processes, close_process,
    inspect_operation_lock, open_exclusive_file, open_folder, open_provider_config_guard,
    operation_lock_path, reopen_codex, reveal_file, OperationLock,
};

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn native_operation_lock_is_exclusive_and_not_age_based() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let home = std::env::temp_dir().join(format!("provider-hub-lock-{nonce}"));
        let first = acquire_operation_lock(&home, "test").unwrap();
        let active = inspect_operation_lock(&home).unwrap();
        assert_eq!(active.state, "active");
        assert!(acquire_operation_lock(&home, "second").is_err());
        drop(first);
        let clear = inspect_operation_lock(&home).unwrap();
        assert_eq!(clear.state, "clear");
        let _ = std::fs::remove_file(operation_lock_path(&home).unwrap());
    }

    #[test]
    fn atomic_replace_preserves_a_complete_target() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("provider-hub-replace-{nonce}"));
        std::fs::create_dir(&root).unwrap();
        let source = root.join("new.json");
        let target = root.join("current.json");
        std::fs::write(&source, b"new").unwrap();
        std::fs::write(&target, b"old").unwrap();
        atomic_replace_file(&source, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert!(!source.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn exclusive_file_guard_blocks_concurrent_writers() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("provider-hub-file-guard-{nonce}"));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("state.json");
        std::fs::write(&path, b"{}").unwrap();
        let first = open_exclusive_file(&path).unwrap();
        assert!(open_exclusive_file(&path).is_err());
        drop(first);
        assert!(open_exclusive_file(&path).is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn process_start_time_survives_json_without_javascript_number_loss() {
        let value = ProcessIdentity {
            pid: 42,
            name: "codex.exe".into(),
            path: Some("C:\\codex.exe".into()),
            started_at: Some(u64::MAX.to_string()),
            parent_pid: None,
            session_id: Some(1),
            verified: true,
            is_current: false,
            is_ancestor: false,
        };
        let json = serde_json::to_string(&value).unwrap();
        assert!(json.contains(&format!("\"{}\"", u64::MAX)));
        let decoded: ProcessIdentity = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.started_at, value.started_at);
    }

    #[test]
    fn closing_an_already_exited_process_is_idempotent() {
        let identity = ProcessIdentity {
            pid: u32::MAX,
            name: "codex.exe".into(),
            path: Some("C:\\missing\\codex.exe".into()),
            started_at: Some("1".into()),
            parent_pid: None,
            session_id: Some(1),
            verified: true,
            is_current: false,
            is_ancestor: false,
        };
        let result = close_process(
            Path::new("C:\\definitely-missing-codex-home"),
            &identity,
            false,
        );
        assert!(!result.requested);
        assert!(result.exited);
        assert_eq!(result.message, "process already exited");
    }
}
