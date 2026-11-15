use anyhow::{anyhow, Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
pub const PROTOCOL_VERSION: u32 = 1;
#[derive(Serialize, Deserialize)]
struct Envelope<T> {
    version: u32,
    payload: T,
}
const MAX_FRAME: usize = 8 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(30);

async fn read_frame<R: AsyncRead + Unpin, T: DeserializeOwned>(r: &mut R) -> Result<T> {
    let size = r.read_u32().await? as usize;
    if size == 0 || size > MAX_FRAME {
        return Err(anyhow!("invalid frame length"));
    }
    let mut bytes = vec![0; size];
    r.read_exact(&mut bytes).await?;
    let envelope: Envelope<T> = serde_json::from_slice(&bytes)?;
    if envelope.version != PROTOCOL_VERSION {
        return Err(anyhow!("unsupported protocol version"));
    }
    Ok(envelope.payload)
}
async fn write_frame<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, value: &T) -> Result<()> {
    let mut bytes = Vec::new();
    serde_json::to_writer(
        LimitedWriter(&mut bytes),
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: value,
        },
    )?;
    if bytes.is_empty() || bytes.len() > MAX_FRAME {
        return Err(anyhow!("invalid frame length"));
    }
    w.write_u32(bytes.len() as u32).await?;
    w.write_all(&bytes).await?;
    w.flush().await?;
    Ok(())
}
struct LimitedWriter<'a>(&'a mut Vec<u8>);
impl std::io::Write for LimitedWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_FRAME.saturating_sub(self.0.len()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "frame too large",
            ));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
