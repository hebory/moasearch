//! Single-instance, per-user IPC for the Windows desktop application.
//!
//! Explorer handoff is deliberately limited to a small, versioned message set.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, sync::Arc, time::Duration};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Open { folder: Option<PathBuf> },
    Status,
    Quit,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reply {
    pub app: String,
    pub version: String,
    pub protocol: u32,
    pub data_dir: String,
    pub pid: u32,
    pub ready: bool,
    pub accepted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug)]
pub enum InstanceCommand {
    Open(Option<PathBuf>),
    Quit,
}

pub type InstanceHandler = Arc<dyn Fn(InstanceCommand) -> Result<()> + Send + Sync + 'static>;

#[cfg(windows)]
mod windows_pipe {
    use super::*;
    use anyhow::{Context, ensure};
    use std::{
        ffi::c_void,
        io,
        os::windows::io::AsRawHandle,
        path::Path,
        sync::{
            Mutex,
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        thread::{self, JoinHandle},
        time::Instant,
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::windows::named_pipe::{ClientOptions, NamedPipeServer, PipeMode, ServerOptions},
        runtime::Builder,
        sync::watch,
        time::timeout,
    };
    use windows::Win32::{
        Foundation::{CloseHandle, HLOCAL, LocalFree},
        Security::Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            SDDL_REVISION_1,
        },
        Security::{
            GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
            TOKEN_USER, TokenUser,
        },
        System::{
            Pipes::GetNamedPipeServerProcessId,
            Threading::{
                GetCurrentProcess, OpenProcess, OpenProcessToken,
                PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
            },
        },
    };
    use windows::core::{PCWSTR, PWSTR};

    const MAX_FRAME: usize = 128 * 1024;
    const MAX_CLIENTS: usize = 32;
    const PROTOCOL: u32 = 1;
    const PIPE_TIMEOUT: Duration = Duration::from_secs(3);

    pub struct Instance {
        ready: Arc<AtomicBool>,
        shutdown: watch::Sender<bool>,
        thread: Mutex<Option<JoinHandle<()>>>,
    }

    impl Instance {
        pub fn bind(data_dir: &Path, handler: InstanceHandler) -> Result<Self> {
            let data_dir = crate::config::canonical(data_dir)?;
            let data_string = data_dir
                .to_str()
                .context("앱 데이터 경로가 UTF-8이 아닙니다.")?
                .to_owned();
            let pipe_name = pipe_name(&data_dir)?;
            let ready = Arc::new(AtomicBool::new(false));
            let ready_for_thread = ready.clone();
            let data_for_thread = data_string.clone();
            let pipe_for_thread = pipe_name.clone();
            let (shutdown, shutdown_rx) = watch::channel(false);
            let (startup_tx, startup_rx) = mpsc::sync_channel(1);

            let thread = thread::Builder::new()
                .name("moasearch-instance-pipe".into())
                .spawn(move || {
                    let result = (|| -> Result<()> {
                        let runtime = Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .context("앱 연결 통로를 초기화하지 못했습니다.")?;
                        let first_pipe = runtime
                            .block_on(create_pipe(&pipe_for_thread, true))
                            .context("앱 연결 통로를 만들지 못했습니다.")?;
                        let _ = startup_tx.send(Ok(()));
                        runtime.block_on(serve(
                            first_pipe,
                            pipe_for_thread,
                            data_for_thread,
                            ready_for_thread,
                            handler,
                            shutdown_rx,
                        ))
                    })();
                    if let Err(error) = result {
                        let _ = startup_tx.send(Err(format!("{error:#}")));
                    }
                })
                .context("앱 연결 통로 작업을 시작하지 못했습니다.")?;

            match startup_rx.recv_timeout(Duration::from_secs(5)) {
                Ok(Ok(())) => Ok(Self {
                    ready,
                    shutdown,
                    thread: Mutex::new(Some(thread)),
                }),
                Ok(Err(error)) => {
                    let _ = thread.join();
                    bail!("{error}");
                }
                Err(error) => {
                    let _ = shutdown.send(true);
                    let _ = thread.join();
                    bail!("앱 연결 통로 준비 시간이 초과되었습니다: {error}");
                }
            }
        }

        pub fn mark_ready(&self) {
            self.ready.store(true, Ordering::Release);
        }
    }

    impl Drop for Instance {
        fn drop(&mut self) {
            let _ = self.shutdown.send(true);
            if let Some(thread) = self.thread.lock().unwrap().take() {
                let _ = thread.join();
            }
        }
    }

