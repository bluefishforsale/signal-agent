//! Windows named pipe endpoint. The pipe namespace is shared by every user on
//! the machine, so ownership is proven both ways: the server holds the name
//! exclusively with a DACL for the user's SID alone, and each side checks the
//! other process runs as that same SID before trusting it.

use std::{
    ffi::c_void,
    io,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    ptr,
    time::Duration,
};

use anyhow::{Context, bail};
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use windows_sys::Win32::{
    Foundation::{ERROR_PIPE_BUSY, HANDLE, LocalFree},
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            SDDL_REVISION_1,
        },
        GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
        TokenUser,
    },
    System::{
        Pipes::{GetNamedPipeClientProcessId, GetNamedPipeServerProcessId},
        Threading::{
            GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
        },
    },
};

/// A pipe endpoint that accepts only connections from this process's user.
pub struct Listener {
    name: String,
    next: NamedPipeServer,
    security: SecurityDescriptor,
    owner: String,
}

impl Listener {
    pub async fn bind(endpoint: &str) -> anyhow::Result<Self> {
        let owner = current_user_sid()?;
        let security = SecurityDescriptor::user_only(&owner)?;
        let next = create(endpoint, &security, true).with_context(|| {
            format!(
                "unable to create pipe \"{endpoint}\"; an agent or another program already holds it"
            )
        })?;
        Ok(Self {
            name: endpoint.to_owned(),
            next,
            security,
            owner,
        })
    }

    /// The next connection from this user. Connections from anyone else are
    /// closed and skipped, so a stranger can neither talk nor stop the agent.
    pub async fn accept(&mut self) -> anyhow::Result<NamedPipeServer> {
        loop {
            self.next.connect().await.context("accept failed")?;
            let fresh = create(&self.name, &self.security, false).with_context(|| {
                format!("unable to create another instance of \"{}\"", self.name)
            })?;
            let connected = std::mem::replace(&mut self.next, fresh);
            match peer_sid(&connected, GetNamedPipeClientProcessId) {
                Ok(sid) if sid == self.owner => return Ok(connected),
                Ok(sid) => tracing::warn!("refused a pipe connection from {sid}"),
                Err(e) => tracing::warn!("refused a pipe connection of unknown origin: {e:#}"),
            }
        }
    }
}

/// Connects to the agent listening at `endpoint`, refusing a server that runs
/// as anyone but this user.
pub async fn connect(endpoint: &str) -> anyhow::Result<NamedPipeClient> {
    let mut attempts = 0;
    let client = loop {
        match ClientOptions::new().open(endpoint) {
            Ok(client) => break client,
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) && attempts < 50 => {
                attempts += 1;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => {
                return Err(e).with_context(|| format!("unable to connect to \"{endpoint}\""));
            }
        }
    };
    verify_server(&client, &current_user_sid()?)?;
    Ok(client)
}

// NOTE: this runs before the client sends a single byte. Without it, a process
// of another user that grabbed the pipe name first would receive every MCP
// request, and could answer them as if it were the user's agent.
fn verify_server(client: &NamedPipeClient, expected: &str) -> anyhow::Result<()> {
    let sid = peer_sid(client, GetNamedPipeServerProcessId)?;
    if sid != expected {
        bail!("the pipe's server runs as {sid}, not this user ({expected}); refusing to use it");
    }
    Ok(())
}

/// The SID of the user this process runs as, from its token.
pub(crate) fn current_user_sid() -> anyhow::Result<String> {
    // SAFETY: GetCurrentProcess returns a pseudo-handle that needs no closing.
    token_user_sid(unsafe { GetCurrentProcess() })
}

fn create(name: &str, security: &SecurityDescriptor, first: bool) -> io::Result<NamedPipeServer> {
    let mut attrs = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: security.0,
        bInheritHandle: 0,
    };
    let mut options = ServerOptions::new();
    options
        .first_pipe_instance(first)
        .reject_remote_clients(true);
    // SAFETY: `attrs` is a valid SECURITY_ATTRIBUTES whose descriptor outlives
    // this call, which copies what it needs.
    unsafe { options.create_with_security_attributes_raw(name, (&raw mut attrs).cast::<c_void>()) }
}

type PipeProcessId = unsafe extern "system" fn(HANDLE, *mut u32) -> i32;

