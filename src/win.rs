//! Safe wrappers over the Win32 calls ccdash makes on Windows.

use std::{
    ffi::c_void,
    io,
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    ptr,
    time::Duration,
};

use windows_sys::{
    Wdk::System::Threading::NtQueryInformationProcess,
    Wdk::System::Threading::PROCESSINFOCLASS,
    Wdk::System::Threading::ProcessBasicInformation,
    Win32::{
        Foundation::{
            ERROR_ACCESS_DENIED, ERROR_BUFFER_OVERFLOW, ERROR_INVALID_FUNCTION, ERROR_IO_PENDING,
            ERROR_NOTIFY_ENUM_DIR, ERROR_OPERATION_ABORTED, ERROR_SUCCESS, FILETIME, GENERIC_READ,
            GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, LocalFree, UNICODE_STRING, WAIT_FAILED,
            WAIT_OBJECT_0, WAIT_TIMEOUT,
        },
        NetworkManagement::IpHelper::GAA_FLAG_SKIP_ANYCAST,
        NetworkManagement::IpHelper::GAA_FLAG_SKIP_DNS_SERVER,
        NetworkManagement::IpHelper::GAA_FLAG_SKIP_MULTICAST,
        NetworkManagement::IpHelper::GetAdaptersAddresses,
        NetworkManagement::IpHelper::GetIfEntry2,
        NetworkManagement::IpHelper::IF_TYPE_ETHERNET_CSMACD,
        NetworkManagement::IpHelper::IP_ADAPTER_ADDRESSES_LH,
        NetworkManagement::IpHelper::MIB_IF_ROW2,
        NetworkManagement::Ndis::NET_LUID_LH,
        Networking::WinSock::AF_INET,
        Networking::WinSock::SOCKADDR_IN,
        Security::{
            ACL,
            Authorization::{
                ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertSidToStringSidW,
                ConvertStringSecurityDescriptorToSecurityDescriptorW, GetNamedSecurityInfoW,
                GetSecurityInfo, SDDL_REVISION_1, SE_FILE_OBJECT, SE_KERNEL_OBJECT,
                SetNamedSecurityInfoW,
            },
            DACL_SECURITY_INFORMATION, EqualSid, GetSecurityDescriptorDacl, GetTokenInformation,
            OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
            PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
        },
        Storage::FileSystem::{
            CreateFileW, FILE_ACTION_ADDED, FILE_ACTION_MODIFIED, FILE_ACTION_REMOVED,
            FILE_ACTION_RENAMED_NEW_NAME, FILE_ACTION_RENAMED_OLD_NAME, FILE_ATTRIBUTE_DIRECTORY,
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY,
            FILE_NOTIFY_CHANGE_ATTRIBUTES, FILE_NOTIFY_CHANGE_DIR_NAME,
            FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE,
            FILE_NOTIFY_EXTENDED_INFORMATION, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
            GetDiskFreeSpaceExW, GetVolumePathNameW, OPEN_EXISTING, ReadDirectoryChangesExW,
            ReadDirectoryNotifyExtendedInformation, ReadFile, WriteFile,
        },
        System::{
            Console::{
                AttachConsole, CONSOLE_MODE, ENABLE_AUTO_POSITION, ENABLE_ECHO_INPUT,
                ENABLE_EXTENDED_FLAGS, ENABLE_INSERT_MODE, ENABLE_LINE_INPUT, ENABLE_MOUSE_INPUT,
                ENABLE_PROCESSED_INPUT, ENABLE_PROCESSED_OUTPUT, ENABLE_QUICK_EDIT_MODE,
                ENABLE_VIRTUAL_TERMINAL_PROCESSING, FreeConsole, GetConsoleMode,
                GetConsoleProcessList, SetConsoleCtrlHandler, SetConsoleMode, WriteConsoleW,
            },
            Diagnostics::ToolHelp::CreateToolhelp32Snapshot,
            Diagnostics::ToolHelp::PROCESSENTRY32W,
            Diagnostics::ToolHelp::Process32FirstW,
            Diagnostics::ToolHelp::Process32NextW,
            Diagnostics::ToolHelp::TH32CS_SNAPPROCESS,
            IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED},
            JobObjects::AssignProcessToJobObject,
            JobObjects::CreateJobObjectW,
            JobObjects::JOBOBJECT_BASIC_AND_IO_ACCOUNTING_INFORMATION,
            JobObjects::JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JobObjects::JobObjectBasicAndIoAccountingInformation,
            JobObjects::JobObjectExtendedLimitInformation,
            JobObjects::QueryInformationJobObject,
            JobObjects::TerminateJobObject,
            Pipes::WaitNamedPipeW,
            Power::GetSystemPowerStatus,
            Power::SYSTEM_POWER_STATUS,
            ProcessStatus::K32GetProcessMemoryInfo,
            ProcessStatus::PROCESS_MEMORY_COUNTERS,
            Registry::HKEY_LOCAL_MACHINE,
            Registry::RRF_RT_REG_DWORD,
            Registry::RRF_RT_REG_SZ,
            Registry::RegGetValueW,
            RemoteDesktop::WTS_CURRENT_SERVER_HANDLE,
            RemoteDesktop::WTS_CURRENT_SESSION,
            RemoteDesktop::WTS_SESSIONSTATE_LOCK,
            RemoteDesktop::WTS_SESSIONSTATE_UNLOCK,
            RemoteDesktop::WTSFreeMemory,
            RemoteDesktop::WTSINFOEXW,
            RemoteDesktop::WTSQuerySessionInformationW,
            RemoteDesktop::WTSSessionInfoEx,
            SystemInformation::{
                ComputerNameDnsHostname, GetComputerNameExW, GetTickCount64, GlobalMemoryStatusEx,
                MEMORYSTATUSEX,
            },
            Threading::{
                CREATE_NEW_CONSOLE, CreateEventW, CreateProcessW, GetCurrentProcess,
                GetCurrentThread, GetExitCodeProcess, GetProcessIoCounters, GetProcessTimes,
                GetSystemTimes, GetThreadTimes, INFINITE, IO_COUNTERS, OpenProcess,
                OpenProcessToken, PROCESS_BASIC_INFORMATION, PROCESS_INFORMATION,
                PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
                PROCESS_VM_READ, QueryFullProcessImageNameW, STARTUPINFOW, TerminateProcess,
                WaitForSingleObject,
            },
            WindowsProgramming::QueryUnbiasedInterruptTime,
        },
        UI::Shell::CommandLineToArgvW,
    },
    core::{BOOL, PWSTR},
};

/// The DNS host name, as configured.
pub fn hostname() -> Option<String> {
    let mut len: u32 = 0;
    // SAFETY: a null buffer asks for the length in UTF-16 units, terminator included
    unsafe { GetComputerNameExW(ComputerNameDnsHostname, ptr::null_mut(), &raw mut len) };
    let mut buf = vec![0u16; len as usize];
    // SAFETY: `buf` holds `len` units; on success `len` is the name's length without the terminator
    if unsafe { GetComputerNameExW(ComputerNameDnsHostname, buf.as_mut_ptr(), &raw mut len) } == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..len as usize]))
}

/// `s` as a NUL-terminated UTF-16 string.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// A NUL-terminated UTF-16 string at `p`.
///
/// # Safety
///
/// `p` points at a NUL-terminated UTF-16 string.
unsafe fn from_wide(p: *const u16) -> String {
    let mut len = 0;
    // SAFETY: the caller promises a terminator
    while unsafe { *p.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: `len` units were just read
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, len) })
}

fn millis(timeout: Option<Duration>) -> u32 {
    timeout.map_or(INFINITE, |t| {
        u32::try_from(t.as_millis()).map_or(INFINITE - 1, |ms| ms.clamp(1, INFINITE - 1))
    })
}

/// A manual-reset event for overlapped I/O.
pub struct Event(OwnedHandle);

impl Event {
    pub fn new() -> io::Result<Self> {
        // SAFETY: no attributes, no name; manual reset, not signalled
        let handle = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh handle nothing else owns
        Ok(Self(unsafe { OwnedHandle::from_raw_handle(handle) }))
    }
}

/// One read on `file` (opened with `FILE_FLAG_OVERLAPPED`), given up after
/// `timeout`.
pub fn read_overlapped(
    file: &impl AsRawHandle,
    event: &Event,
    buf: &mut [u8],
    timeout: Option<Duration>,
) -> io::Result<usize> {
    let handle = file.as_raw_handle();
    let len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
    let data = buf.as_mut_ptr();
    // SAFETY: `buf` outlives the operation: `overlapped` waits for it to end
    overlapped(handle, event, timeout, |ov| unsafe {
        ReadFile(handle, data, len, ptr::null_mut(), ov)
    })
}

/// One write on `file` (opened with `FILE_FLAG_OVERLAPPED`), given up
/// after `timeout`.
pub fn write_overlapped(
    file: &impl AsRawHandle,
    event: &Event,
    buf: &[u8],
    timeout: Option<Duration>,
) -> io::Result<usize> {
    let handle = file.as_raw_handle();
    let len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
    let data = buf.as_ptr();
    // SAFETY: `buf` outlives the operation: `overlapped` waits for it to end
    overlapped(handle, event, timeout, |ov| unsafe {
        WriteFile(handle, data, len, ptr::null_mut(), ov)
    })
}