    pub fn request(data_dir: &Path, request: &Request) -> Result<Option<Reply>> {
        let data_dir = crate::config::canonical(data_dir)?;
        let pipe_name = pipe_name(&data_dir)?;
        let request_bytes = serde_json::to_vec(request)?;
        ensure!(
            request_bytes.len() <= MAX_FRAME,
            "앱 연결 요청이 너무 큽니다."
        );
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .context("앱 연결 요청을 초기화하지 못했습니다.")?;
        let deadline = Instant::now() + Duration::from_secs(4);
        let mut first_attempt = true;
        loop {
            match runtime.block_on(async {
                timeout(PIPE_TIMEOUT, transact(&pipe_name, &request_bytes)).await
            }) {
                Err(_) => bail!("모아검색 응답 시간이 초과되었습니다."),
                Ok(Err(error)) if is_not_found(&error) && first_attempt => return Ok(None),
                Ok(Err(error)) if is_transient(&error) && Instant::now() < deadline => {
                    first_attempt = false;
                    std::thread::sleep(Duration::from_millis(40));
                }
                Ok(Err(error)) if is_not_found(&error) => {
                    return Err(error).context("모아검색 연결 통로가 사라졌습니다.");
                }
                Ok(Err(error)) => return Err(error).context("모아검색 앱에 연결하지 못했습니다."),
                Ok(Ok(reply)) => {
                    validate_reply(&reply, &data_dir)?;
                    return Ok(Some(reply));
                }
            }
        }
    }

    async fn serve(
        mut pipe: NamedPipeServer,
        pipe_name: String,
        data_dir: String,
        ready: Arc<AtomicBool>,
        handler: InstanceHandler,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<()> {
        let mut clients = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { break; }
                    continue;
                }
                Some(_) = clients.join_next() => continue,
                connected = pipe.connect(), if clients.len() < MAX_CLIENTS => {
                    if let Err(error) = connected {
                        if error.kind() == io::ErrorKind::Interrupted { continue; }
                        return Err(error).context("앱 연결 요청을 수락하지 못했습니다.");
                    }
                }
            }