fn peer_sid(pipe: &impl AsRawHandle, query: PipeProcessId) -> anyhow::Result<String> {
    let mut pid = 0u32;
    // SAFETY: the pipe handle is open for the duration of the call.
    if unsafe { query(pipe.as_raw_handle(), &mut pid) } == 0 {
        return Err(io::Error::last_os_error())
            .context("unable to identify the pipe's peer process");
    }
    // SAFETY: OpenProcess has no preconditions; a null result is checked below.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("unable to open process {pid}"));
    }
    // SAFETY: `process` is a fresh handle we own.
    let process = unsafe { OwnedHandle::from_raw_handle(process) };
    token_user_sid(process.as_raw_handle())
        .with_context(|| format!("unable to read the user of process {pid}"))
}

fn token_user_sid(process: HANDLE) -> anyhow::Result<String> {
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: `process` is a valid process handle and `token` a valid out pointer.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error()).context("unable to open process token");
    }
    // SAFETY: OpenProcessToken succeeded, so `token` is a handle we own.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };

    let mut len = 0u32;
    // SAFETY: a size query with a null buffer; failure with the needed length is expected.
    unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            ptr::null_mut(),
            0,
            &mut len,
        )
    };
    // u64 words keep TOKEN_USER's pointer field aligned.
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    // SAFETY: `buf` holds at least `len` bytes.
    let ok = unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buf.as_mut_ptr().cast(),
            len,
            &mut len,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error()).context("unable to read the token's user");
    }
    // SAFETY: GetTokenInformation filled `buf` with a TOKEN_USER.
    let user = unsafe { &*buf.as_ptr().cast::<TOKEN_USER>() };

    let mut text: *mut u16 = ptr::null_mut();
    // SAFETY: the SID points into `buf`, which is alive; `text` is a valid out pointer.
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error()).context("unable to format the token's SID");
    }
    // SAFETY: ConvertSidToStringSidW returned a NUL-terminated LocalAlloc'd string.
    let sid = unsafe { from_wide(text) };
    // SAFETY: `text` came from LocalAlloc and is freed once.
    unsafe { LocalFree(text.cast()) };
    Ok(sid)
}

/// A self-relative security descriptor allocated by Windows and freed on drop.
struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

// SAFETY: the descriptor is immutable after creation and only read by Windows.
unsafe impl Send for SecurityDescriptor {}
// SAFETY: as above; shared reads of an immutable descriptor are safe.
unsafe impl Sync for SecurityDescriptor {}