/// Starts an operation with `start` and waits for it; when the wait times
/// out or fails it is cancelled, and still waited for, since it holds
/// `OVERLAPPED`: it may have completed meanwhile, and then counts.
fn overlapped(
    handle: HANDLE,
    event: &Event,
    timeout: Option<Duration>,
    start: impl FnOnce(*mut OVERLAPPED) -> BOOL,
) -> io::Result<usize> {
    // SAFETY: all-zero is a valid OVERLAPPED
    let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
    ov.hEvent = event.0.as_raw_handle();
    let mut cut_short = None;
    if start(&raw mut ov) == 0 {
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
            return Err(e);
        }
        // SAFETY: the event belongs to `event`, alive here
        match unsafe { WaitForSingleObject(ov.hEvent, millis(timeout)) } {
            WAIT_OBJECT_0 => {}
            WAIT_TIMEOUT => cut_short = Some(io::ErrorKind::TimedOut.into()),
            _ => cut_short = Some(io::Error::last_os_error()),
        }
        if cut_short.is_some() {
            // SAFETY: `ov` is the pending operation's; the wait below outlives it
            unsafe { CancelIoEx(handle, &raw const ov) };
        }
    }
    let mut done = 0;
    // SAFETY: the operation of `ov` has ended, or ends (cancelled) while this waits
    if unsafe { GetOverlappedResult(handle, &raw const ov, &raw mut done, 1) } == 0 {
        let e = io::Error::last_os_error();
        return Err(match cut_short {
            Some(why) if e.raw_os_error() == Some(ERROR_OPERATION_ABORTED as i32) => why,
            _ => e,
        });
    }
    Ok(done as usize)
}

/// Waits up to `timeout` for an instance of the pipe `name` to be free.
pub fn wait_named_pipe(name: &str, timeout: Duration) -> bool {
    let name = wide(name);
    // SAFETY: `name` is NUL-terminated
    unsafe { WaitNamedPipeW(name.as_ptr(), millis(Some(timeout))) != 0 }
}

/// Security attributes that let only the current user in, and make the
/// user the owner (an elevated token would make it Administrators).
pub struct OwnerOnly {
    /// Held: `attributes` points at it.
    _descriptor: Descriptor,
    attributes: SECURITY_ATTRIBUTES,
}

// SAFETY: the descriptor is heap memory only this value refers to and frees
unsafe impl Send for OwnerOnly {}

impl OwnerOnly {
    pub fn new() -> io::Result<Self> {
        let sid = user_sid()?;
        let descriptor = Descriptor::from_sddl(&format!("O:{sid}D:P(A;;GA;;;{sid})"))?;
        Ok(Self {
            attributes: SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor.0,
                bInheritHandle: 0,
            },
            _descriptor: descriptor,
        })
    }

    /// The `SECURITY_ATTRIBUTES`, valid while `self` is.
    pub fn as_ptr(&self) -> *mut c_void {
        (&raw const self.attributes).cast_mut().cast()
    }
}

