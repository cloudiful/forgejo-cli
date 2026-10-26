use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use forgejo_api::Forgejo;

use crate::repo::RepoName;

#[derive(Clone)]
pub(super) struct MockResponse {
    status: u16,
    body: Vec<u8>,
    content_type: &'static str,
}

impl MockResponse {
    pub(super) fn json(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            body: body.into(),
            content_type: "application/json",
        }
    }

    pub(super) fn text(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            body: body.into(),
            content_type: "text/plain",
        }
    }
}

pub(super) struct Captured {
    method: String,
    path: String,
    query: Option<String>,
}

pub(super) type CapturedList = Arc<Mutex<Vec<Captured>>>;
pub(super) type Routes = Arc<Mutex<HashMap<String, Vec<MockResponse>>>>;

pub(super) struct MockServer {
    pub(super) addr: std::net::SocketAddr,
    captured: CapturedList,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl MockServer {
    pub(super) fn start(routes: Vec<(&'static str, Vec<MockResponse>)>) -> Self {
        let mut map: HashMap<String, Vec<MockResponse>> = HashMap::new();
        for (path, responses) in routes {
            map.insert(path.to_owned(), responses);
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let captured: CapturedList = Arc::new(Mutex::new(Vec::new()));
        let routes: Routes = Arc::new(Mutex::new(map));
        let stop = Arc::new(AtomicBool::new(false));
        let captured_clone = Arc::clone(&captured);
        let routes_clone = Arc::clone(&routes);
        let stop_clone = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline && !stop_clone.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = handle_one(stream, &captured_clone, &routes_clone);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            addr,
            captured,
            stop,
            handle: Some(handle),
        }
    }

    pub(super) fn requests(&self) -> Vec<(String, String, Option<String>)> {
        self.captured
            .lock()
            .unwrap()
            .iter()
            .map(|captured| {
                (
                    captured.method.clone(),
                    captured.path.clone(),
                    captured.query.clone(),
                )
            })
            .collect()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

pub(super) fn handle_one(
    mut stream: std::net::TcpStream,
    captured: &CapturedList,
    routes: &Routes,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    loop {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let header_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "no headers"))?
        + 4;
    let header_str = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let request_line = header_str
        .lines()
        .next()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "no request"))?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let target = parts.next().unwrap_or_default().to_owned();
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_owned(), Some(query.to_owned())),
        None => (target, None),
    };

    captured.lock().unwrap().push(Captured {
        method,
        path: path.clone(),
        query: query.clone(),
    });

    let response = {
        let mut routes = routes.lock().unwrap();
        match routes.get_mut(&path) {
            Some(responses) if !responses.is_empty() => {
                if responses.len() == 1 {
                    responses[0].clone()
                } else {
                    responses.remove(0)
                }
            }
            _ => MockResponse::json(404, br#"{"message":"not found"}"#.to_vec()),
        }
    };

    let reason = match response.status {
        200 => "OK",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Error",
    };
    let header = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.status,
        reason,
        response.content_type,
        response.body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(&response.body)?;
    stream.flush()?;
    Ok(())
}

pub(super) fn test_api(addr: std::net::SocketAddr) -> Forgejo {
    let url = url::Url::parse(&format!("http://{addr}/")).unwrap();
    Forgejo::with_user_agent(forgejo_api::Auth::None, url, crate::USER_AGENT).unwrap()
}

pub(super) fn test_repo() -> RepoName {
    RepoName::new("o".to_owned(), "r".to_owned())
}

pub(super) fn init_render() {
    let _ = crate::SPECIAL_RENDER.set(crate::SpecialRender::new(crate::Style::Minimal));
}

pub(super) fn run_json(id: i64, number: i64, status: &str) -> String {
    format!(
        r#"{{"id":{id},"index_in_repo":{number},"status":"{status}","workflow_id":"ci.yml","commit_sha":"0123456789abcdef","event":"push","title":"Some title"}}"#
    )
}

pub(super) fn runs_response(runs: &[String], total: i64) -> Vec<u8> {
    format!(
        r#"{{"total_count":{total},"workflow_runs":[{}]}}"#,
        runs.join(",")
    )
    .into_bytes()
}

pub(super) fn job_json(id: i64, name: &str, status: &str, steps: &str) -> String {
    format!(r#"{{"id":{id},"name":"{name}","status":"{status}","run_id":900,"steps":{steps}}}"#)
}

pub(super) fn jobs_response(jobs: &[String]) -> Vec<u8> {
    format!("[{}]", jobs.join(",")).into_bytes()
}

pub(super) fn step_json(number: i64, name: &str, status: &str) -> String {
    format!(r#"{{"number":{number},"name":"{name}","status":"{status}"}}"#)
}

pub(super) const RUN_PATH: &str = "/api/v1/repos/o/r/actions/runs";
pub(super) const RUN_900_PATH: &str = "/api/v1/repos/o/r/actions/runs/900";
pub(super) const RUN_900_JOBS_PATH: &str = "/api/v1/repos/o/r/actions/runs/900/jobs";
pub(super) const JOB_5_PATH: &str = "/api/v1/repos/o/r/actions/jobs/5";
pub(super) const JOB_5_LOGS_PATH: &str = "/api/v1/repos/o/r/actions/jobs/5/logs";