impl SecurityDescriptor {
    /// A protected DACL granting full access to `sid` and nobody else.
    fn user_only(sid: &str) -> anyhow::Result<Self> {
        let sddl = wide(&format!("D:P(A;;GA;;;{sid})"));
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: `sddl` is NUL-terminated and `descriptor` a valid out pointer.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("unable to build a security descriptor for {sid}"));
        }
        Ok(Self(descriptor))
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: the descriptor came from LocalAlloc and is freed once.
        unsafe { LocalFree(self.0) };
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// # Safety
/// `p` must point at a NUL-terminated UTF-16 string.
unsafe fn from_wide(p: *const u16) -> String {
    let mut len = 0;
    // SAFETY: the caller guarantees a terminator within the allocation.
    while unsafe { *p.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: `len` code units before the terminator are initialized.
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, len) })
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncReadExt;
    use windows_sys::Win32::Security::{
        Authorization::{
            ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertStringSidToSidW,
            GetSecurityInfo, SE_KERNEL_OBJECT,
        },
        DACL_SECURITY_INFORMATION, PSID,
    };

    use super::*;

    fn test_pipe() -> String {
        format!(r"\\.\pipe\signal-agent-test-{}", uuid::Uuid::new_v4())
    }

    fn dacl_of(pipe: &impl AsRawHandle) -> String {
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        let err = unsafe {
            GetSecurityInfo(
                pipe.as_raw_handle(),
                SE_KERNEL_OBJECT,
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                &mut descriptor,
            )
        };
        assert_eq!(err, 0, "GetSecurityInfo failed");
        let mut text: *mut u16 = ptr::null_mut();
        let ok = unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                descriptor,
                SDDL_REVISION_1,
                DACL_SECURITY_INFORMATION,
                &mut text,
                ptr::null_mut(),
            )
        };
        assert_ne!(ok, 0, "formatting the descriptor failed");
        let sddl = unsafe { from_wide(text) };
        unsafe {
            LocalFree(text.cast());
            LocalFree(descriptor);
        }
        sddl
    }

    fn full_sid(sddl_trustee: &str) -> String {
        let text = wide(sddl_trustee);
        let mut sid: PSID = ptr::null_mut();
        let ok = unsafe { ConvertStringSidToSidW(text.as_ptr(), &mut sid) };
        assert_ne!(ok, 0, "not a SID or alias: {sddl_trustee}");
        let mut out: *mut u16 = ptr::null_mut();
        let ok = unsafe { ConvertSidToStringSidW(sid, &mut out) };
        assert_ne!(ok, 0, "formatting {sddl_trustee} failed");
        let full = unsafe { from_wide(out) };
        unsafe {
            LocalFree(out.cast());
            LocalFree(sid);
        }
        full
    }

    /// The pipe name and DACL must come from the token's SID, a real
    /// `S-1-...` string, never USERNAME or an empty fallback.
    #[test]
    fn current_user_sid_is_a_sid() {
        let sid = current_user_sid().expect("sid");
        assert!(sid.starts_with("S-1-"), "not a SID: {sid}");
        let username = std::env::var("USERNAME").unwrap_or_default();
        assert_ne!(sid, username, "SID fell back to USERNAME");
    }

    /// The pipe's DACL must grant exactly one principal, this user, so no
    /// other local account can open it.
    #[tokio::test]
    async fn the_pipe_dacl_grants_only_this_user() {
        let name = test_pipe();
        let listener = Listener::bind(&name).await.expect("bind");
        let sddl = dacl_of(&listener.next);
        let sid = current_user_sid().expect("sid");
        assert_eq!(sddl.matches("(A;").count(), 1, "extra ACEs in {sddl}");
        // SDDL abbreviates well-known SIDs ("LA" for the built-in admin CI runs
        // as), so compare the ACE's trustee as a full SID.
        let trustee = sddl
            .split(";;;")
            .nth(1)
            .and_then(|rest| rest.split(')').next())
            .expect("an ACE trustee");
        assert_eq!(full_sid(trustee), sid, "ACE is not for this user: {sddl}");
        assert!(sddl.starts_with("D:P"), "DACL inherits entries: {sddl}");
    }

    /// Someone who created the pipe name first (a squatter, or a live agent)
    /// must make bind fail, not silently join their pipe as another instance.
    #[tokio::test]
    async fn bind_refuses_a_name_someone_else_already_holds() {
        let name = test_pipe();
        let _squatter = ServerOptions::new().create(&name).expect("squatter pipe");
        let bound = Listener::bind(&name).await;
        assert!(bound.is_err(), "bound onto a squatted pipe name");
    }

    /// A client must refuse a server running as another user before sending
    /// anything. Expecting SYSTEM's SID makes this test's own server the impostor.
    #[tokio::test]
    async fn the_client_refuses_a_server_of_another_user() {
        let name = test_pipe();
        let mut listener = Listener::bind(&name).await.expect("bind");
        let accepting = tokio::spawn(async move { listener.accept().await.map(|_| ()) });
        let client = ClientOptions::new().open(&name).expect("open");
        let verdict = verify_server(&client, "S-1-5-18");
        accepting.abort();
        assert!(verdict.is_err(), "trusted a server of another user");
    }

    /// The server must drop a client of another user unheard and keep serving.
    /// Telling the listener it belongs to SYSTEM makes this test the stranger.
    #[tokio::test]
    async fn connections_from_another_user_are_dropped_unheard() {
        let name = test_pipe();
        let mut listener = Listener::bind(&name).await.expect("bind");
        listener.owner = "S-1-5-18".into();
        let accepted = tokio::spawn(async move { listener.accept().await.map(|_| ()) });
        let mut stranger = connect(&name).await.expect("connect");
        let mut buf = [0u8; 1];
        let read = tokio::time::timeout(Duration::from_secs(5), stranger.read(&mut buf))
            .await
            .expect("stranger's connection left open");
        assert!(
            matches!(read, Ok(0) | Err(_)),
            "stranger got data: {read:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !accepted.is_finished(),
            "accept handed over or gave up on a stranger"
        );
        accepted.abort();
    }
}