/// Whether the current user owns the kernel object behind `handle` (it
/// needs `READ_CONTROL`).
pub fn owned_by_me(handle: &impl AsRawHandle) -> io::Result<bool> {
    let mut owner: PSID = ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: a live handle; `owner` points into `descriptor`, LocalFree'd below
    let status = unsafe {
        GetSecurityInfo(
            handle.as_raw_handle(),
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &raw mut owner,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &raw mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let same = token_user().map(|buf| {
        // SAFETY: token_user() wrote a TOKEN_USER at the start of `buf`
        let user = unsafe { &*buf.as_ptr().cast::<TOKEN_USER>() };
        // SAFETY: both SIDs are valid while `descriptor` and `buf` live
        unsafe { EqualSid(owner, user.User.Sid) != 0 }
    });
    // SAFETY: allocated by GetSecurityInfo
    unsafe { LocalFree(descriptor) };
    same
}

/// A security descriptor from SDDL, freed on drop.
struct Descriptor(PSECURITY_DESCRIPTOR);

impl Descriptor {
    fn from_sddl(sddl: &str) -> io::Result<Self> {
        let sddl = wide(sddl);
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: `sddl` is NUL-terminated; the descriptor is LocalFree'd on drop
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &raw mut descriptor,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(descriptor))
    }

    /// The DACL as SDDL, the form two DACLs compare in.
    fn dacl_text(&self) -> io::Result<String> {
        let mut text: PWSTR = ptr::null_mut();
        // SAFETY: a valid descriptor; `text` is LocalFree'd below
        let ok = unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                self.0,
                SDDL_REVISION_1,
                DACL_SECURITY_INFORMATION,
                &raw mut text,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a NUL-terminated string from the call above
        let sddl = unsafe { from_wide(text) };
        // SAFETY: allocated by ConvertSecurityDescriptorToStringSecurityDescriptorW
        unsafe { LocalFree(text.cast()) };
        Ok(sddl)
    }
}

impl Drop for Descriptor {
    fn drop(&mut self) {
        // SAFETY: allocated by the system, freed once
        unsafe { LocalFree(self.0) };
    }
}

/// The DACL of the file or directory `path` (NUL-terminated), as SDDL.
fn file_dacl_text(path: &[u16]) -> io::Result<String> {
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: `path` is NUL-terminated; the descriptor is freed by `Descriptor`
    let status = unsafe {
        GetNamedSecurityInfoW(
            path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &raw mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    Descriptor(descriptor).dacl_text()
}

/// Gives `path` a DACL of its own that lets only the current user in, as
/// mode 0600/0700 does; a directory's passes on to what is created in it
/// (and to what it holds). Left alone when it has exactly that already.
pub fn owner_only(path: &std::path::Path) -> io::Result<()> {
    let inherit = if path.is_dir() { "OICI" } else { "" };
    // NOTE: AI, as the system marks a DACL it set, so an equal one compares equal
    let wanted = Descriptor::from_sddl(&format!("D:PAI(A;{inherit};FA;;;{})", user_sid()?))?;
    let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    if file_dacl_text(&name)? == wanted.dacl_text()? {
        return Ok(());
    }
    let (mut present, mut defaulted) = (0, 0);
    let mut dacl: *mut ACL = ptr::null_mut();
    // SAFETY: `dacl` points into `wanted`, alive below
    if unsafe {
        GetSecurityDescriptorDacl(
            wanted.0,
            &raw mut present,
            &raw mut dacl,
            &raw mut defaulted,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `name` is NUL-terminated, `dacl` valid while `wanted` is
    let status = unsafe {
        SetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            dacl,
            ptr::null(),
        )
    };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    Ok(())
}

/// The current user's SID, as `S-1-5-…`.
fn user_sid() -> io::Result<String> {
    let buf = token_user()?;
    // SAFETY: token_user() wrote a TOKEN_USER at the start of `buf`
    let user = unsafe { &*buf.as_ptr().cast::<TOKEN_USER>() };
    let mut text: PWSTR = ptr::null_mut();
    // SAFETY: the SID lives in `buf`; `text` is LocalFree'd below
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &raw mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: ConvertSidToStringSidW returned a NUL-terminated string
    let sid = unsafe { from_wide(text) };
    // SAFETY: allocated by ConvertSidToStringSidW
    unsafe { LocalFree(text.cast()) };
    Ok(sid)
}

/// A process in the system's process list.
pub struct Process {
    pub pid: u32,
    pub ppid: u32,
    /// The executable's file name.
    pub exe: String,
}

/// Every process, from a Toolhelp snapshot.
pub fn processes() -> Vec<Process> {
    // SAFETY: plain call; the handle is owned below
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Vec::new();
    }
    // SAFETY: a fresh handle nothing else owns
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
    // SAFETY: all-zero is a valid PROCESSENTRY32W
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
    let mut out = Vec::new();
    // SAFETY: `entry` is a PROCESSENTRY32W with dwSize set
    let mut more = unsafe { Process32FirstW(snapshot.as_raw_handle(), &raw mut entry) } != 0;
    while more {
        let name = &entry.szExeFile;
        let len = name.iter().position(|c| *c == 0).unwrap_or(name.len());
        out.push(Process {
            pid: entry.th32ProcessID,
            ppid: entry.th32ParentProcessID,
            exe: String::from_utf16_lossy(&name[..len]),
        });
        // SAFETY: as above
        more = unsafe { Process32NextW(snapshot.as_raw_handle(), &raw mut entry) } != 0;
    }
    out
}

fn open_process(pid: u32, access: u32) -> Option<OwnedHandle> {
    // SAFETY: plain call; null on failure
    let handle = unsafe { OpenProcess(access, 0, pid) };
    // SAFETY: a fresh handle nothing else owns
    (!handle.is_null()).then(|| unsafe { OwnedHandle::from_raw_handle(handle) })
}

/// 100 ns intervals between 1601-01-01 and 1970-01-01.
const EPOCH_DIFF: u64 = 116_444_736_000_000_000;

fn filetime(t: FILETIME) -> u64 {
    (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)
}

/// When a process started and the CPU time it used.
pub struct Times {
    /// Milliseconds since the Unix epoch.
    pub started_ms: i64,
    /// Kernel plus user time, in 100 ns units.
    pub cpu_100ns: u64,
}

pub fn process_times(pid: u32) -> Option<Times> {
    let process = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: four FILETIMEs to fill, a live handle
    let ok = unsafe {
        GetProcessTimes(
            process.as_raw_handle(),
            &raw mut created,
            &raw mut exited,
            &raw mut kernel,
            &raw mut user,
        )
    };
    (ok != 0).then(|| Times {
        started_ms: (filetime(created).saturating_sub(EPOCH_DIFF) / 10_000) as i64,
        cpu_100ns: filetime(kernel) + filetime(user),
    })
}

/// The working set of a process, in bytes.
pub fn process_memory(pid: u32) -> Option<u64> {
    let process = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ)
        .or_else(|| open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION))?;
    // SAFETY: all-zero is a valid PROCESS_MEMORY_COUNTERS
    let mut counters: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    let size = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    // SAFETY: `counters` has `size` bytes, a live handle
    let ok = unsafe { K32GetProcessMemoryInfo(process.as_raw_handle(), &raw mut counters, size) };
    (ok != 0).then_some(counters.WorkingSetSize as u64)
}

/// The pid that started `pid` (it may be gone, its pid another
/// process's by now).
pub fn parent_of(pid: u32) -> Option<u32> {
    let process = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
    // SAFETY: all-zero is a valid PROCESS_BASIC_INFORMATION
    let mut info: PROCESS_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a live handle; `info` has the size given
    let status = unsafe {
        NtQueryInformationProcess(
            process.as_raw_handle(),
            ProcessBasicInformation,
            (&raw mut info).cast(),
            size_of::<PROCESS_BASIC_INFORMATION>() as u32,
            ptr::null_mut(),
        )
    };
    if status < 0 {
        return None;
    }
    u32::try_from(info.InheritedFromUniqueProcessId).ok()
}

/// The file name of the executable `pid` runs.
pub fn exe_name(pid: u32) -> Option<String> {
    let process = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
    let mut buf = vec![0u16; 32_768];
    let mut len = buf.len() as u32;
    // SAFETY: a live handle; `buf` holds `len` units
    if unsafe {
        QueryFullProcessImageNameW(process.as_raw_handle(), 0, buf.as_mut_ptr(), &raw mut len)
    } == 0
    {
        return None;
    }
    let path = String::from_utf16_lossy(&buf[..len as usize]);
    Some(path.rsplit(['\\', '/']).next().unwrap_or(&path).to_owned())
}

/// `ProcessCommandLineInformation`: the command line as a UNICODE_STRING.
const PROCESS_COMMAND_LINE_INFORMATION: PROCESSINFOCLASS = 60;

/// The command line a process was started with.
pub fn command_line(pid: u32) -> Option<String> {
    let process = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
    let mut len = 0u32;
    // SAFETY: a zero-length buffer asks for the size
    unsafe {
        NtQueryInformationProcess(
            process.as_raw_handle(),
            PROCESS_COMMAND_LINE_INFORMATION,
            ptr::null_mut(),
            0,
            &raw mut len,
        )
    };
    if len == 0 {
        return None;
    }
    // NOTE: u64s, so the UNICODE_STRING read from it is aligned
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    // SAFETY: `buf` holds at least `len` bytes
    let status = unsafe {
        NtQueryInformationProcess(
            process.as_raw_handle(),
            PROCESS_COMMAND_LINE_INFORMATION,
            buf.as_mut_ptr().cast(),
            len,
            &raw mut len,
        )
    };
    if status < 0 {
        return None;
    }
    // SAFETY: the call wrote a UNICODE_STRING at the start of `buf`, its text after it in `buf`
    let text = unsafe {
        let s = &*buf.as_ptr().cast::<UNICODE_STRING>();
        if s.Buffer.is_null() {
            return Some(String::new());
        }
        std::slice::from_raw_parts(s.Buffer, usize::from(s.Length) / 2)
    };
    Some(String::from_utf16_lossy(text))
}

/// A command line split into arguments, as programs' C runtimes split it.
pub fn split_command_line(line: &str) -> Vec<String> {
    if line.trim().is_empty() {
        return Vec::new();
    }
    let wide_line = wide(line);
    let mut count = 0;
    // SAFETY: `wide_line` is NUL-terminated; the result is LocalFree'd below
    let argv = unsafe { CommandLineToArgvW(wide_line.as_ptr(), &raw mut count) };
    if argv.is_null() {
        return vec![line.to_owned()];
    }
    let args = (0..usize::try_from(count).unwrap_or(0))
        // SAFETY: `argv` holds `count` NUL-terminated strings
        .map(|i| unsafe { from_wide(*argv.add(i)) })
        .collect();
    // SAFETY: allocated by CommandLineToArgvW
    unsafe { LocalFree(argv.cast()) };
    args
}

/// A process in a console of its own whose standard handles are that
/// console's: std's `Command` always hands a child handles of its own.
pub struct ConsoleChild {
    process: OwnedHandle,
    pub pid: u32,
}

impl ConsoleChild {
    /// Starts `program` ([`find_program`]; a batch file through `cmd.exe`)
    /// with `args` in a new console.
    pub fn spawn(program: &str, args: &[String]) -> io::Result<Self> {
        Self::spawn_with(program, args, CREATE_NEW_CONSOLE)
    }

    fn spawn_with(program: &str, args: &[String], flags: u32) -> io::Result<Self> {
        let mut exe = find_program(program).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("{program} is not on PATH"))
        })?;
        let line = if is_batch(&exe) {
            let line = format!("cmd.exe {}", batch_arguments(&exe, args)?);
            exe = command_processor();
            line
        } else {
            let mut line = quote_arg(program);
            for arg in args {
                line.push(' ');
                line.push_str(&quote_arg(arg));
            }
            line
        };
        let exe: Vec<u16> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut line = wide(&line);
        // SAFETY: all-zero is a valid STARTUPINFOW and PROCESS_INFORMATION
        let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
        startup.cb = size_of::<STARTUPINFOW>() as u32;
        // SAFETY: as above
        let mut info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        // NOTE: no STARTF_USESTDHANDLES and no inherited handles: the new console's are used
        // SAFETY: NUL-terminated strings, a mutable command line, structs to fill
        let ok = unsafe {
            CreateProcessW(
                exe.as_ptr(),
                line.as_mut_ptr(),
                ptr::null(),
                ptr::null(),
                0,
                flags,
                ptr::null(),
                ptr::null(),
                &raw const startup,
                &raw mut info,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fresh handles nothing else owns; the thread's is closed at once
        let process = unsafe {
            drop(OwnedHandle::from_raw_handle(info.hThread));
            OwnedHandle::from_raw_handle(info.hProcess)
        };
        Ok(Self {
            process,
            pid: info.dwProcessId,
        })
    }

    /// The exit code once the process has ended; none while it runs.
    pub fn try_wait(&self) -> io::Result<Option<u32>> {
        // SAFETY: a live process handle; a zero wait only looks
        match unsafe { WaitForSingleObject(self.process.as_raw_handle(), 0) } {
            WAIT_TIMEOUT => return Ok(None),
            WAIT_FAILED => return Err(io::Error::last_os_error()),
            _ => {}
        }
        let mut code = 0u32;
        // SAFETY: as above
        if unsafe { GetExitCodeProcess(self.process.as_raw_handle(), &raw mut code) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Some(code))
    }
}

/// What a program without an extension may be, in this order: npm and
/// other installers leave `.cmd` shims.
const PROGRAM_EXTENSIONS: &[&str] = &["exe", "com", "cmd", "bat"];

/// `program` itself when it is a path, else the first match on PATH; one of
/// `PROGRAM_EXTENSIONS` is added when it has no extension.
pub fn find_program(program: &str) -> Option<std::path::PathBuf> {
    find_in(program, &std::env::var_os("PATH").unwrap_or_default())
}

fn find_in(program: &str, path_var: &std::ffi::OsStr) -> Option<std::path::PathBuf> {
    let path = std::path::Path::new(program);
    let names: Vec<std::path::PathBuf> = if path.extension().is_some() {
        vec![path.to_path_buf()]
    } else {
        PROGRAM_EXTENSIONS
            .iter()
            .map(|ext| path.with_extension(ext))
            .collect()
    };
    // NOTE: an app execution alias (WindowsApps) cannot be followed, only seen
    let exists = |p: &std::path::Path| p.symlink_metadata().is_ok_and(|m| !m.is_dir());
    if program.contains(['/', '\\', ':']) {
        return names.into_iter().find(|p| exists(p));
    }
    std::env::split_paths(path_var)
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|p| exists(p))
}

fn is_batch(exe: &std::path::Path) -> bool {
    exe.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"))
}

/// `cmd.exe`'s arguments that run the batch file `exe` with `args`.
/// Arguments `cmd` would expand or split (`"`, `%`, line breaks) are refused.
fn batch_arguments(exe: &std::path::Path, args: &[String]) -> io::Result<String> {
    let mut inner = format!("\"{}\"", exe.display());
    for arg in args {
        if arg.contains(['"', '%', '\r', '\n']) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{arg:?} cannot be passed to a batch file"),
            ));
        }
        // NOTE: the program behind the shim splits as the C runtime does: trailing backslashes double
        let trailing = arg.len() - arg.trim_end_matches('\\').len();
        inner.push_str(" \"");
        inner.push_str(arg);
        inner.extend(std::iter::repeat_n('\\', trailing));
        inner.push('"');
    }
    // NOTE: /s: the outer quotes go, the rest runs as written; /v:OFF: `!` stays literal
    Ok(format!("/d /e:ON /v:OFF /s /c \"{inner}\""))
}

