use std::path::{Path, PathBuf};

use eyre::Context;
use forgejo_api::{Forgejo, structs::IssueCreateIssueAttachmentQuery};

use crate::{ftl_bail, ftl_format, repo::RepoName};

pub(super) async fn attach_files(
    repo: &RepoName,
    api: &Forgejo,
    pr_number: i64,
    files: &[PathBuf],
    out: &mut dyn std::io::Write,
) -> eyre::Result<()> {
    let mut basenames = Vec::with_capacity(files.len());
    for path in files {
        if path == Path::new("-") {
            crate::ftl_bail!("msg-pr-attach-stdin_unsupported");
        }
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                crate::ftl_eyre!(
                    "msg-pr-attach-invalid_filename",
                    file = path.to_string_lossy().into_owned()
                )
            })?;
        tokio::fs::metadata(path)
            .await
            .wrap_err_with(|| {
                crate::ftl_format!(
                    "msg-pr-attach-access_failed",
                    file = path.to_string_lossy().into_owned()
                )
                .into_owned()
            })?
            .file_type()
            .is_file()
            .then_some(())
            .ok_or_else(|| {
                crate::ftl_eyre!(
                    "msg-pr-attach-not_regular",
                    file = path.to_string_lossy().into_owned()
                )
            })?;
        basenames.push(name.to_owned());
    }
    api.repo_get_pull_request(repo.owner(), repo.name(), pr_number)
        .await
        .wrap_err_with(|| {
            crate::ftl_format!("msg-pr-attach-pr_verify_failed", number = pr_number).into_owned()
        })?;
    let mut succeeded: usize = 0;
    for (path, basename) in files.iter().zip(basenames.iter()) {
        let step: eyre::Result<()> = async {
            let bytes = tokio::fs::read(path).await.wrap_err_with(|| {
                crate::ftl_format!(
                    "msg-pr-attach-read_failed",
                    file = path.to_string_lossy().into_owned()
                )
                .into_owned()
            })?;
            let query = IssueCreateIssueAttachmentQuery {
                name: Some(basename.clone()),
                updated_at: None,
            };
            let attachment = api
                .issue_create_issue_attachment(repo.owner(), repo.name(), pr_number, &bytes, query)
                .await
                .wrap_err_with(|| {
                    crate::ftl_format!(
                        "msg-pr-attach-upload_failed",
                        file = path.to_string_lossy().into_owned()
                    )
                    .into_owned()
                })?;
            let url = attachment
                .browser_download_url
                .as_ref()
                .filter(|u| !u.as_str().is_empty())
                .ok_or_else(|| {
                    crate::ftl_eyre!(
                        "msg-pr-attach-missing_url",
                        file = path.to_string_lossy().into_owned()
                    )
                })?;
            if !(url.scheme() == "http" || url.scheme() == "https")
                || url.host_str().is_none()
                || url.as_str().contains('<')
                || url.as_str().contains('>')
                || url.as_str().contains('\n')
                || url.as_str().contains('\r')
            {
                ftl_bail!(
                    "msg-pr-attach-invalid_url",
                    file = path.to_string_lossy().into_owned()
                );
            }
            let url_str = url.as_str();
            let raw_label = attachment
                .name
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(basename);
            let label = escape_label(raw_label);
            writeln!(out, "[{label}](<{url_str}>)").wrap_err_with(|| {
                ftl_format!(
                    "msg-pr-attach-write_failed",
                    file = path.to_string_lossy().into_owned()
                )
                .into_owned()
            })?;
            out.flush().wrap_err_with(|| {
                ftl_format!(
                    "msg-pr-attach-write_failed",
                    file = path.to_string_lossy().into_owned()
                )
                .into_owned()
            })?;
            Ok(())
        }
        .await;
        match step {
            Ok(()) => succeeded += 1,
            Err(e) => {
                if succeeded > 0 {
                    return Err(e.wrap_err(
                        ftl_format!("msg-pr-attach-partial", succeeded = succeeded as i64)
                            .into_owned(),
                    ));
                }
                return Err(e);
            }
        }
    }
    Ok(())
}

