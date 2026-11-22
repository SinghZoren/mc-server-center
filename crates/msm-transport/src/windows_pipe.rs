//! Windows named-pipe local transport (ADR-002, threat T7).
//!
//! Defences, in order:
//! 1. **DACL** on every pipe instance grants access only to the current user's SID
//!    (`D:P(A;;GA;;;<sid>)`, protected so nothing is inherited). Other local users, including
//!    other non-elevated sessions, cannot open it.
//! 2. **Remote clients rejected** (`PIPE_REJECT_REMOTE_CLIENTS`), so the pipe is not reachable over
//!    SMB from other machines.
//! 3. **First instance only**: creating the first instance fails if the name already exists,
//!    so a malicious process cannot pre-create ("squat") the pipe to impersonate the daemon.
//! 4. **Peer checks on both ends**: the server verifies the connecting process runs as the same
//!    user; the client verifies the serving process runs as the same user.
#![allow(unsafe_code)]

use std::ffi::c_void;
use std::io;
use std::os::windows::io::AsRawHandle;
use std::ptr::null_mut;

use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_PIPE_BUSY, HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    TokenUser,
};
use windows_sys::Win32::System::Pipes::{GetNamedPipeClientProcessId, GetNamedPipeServerProcessId};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// Closes a Win32 handle on drop.
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: we own this handle and close it exactly once.
            unsafe { CloseHandle(self.0) };
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Returns the string SID (`S-1-5-21-…`) of the user a process runs as.
fn process_user_sid(process: HANDLE) -> io::Result<String> {
    let mut token: HANDLE = null_mut();
    // SAFETY: `process` is a valid process handle with query rights; `token` is an out-pointer.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &raw mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = OwnedHandle(token);

    let mut len = 0u32;
    // SAFETY: size query with a null buffer; failure with ERROR_INSUFFICIENT_BUFFER is expected.
    unsafe { GetTokenInformation(token.0, TokenUser, null_mut(), 0, &raw mut len) };
    if len == 0 {
        return Err(io::Error::last_os_error());
    }
    // u64 storage gives the pointer alignment TOKEN_USER requires.
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    // SAFETY: `buf` is at least `len` bytes and suitably aligned.
    let ok = unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            buf.as_mut_ptr().cast::<c_void>(),
            len,
            &raw mut len,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: on success the buffer starts with a TOKEN_USER whose SID points into `buf`.
    let sid = unsafe { (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut out: *mut u16 = null_mut();
    // SAFETY: `sid` is valid while `buf` lives; `out` receives a LocalAlloc'd string.
    if unsafe { ConvertSidToStringSidW(sid, &raw mut out) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `out` is a NUL-terminated UTF-16 string allocated by the system.
    let text = unsafe {
        let mut n = 0;
        while *out.add(n) != 0 {
            n += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(out, n))
    };
    // SAFETY: frees the string allocated by ConvertSidToStringSidW.
    unsafe { LocalFree(out.cast::<c_void>()) };
    Ok(text)
}

/// SID of the user running this process.
///
/// # Errors
/// Win32 errors querying the process token.
pub fn current_user_sid() -> io::Result<String> {
    // SAFETY: GetCurrentProcess returns a pseudo-handle that needs no closing.
    process_user_sid(unsafe { GetCurrentProcess() })
}

fn sid_of_pid(pid: u32) -> io::Result<String> {
    // SAFETY: plain call; a null result is checked below.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    let handle = OwnedHandle(handle);
    process_user_sid(handle.0)
}

/// Default pipe name for the current user's daemon.
///
/// # Errors
/// If the current user's SID cannot be determined.
pub fn default_pipe_name() -> io::Result<String> {
    Ok(format!(r"\\.\pipe\msm-{}", current_user_sid()?))
}

/// A security descriptor granting access to exactly one SID.
struct OwnerOnlySecurity {
    descriptor: PSECURITY_DESCRIPTOR,
    attributes: SECURITY_ATTRIBUTES,
}

// SAFETY: the descriptor is an immutable, self-contained allocation only read by the OS.
unsafe impl Send for OwnerOnlySecurity {}
// SAFETY: as above; never mutated after construction.
unsafe impl Sync for OwnerOnlySecurity {}

impl OwnerOnlySecurity {
    fn new(sid: &str) -> io::Result<Box<Self>> {
        // P = protected (no inherited ACEs); GA = generic all; only the owner's SID.
        let sddl = wide(&format!("D:P(A;;GA;;;{sid})"));
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: `sddl` is NUL-terminated; `descriptor` receives a LocalAlloc'd descriptor.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &raw mut descriptor,
                null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut boxed = Box::new(Self {
            descriptor,
            attributes: SECURITY_ATTRIBUTES {
                nLength: u32::try_from(std::mem::size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(0),
                lpSecurityDescriptor: descriptor,
                bInheritHandle: 0,
            },
        });
        boxed.attributes.lpSecurityDescriptor = boxed.descriptor;
        Ok(boxed)
    }
}

impl Drop for OwnerOnlySecurity {
    fn drop(&mut self) {
        // SAFETY: frees the descriptor allocated in `new`, exactly once.
        unsafe { LocalFree(self.descriptor.cast::<c_void>()) };
    }
}

/// Listens on a user-private named pipe.
pub struct PipeListener {
    name: String,
    user_sid: String,
    security: Box<OwnerOnlySecurity>,
    next: NamedPipeServer,
}

impl std::fmt::Debug for PipeListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipeListener")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl PipeListener {
    fn create(
        name: &str,
        security: &OwnerOnlySecurity,
        first: bool,
    ) -> io::Result<NamedPipeServer> {
        let mut opts = ServerOptions::new();
        opts.first_pipe_instance(first).reject_remote_clients(true);
        let attrs = std::ptr::from_ref(&security.attributes)
            .cast_mut()
            .cast::<c_void>();
        // SAFETY: `attrs` points to a valid SECURITY_ATTRIBUTES that outlives this call; the OS
        // copies the descriptor into the pipe object.
        unsafe { opts.create_with_security_attributes_raw(name, attrs) }
    }

    /// Creates the pipe. Fails if a pipe with this name already exists (squatting defence).
    ///
    /// # Errors
    /// Win32 errors, including access-denied when the name is already taken.
    pub fn bind(name: &str) -> io::Result<Self> {
        let user_sid = current_user_sid()?;
        let security = OwnerOnlySecurity::new(&user_sid)?;
        let next = Self::create(name, &security, true)?;
        Ok(Self {
            name: name.to_owned(),
            user_sid,
            security,
            next,
        })
    }

    /// Waits for a client, verifies it runs as the same user, and returns the connection.
    /// Connections from other users are dropped and reported as `PermissionDenied`.
    ///
    /// # Errors
    /// I/O errors, or `PermissionDenied` for a foreign client.
    pub async fn accept(&mut self) -> io::Result<NamedPipeServer> {
        self.next.connect().await?;
        let fresh = Self::create(&self.name, &self.security, false)?;
        let conn = std::mem::replace(&mut self.next, fresh);

        let mut pid = 0u32;
        // SAFETY: valid connected pipe handle; `pid` is an out-pointer.
        let ok = unsafe { GetNamedPipeClientProcessId(conn.as_raw_handle(), &raw mut pid) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let client_sid = sid_of_pid(pid)?;
        if client_sid != self.user_sid {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "pipe client runs as a different user",
            ));
        }
        Ok(conn)
    }
}

/// Connects to a daemon pipe and verifies the server runs as the same user.
///
/// # Errors
/// `NotFound` if no daemon is listening, `PermissionDenied` if the server is another user's.
pub async fn connect(name: &str) -> io::Result<NamedPipeClient> {
    let client = loop {
        match ClientOptions::new().open(name) {
            Ok(c) => break c,
            Err(e) if e.raw_os_error() == Some(i32::try_from(ERROR_PIPE_BUSY).unwrap_or(231)) => {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            Err(e) => return Err(e),
        }
    };
    let mut pid = 0u32;
    // SAFETY: valid connected pipe handle; `pid` is an out-pointer.
    let ok = unsafe { GetNamedPipeServerProcessId(client.as_raw_handle(), &raw mut pid) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    if sid_of_pid(pid)? != current_user_sid()? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "pipe server runs as a different user",
        ));
    }
    Ok(client)
}