/// `cmd.exe`: `ComSpec`, else the system's.
fn command_processor() -> std::path::PathBuf {
    std::env::var_os("ComSpec").map_or_else(
        || {
            let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
            std::path::Path::new(&root).join(r"System32\cmd.exe")
        },
        Into::into,
    )
}

/// `arg` quoted for a command line as the C runtime splits it.
fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '\x0b', '"']) {
        return arg.to_owned();
    }
    let mut out = String::from('"');
    let mut backslashes = 0;
    for c in arg.chars() {
        if c == '\\' {
            backslashes += 1;
            continue;
        }
        // NOTE: backslashes double only before a quote
        let n = if c == '"' {
            backslashes * 2 + 1
        } else {
            backslashes
        };
        out.extend(std::iter::repeat_n('\\', n));
        backslashes = 0;
        out.push(c);
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

/// Leaves the console when it was made for this process alone (started
/// from the Run key or Explorer), so its window closes; one shared with a
/// shell stays. Output to it is dropped then.
pub fn leave_own_console() {
    let mut pids = [0u32; 2];
    // SAFETY: room for two pids, as given
    if unsafe { GetConsoleProcessList(pids.as_mut_ptr(), 2) } == 1 {
        // SAFETY: plain call
        unsafe { FreeConsole() };
    }
}

/// Whether a process with this id runs. One this user may not open
/// (elevated, another user's, protected) runs too, as `/proc/<pid>` exists
/// for another user's; a gone pid is `ERROR_INVALID_PARAMETER`.
pub fn pid_alive(pid: u32) -> bool {
    // SAFETY: plain call; null on failure
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        return io::Error::last_os_error().raw_os_error() == Some(ERROR_ACCESS_DENIED as i32);
    }
    // SAFETY: a fresh handle nothing else owns
    let process = unsafe { OwnedHandle::from_raw_handle(handle) };
    // SAFETY: a live handle with SYNCHRONIZE; a zero wait only looks
    unsafe { WaitForSingleObject(process.as_raw_handle(), 0) == WAIT_TIMEOUT }
}