fn escape_label(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        match c {
            '\\' | '[' | ']' => {
                out.push('\\');
                out.push(c);
            }
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::io::{Read, Write};
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::{Duration, Instant};

    struct Captured {
        method: String,
        path: String,
        query: Option<String>,
        content_type: Option<String>,
        body: Vec<u8>,
    }

    type PostResponse = (u16, Vec<u8>);
    type PostQueue = Arc<Mutex<VecDeque<PostResponse>>>;
    type RequestSummary = (String, String, Option<String>, Vec<u8>, Option<String>);

    struct Server {
        addr: std::net::SocketAddr,
        captured: Arc<Mutex<Vec<Captured>>>,
        stop: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl Server {
        fn start(pr_status: u16, pr_body: Vec<u8>, posts: Vec<PostResponse>) -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let addr = listener.local_addr().unwrap();
            let captured: Arc<Mutex<Vec<Captured>>> = Arc::new(Mutex::new(Vec::new()));
            let queue: PostQueue = Arc::new(Mutex::new(posts.into_iter().collect()));
            let pr = Arc::new((pr_status, pr_body));
            let stop = Arc::new(AtomicBool::new(false));
            let captured_clone = Arc::clone(&captured);
            let queue_clone = Arc::clone(&queue);
            let pr_clone = Arc::clone(&pr);
            let stop_clone = Arc::clone(&stop);
            let handle = std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(5);
                while Instant::now() < deadline && !stop_clone.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let _ = handle_one(stream, &captured_clone, &queue_clone, &pr_clone);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(10));
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

        fn requests(&self) -> Vec<RequestSummary> {
            self.captured
                .lock()
                .unwrap()
                .iter()
                .map(|c| {
                    (
                        c.method.clone(),
                        c.path.clone(),
                        c.query.clone(),
                        c.body.clone(),
                        c.content_type.clone(),
                    )
                })
                .collect()
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    fn handle_one(
        mut stream: std::net::TcpStream,
        captured: &Arc<Mutex<Vec<Captured>>>,
        queue: &PostQueue,
        pr: &Arc<PostResponse>,
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
            if buf.len() > 1024 * 1024 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "headers too large",
                ));
            }
        }
        let header_end = buf
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "no headers"))?
            + 4;
        let header_str = String::from_utf8_lossy(&buf[..header_end]).into_owned();
        let mut lines = header_str.lines();
        let request_line = lines
            .next()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "no request"))?;
        let mut parts = request_line.split_whitespace();
        let method = parts
            .next()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "no method"))?
            .to_owned();
        let target = parts
            .next()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "no target"))?
            .to_owned();
        let mut content_length = 0usize;
        let mut content_type = None;
        let mut expect_continue = false;
        for line in lines {
            if line.is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once(':') {
                let k = k.trim().to_ascii_lowercase();
                let v = v.trim();
                if k == "content-length" {
                    content_length = v.parse().unwrap_or(0);
                }
                if k == "content-type" {
                    content_type = Some(v.to_owned());
                }
                if k == "expect" && v.to_ascii_lowercase().contains("100-continue") {
                    expect_continue = true;
                }
            }
        }
        let (path, query) = if target.starts_with("http://") || target.starts_with("https://") {
            match url::Url::parse(&target) {
                Ok(u) => (u.path().to_owned(), u.query().map(|s| s.to_owned())),
                Err(_) => match target.split_once('?') {
                    Some((p, q)) => (p.to_owned(), Some(q.to_owned())),
                    None => (target.clone(), None),
                },
            }
        } else {
            match target.split_once('?') {
                Some((p, q)) => (p.to_owned(), Some(q.to_owned())),
                None => (target.clone(), None),
            }
        };
        if expect_continue {
            stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
            stream.flush()?;
        }
        let mut body = buf[header_end..].to_vec();
        while body.len() < content_length {
            let n = stream.read(&mut tmp)?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&tmp[..n]);
            if body.len() > 8 * 1024 * 1024 {
                break;
            }
        }
        body.truncate(content_length);
        captured.lock().unwrap().push(Captured {
            method: method.clone(),
            path: path.clone(),
            query: query.clone(),
            content_type: content_type.clone(),
            body: body.clone(),
        });
        let (status, resp_body) = if method == "GET" && path.contains("/pulls/") {
            (pr.0, pr.1.clone())
        } else if method == "POST" && path.contains("/issues/") && path.ends_with("/assets") {
            queue
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or((500, br#"{"message":"no more mocked responses"}"#.to_vec()))
        } else {
            (404, br#"{"message":"not found"}"#.to_vec())
        };
        let reason = match status {
            200 => "OK",
            201 => "Created",
            404 => "Not Found",
            500 => "Internal Server Error",
            _ => "OK",
        };
        let header = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            resp_body.len()
        );
        stream.write_all(header.as_bytes())?;
        stream.write_all(&resp_body)?;
        stream.flush()?;
        Ok(())
    }

    fn scratch_base() -> PathBuf {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".cache/ai/ci-release");
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let base = scratch_base();
            for _ in 0..100 {
                let n = COUNTER.fetch_add(1, Ordering::SeqCst);
                let dir = base.join(format!("fj-attach-{}-{n}", std::process::id()));
                match std::fs::create_dir(&dir) {
                    Ok(()) => return Self(dir),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(e) => panic!("cannot create scratch dir: {e:?}"),
                }
            }
            panic!("cannot create unique scratch dir");
        }

        fn path(&self) -> &PathBuf {
            &self.0
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct TempFile {
        _dir: ScratchDir,
        path: PathBuf,
    }

    impl TempFile {
        fn new(name: &str, contents: &[u8]) -> Self {
            assert!(
                !name.contains('/'),
                "fixture name must be a single path component"
            );
            let dir = ScratchDir::new();
            let path = dir.path().join(name);
            std::fs::write(&path, contents).unwrap();
            Self { _dir: dir, path }
        }

        fn path(&self) -> &PathBuf {
            &self.path
        }
    }

    struct MissingPath {
        _dir: ScratchDir,
        path: PathBuf,
    }

    impl MissingPath {
        fn new(name: &str) -> Self {
            let dir = ScratchDir::new();
            let path = dir.path().join(name);
            assert!(!path.exists(), "scratch collision");
            Self { _dir: dir, path }
        }

        fn path(&self) -> &PathBuf {
            &self.path
        }
    }

    fn test_api(addr: std::net::SocketAddr) -> Forgejo {
        let url = url::Url::parse(&format!("http://{addr}/")).unwrap();
        Forgejo::with_user_agent(forgejo_api::Auth::None, url, crate::USER_AGENT).unwrap()
    }

    fn test_repo() -> RepoName {
        RepoName::new("o".to_owned(), "r".to_owned())
    }

    fn query_name(query: Option<String>) -> Option<String> {
        let q = query?;
        let parsed = url::Url::parse(&format!("http://localhost/?{q}")).ok()?;
        for (k, v) in parsed.query_pairs() {
            if k == "name" {
                return Some(v.into_owned());
            }
        }
        None
    }

    fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
        if needle.is_empty() {
            return true;
        }
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    #[tokio::test]
    async fn single_file_upload_emits_link_with_binary_and_name_query() {
        let content: Vec<u8> = vec![0xff, 0xfe, 0x00, b'h', b'i', 0x80, b'\n'];
        let f = TempFile::new("hello.bin", &content);
        let pr_body = br#"{"number":7,"requested_reviewers":null}"#.to_vec();
        let asset_body =
            br#"{"browser_download_url":"https://example.com/attachments/abc","name":"hello.bin"}"#
                .to_vec();
        let server = Server::start(200, pr_body, vec![(201, asset_body)]);
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let res = tokio::time::timeout(
            Duration::from_secs(5),
            attach_files(
                &test_repo(),
                &api,
                7,
                std::slice::from_ref(f.path()),
                &mut out,
            ),
        )
        .await
        .expect("timed out");
        assert!(res.is_ok(), "upload failed: {res:?}");
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text, "[hello.bin](<https://example.com/attachments/abc>)\n");
        let reqs = server.requests();
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[0].0, "GET");
        assert_eq!(reqs[0].1, "/api/v1/repos/o/r/pulls/7");
        assert_eq!(reqs[1].0, "POST");
        assert_eq!(reqs[1].1, "/api/v1/repos/o/r/issues/7/assets");
        let expected_basename = f.path().file_name().unwrap().to_str().unwrap().to_owned();
        assert_eq!(query_name(reqs[1].2.clone()), Some(expected_basename));
        let body = &reqs[1].3;
        assert!(contains_subslice(body, &content));
        let body_str = String::from_utf8_lossy(body);
        assert!(body_str.contains("filename=\"file\""));
        assert!(body_str.contains("name=\"attachment\""));
        assert!(!reqs.iter().any(|(_, p, _, _, _)| p.contains("/comments")));
    }

    #[tokio::test]
    async fn multiple_files_preserve_order() {
        let a = TempFile::new("a.txt", b"aaa");
        let b = TempFile::new("b.txt", b"bbb");
        let pr_body = br#"{"number":7,"requested_reviewers":null}"#.to_vec();
        let posts = vec![
            (
                201,
                br#"{"browser_download_url":"https://example.com/a","name":"a.txt"}"#.to_vec(),
            ),
            (
                201,
                br#"{"browser_download_url":"https://example.com/b","name":"b.txt"}"#.to_vec(),
            ),
        ];
        let server = Server::start(200, pr_body, posts);
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let files = vec![a.path().clone(), b.path().clone()];
        let res = tokio::time::timeout(
            Duration::from_secs(5),
            attach_files(&test_repo(), &api, 7, &files, &mut out),
        )
        .await
        .expect("timed out");
        assert!(res.is_ok(), "{res:?}");
        let text = String::from_utf8(out).unwrap();
        assert_eq!(
            text,
            "[a.txt](<https://example.com/a>)\n[b.txt](<https://example.com/b>)\n"
        );
        let reqs = server.requests();
        assert_eq!(reqs.len(), 3);
        let expected_a = a.path().file_name().unwrap().to_str().unwrap().to_owned();
        let expected_b = b.path().file_name().unwrap().to_str().unwrap().to_owned();
        assert_eq!(query_name(reqs[1].2.clone()), Some(expected_a));
        assert_eq!(query_name(reqs[2].2.clone()), Some(expected_b));
    }

    #[tokio::test]
    async fn label_escaping_uses_returned_name() {
        let f = TempFile::new("plain.txt", b"x");
        let pr_body = br#"{"number":7,"requested_reviewers":null}"#.to_vec();
        let asset_body =
            br#"{"browser_download_url":"https://example.com/u","name":"a[b]\\c"}"#.to_vec();
        let server = Server::start(200, pr_body, vec![(201, asset_body)]);
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let res = tokio::time::timeout(
            Duration::from_secs(5),
            attach_files(
                &test_repo(),
                &api,
                7,
                std::slice::from_ref(f.path()),
                &mut out,
            ),
        )
        .await
        .expect("timed out");
        assert!(res.is_ok(), "{res:?}");
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text, "[a\\[b\\]\\\\c](<https://example.com/u>)\n");
    }

    #[tokio::test]
    async fn fallback_to_basename_when_returned_name_empty() {
        let f = TempFile::new("keep me.txt", b"x");
        let pr_body = br#"{"number":7,"requested_reviewers":null}"#.to_vec();
        let asset_body = br#"{"browser_download_url":"https://example.com/u","name":""}"#.to_vec();
        let server = Server::start(200, pr_body, vec![(201, asset_body)]);
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let res = tokio::time::timeout(
            Duration::from_secs(5),
            attach_files(
                &test_repo(),
                &api,
                7,
                std::slice::from_ref(f.path()),
                &mut out,
            ),
        )
        .await
        .expect("timed out");
        assert!(res.is_ok(), "{res:?}");
        let text = String::from_utf8(out).unwrap();
        let expected_basename = f.path().file_name().unwrap().to_str().unwrap().to_owned();
        assert_eq!(
            text,
            format!("[{expected_basename}](<https://example.com/u>)\n")
        );
    }

    #[tokio::test]
    async fn missing_url_is_error() {
        let f = TempFile::new("m.txt", b"x");
        let server = Server::start(
            200,
            br#"{"number":7,"requested_reviewers":null}"#.to_vec(),
            vec![(201, b"{}".to_vec())],
        );
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let res = tokio::time::timeout(
            Duration::from_secs(5),
            attach_files(
                &test_repo(),
                &api,
                7,
                std::slice::from_ref(f.path()),
                &mut out,
            ),
        )
        .await
        .expect("timed out");
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn invalid_url_scheme_is_error() {
        let f = TempFile::new("m.txt", b"x");
        let asset_body =
            br#"{"browser_download_url":"ftp://example.com/x","name":"m.txt"}"#.to_vec();
        let server = Server::start(
            200,
            br#"{"number":7,"requested_reviewers":null}"#.to_vec(),
            vec![(201, asset_body)],
        );
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let res = tokio::time::timeout(
            Duration::from_secs(5),
            attach_files(
                &test_repo(),
                &api,
                7,
                std::slice::from_ref(f.path()),
                &mut out,
            ),
        )
        .await
        .expect("timed out");
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn preflight_nonexistent_makes_no_requests() {
        let missing = MissingPath::new("missing-file.txt");
        let server = Server::start(
            200,
            br#"{"number":7,"requested_reviewers":null}"#.to_vec(),
            vec![],
        );
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let res = tokio::time::timeout(
            Duration::from_secs(5),
            attach_files(
                &test_repo(),
                &api,
                7,
                std::slice::from_ref(missing.path()),
                &mut out,
            ),
        )
        .await
        .expect("timed out");
        assert!(res.is_err());
        assert!(server.requests().is_empty());
    }

    #[tokio::test]
    async fn preflight_directory_makes_no_requests() {
        let d = ScratchDir::new();
        let server = Server::start(
            200,
            br#"{"number":7,"requested_reviewers":null}"#.to_vec(),
            vec![],
        );
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let res = tokio::time::timeout(
            Duration::from_secs(5),
            attach_files(
                &test_repo(),
                &api,
                7,
                std::slice::from_ref(d.path()),
                &mut out,
            ),
        )
        .await
        .expect("timed out");
        assert!(res.is_err());
        assert!(server.requests().is_empty());
    }

    #[tokio::test]
    async fn preflight_dash_makes_no_requests() {
        let dash = PathBuf::from("-");
        let server = Server::start(
            200,
            br#"{"number":7,"requested_reviewers":null}"#.to_vec(),
            vec![],
        );
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let res = tokio::time::timeout(
            Duration::from_secs(5),
            attach_files(&test_repo(), &api, 7, std::slice::from_ref(&dash), &mut out),
        )
        .await
        .expect("timed out");
        let err = res.unwrap_err();
        let msg = format!("{err:?}");
        assert!(!msg.is_empty());
        assert!(server.requests().is_empty());
    }

    #[tokio::test]
    async fn http_failure_reports_file() {
        let f = TempFile::new("bad.txt", b"x");
        let server = Server::start(
            200,
            br#"{"number":7,"requested_reviewers":null}"#.to_vec(),
            vec![(500, br#"{"message":"boom"}"#.to_vec())],
        );
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let res = tokio::time::timeout(
            Duration::from_secs(5),
            attach_files(
                &test_repo(),
                &api,
                7,
                std::slice::from_ref(f.path()),
                &mut out,
            ),
        )
        .await
        .expect("timed out");
        let err = res.unwrap_err();
        assert!(format!("{err:?}").contains("bad.txt"));
        let reqs = server.requests();
        assert_eq!(reqs.len(), 2);
    }

    #[tokio::test]
    async fn second_file_failure_retains_first_output() {
        let a = TempFile::new("first.txt", b"one");
        let b = TempFile::new("second.txt", b"two");
        let posts = vec![
            (
                201,
                br#"{"browser_download_url":"https://example.com/1","name":"first.txt"}"#.to_vec(),
            ),
            (500, br#"{"message":"boom"}"#.to_vec()),
        ];
        let server = Server::start(
            200,
            br#"{"number":7,"requested_reviewers":null}"#.to_vec(),
            posts,
        );
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let files = vec![a.path().clone(), b.path().clone()];
        let res = tokio::time::timeout(
            Duration::from_secs(5),
            attach_files(&test_repo(), &api, 7, &files, &mut out),
        )
        .await
        .expect("timed out");
        let err = res.unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("second.txt"));
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text, "[first.txt](<https://example.com/1>)\n");
        let reqs = server.requests();
        assert_eq!(reqs.len(), 3);
    }

    #[tokio::test]
    async fn malformed_url_stops_before_second_upload() {
        let a = TempFile::new("malformed-first.txt", b"one");
        let b = TempFile::new("never-second.txt", b"two");
        let posts = vec![
            (
                201,
                br#"{"browser_download_url":"not-a-valid-url","name":"malformed-first.txt"}"#
                    .to_vec(),
            ),
            (
                201,
                br#"{"browser_download_url":"https://example.com/never","name":"never-second.txt"}"#
                    .to_vec(),
            ),
        ];
        let server = Server::start(
            200,
            br#"{"number":7,"requested_reviewers":null}"#.to_vec(),
            posts,
        );
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let files = vec![a.path().clone(), b.path().clone()];
        let res = tokio::time::timeout(
            Duration::from_secs(5),
            attach_files(&test_repo(), &api, 7, &files, &mut out),
        )
        .await
        .expect("timed out");
        let err = res.unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("malformed-first.txt"));
        assert!(msg.contains("may already exist"));
        assert!(out.is_empty());
        let reqs = server.requests();
        assert_eq!(reqs.len(), 2);
    }

    struct FlushFailWriter {
        buf: Vec<u8>,
    }

    impl Write for FlushFailWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.buf.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other("flush boom"))
        }
    }

    #[tokio::test]
    async fn flush_only_failure_stops_before_second_upload() {
        let a = TempFile::new("flush-first.txt", b"one");
        let b = TempFile::new("flush-never.txt", b"two");
        let posts = vec![
            (
                201,
                br#"{"browser_download_url":"https://example.com/flush1","name":"flush-first.txt"}"#
                    .to_vec(),
            ),
            (
                201,
                br#"{"browser_download_url":"https://example.com/flush2","name":"flush-never.txt"}"#
                    .to_vec(),
            ),
        ];
        let server = Server::start(
            200,
            br#"{"number":7,"requested_reviewers":null}"#.to_vec(),
            posts,
        );
        let api = test_api(server.addr);
        let mut out = FlushFailWriter { buf: Vec::new() };
        let files = vec![a.path().clone(), b.path().clone()];
        let res = tokio::time::timeout(
            Duration::from_secs(5),
            attach_files(&test_repo(), &api, 7, &files, &mut out),
        )
        .await
        .expect("timed out");
        let err = res.unwrap_err();
        assert!(format!("{err:?}").contains("flush-first.txt"));
        assert_eq!(
            String::from_utf8(out.buf).unwrap(),
            "[flush-first.txt](<https://example.com/flush1>)\n"
        );
        let reqs = server.requests();
        assert_eq!(reqs.len(), 2);
    }

    #[tokio::test]
    async fn special_character_filename_query_encoding() {
        let special = "sp ace+&=?#%_uniçødé[b].txt";
        let content = b"special-bytes";
        let f = TempFile::new(special, content);
        let asset_body =
            br#"{"browser_download_url":"https://example.com/special","name":"ok.txt"}"#.to_vec();
        let server = Server::start(
            200,
            br#"{"number":7,"requested_reviewers":null}"#.to_vec(),
            vec![(201, asset_body)],
        );
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let res = tokio::time::timeout(
            Duration::from_secs(5),
            attach_files(
                &test_repo(),
                &api,
                7,
                std::slice::from_ref(f.path()),
                &mut out,
            ),
        )
        .await
        .expect("timed out");
        assert!(res.is_ok(), "{res:?}");
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text, "[ok.txt](<https://example.com/special>)\n");
        let reqs = server.requests();
        assert_eq!(reqs.len(), 2);
        assert_eq!(query_name(reqs[1].2.clone()), Some(special.to_owned()));
        assert!(contains_subslice(&reqs[1].3, content));
        assert!(!reqs.iter().any(|(_, p, _, _, _)| p.contains("/comments")));
    }

    #[tokio::test]
    async fn later_invalid_file_preflight_makes_no_requests() {
        let valid = TempFile::new("valid-first.txt", b"ok");
        let missing = MissingPath::new("missing-second.txt");
        let server = Server::start(
            200,
            br#"{"number":7,"requested_reviewers":null}"#.to_vec(),
            vec![],
        );
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let files = vec![valid.path().clone(), missing.path().clone()];
        let res = tokio::time::timeout(
            Duration::from_secs(5),
            attach_files(&test_repo(), &api, 7, &files, &mut out),
        )
        .await
        .expect("timed out");
        assert!(res.is_err());
        assert!(out.is_empty());
        assert!(server.requests().is_empty());
    }

    struct FailWriter;

    impl Write for FailWriter {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("writer boom"))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other("flush boom"))
        }
    }

    #[tokio::test]
    async fn writer_error_is_reported() {
        let f = TempFile::new("w.txt", b"x");
        let asset_body =
            br#"{"browser_download_url":"https://example.com/u","name":"w.txt"}"#.to_vec();
        let server = Server::start(
            200,
            br#"{"number":7,"requested_reviewers":null}"#.to_vec(),
            vec![(201, asset_body)],
        );
        let api = test_api(server.addr);
        let mut out = FailWriter;
        let res = tokio::time::timeout(
            Duration::from_secs(5),
            attach_files(
                &test_repo(),
                &api,
                7,
                std::slice::from_ref(f.path()),
                &mut out,
            ),
        )
        .await
        .expect("timed out");
        assert!(res.is_err());
        assert_eq!(server.requests().len(), 2);
    }

    #[tokio::test]
    async fn pr_not_found_makes_no_upload() {
        let f = TempFile::new("x.txt", b"x");
        let server = Server::start(
            404,
            br#"{"message":"not found"}"#.to_vec(),
            vec![(
                201,
                br#"{"browser_download_url":"https://example.com/u","name":"x.txt"}"#.to_vec(),
            )],
        );
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let res = tokio::time::timeout(
            Duration::from_secs(5),
            attach_files(
                &test_repo(),
                &api,
                7,
                std::slice::from_ref(f.path()),
                &mut out,
            ),
        )
        .await
        .expect("timed out");
        assert!(res.is_err());
        let reqs = server.requests();
        assert_eq!(reqs.len(), 1);
        assert!(reqs[0].1.contains("/pulls/"));
    }

    #[test]
    fn escape_label_cases() {
        assert_eq!(escape_label("a[b]c"), "a\\[b\\]c");
        assert_eq!(escape_label("a\\b"), "a\\\\b");
        assert_eq!(escape_label("a\nb\rc"), "a b c");
    }
}
