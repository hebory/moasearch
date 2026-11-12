//! Explorer handoff: reuse the matching local server, or start one without a console.
use anyhow::{Context, Result, bail, ensure};
use moasearch::{config, search::canonical_folder};
use reqwest::{blocking::Client, redirect::Policy};
use serde_json::{Value, json};
use std::{
    fs::OpenOptions,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

// A detached server must not keep the invoking process's redirected pipes alive.
// Windows otherwise inherits them even though its own stdout/stderr use a log.
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

pub(crate) fn folder_url(port: u16, folder: &Path) -> Result<String> {
    let path = folder
        .to_str()
        .context("폴더 경로를 UTF-8로 읽을 수 없습니다.")?;
    let mut encoded = String::new();
    for byte in path.as_bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(byte) {
            encoded.push(*byte as char);
        } else {
            use std::fmt::Write;
            write!(encoded, "%{byte:02X}")?;
        }
    }
    // Each app window keeps its own scope; fragments are not sent with HTTP requests.
    Ok(format!("http://127.0.0.1:{port}/#folder={encoded}"))
}

struct Instance {
    info: Value,
    token: String,
}

fn client() -> Result<Client> {
    Ok(Client::builder()
        .no_proxy()
        .redirect(Policy::none())
        .connect_timeout(Duration::from_millis(500))
        .timeout(Duration::from_secs(2))
        .build()?)
}

fn show_window(
    client: &Client,
    port: u16,
    instance: &Instance,
    folder: Option<&Path>,
) -> Result<()> {
    ensure!(
        instance.info["desktop"] == true,
        "같은 포트에서 창 없는 개발 서버가 실행 중입니다. 개발 서버를 종료한 뒤 모아검색을 실행하세요."
    );
    let response = client
        .post(format!("http://127.0.0.1:{port}/api/window"))
        .header("X-MoaSearch-Token", &instance.token)
        .json(&json!({"folder":folder}))
        .send()?;
    ensure!(
        response.status().is_success(),
        "모아검색 창을 열지 못했습니다: {}",
        response.text()?
    );
    Ok(())
}

pub fn focus_existing(data_dir: &Path, folder: Option<&Path>, port: u16) -> Result<bool> {
    let client = client()?;
    if let Some(instance) = probe(&client, port, data_dir)? {
        show_window(&client, port, &instance, folder)?;
        return Ok(true);
    }
    Ok(false)
}

fn probe(client: &Client, port: u16, data_dir: &Path) -> Result<Option<Instance>> {
    let base = format!("http://127.0.0.1:{port}");
    let page = match client.get(&base).send() {
        Ok(response) => response,
        Err(error) if error.is_connect() => return Ok(None),
        Err(error) => return Err(error).context("실행 중인 앱에 연결할 수 없습니다."),
    };
    ensure!(
        page.status().is_success(),
        "포트 {port}를 다른 프로그램이 사용 중입니다."
    );
    let html = page.text()?;
    let token = html
        .split("name=\"moasearch-token\" content=\"")
        .nth(1)
        .and_then(|tail| tail.split('"').next())
        .filter(|token| token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit()))
        .context("이 포트의 프로그램은 모아검색이 아닙니다. 실행 중인 앱을 확인하세요.")?;
    let response = client
        .get(format!("{base}/api/instance"))
        .header("X-MoaSearch-Token", token)
        .send()?;
    ensure!(
        response.status().is_success(),
        "실행 중인 모아검색이 폴더 검색을 지원하지 않습니다. 이전 앱을 종료한 뒤 다시 실행하세요."
    );
    let instance: Value = response.json()?;
    ensure!(
        instance["app"] == "moasearch" && instance["folder_protocol"] == 1,
        "실행 중인 앱과 폴더 검색 연결 규격이 다릅니다."
    );
    let running_data = instance["data_dir"]
        .as_str()
        .context("앱 데이터 위치를 확인하지 못했습니다.")?;
    ensure!(
        config::within(data_dir, Path::new(running_data))
            && config::within(Path::new(running_data), data_dir),
        "다른 데이터 위치를 사용하는 모아검색이 포트 {port}에서 실행 중입니다. 해당 앱을 종료하거나 같은 데이터 위치를 지정하세요."
    );
    Ok(Some(Instance {
        info: instance,
        token: token.to_owned(),
    }))
}

pub fn open_folder(data_dir: &Path, path: &Path, port: u16, no_browser: bool) -> Result<Value> {
    let folder = canonical_folder(path)?;
    let url = folder_url(port, &folder)?;
    let client = client()?;
    let mut instance = probe(&client, port, data_dir)?;
    let reused = instance.is_some();
    if instance.is_none() {
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(data_dir.join("server.log"))?;
        let mut command = Command::new(std::env::current_exe()?);
        command
            .arg("--data-dir")
            .arg(data_dir)
            .args([
                if no_browser { "serve" } else { "desktop" },
                "--port",
                &port.to_string(),
            ])
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        if !no_browser {
            command.arg("--folder").arg(&folder).arg("--background");
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000); // CREATE_NO_WINDOW
        }
        #[cfg(windows)]
        let standard_handle_inheritance = StandardHandleInheritance::disable()?;
        let mut child = command
            .spawn()
            .context("모아검색 서버를 시작하지 못했습니다.")?;
        #[cfg(windows)]
        drop(standard_handle_inheritance);
        let started = (|| -> Result<Instance> {
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                if let Some(instance) = probe(&client, port, data_dir)? {
                    if no_browser || instance.info["desktop"] == true {
                        return Ok(instance);
                    }
                }
                if let Some(status) = child.try_wait()? {
                    // Another simultaneous Explorer invocation can have won the bind.
                    if let Some(instance) = probe(&client, port, data_dir)? {
                        if no_browser || instance.info["desktop"] == true {
                            return Ok(instance);
                        }
                        // A simultaneous launch can still be constructing its first WebView.
                        ensure!(
                            Instant::now() < deadline,
                            "모아검색 창 준비 시간이 초과되었습니다."
                        );
                        std::thread::sleep(Duration::from_millis(100));
                        continue;
                    }
                    bail!(
                        "모아검색 서버가 종료되었습니다 ({status}). 데이터 폴더의 server.log를 확인하세요."
                    );
                }
                ensure!(
                    Instant::now() < deadline,
                    "모아검색 시작 시간이 초과되었습니다. 데이터 폴더의 server.log를 확인하세요."
                );
                std::thread::sleep(Duration::from_millis(100));
            }
        })();
        match started {
            Ok(value) => instance = Some(value),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        }
    }
    let instance = instance.unwrap();
    if !no_browser {
        show_window(&client, port, &instance, Some(&folder))?;
    }
    Ok(
        json!({"url":url,"folder":folder,"reused":reused,"desktop":instance.info["desktop"],"pid":instance.info["pid"]}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explorer_url_round_trips_special_characters_without_query_injection() -> Result<()> {
        for path in [
            r"C:\",
            r"C:\한글 폴더\100%_계약 + # &=",
            r"\\server\share\문서",
        ] {
            let url = reqwest::Url::parse(&folder_url(18765, Path::new(path))?)?;
            assert_eq!(url.query(), None);
            let decoded =
                reqwest::Url::parse(&format!("http://localhost/?{}", url.fragment().unwrap()))?;
            let pairs: Vec<_> = decoded.query_pairs().collect();
            assert_eq!(pairs.len(), 1);
            assert_eq!(pairs[0].0, "folder");
            assert_eq!(pairs[0].1, path);
        }
        Ok(())
    }
}