/// Ends a process at once (`TerminateProcess`); it gets no say.
pub fn terminate(pid: u32) -> io::Result<()> {
    let process = open_process(pid, PROCESS_TERMINATE).ok_or_else(io::Error::last_os_error)?;
    // SAFETY: a live handle with PROCESS_TERMINATE
    if unsafe { TerminateProcess(process.as_raw_handle(), 1) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Waits up to `limit` for `pid` to be gone (`TerminateProcess` only
/// starts its end); whether it is.
pub fn wait_gone(pid: u32, limit: Duration) -> bool {
    let Some(process) = open_process(pid, PROCESS_SYNCHRONIZE) else {
        return !pid_alive(pid);
    };
    let ms = u32::try_from(limit.as_millis()).unwrap_or(INFINITE - 1);
    // SAFETY: a live handle with SYNCHRONIZE
    unsafe { WaitForSingleObject(process.as_raw_handle(), ms) == WAIT_OBJECT_0 }
}

/// What a full-screen program turns on in its terminal and off again as it
/// exits; ended at once, it leaves them on. Claude Code's: mouse tracking
/// (SGR pixels, SGR, any-event, button-event, normal), focus events,
/// bracketed paste, theme reports, synchronized output, a hidden cursor, a
/// kitty keyboard mode and the alternate screen. Leaving the alternate
/// screen restores the cursor saved for the main one; ESC 7 saves the
/// current one there first, for when there is no alternate screen to leave
/// (each screen saves its own). The kitty mode is popped on both screens,
/// each keeping its own stack; an empty stack pops to no flags.
const TERMINAL_RESET: &str = "\x1b[m\x1b[?1016l\x1b[?1006l\x1b[?1003l\x1b[?1002l\x1b[?1000l\
    \x1b[?1004l\x1b[?2004l\x1b[?2031l\x1b[?2026l\x1b[?25h\x1b7\x1b[<u\x1b[?1049l\x1b[<u";

/// The input mode of a new console: lines edited and echoed by the
/// console, Ctrl+C a signal.
const COOKED_INPUT: CONSOLE_MODE = ENABLE_PROCESSED_INPUT
    | ENABLE_LINE_INPUT
    | ENABLE_ECHO_INPUT
    | ENABLE_MOUSE_INPUT
    | ENABLE_INSERT_MODE
    | ENABLE_QUICK_EDIT_MODE
    | ENABLE_EXTENDED_FLAGS
    | ENABLE_AUTO_POSITION;

/// Another process's console, which this process is attached to for as
/// long as the value lives.
pub struct ForeignConsole(());

impl ForeignConsole {
    /// Attaches to the console `pid` runs in, leaving this process's own
    /// first.
    ///
    /// # Errors
    ///
    /// When `pid` has no console or this process may not attach to it.
    pub fn attach(pid: u32) -> io::Result<Self> {
        // SAFETY: plain calls
        unsafe { FreeConsole() };
        // SAFETY: as above
        if unsafe { AttachConsole(pid) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // NOTE: a Ctrl+C typed there now reaches this process too, and is not for it
        // SAFETY: no routine: this process ignores Ctrl+C
        unsafe { SetConsoleCtrlHandler(None, 1) };
        Ok(Self(()))
    }

    /// Undoes what a full-screen program ended at once left in the console:
    /// turns off its terminal modes ([`TERMINAL_RESET`]) and, when the input
    /// is still raw, gives it a new console's line input. A shell it ran
    /// from would read every mouse move as typed text otherwise, and pass the
    /// raw input on to the programs it starts (PowerShell does).
    pub fn restore(&self) -> io::Result<()> {
        let output = open_console("CONOUT$")?;
        let mut mode: CONSOLE_MODE = 0;
        // SAFETY: a live console handle
        if unsafe { GetConsoleMode(output.as_raw_handle(), &raw mut mode) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let vt = mode | ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING;
        // SAFETY: as above
        unsafe { SetConsoleMode(output.as_raw_handle(), vt) };
        let text: Vec<u16> = TERMINAL_RESET.encode_utf16().collect();
        let mut written = 0;
        // SAFETY: `text` holds that many units; no reserved argument
        let wrote = unsafe {
            WriteConsoleW(
                output.as_raw_handle(),
                text.as_ptr(),
                text.len() as u32,
                &raw mut written,
                ptr::null(),
            )
        };
        let error = io::Error::last_os_error();
        // SAFETY: as above
        unsafe { SetConsoleMode(output.as_raw_handle(), mode) };
        if wrote == 0 {
            return Err(error);
        }
        // NOTE: the input after the modes: ConPTY passes a mouse mode on to the
        // terminal only while the input is VT, and the terminal is what sends mouse moves
        let input = open_console("CONIN$")?;
        let mut mode: CONSOLE_MODE = 0;
        // SAFETY: a live console handle
        let known = unsafe { GetConsoleMode(input.as_raw_handle(), &raw mut mode) } != 0;
        // SAFETY: as above
        if known
            && mode & ENABLE_LINE_INPUT == 0
            && unsafe { SetConsoleMode(input.as_raw_handle(), COOKED_INPUT) } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for ForeignConsole {
    fn drop(&mut self) {
        // SAFETY: plain call
        unsafe { FreeConsole() };
    }
}

/// The input or the screen (`CONIN$`, `CONOUT$`) of this process's console,
/// whatever its standard handles are.
fn open_console(name: &str) -> io::Result<OwnedHandle> {
    let name = wide(name);
    // SAFETY: a NUL-terminated name; no security attributes, no template
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh handle nothing else owns
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

/// Milliseconds since boot, sleep included.
pub fn uptime_ms() -> i64 {
    // SAFETY: plain call
    i64::try_from(unsafe { GetTickCount64() }).unwrap_or(i64::MAX)
}

/// Milliseconds asleep since boot: the tick count goes on in sleep, the
/// unbiased interrupt time stops.
pub fn slept_ms() -> i64 {
    let mut unbiased = 0u64;
    // SAFETY: writes one u64
    unsafe { QueryUnbiasedInterruptTime(&raw mut unbiased) };
    (uptime_ms() - i64::try_from(unbiased / 10_000).unwrap_or(0)).max(0)
}

/// Busy and total CPU time of all cores, in 100 ns units.
pub fn cpu_times() -> Option<(u64, u64)> {
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut idle, mut kernel, mut user) = (zero, zero, zero);
    // SAFETY: three FILETIMEs to fill
    if unsafe { GetSystemTimes(&raw mut idle, &raw mut kernel, &raw mut user) } == 0 {
        return None;
    }
    // NOTE: kernel time includes idle time
    let total = filetime(kernel) + filetime(user);
    Some((total.saturating_sub(filetime(idle)), total))
}

/// Physical memory and the commit limit (memory plus page files), bytes.
pub struct MemoryStatus {
    pub total: u64,
    pub avail: u64,
    pub commit_limit: u64,
    pub commit_avail: u64,
}

pub fn memory_status() -> Option<MemoryStatus> {
    // SAFETY: all-zero is a valid MEMORYSTATUSEX
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: dwLength set
    (unsafe { GlobalMemoryStatusEx(&raw mut status) } != 0).then_some(MemoryStatus {
        total: status.ullTotalPhys,
        avail: status.ullAvailPhys,
        commit_limit: status.ullTotalPageFile,
        commit_avail: status.ullAvailPageFile,
    })
}

/// A volume's size, bytes.
pub struct Space {
    pub total: u64,
    pub free: u64,
    /// What this user may still write (quotas).
    pub avail: u64,
}

pub fn disk_space(path: &std::path::Path) -> Option<Space> {
    let path = wide(&path.to_string_lossy());
    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    // SAFETY: NUL-terminated path, three u64s to fill
    let ok = unsafe {
        GetDiskFreeSpaceExW(path.as_ptr(), &raw mut avail, &raw mut total, &raw mut free)
    };
    (ok != 0).then_some(Space { total, free, avail })
}

/// The root of the volume `path` lies on (`C:\`, a mount folder).
pub fn volume_of(path: &std::path::Path) -> Option<String> {
    let path = wide(&path.to_string_lossy());
    let mut buf = [0u16; 1024];
    // SAFETY: NUL-terminated path; the buffer's length in units
    let ok = unsafe { GetVolumePathNameW(path.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
    let len = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    (ok != 0).then(|| String::from_utf16_lossy(&buf[..len]))
}

/// The battery and the mains, as `GetSystemPowerStatus` sees them.
pub struct PowerStatus {
    pub ac: Option<bool>,
    /// None without a system battery.
    pub battery_pct: Option<u8>,
    pub charging: bool,
}

pub fn power_status() -> Option<PowerStatus> {
    // SAFETY: all-zero is a valid SYSTEM_POWER_STATUS
    let mut status: SYSTEM_POWER_STATUS = unsafe { std::mem::zeroed() };
    // SAFETY: one struct to fill
    if unsafe { GetSystemPowerStatus(&raw mut status) } == 0 {
        return None;
    }
    // NOTE: flag 128 is "no system battery", 255 "unknown"
    let battery = status.BatteryFlag & 128 == 0
        && status.BatteryFlag != 255
        && status.BatteryLifePercent <= 100;
    Some(PowerStatus {
        ac: match status.ACLineStatus {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        },
        battery_pct: battery.then_some(status.BatteryLifePercent),
        charging: status.BatteryFlag & 8 != 0,
    })
}

/// A network adapter with its IPv4 addresses.
pub struct Adapter {
    /// The name Windows shows (`Ethernet`).
    pub name: String,
    /// The device's name (`Intel(R) Ethernet Controller I225-V`).
    pub description: String,
    pub mac: Option<[u8; 6]>,
    /// A physical Ethernet card with a connector, not a virtual switch.
    pub wired: bool,
    pub ipv4: Vec<(std::net::Ipv4Addr, u8)>,
}

pub fn adapters() -> io::Result<Vec<Adapter>> {
    let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
    let mut size = 16 * 1024u32;
    let mut buf: Vec<u64>;
    loop {
        // NOTE: u64s, so the IP_ADAPTER_ADDRESSES_LH read from it are aligned
        buf = vec![0u64; (size as usize).div_ceil(8)];
        // SAFETY: `buf` holds `size` bytes
        let rc = unsafe {
            GetAdaptersAddresses(
                u32::from(AF_INET),
                flags,
                ptr::null(),
                buf.as_mut_ptr().cast(),
                &raw mut size,
            )
        };
        match rc {
            0 => break,
            ERROR_BUFFER_OVERFLOW => {}
            rc => return Err(io::Error::from_raw_os_error(rc as i32)),
        }
    }
    let mut out = Vec::new();
    let mut cur = buf.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
    while !cur.is_null() {
        // SAFETY: a node of the list GetAdaptersAddresses wrote into `buf`, alive here
        let a = unsafe { &*cur };
        cur = a.Next;
        let mac = (a.PhysicalAddressLength == 6).then(|| {
            let mut mac = [0u8; 6];
            mac.copy_from_slice(&a.PhysicalAddress[..6]);
            mac
        });
        let mut ipv4 = Vec::new();
        let mut unicast = a.FirstUnicastAddress;
        while !unicast.is_null() {
            // SAFETY: a node of the adapter's address list, in `buf`
            let u = unsafe { &*unicast };
            unicast = u.Next;
            let sockaddr = u.Address.lpSockaddr;
            // SAFETY: an AF_INET sockaddr is a SOCKADDR_IN
            if !sockaddr.is_null() && unsafe { (*sockaddr).sa_family } == AF_INET {
                let sin = unsafe { &*sockaddr.cast::<SOCKADDR_IN>() };
                // SAFETY: S_addr is the union's u32 view, network order
                let raw = unsafe { sin.sin_addr.S_un.S_addr };
                ipv4.push((
                    std::net::Ipv4Addr::from(u32::from_be(raw)),
                    u.OnLinkPrefixLength,
                ));
            }
        }
        out.push(Adapter {
            // SAFETY: NUL-terminated strings in `buf`
            name: unsafe { from_wide(a.FriendlyName) },
            description: unsafe { from_wide(a.Description) },
            mac,
            wired: a.IfType == IF_TYPE_ETHERNET_CSMACD && hardware_with_connector(a.Luid),
            ipv4,
        });
    }
    Ok(out)
}

/// Whether the interface is hardware with a connector (bits 0 and 2 of
/// `InterfaceAndOperStatusFlags`): virtual switches and VPNs are not.
fn hardware_with_connector(luid: NET_LUID_LH) -> bool {
    // SAFETY: all-zero is a valid MIB_IF_ROW2
    let mut row: MIB_IF_ROW2 = unsafe { std::mem::zeroed() };
    row.InterfaceLuid = luid;
    // SAFETY: the row names its interface by LUID
    if unsafe { GetIfEntry2(&raw mut row) } != 0 {
        return false;
    }
    let bits = row.InterfaceAndOperStatusFlags._bitfield;
    bits & 0b001 != 0 && bits & 0b100 != 0
}

/// This process's Remote Desktop session (the console's, usually) and
/// whether it is locked.
pub fn session() -> Option<(u32, Option<bool>)> {
    let mut info: PWSTR = ptr::null_mut();
    let mut bytes = 0u32;
    // SAFETY: `info` receives a buffer freed with WTSFreeMemory below
    let ok = unsafe {
        WTSQuerySessionInformationW(
            WTS_CURRENT_SERVER_HANDLE,
            WTS_CURRENT_SESSION,
            WTSSessionInfoEx,
            &raw mut info,
            &raw mut bytes,
        )
    };
    if ok == 0 || info.is_null() {
        return None;
    }
    // SAFETY: WTSSessionInfoEx answers a WTSINFOEXW
    let ex = unsafe { &*info.cast::<WTSINFOEXW>() };
    let out = (ex.Level == 1).then(|| {
        // SAFETY: level 1 is the union's only member
        let level1 = unsafe { &ex.Data.WTSInfoExLevel1 };
        let locked = match u32::try_from(level1.SessionFlags) {
            Ok(WTS_SESSIONSTATE_LOCK) => Some(true),
            Ok(WTS_SESSIONSTATE_UNLOCK) => Some(false),
            _ => None,
        };
        (level1.SessionId, locked)
    });
    // SAFETY: allocated by WTSQuerySessionInformationW
    unsafe { WTSFreeMemory(info.cast()) };
    out
}

fn registry_string(key: &str, value: &str) -> Option<String> {
    let (key, value) = (wide(key), wide(value));
    let mut buf = [0u16; 256];
    let mut len = (buf.len() * 2) as u32;
    // SAFETY: NUL-terminated names; `len` is the buffer's size in bytes
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &raw mut len,
        )
    };
    let units = (len as usize / 2).min(buf.len());
    let text = &buf[..units];
    let end = text.iter().position(|c| *c == 0).unwrap_or(units);
    (rc == 0).then(|| String::from_utf16_lossy(&text[..end]))
}

fn registry_dword(key: &str, value: &str) -> Option<u32> {
    let (key, value) = (wide(key), wide(value));
    let mut data = 0u32;
    let mut len = 4u32;
    // SAFETY: NUL-terminated names; four bytes to fill
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            ptr::null_mut(),
            (&raw mut data).cast(),
            &raw mut len,
        )
    };
    (rc == 0).then_some(data)
}

const CURRENT_VERSION: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";

/// `Windows 11 Pro 25H2` and `10.0.26200.7171`, from the registry.
pub fn os_version() -> (Option<String>, String) {
    let build = registry_string(CURRENT_VERSION, "CurrentBuild").unwrap_or_default();
    let ubr = registry_dword(CURRENT_VERSION, "UBR");
    let kernel = match ubr {
        Some(ubr) => format!("10.0.{build}.{ubr}"),
        None => format!("10.0.{build}"),
    };
    // NOTE: ProductName still says "Windows 10" on Windows 11 (builds from 22000)
    let eleven = build.parse::<u32>().is_ok_and(|b| b >= 22_000);
    let pretty = registry_string(CURRENT_VERSION, "ProductName").map(|name| {
        let name = if eleven {
            name.replacen("Windows 10", "Windows 11", 1)
        } else {
            name
        };
        match registry_string(CURRENT_VERSION, "DisplayVersion") {
            Some(version) => format!("{name} {version}"),
            None => name,
        }
    });
    (pretty, kernel)
}

/// What happened to a path under a [`DirWatch`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Added,
    Removed,
    Modified,
    /// Renamed away; a `RenamedTo` with the new name follows.
    RenamedFrom,
    RenamedTo,
}

/// One change, its path relative to the watched root with `/` separators.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEvent {
    pub change: Change,
    pub path: String,
    pub is_dir: bool,
}

/// A watch on a directory and everything under it
/// (`ReadDirectoryChangesExW`, extended information; local disks only, a
/// network share refuses it). A read is outstanding from `open` on: the
/// system records changes only once one was issued.
pub struct DirWatch {
    dir: std::fs::File,
    /// Signalled by the outstanding read; held so its handle stays open.
    _event: Event,
    /// The outstanding read's; boxed, as the system holds its address.
    overlapped: Box<OVERLAPPED>,
    /// The outstanding read's buffer; never resized while it is pending.
    buf: Vec<u64>,
    /// A read is outstanding: `drop` waits only for one.
    pending: bool,
}

// SAFETY: the OVERLAPPED and the buffer belong to this value alone; the
// system writes them only for the outstanding read, which `drop` ends
unsafe impl Send for DirWatch {}

/// Bytes of change records one read can hold: a build in a watched tree
/// writes thousands of files between two reads.
const WATCH_BUFFER: usize = 1 << 20;

impl DirWatch {
    pub fn open(root: &std::path::Path) -> io::Result<Self> {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = std::fs::OpenOptions::new()
            .access_mode(FILE_LIST_DIRECTORY)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED)
            .open(root)?;
        let event = Event::new()?;
        // SAFETY: all-zero is a valid OVERLAPPED
        let mut overlapped: Box<OVERLAPPED> = Box::new(unsafe { std::mem::zeroed() });
        overlapped.hEvent = event.0.as_raw_handle();
        let mut watch = Self {
            dir,
            _event: event,
            overlapped,
            buf: vec![0; WATCH_BUFFER / 8],
            pending: false,
        };
        watch.arm().map_err(|e| {
            if e.raw_os_error() == Some(ERROR_INVALID_FUNCTION as i32) {
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("{} cannot be watched (a network share?)", root.display()),
                )
            } else {
                e
            }
        })?;
        Ok(watch)
    }

    /// Issues the next read.
    fn arm(&mut self) -> io::Result<()> {
        let filter = FILE_NOTIFY_CHANGE_FILE_NAME
            | FILE_NOTIFY_CHANGE_DIR_NAME
            | FILE_NOTIFY_CHANGE_ATTRIBUTES
            | FILE_NOTIFY_CHANGE_SIZE
            | FILE_NOTIFY_CHANGE_LAST_WRITE;
        // SAFETY: the buffer and the OVERLAPPED outlive the read: `read` waits
        // for it before touching them, `drop` cancels and waits
        let ok = unsafe {
            ReadDirectoryChangesExW(
                self.dir.as_raw_handle(),
                self.buf.as_mut_ptr().cast(),
                (self.buf.len() * 8) as u32,
                1,
                filter,
                ptr::null_mut(),
                &raw mut *self.overlapped,
                None,
                ReadDirectoryNotifyExtendedInformation,
            )
        };
        if ok == 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
                return Err(e);
            }
        }
        self.pending = true;
        Ok(())
    }

    /// Blocks until something under the root changes; None when changes
    /// were lost (the buffer overflowed).
    pub fn read(&mut self) -> io::Result<Option<Vec<DirEvent>>> {
        if !self.pending {
            self.arm()?;
        }
        let mut bytes = 0u32;
        // SAFETY: the outstanding read's OVERLAPPED; waits for it to end
        let ok = unsafe {
            GetOverlappedResult(
                self.dir.as_raw_handle(),
                &raw const *self.overlapped,
                &raw mut bytes,
                1,
            )
        };
        self.pending = false;
        if ok == 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(ERROR_NOTIFY_ENUM_DIR as i32) {
                return Err(e);
            }
            bytes = 0;
        }
        let events = (bytes > 0).then(|| self.parse(bytes as usize));
        self.arm()?;
        Ok(events)
    }

    /// The records of a finished read, `bytes` long.
    fn parse(&self, bytes: usize) -> Vec<DirEvent> {
        let mut events = Vec::new();
        let base = self.buf.as_ptr().cast::<u8>();
        let mut offset = 0usize;
        while offset < bytes {
            // SAFETY: the system wrote the records within `bytes`, the name after
            // each header; records are only DWORD-aligned, so the header (with
            // 8-byte fields) is copied out unaligned, inside the buffer
            let (next, action, attributes, name) = unsafe {
                let record = base.add(offset);
                let info = ptr::read_unaligned(record.cast::<FILE_NOTIFY_EXTENDED_INFORMATION>());
                let at = record
                    .add(std::mem::offset_of!(
                        FILE_NOTIFY_EXTENDED_INFORMATION,
                        FileName
                    ))
                    .cast::<u16>();
                let name = std::slice::from_raw_parts(at, info.FileNameLength as usize / 2);
                (info.NextEntryOffset, info.Action, info.FileAttributes, name)
            };
            let change = match action {
                FILE_ACTION_ADDED => Some(Change::Added),
                FILE_ACTION_REMOVED => Some(Change::Removed),
                FILE_ACTION_MODIFIED => Some(Change::Modified),
                FILE_ACTION_RENAMED_OLD_NAME => Some(Change::RenamedFrom),
                FILE_ACTION_RENAMED_NEW_NAME => Some(Change::RenamedTo),
                _ => None,
            };
            if let Some(change) = change {
                events.push(DirEvent {
                    change,
                    path: String::from_utf16_lossy(name).replace('\\', "/"),
                    is_dir: attributes & FILE_ATTRIBUTE_DIRECTORY != 0,
                });
            }
            if next == 0 {
                break;
            }
            offset += next as usize;
        }
        events
    }
}