#[cfg(windows)]
mod platform {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::{ffi::c_void, os::windows::io::AsRawHandle, ptr};
    use tokio::net::windows::named_pipe::{ClientOptions, ServerOptions};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, LocalFree, HANDLE},
        Security::Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        },
        Security::{GetTokenInformation, TokenUser, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER},
        System::{
            Pipes::GetNamedPipeServerProcessId,
            Threading::{
                GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
            },
        },
    };
    fn sid() -> Result<String> {
        process_sid(unsafe { GetCurrentProcess() })
    }
    fn process_sid(process: HANDLE) -> Result<String> {
        unsafe {
            let mut token = ptr::null_mut();
            if OpenProcessToken(process, TOKEN_QUERY, &mut token) == 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let mut len = 0;
            GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut len);
            let mut data = vec![0u64; (len as usize).div_ceil(8)];
            let ok = GetTokenInformation(token, TokenUser, data.as_mut_ptr().cast(), len, &mut len);
            CloseHandle(token);
            if ok == 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let mut out = ptr::null_mut();
            if ConvertSidToStringSidW((*(data.as_ptr() as *const TOKEN_USER)).User.Sid, &mut out)
                == 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            let mut n = 0;
            while *out.add(n) != 0 {
                n += 1;
            }
            let result = String::from_utf16(std::slice::from_raw_parts(out, n));
            LocalFree(out.cast());
            Ok(result?)
        }
    }
    fn verify_peer_for_sid(
        pipe: &tokio::net::windows::named_pipe::NamedPipeClient,
        expected: &str,
    ) -> Result<()> {
        unsafe {
            let mut pid = 0;
            if GetNamedPipeServerProcessId(pipe.as_raw_handle() as _, &mut pid) == 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if process.is_null() {
                return Err(std::io::Error::last_os_error().into());
            }
            let actual = process_sid(process);
            CloseHandle(process);
            if actual? != expected {
                return Err(anyhow!("pipe server user mismatch"));
            }
            Ok(())
        }
    }
    pub fn default_pipe_name() -> String {
        // Failure yields an unusable per-process name rather than falling back to a shared pipe.
        match sid() {
            Ok(s) => format!(r"\\.\pipe\magi-{:x}", Sha256::digest(s.as_bytes())),
            Err(_) => format!(r"\\.\pipe\magi-unavailable-{}", std::process::id()),
        }
    }
    fn server(name: &str, first: bool) -> Result<tokio::net::windows::named_pipe::NamedPipeServer> {
        unsafe {
            let descriptor = format!("D:P(A;;GA;;;{})\0", sid()?);
            let wide: Vec<u16> = descriptor.encode_utf16().collect();
            let mut sd: *mut c_void = ptr::null_mut();
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                1,
                &mut sd,
                ptr::null_mut(),
            ) == 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            let mut attrs = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: sd,
                bInheritHandle: 0,
            };
            let result = ServerOptions::new()
                .first_pipe_instance(first)
                .reject_remote_clients(true)
                .create_with_security_attributes_raw(
                    name,
                    (&mut attrs as *mut SECURITY_ATTRIBUTES).cast(),
                );
            LocalFree(sd);
            Ok(result?)
        }
    }
    pub async fn request_typed<Q: Serialize, R: DeserializeOwned>(
        name: &str,
        request: &Q,
    ) -> Result<R> {
        request_with_verifier(name, request, |pipe| verify_peer_for_sid(pipe, &sid()?)).await
    }
    async fn request_with_verifier<Q: Serialize, R: DeserializeOwned>(
        name: &str,
        request: &Q,
        verify: impl FnOnce(&tokio::net::windows::named_pipe::NamedPipeClient) -> Result<()>,
    ) -> Result<R> {
        if !name.starts_with(r"\\.\pipe\")
            || name.len() == r"\\.\pipe\".len()
            || name.contains('\0')
        {
            return Err(anyhow!("invalid local pipe name"));
        }
        tokio::time::timeout(TIMEOUT, async {
            let mut pipe = loop {
                match ClientOptions::new().open(name) {
                    Ok(pipe) => break pipe,
                    Err(e) if e.raw_os_error() == Some(231) => {
                        tokio::time::sleep(Duration::from_millis(20)).await
                    }
                    Err(e) => return Err(e.into()),
                }
            };
            verify(&pipe)?;
            // 연결 이후에는 요청을 재전송하지 않는다.
            write_frame(&mut pipe, request).await?;
            read_frame(&mut pipe).await
        })
        .await
        .context("request timed out")?
    }
    pub async fn request(name: &str, request: &domain::Request) -> Result<domain::Response> {
        let response: domain::Response = request_typed(name, request).await?;
        if response.id != request.id {
            return Err(anyhow!("response ID mismatch"));
        }
        Ok(response)
    }
    pub async fn serve_typed<Q, R>(
        name: &str,
        requests: mpsc::Sender<(Q, oneshot::Sender<R>)>,
    ) -> Result<()>
    where
        Q: DeserializeOwned + Send + 'static,
        R: Serialize + Send + Sync + 'static,
    {
        let mut current = server(name, true)?;
        let permits = std::sync::Arc::new(tokio::sync::Semaphore::new(32));
        // JoinSet을 소유하여 서버 취소 시 연결 작업도 함께 취소한다.
        let mut tasks = tokio::task::JoinSet::new();
        loop {
            while tasks.try_join_next().is_some() {}
            current.connect().await?;
            let next = server(name, false)?;
            let mut connected = std::mem::replace(&mut current, next);
            let permit = match tokio::time::timeout(TIMEOUT, permits.clone().acquire_owned()).await
            {
                Ok(Ok(permit)) => permit,
                _ => continue,
            };
            let tx = requests.clone();
            tasks.spawn(async move {
                let _permit = permit;
                let _ = tokio::time::timeout(TIMEOUT, async {
                    let request = read_frame(&mut connected).await?;
                    let (response_tx, response_rx) = oneshot::channel();
                    tx.send((request, response_tx))
                        .await
                        .map_err(|_| anyhow!("engine unavailable"))?;
                    let response = response_rx.await?;
                    write_frame(&mut connected, &response).await
                })
                .await;
            });
        }
    }
    pub async fn serve(
        name: &str,
        requests: mpsc::Sender<(domain::Request, oneshot::Sender<domain::Response>)>,
    ) -> Result<()> {
        serve_typed(name, requests).await
    }
    #[cfg(test)]
    mod security_tests {
        use super::*;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
        use windows_sys::Win32::Security::{
            GetAce, GetSecurityDescriptorControl, ACCESS_ALLOWED_ACE, ACL,
            DACL_SECURITY_INFORMATION, SE_DACL_PROTECTED,
        };
        #[tokio::test]
        async fn rejects_nonlocal_and_invalid_pipe_names_before_opening() {
            for name in [
                "",
                r"\\remote\pipe\magi",
                r"C:\temp\file",
                r"\\.\pipe\",
                "\\\\.\\pipe\\bad\0name",
            ] {
                let result =
                    request_typed::<_, serde_json::Value>(name, &serde_json::json!({})).await;
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("invalid local pipe name"),
                    "{name:?}"
                );
            }
        }
        #[tokio::test]
        async fn untrusted_or_unverifiable_server_receives_no_request_bytes() {
            for (i, reason) in ["SID mismatch", "SID lookup failed"].iter().enumerate() {
                let name = format!(r"\\.\pipe\magi-peer-test-{}-{i}", std::process::id());
                let mut listener = server(&name, true).unwrap();
                let reader = tokio::spawn(async move {
                    listener.connect().await.unwrap();
                    let mut byte = [0u8; 1];
                    listener.read(&mut byte).await.unwrap()
                });
                let result = request_with_verifier::<_, serde_json::Value>(
                    &name,
                    &serde_json::json!({"message":"test"}),
                    |pipe| {
                        if i == 0 {
                            verify_peer_for_sid(pipe, "S-1-0-0")
                        } else {
                            Err(anyhow!(*reason))
                        }
                    },
                )
                .await;
                assert!(result.is_err());
                assert_eq!(reader.await.unwrap(), 0, "{reason}");
            }
        }
        #[tokio::test]
        async fn pipe_dacl_is_protected_and_only_grants_current_sid() {
            unsafe {
                let name = format!(r"\\.\pipe\magi-acl-{}", std::process::id());
                let pipe = server(&name, true).unwrap();
                let mut acl: *mut ACL = ptr::null_mut();
                let mut descriptor = ptr::null_mut();
                assert_eq!(
                    GetSecurityInfo(
                        pipe.as_raw_handle() as _,
                        SE_KERNEL_OBJECT,
                        DACL_SECURITY_INFORMATION,
                        ptr::null_mut(),
                        ptr::null_mut(),
                        &mut acl,
                        ptr::null_mut(),
                        &mut descriptor
                    ),
                    0
                );
                assert!(!acl.is_null());
                let mut control = 0;
                let mut revision = 0;
                assert_ne!(
                    GetSecurityDescriptorControl(descriptor, &mut control, &mut revision),
                    0
                );
                assert_ne!(control & SE_DACL_PROTECTED, 0);
                assert_eq!((*acl).AceCount, 1);
                let mut ace = ptr::null_mut();
                assert_ne!(GetAce(acl, 0, &mut ace), 0);
                let ace = &*(ace as *const ACCESS_ALLOWED_ACE);
                assert_eq!(ace.Header.AceType, 0);
                let mut actual = ptr::null_mut();
                assert_ne!(
                    ConvertSidToStringSidW(
                        (&ace.SidStart as *const u32 as *mut u32).cast(),
                        &mut actual
                    ),
                    0
                );
                let mut n = 0;
                while *actual.add(n) != 0 {
                    n += 1;
                }
                let actual_sid = String::from_utf16(std::slice::from_raw_parts(actual, n)).unwrap();
                LocalFree(actual.cast());
                LocalFree(descriptor);
                assert_eq!(actual_sid, sid().unwrap());
            }
        }
    }
}
#[cfg(windows)]
pub use platform::{default_pipe_name, request, request_typed, serve, serve_typed};
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn rejects_missing_or_unsupported_protocol_version() {
        for bytes in [
            br#"{"payload":{"message":"hello"}}"#.as_slice(),
            br#"{"version":999,"payload":{"message":"hello"}}"#.as_slice(),
        ] {
            let (mut writer, mut reader) = tokio::io::duplex(256);
            writer.write_u32(bytes.len() as u32).await.unwrap();
            writer.write_all(bytes).await.unwrap();
            assert!(read_frame::<_, serde_json::Value>(&mut reader)
                .await
                .is_err());
        }
    }
    #[tokio::test]
    async fn frame_rejects_oversize_and_truncated() {
        let (mut writer, mut reader) = tokio::io::duplex(32);
        writer.write_u32((MAX_FRAME + 1) as u32).await.unwrap();
        assert!(read_frame::<_, serde_json::Value>(&mut reader)
            .await
            .is_err());
        let (mut writer, mut reader) = tokio::io::duplex(32);
        writer.write_u32(10).await.unwrap();
        writer.write_all(b"{}").await.unwrap();
        drop(writer);
        assert!(read_frame::<_, serde_json::Value>(&mut reader)
            .await
            .is_err());
    }
    #[tokio::test]
    async fn frame_rejects_invalid_json() {
        let (mut writer, mut reader) = tokio::io::duplex(32);
        writer.write_u32(3).await.unwrap();
        writer.write_all(b"bad").await.unwrap();
        assert!(read_frame::<_, serde_json::Value>(&mut reader)
            .await
            .is_err());
    }
}