            // Keep another instance available before processing an untrusted client.
            let next = create_pipe(&pipe_name, false)
                .await
                .context("다음 앱 연결 통로를 만들지 못했습니다.")?;
            let connected = pipe;
            let handler = handler.clone();
            let ready = ready.clone();
            let data_dir = data_dir.clone();
            clients.spawn(async move {
                let result = timeout(
                    PIPE_TIMEOUT,
                    handle_client(connected, handler, ready, data_dir),
                )
                .await;
                if let Ok(Err(error)) = result {
                    eprintln!(
                        "앱 연결 요청 오류: {}",
                        error.to_string().chars().take(500).collect::<String>()
                    );
                }
            });
            pipe = next;
        }
        drop(pipe);
        // A quit request may close the window immediately. Finish its reply before
        // dropping the runtime, otherwise the invoking process receives a truncated frame.
        while clients.join_next().await.is_some() {}
        Ok(())
    }

    async fn create_pipe(name: &str, first_instance: bool) -> io::Result<NamedPipeServer> {
        let (descriptor, attributes) = security_attributes().map_err(io::Error::other)?;
        let mut options = ServerOptions::new();
        options
            .first_pipe_instance(first_instance)
            .pipe_mode(PipeMode::Byte)
            .reject_remote_clients(true)
            .max_instances(254);
        let result = unsafe {
            options.create_with_security_attributes_raw(
                name,
                &attributes as *const SECURITY_ATTRIBUTES as *mut c_void,
            )
        };
        drop(descriptor);
        result
    }

    async fn handle_client(
        mut client: NamedPipeServer,
        handler: InstanceHandler,
        ready: Arc<AtomicBool>,
        data_dir: String,
    ) -> Result<()> {
        let request_bytes = read_frame(&mut client).await?;
        let request: Request = serde_json::from_slice(&request_bytes)
            .context("앱 연결 요청 형식이 올바르지 않습니다.")?;
        let (accepted, detail) = match request {
            Request::Status => (true, None),
            Request::Open { folder } => {
                match folder
                    .map(|folder| crate::search::canonical_folder(&folder))
                    .transpose()
                {
                    Ok(folder) => match handler(InstanceCommand::Open(folder)) {
                        Ok(()) => (true, None),
                        Err(error) => (false, Some(error_detail(&error))),
                    },
                    Err(error) => (false, Some(error_detail(&error))),
                }
            }
            Request::Quit => match handler(InstanceCommand::Quit) {
                Ok(()) => (true, None),
                Err(error) => (false, Some(error_detail(&error))),
            },
        };
        let reply = Reply {
            app: "moasearch".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            protocol: PROTOCOL,
            data_dir,
            pid: std::process::id(),
            ready: ready.load(Ordering::Acquire),
            accepted,
            detail,
        };
        write_frame(&mut client, &serde_json::to_vec(&reply)?).await?;
        client.flush().await?;
        // Tokio's pipe flush does not wait for the peer to consume Windows buffers.
        // Keep the server handle alive until the client has read the complete reply.
        ensure!(
            client.read_u8().await? == 6,
            "앱 연결 응답 확인이 올바르지 않습니다."
        );
        Ok(())
    }

    fn error_detail(error: &anyhow::Error) -> String {
        // Even an invalid maximum-size path must leave room for the reply envelope.
        format!("{error:#}").chars().take(2000).collect()
    }

    async fn transact(name: &str, request: &[u8]) -> io::Result<Reply> {
        let mut client = ClientOptions::new().open(name)?;
        verify_server_process(&client)?;
        write_frame(&mut client, request).await?;
        client.flush().await?;
        let reply_bytes = read_frame(&mut client).await?;
        client.write_u8(6).await?;
        serde_json::from_slice(&reply_bytes).map_err(io::Error::other)
    }

    async fn read_frame(pipe: &mut (impl AsyncReadExt + Unpin)) -> io::Result<Vec<u8>> {
        let size = pipe.read_u32_le().await? as usize;
        if size == 0 || size > MAX_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "앱 연결 메시지 크기가 올바르지 않습니다.",
            ));
        }
        let mut bytes = vec![0; size];
        pipe.read_exact(&mut bytes).await?;
        Ok(bytes)
    }

    async fn write_frame(pipe: &mut (impl AsyncWriteExt + Unpin), bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() || bytes.len() > MAX_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "앱 응답 크기가 올바르지 않습니다.",
            ));
        }
        pipe.write_u32_le(bytes.len() as u32).await?;
        pipe.write_all(bytes).await
    }

    fn validate_reply(reply: &Reply, data_dir: &Path) -> Result<()> {
        ensure!(
            reply.app == "moasearch"
                && reply.version == env!("CARGO_PKG_VERSION")
                && reply.protocol == PROTOCOL,
            "실행 중인 모아검색 앱의 연결 규격이 다릅니다."
        );
        ensure!(
            crate::config::within(data_dir, Path::new(&reply.data_dir))
                && crate::config::within(Path::new(&reply.data_dir), data_dir),
            "다른 데이터 폴더의 모아검색이 실행 중입니다."
        );
        Ok(())
    }

    fn pipe_name(data_dir: &Path) -> Result<String> {
        let sid = current_user_sid()?;
        let user_hash = sha256_hex(sid.as_bytes());
        let path = data_dir
            .to_str()
            .context("앱 데이터 경로가 UTF-8이 아닙니다.")?
            .to_lowercase();
        let data_hash = sha256_hex(path.as_bytes());
        Ok(format!(
            r"\\.\pipe\moasearch-{}-{}",
            &user_hash[..16],
            &data_hash[..24]
        ))
    }

    fn sha256_hex(value: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        Sha256::digest(value)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn security_attributes() -> Result<(OwnedSecurityDescriptor, SECURITY_ATTRIBUTES)> {
        let sid = current_user_sid()?;
        let sddl: Vec<u16> = format!("D:P(A;;GA;;;SY)(A;;GA;;;{sid})")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
        }
        .context("앱 연결 통로 보안 ACL을 만들지 못했습니다.")?;
        let owner = OwnedSecurityDescriptor(descriptor);
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: false.into(),
        };
        Ok((owner, attributes))
    }

    struct OwnedSecurityDescriptor(PSECURITY_DESCRIPTOR);
    impl Drop for OwnedSecurityDescriptor {
        fn drop(&mut self) {
            unsafe { LocalFree(Some(HLOCAL(self.0.0))) };
        }
    }

    struct OwnedHandle(windows::Win32::Foundation::HANDLE);
    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

    fn current_user_sid() -> Result<String> {
        let mut token = windows::Win32::Foundation::HANDLE::default();
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
            .context("현재 Windows 사용자 SID를 확인하지 못했습니다.")?;
        let token = OwnedHandle(token);
        let mut bytes_needed = 0;
        let _ = unsafe { GetTokenInformation(token.0, TokenUser, None, 0, &mut bytes_needed) };
        ensure!(
            bytes_needed > 0,
            "현재 Windows 사용자 SID 크기를 확인하지 못했습니다."
        );
        let mut storage =
            vec![0usize; (bytes_needed as usize).div_ceil(std::mem::size_of::<usize>())];
        unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                Some(storage.as_mut_ptr().cast()),
                bytes_needed,
                &mut bytes_needed,
            )
        }
        .context("현재 Windows 사용자 SID를 읽지 못했습니다.")?;
        let user = unsafe { &*storage.as_ptr().cast::<TOKEN_USER>() };
        let mut string_sid = PWSTR::null();
        unsafe { ConvertSidToStringSidW(user.User.Sid, &mut string_sid) }
            .context("현재 Windows 사용자 SID를 변환하지 못했습니다.")?;
        let sid = unsafe {
            let mut length = 0usize;
            while *string_sid.0.add(length) != 0 {
                length += 1;
            }
            String::from_utf16(std::slice::from_raw_parts(string_sid.0, length))?
        };
        unsafe { LocalFree(Some(HLOCAL(string_sid.0.cast()))) };
        Ok(sid)
    }

    fn verify_server_process(
        pipe: &tokio::net::windows::named_pipe::NamedPipeClient,
    ) -> io::Result<()> {
        let mut pid = 0u32;
        let handle = windows::Win32::Foundation::HANDLE(pipe.as_raw_handle() as *mut c_void);
        unsafe { GetNamedPipeServerProcessId(handle, &mut pid) }.map_err(io::Error::other)?;
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
            .map_err(io::Error::other)?;
        let process = OwnedHandle(process);
        let mut path = vec![0u16; 32768];
        let mut length = path.len() as u32;
        unsafe {
            QueryFullProcessImageNameW(
                process.0,
                Default::default(),
                PWSTR(path.as_mut_ptr()),
                &mut length,
            )
        }
        .map_err(io::Error::other)?;
        path.truncate(length as usize);
        let path = PathBuf::from(String::from_utf16(&path).map_err(io::Error::other)?);
        let server_path = crate::config::canonical(&path).map_err(io::Error::other)?;
        let own_path = std::env::current_exe()?;
        let own_path = crate::config::canonical(&own_path).map_err(io::Error::other)?;
        if !server_path
            .to_string_lossy()
            .eq_ignore_ascii_case(&own_path.to_string_lossy())
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "모아검색이 아닌 앱 연결 통로입니다.",
            ));
        }
        Ok(())
    }

    fn is_not_found(error: &io::Error) -> bool {
        error.kind() == io::ErrorKind::NotFound
    }

    fn is_transient(error: &io::Error) -> bool {
        error.raw_os_error() == Some(231)
            || matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
            )
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        fn data_directory(root: &Path, name: &str) -> PathBuf {
            let path = root.join(name);
            std::fs::create_dir_all(&path).unwrap();
            path
        }

        #[test]
        fn stalled_clients_are_bounded_and_do_not_stop_the_instance() -> Result<()> {
            let temp = tempfile::tempdir()?;
            let data = data_directory(temp.path(), "bounded-clients");
            let instance = Instance::bind(&data, Arc::new(|_| Ok(())))?;
            let name = pipe_name(&crate::config::canonical(&data)?)?;
            let runtime = Builder::new_current_thread().enable_all().build()?;
            let clients = runtime.block_on(async {
                let mut clients = Vec::new();
                for _ in 0..MAX_CLIENTS * 2 {
                    let opened = timeout(Duration::from_millis(150), async {
                        loop {
                            match ClientOptions::new().open(&name) {
                                Err(error) if is_transient(&error) => {
                                    tokio::time::sleep(Duration::from_millis(1)).await;
                                }
                                result => break result,
                            }
                        }
                    })
                    .await;
                    match opened {
                        Ok(Ok(client)) => clients.push(client),
                        Ok(Err(error)) => return Err(error),
                        Err(_) => break,
                    }
                }
                Ok(clients)
            })?;
            // One additional connection may wait on the next listening pipe.
            ensure!(
                clients.len() <= MAX_CLIENTS + 1,
                "대기 연결 수가 제한을 넘었습니다."
            );
            drop(clients);
            ensure!(
                request(&data, &Request::Status)?.is_some(),
                "대기 연결 해제 후 앱에 다시 연결하지 못했습니다."
            );
            drop(instance);
            Ok(())
        }

        #[test]
        fn named_pipe_identity_scope_lifecycle_and_payload_boundaries() -> Result<()> {
            let temp = tempfile::tempdir()?;
            let data = data_directory(temp.path(), "data-one");
            let other_data = data_directory(temp.path(), "data-two");
            let valid_folder = data_directory(temp.path(), "한글 %_ # & 폴더");
            let missing_folder = temp.path().join("없는 폴더");
            let calls = Arc::new(Mutex::new(Vec::<InstanceCommand>::new()));
            let recorded = calls.clone();
            let handler: InstanceHandler = Arc::new(move |command| {
                recorded.lock().unwrap().push(command);
                Ok(())
            });

            let first = Instance::bind(&data, handler.clone())?;
            let initial = request(&data, &Request::Status)?
                .context("첫 named pipe 인스턴스를 찾지 못했습니다.")?;
            ensure!(
                !initial.ready && initial.accepted,
                "초기 앱 상태가 올바르지 않습니다."
            );
            first.mark_ready();
            let ready =
                request(&data, &Request::Status)?.context("ready 응답을 받지 못했습니다.")?;
            ensure!(
                ready.ready,
                "창 준비 상태가 named pipe 응답에 반영되지 않았습니다."
            );

            ensure!(
                Instance::bind(&data, handler.clone()).is_err(),
                "같은 사용자와 데이터 위치에 두 번째 서버 인스턴스를 만들었습니다."
            );

            let valid = request(
                &data,
                &Request::Open {
                    folder: Some(valid_folder.clone()),
                },
            )?
            .context("유효한 폴더 요청을 받지 못했습니다.")?;
            ensure!(valid.accepted, "유효한 폴더가 거절되었습니다.");
            let invalid = request(
                &data,
                &Request::Open {
                    folder: Some(missing_folder),
                },
            )?
            .context("잘못된 폴더 응답을 받지 못했습니다.")?;
            ensure!(!invalid.accepted, "존재하지 않는 폴더 요청을 수락했습니다.");
            ensure!(
                calls.lock().unwrap().len() == 1,
                "잘못된 경로가 창 핸들러에 전달되었습니다."
            );
            let received = {
                let recorded = calls.lock().unwrap();
                match recorded.first() {
                    Some(InstanceCommand::Open(Some(received))) => received.clone(),
                    _ => bail!("폴더 창 명령이 핸들러에 전달되지 않았습니다."),
                }
            };
            ensure!(
                crate::config::within(&received, &valid_folder)
                    && crate::config::within(&valid_folder, &received),
                "창 핸들러에 정규화되지 않은 경로를 전달했습니다."
            );

            let isolated = Instance::bind(&other_data, handler.clone())?;
            let other_status = request(&other_data, &Request::Status)?
                .context("별도 데이터 위치의 인스턴스를 찾지 못했습니다.")?;
            ensure!(
                other_status.data_dir != ready.data_dir,
                "데이터 위치별 named pipe가 격리되지 않았습니다."
            );

            let empty_path = PathBuf::new();
            let overhead = serde_json::to_vec(&Request::Open {
                folder: Some(empty_path.clone()),
            })?
            .len();
            let boundary_path = PathBuf::from("a".repeat(MAX_FRAME - overhead));
            let boundary_request = Request::Open {
                folder: Some(boundary_path),
            };
            ensure!(
                serde_json::to_vec(&boundary_request)?.len() == MAX_FRAME,
                "경계 크기 fixture가 정확히 제한과 일치하지 않습니다."
            );
            ensure!(
                request(&data, &boundary_request)?.is_some(),
                "제한 크기 요청을 처리하지 못했습니다."
            );
            let oversized = Request::Open {
                folder: Some(PathBuf::from("a".repeat(MAX_FRAME - overhead + 1))),
            };
            ensure!(
                request(&data, &oversized).is_err(),
                "제한을 넘는 앱 요청을 전송했습니다."
            );

            drop(isolated);
            drop(first);
            let rebound = Instance::bind(&data, handler)?;
            ensure!(
                request(&data, &Request::Status)?.is_some(),
                "서버 종료 후 같은 데이터 위치에 다시 바인딩하지 못했습니다."
            );
            drop(rebound);
            Ok(())
        }
    }
}

#[cfg(windows)]
pub use windows_pipe::Instance;

#[cfg(windows)]
pub fn request(data_dir: &std::path::Path, request: &Request) -> Result<Option<Reply>> {
    windows_pipe::request(data_dir, request)
}