impl Drop for DirWatch {
    fn drop(&mut self) {
        if !self.pending {
            return;
        }
        let handle = self.dir.as_raw_handle();
        let mut bytes = 0u32;
        // SAFETY: ends the outstanding read before its buffer and OVERLAPPED go
        unsafe {
            CancelIoEx(handle, &raw const *self.overlapped);
            GetOverlappedResult(handle, &raw const *self.overlapped, &raw mut bytes, 1);
        }
    }
}

/// CPU time of the calling thread (kernel + user), in ms.
pub fn thread_cpu_ms() -> u64 {
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: the pseudo handle of this thread; four FILETIMEs to fill
    let ok = unsafe {
        GetThreadTimes(
            GetCurrentThread(),
            &raw mut created,
            &raw mut exited,
            &raw mut kernel,
            &raw mut user,
        )
    };
    if ok == 0 {
        return 0;
    }
    (filetime(kernel) + filetime(user)) / 10_000
}

/// A job object: a process and everything it starts, ended and counted
/// together (`git` is `cmd\git.exe` starting `mingw64\bin\git.exe`, npm's
/// `claude` is `cmd.exe` starting `node.exe`).
pub struct Job(OwnedHandle);

impl Job {
    /// Puts `process` (started with `CREATE_SUSPENDED`) in a new job and
    /// lets it run, also when that fails: started suspended, it has started
    /// nothing outside the job.
    pub fn adopt(process: std::os::windows::io::RawHandle) -> io::Result<Self> {
        let job = Self::new().and_then(|job| {
            // SAFETY: two live handles
            if unsafe { AssignProcessToJobObject(job.0.as_raw_handle(), process) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(job)
        });
        // SAFETY: a live process handle with PROCESS_SUSPEND_RESUME (its creator's)
        let status = unsafe { NtResumeProcess(process) };
        let job = job?;
        if status < 0 {
            return Err(io::Error::other(format!("resuming: NTSTATUS {status:#x}")));
        }
        Ok(job)
    }

    fn new() -> io::Result<Self> {
        // SAFETY: no attributes, no name
        let handle = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh handle nothing else owns
        Ok(Self(unsafe { OwnedHandle::from_raw_handle(handle) }))
    }

    /// Ends every process in the job at once.
    pub fn terminate(&self) {
        // SAFETY: a live job handle
        unsafe { TerminateJobObject(self.0.as_raw_handle(), 1) };
    }

    /// What every process that ran in the job used, the ended ones too;
    /// the peak is the largest commit of any one process.
    pub fn usage(&self) -> Used {
        // SAFETY: all-zero is a valid value of both structs
        let mut accounting: JOBOBJECT_BASIC_AND_IO_ACCOUNTING_INFORMATION =
            unsafe { std::mem::zeroed() };
        // SAFETY: as above
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a live job handle; each struct with its own size
        let (counted, measured) = unsafe {
            (
                QueryInformationJobObject(
                    self.0.as_raw_handle(),
                    JobObjectBasicAndIoAccountingInformation,
                    (&raw mut accounting).cast(),
                    size_of::<JOBOBJECT_BASIC_AND_IO_ACCOUNTING_INFORMATION>() as u32,
                    ptr::null_mut(),
                ) != 0,
                QueryInformationJobObject(
                    self.0.as_raw_handle(),
                    JobObjectExtendedLimitInformation,
                    (&raw mut limits).cast(),
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                    ptr::null_mut(),
                ) != 0,
            )
        };
        let basic = &accounting.BasicInfo;
        let cpu_100ns = basic.TotalUserTime.max(0) + basic.TotalKernelTime.max(0);
        Used {
            cpu_ms: if counted {
                cpu_100ns as u64 / 10_000
            } else {
                0
            },
            read_bytes: if counted {
                accounting.IoInfo.ReadTransferCount
            } else {
                0
            },
            write_bytes: if counted {
                accounting.IoInfo.WriteTransferCount
            } else {
                0
            },
            peak_kb: if measured {
                limits.PeakProcessMemoryUsed as u64 / 1024
            } else {
                0
            },
        }
    }
}

#[link(name = "ntdll")]
unsafe extern "system" {
    /// Resumes every thread of a process: the one of a process started
    /// suspended, without looking for it.
    fn NtResumeProcess(process: HANDLE) -> i32;
}

/// What a process used, read from its handle; it may have ended.
pub struct Used {
    pub cpu_ms: u64,
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub peak_kb: u64,
}

pub fn usage_of(process: &impl AsRawHandle) -> Used {
    let handle = process.as_raw_handle();
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: a live process handle; four FILETIMEs to fill
    let timed = unsafe {
        GetProcessTimes(
            handle,
            &raw mut created,
            &raw mut exited,
            &raw mut kernel,
            &raw mut user,
        )
    } != 0;
    // SAFETY: all-zero is a valid IO_COUNTERS
    let mut io: IO_COUNTERS = unsafe { std::mem::zeroed() };
    // SAFETY: a live process handle; one struct to fill
    let counted = unsafe { GetProcessIoCounters(handle, &raw mut io) } != 0;
    // SAFETY: all-zero is a valid PROCESS_MEMORY_COUNTERS
    let mut memory: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    let size = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    // SAFETY: `memory` has `size` bytes, a live process handle
    let measured = unsafe { K32GetProcessMemoryInfo(handle, &raw mut memory, size) } != 0;
    Used {
        cpu_ms: if timed {
            (filetime(kernel) + filetime(user)) / 10_000
        } else {
            0
        },
        read_bytes: if counted { io.ReadTransferCount } else { 0 },
        write_bytes: if counted { io.WriteTransferCount } else { 0 },
        peak_kb: if measured {
            memory.PeakWorkingSetSize as u64 / 1024
        } else {
            0
        },
    }
}

/// The current process's `TOKEN_USER`, at the start of the buffer.
fn token_user() -> io::Result<Vec<u64>> {
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: the pseudo handle needs no closing; `token` receives a real one
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh handle nothing else owns
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut len = 0u32;
    // SAFETY: a null buffer asks for the size
    unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            ptr::null_mut(),
            0,
            &raw mut len,
        );
    }
    // NOTE: u64s, so the TOKEN_USER read from it is aligned
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    // SAFETY: `buf` holds at least `len` bytes
    let ok = unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buf.as_mut_ptr().cast(),
            len,
            &raw mut len,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn processes_this_user_may_not_open_still_run() {
        // NOTE: the System process (4) refuses an unelevated user's SYNCHRONIZE
        assert!(pid_alive(4));
        assert!(!pid_alive(0), "the idle process is not one");
    }

    /// Set for the copy of this test binary that runs in a console of its own.
    const CONSOLE_CHILD: &str = "CCDASH_TEST_CONSOLE_CHILD";

    /// A console left as Claude Code leaves it when ended at once (raw VT
    /// input, mouse tracking, the alternate screen) is restored by a process
    /// that attaches to it. This binary runs again in a console of its own,
    /// hidden, as the test's console must not be the one it breaks.
    #[test]
    fn a_console_left_raw_and_tracking_the_mouse_is_restored() {
        if std::env::var_os(CONSOLE_CHILD).is_some() {
            return restore_own_console();
        }
        use std::os::windows::process::CommandExt;
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "win::tests::a_console_left_raw_and_tracking_the_mouse_is_restored",
                "--nocapture",
            ])
            .env(CONSOLE_CHILD, "1")
            .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW)
            .output()
            .unwrap();
        let said =
            String::from_utf8_lossy(&output.stdout) + String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{said}");
        assert!(said.contains("1 passed"), "{said}");
    }

    /// A process tree ended with the value, a failed assertion's unwinding too.
    struct Holder(std::process::Child);

    impl Drop for Holder {
        fn drop(&mut self) {
            crate::procs::end_group(self.0.id(), true);
            let _ = self.0.wait();
        }
    }

    /// The processes attached to this process's console.
    fn console_processes() -> Vec<u32> {
        let mut pids = [0u32; 16];
        // SAFETY: room for 16 pids, as given
        let count = unsafe { GetConsoleProcessList(pids.as_mut_ptr(), 16) } as usize;
        pids[..count.min(16)].to_vec()
    }

    fn restore_own_console() {
        // NOTE: a process to attach to, sharing this console and outliving its leaving
        let holder = Holder(
            std::process::Command::new("cmd")
                .args(["/c", "ping -n 30 127.0.0.1 >NUL"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        // NOTE: a new process joins the console as it starts up; leaving first would end it
        let joined = (0..250).any(|_| {
            std::thread::sleep(Duration::from_millis(20));
            console_processes().contains(&holder.0.id())
        });
        assert!(joined, "the holder shares this console");
        let (input, output) = (
            open_console("CONIN$").unwrap(),
            open_console("CONOUT$").unwrap(),
        );
        // SAFETY: live console handles
        unsafe {
            SetConsoleMode(
                output.as_raw_handle(),
                ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING,
            );
            SetConsoleMode(input.as_raw_handle(), 0x208);
        }
        write_console(
            &output,
            "\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h\x1b[?1049h",
        );
        // NOTE: one tracking mode at a time: 1003, set last, stands for 1000 and 1002 too
        let modes = |input: &OwnedHandle, output: &OwnedHandle| {
            [1003, 1006, 1049].map(|mode| dec_mode(input, output, mode))
        };
        assert_eq!(
            modes(&input, &output),
            [Some(1); 3],
            "set (DECRQM: 1 set, 2 reset)"
        );
        drop((input, output));
        let console =
            ForeignConsole::attach(holder.0.id()).expect("attached to the holder's console");
        console.restore().unwrap();
        let (input, output) = (
            open_console("CONIN$").unwrap(),
            open_console("CONOUT$").unwrap(),
        );
        let mut mode = 0;
        // SAFETY: a live console handle
        unsafe { GetConsoleMode(input.as_raw_handle(), &raw mut mode) };
        assert_eq!(mode, COOKED_INPUT, "{mode:#x}");
        // SAFETY: as above
        unsafe {
            SetConsoleMode(
                output.as_raw_handle(),
                ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING,
            )
        };
        assert_eq!(modes(&input, &output), [Some(2); 3], "reset");
        drop(console);
    }

    fn write_console(output: &OwnedHandle, text: &str) {
        let text: Vec<u16> = text.encode_utf16().collect();
        let mut written = 0;
        // SAFETY: `text` holds that many units
        let ok = unsafe {
            WriteConsoleW(
                output.as_raw_handle(),
                text.as_ptr(),
                text.len() as u32,
                &raw mut written,
                ptr::null(),
            )
        };
        assert_ne!(ok, 0, "{}", io::Error::last_os_error());
    }

    /// DEC private mode `mode` as the console answers DECRQM: 1 set, 2 reset.
    fn dec_mode(input: &OwnedHandle, output: &OwnedHandle, mode: u32) -> Option<u32> {
        use windows_sys::Win32::System::Console::{INPUT_RECORD, KEY_EVENT, ReadConsoleInputW};
        write_console(output, &format!("\x1b[?{mode}$p"));
        let mut reply = String::new();
        while !reply.ends_with("$y") {
            // SAFETY: a live console handle
            if unsafe { WaitForSingleObject(input.as_raw_handle(), 2000) } != WAIT_OBJECT_0 {
                return None;
            }
            // SAFETY: all-zero is a valid INPUT_RECORD
            let mut record: INPUT_RECORD = unsafe { std::mem::zeroed() };
            let mut read = 0;
            // SAFETY: room for one record
            unsafe { ReadConsoleInputW(input.as_raw_handle(), &raw mut record, 1, &raw mut read) };
            // SAFETY: the union member EventType names
            if read == 1
                && u32::from(record.EventType) == KEY_EVENT
                && unsafe { record.Event.KeyEvent.bKeyDown } != 0
            {
                // SAFETY: as above
                reply.extend(char::from_u32(u32::from(unsafe {
                    record.Event.KeyEvent.uChar.UnicodeChar
                })));
            }
        }
        reply
            .strip_prefix(&format!("\x1b[?{mode};"))?
            .strip_suffix("$y")?
            .parse()
            .ok()
    }

    fn ended(child: &ConsoleChild) -> u32 {
        loop {
            if let Some(code) = child.try_wait().unwrap() {
                return code;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn quotes_arguments_as_they_split() {
        let args = [
            "plain",
            "with space",
            r#"quo"te"#,
            r"trail\",
            r#"back\"q"#,
            "",
            r"C:\a b\",
        ]
        .map(String::from);
        let line = std::iter::once("prog".to_owned())
            .chain(args.iter().map(|a| quote_arg(a)))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(split_command_line(&line)[1..], args);
    }

    #[test]
    fn programs_are_found_as_exe_or_batch_and_batch_files_run() {
        let dir = std::env::temp_dir().join(format!("ccdash-prog-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let shim = dir.join("ccdash-shim.cmd");
        std::fs::write(&shim, "@exit /b %~1\r\n").unwrap();
        let path_var = std::env::join_paths([dir.clone()]).unwrap();
        assert_eq!(find_in("ccdash-shim", &path_var), Some(shim.clone()));
        assert_eq!(find_in("ccdash-none", &path_var), None);
        assert!(find_program("cmd").is_some_and(|p| p.ends_with("cmd.exe")));
        let child = ConsoleChild::spawn_with(
            shim.to_str().unwrap(),
            &["7".into()],
            windows_sys::Win32::System::Threading::CREATE_NO_WINDOW,
        )
        .unwrap();
        assert_eq!(ended(&child), 7);
        // NOTE: std's Command runs a batch file it is given the path of
        let status = {
            use std::os::windows::process::CommandExt;
            std::process::Command::new(&shim)
                .arg("5")
                .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW)
                .status()
                .unwrap()
        };
        assert_eq!(status.code(), Some(5));
        assert!(batch_arguments(&shim, &["100%".into()]).is_err());
        assert_eq!(
            batch_arguments(&shim, &[r"C:\a b\".into()]).unwrap(),
            format!(r#"/d /e:ON /v:OFF /s /c ""{}" "C:\a b\\"""#, shim.display())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_console_child_has_its_console_for_input() {
        let script = "exit ([int][Console]::IsInputRedirected + 7)";
        let args = ["-NoProfile", "-Command", script].map(String::from);
        // NOTE: CREATE_NO_WINDOW: a console of its own without a window on the desktop
        let child = ConsoleChild::spawn_with(
            "powershell",
            &args,
            windows_sys::Win32::System::Threading::CREATE_NO_WINDOW,
        )
        .unwrap();
        assert_eq!(ended(&child), 7, "stdin is the console, not redirected");
        assert!(ConsoleChild::spawn("ccdash-no-such-program", &[]).is_err());
    }

    #[test]
    #[ignore = r"needs the C$ share over \\localhost"]
    fn a_network_share_is_refused_at_once() {
        let local = std::env::temp_dir();
        let rest = local.to_str().unwrap().strip_prefix(r"C:\").unwrap();
        let unc = std::path::Path::new(r"\\localhost\C$").join(rest);
        // NOTE: a failed first read once left drop waiting for it forever
        let e = DirWatch::open(&unc).err().unwrap();
        assert_eq!(e.kind(), io::ErrorKind::Unsupported, "{e}");
    }

    #[test]
    fn an_owner_only_directory_passes_it_on() {
        let dir = std::env::temp_dir().join(format!("ccdash-acl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("before"), "x").unwrap();
        owner_only(&dir).unwrap();
        std::fs::write(dir.join("after"), "x").unwrap();
        let sid = user_sid().unwrap();
        let text = |name: &str| {
            let path: Vec<u16> = dir
                .join(name)
                .as_os_str()
                .encode_wide()
                .chain(Some(0))
                .collect();
            file_dacl_text(&path).unwrap()
        };
        assert_eq!(text(""), format!("D:PAI(A;OICI;FA;;;{sid})"));
        let wanted = Descriptor::from_sddl(&format!("D:PAI(A;OICI;FA;;;{sid})")).unwrap();
        assert_eq!(
            wanted.dacl_text().unwrap(),
            text(""),
            "a second call compares equal and leaves it"
        );
        for file in ["before", "after"] {
            let acl = text(file);
            // NOTE: one inherited ACE, the user's
            assert_eq!(acl.matches("(A;").count(), 1, "{file}: {acl}");
            assert!(acl.contains(&sid), "{file}: {acl}");
        }
        owner_only(&dir).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_job_ends_and_counts_what_its_process_started() {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::{CREATE_NO_WINDOW, CREATE_SUSPENDED};
        // NOTE: cmd starts ping and waits: a wrapper with a child, as git's is
        let mut child = std::process::Command::new("cmd")
            .args(["/c", "ping -n 30 127.0.0.1 >NUL"])
            .creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED)
            .spawn()
            .unwrap();
        let job = Job::adopt(child.as_raw_handle()).unwrap();
        let ping = (0..100)
            .find_map(|_| {
                std::thread::sleep(Duration::from_millis(20));
                processes()
                    .into_iter()
                    .find(|p| p.ppid == child.id() && p.exe.eq_ignore_ascii_case("ping.exe"))
            })
            .expect("ping under cmd");
        job.terminate();
        assert!(!child.wait().unwrap().success());
        let gone = (0..100).any(|_| {
            std::thread::sleep(Duration::from_millis(20));
            !pid_alive(ping.pid)
        });
        assert!(gone, "ping outlived the job");
        let used = job.usage();
        assert!(used.peak_kb > 0, "counted the processes of the job");
    }

    #[test]
    fn this_process_is_listed_with_its_times_and_command_line() {
        let me = std::process::id();
        let listed = processes().into_iter().find(|p| p.pid == me).unwrap();
        assert!(
            listed.exe.to_ascii_lowercase().ends_with(".exe"),
            "{}",
            listed.exe
        );
        assert!(pid_alive(me) && pid_alive(listed.ppid));
        assert_eq!(parent_of(me), Some(listed.ppid));
        assert!(exe_name(me).is_some_and(|exe| exe.eq_ignore_ascii_case(&listed.exe)));
        let times = process_times(me).unwrap();
        let age_ms = crate::util::now_ms() - times.started_ms;
        assert!((0..600_000).contains(&age_ms), "{age_ms}");
        assert!(process_memory(me).unwrap() > 0);
        let args = split_command_line(&command_line(me).unwrap());
        let exe = std::env::current_exe().unwrap();
        let name = exe.file_stem().unwrap().to_string_lossy();
        assert!(args[0].contains(name.as_ref()), "{args:?}");
    }

    #[test]
    fn this_machine_reports() {
        assert!(uptime_ms() > 0);
        assert!(slept_ms() >= 0);
        let (busy, total) = cpu_times().unwrap();
        assert!(busy <= total && total > 0);
        let memory = memory_status().unwrap();
        assert!(memory.avail <= memory.total && memory.total <= memory.commit_limit);
        let system = std::env::var("SystemDrive").unwrap() + "\\";
        assert!(disk_space(std::path::Path::new(&system)).unwrap().total > 0);
        let windows = std::path::Path::new(&system).join("Windows");
        assert_eq!(volume_of(&windows).as_deref(), Some(system.as_str()));
        assert!(adapters().is_ok());
        assert!(power_status().is_some());
        let (_, kernel) = os_version();
        assert!(kernel.starts_with("10.0."), "{kernel}");
    }

    #[test]
    fn a_tree_watch_sees_files_and_directories() {
        let root = std::env::temp_dir().join(format!("ccdash-dirwatch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut watch = DirWatch::open(&root).unwrap();
        let writer = {
            let root = root.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                std::fs::create_dir(root.join("d")).unwrap();
                std::fs::write(root.join("d").join("f.txt"), "x").unwrap();
                std::fs::remove_dir_all(root.join("d")).unwrap();
            })
        };
        let mut seen = Vec::new();
        while !seen.contains(&(Change::Removed, "d".to_owned(), true)) {
            for e in watch.read().unwrap().unwrap_or_default() {
                seen.push((e.change, e.path, e.is_dir));
            }
        }
        writer.join().unwrap();
        assert!(
            seen.contains(&(Change::Added, "d".to_owned(), true)),
            "{seen:?}"
        );
        assert!(
            seen.contains(&(Change::Added, "d/f.txt".to_owned(), false)),
            "{seen:?}"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn command_lines_split_as_the_c_runtime_does() {
        assert_eq!(
            split_command_line(r#""C:\Program Files\x.exe" -c "a b" c"#),
            [r"C:\Program Files\x.exe", "-c", "a b", "c"]
        );
        assert!(split_command_line(" ").is_empty());
    }

    #[test]
    fn a_child_ends_when_terminated() {
        let mut child = std::process::Command::new("cmd")
            .args(["/c", "ping -n 30 127.0.0.1 >nul"])
            .spawn()
            .unwrap();
        assert!(pid_alive(child.id()));
        terminate(child.id()).unwrap();
        assert!(!child.wait().unwrap().success());
        assert!(!pid_alive(child.id()));
    }

    #[test]
    fn tells_objects_of_other_owners() {
        let system = std::env::var_os("SystemRoot").unwrap();
        // NOTE: TrustedInstaller owns the system's executables
        let notepad =
            std::fs::File::open(std::path::Path::new(&system).join(r"System32\notepad.exe"))
                .unwrap();
        assert!(!owned_by_me(&notepad).unwrap());
    }
}
