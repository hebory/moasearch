//! Explorer handoff and per-data-folder desktop-instance reuse.

use anyhow::{Context, Result, bail, ensure};
use moasearch::{
    config,
    instance::{self, Reply, Request},
    search::canonical_folder,
};
use serde_json::{Value, json};
use std::{
    fs::OpenOptions,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

// A detached app must not keep the Explorer caller's redirected pipes alive.
#[cfg(windows)]
struct StandardHandleInheritance(Vec<(windows::Win32::Foundation::HANDLE, u32)>);
#[cfg(windows)]
impl StandardHandleInheritance {
    fn disable() -> Result<Self> {
        use windows::Win32::{
            Foundation::{
                GetHandleInformation, HANDLE_FLAG_INHERIT, HANDLE_FLAGS, SetHandleInformation,
            },
            System::Console::{
                GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
            },
        };
        let mut guard = Self(Vec::new());
        for kind in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            if let Ok(handle) = unsafe { GetStdHandle(kind) } {
                let mut flags = 0;
                if unsafe { GetHandleInformation(handle, &mut flags) }.is_ok()
                    && flags & HANDLE_FLAG_INHERIT.0 != 0
                {
                    unsafe {
                        SetHandleInformation(handle, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0))
                    }?;
                    guard.0.push((handle, flags));
                }
            }
        }
        Ok(guard)
    }
}
#[cfg(windows)]
impl Drop for StandardHandleInheritance {
    fn drop(&mut self) {
        use windows::Win32::Foundation::{HANDLE_FLAG_INHERIT, HANDLE_FLAGS, SetHandleInformation};
        for &(handle, flags) in &self.0 {
            let _ =
                unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(flags)) };
        }
    }
}

pub fn open_window(data_dir: &Path, folder: Option<&Path>) -> Result<Value> {
    let data_dir = config::canonical(data_dir)?;
    let folder = folder.map(canonical_folder).transpose()?;
    let existing = instance::request(&data_dir, &Request::Status)?;
    let reused = existing.is_some();

    if existing.is_none() {
        let child = spawn_desktop(&data_dir, folder.as_deref())?;
        wait_until_ready(&data_dir, Some(child))?;
    } else if !existing.as_ref().unwrap().ready {
        wait_until_ready(&data_dir, None)?;
    }

    let reply = request_existing(
        &data_dir,
        Request::Open {
            folder: folder.clone(),
        },
    )?;
    Ok(json!({
        "app":"moasearch",
        "folder":folder,
        "reused":reused,
        "desktop":true,
        "pid":reply.pid
    }))
}

pub fn running_instance(data_dir: &Path) -> Result<Option<Value>> {
    instance::request(data_dir, &Request::Status)?
        .map(|reply| serde_json::to_value(reply).map_err(Into::into))
        .transpose()
}

pub fn quit(data_dir: &Path) -> Result<Value> {
    match instance::request(data_dir, &Request::Quit)? {
        Some(reply) => Ok(serde_json::to_value(reply)?),
        None => Ok(json!({"app":"moasearch","running":false,"accepted":false})),
    }
}

fn request_existing(data_dir: &Path, request: Request) -> Result<Reply> {
    let reply = instance::request(data_dir, &request)?;
    let reply = reply.context("모아검색 앱이 실행 중이 아닙니다.")?;
    ensure!(
        reply.accepted,
        "{}",
        reply
            .detail
            .unwrap_or_else(|| "모아검색 요청을 처리하지 못했습니다.".into())
    );
    Ok(reply)
}

fn wait_until_ready(data_dir: &Path, child: Option<Child>) -> Result<Reply> {
    let mut child = SpawnedChild(child);
    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        if let Some(reply) = instance::request(data_dir, &Request::Status)? {
            if reply.ready {
                ensure!(
                    reply.accepted,
                    "{}",
                    reply
                        .detail
                        .unwrap_or_else(|| "앱 상태를 확인하지 못했습니다.".into())
                );
                child.0.take(); // Dropping the child handle leaves the app running.
                return Ok(reply);
            }
        } else if let Some(process) = child.0.as_mut() {
            if let Some(status) = process.try_wait()? {
                bail!(
                    "모아검색 데스크톱 앱이 시작되지 않았습니다 ({status}). 데이터 폴더의 instance.log를 확인하세요."
                );
            }
        }
        ensure!(
            Instant::now() < deadline,
            "모아검색 앱 준비 시간이 초과되었습니다."
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

struct SpawnedChild(Option<Child>);
impl Drop for SpawnedChild {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            if matches!(child.try_wait(), Ok(None)) {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

fn spawn_desktop(data_dir: &Path, folder: Option<&Path>) -> Result<Child> {
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(data_dir.join("instance.log"))
        .context("앱 실행 로그 파일을 열지 못했습니다.")?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("--data-dir")
        .arg(data_dir)
        .args(["desktop", "--background", "--no-dialog"])
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    if let Some(folder) = folder {
        command.arg("--folder").arg(folder);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    #[cfg(windows)]
    let standard_handle_inheritance = StandardHandleInheritance::disable()?;
    let child = command
        .spawn()
        .context("모아검색 앱을 실행하지 못했습니다.")?;
    #[cfg(windows)]
    drop(standard_handle_inheritance);
    Ok(child)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_folder_is_canonicalized_before_handoff() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let folder = temp.path().join("선택 %_ # & 폴더");
        std::fs::create_dir_all(&folder)?;
        assert_eq!(canonical_folder(&folder)?, config::canonical(&folder)?);
        Ok(())
    }
}
