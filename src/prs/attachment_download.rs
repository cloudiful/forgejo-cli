use std::path::{Path, PathBuf};

use eyre::Context;
use forgejo_api::Forgejo;
use reqwest::Url;
use reqwest::redirect::Policy;
use tokio::io::AsyncWriteExt;

use crate::repo::RepoName;
use crate::{ftl_bail, ftl_eyre, ftl_format, keys::KeyInfo};

const MAX_REDIRECTS: usize = 10;

/// Download one attachment of a pull request into the current directory or an
/// explicit output path.
///
/// The success notice is written to `notice`; callers keep it off stdout.
#[allow(clippy::too_many_arguments)]
pub(super) async fn download_attachment(
    repo: &RepoName,
    api: &Forgejo,
    keys: &mut KeyInfo,
    base: &Url,
    pr_number: i64,
    selector: &str,
    output: Option<&Path>,
    notice: &mut dyn std::io::Write,
) -> eyre::Result<()> {
    let attachments = super::attachments::list_pr_attachments(repo, api, pr_number).await?;
    let attachment = super::attachments::select_pr_attachment(&attachments, selector)?;
    let url = attachment
        .browser_download_url
        .as_ref()
        .filter(|url| !url.as_str().is_empty())
        .ok_or_else(|| {
            ftl_eyre!(
                "msg-pr-attachment-missing_url",
                name = display_name(&attachment.name)
            )
        })?;
    validate_http_url(url).map_err(|_| {
        ftl_eyre!(
            "msg-pr-attachment-invalid_url",
            name = display_name(&attachment.name)
        )
    })?;

    let path = match output {
        Some(path) => path.to_owned(),
        None => {
            let name = attachment
                .name
                .as_deref()
                .filter(|name| !name.is_empty())
                .ok_or_else(|| ftl_eyre!("msg-pr-attachment-missing_name"))?;
            PathBuf::from(safe_default_filename(name)?)
        }
    };

    // Refresh the login and only then read the credential this request may use.
    keys.get_api(base).await?;
    let auth = keys.attachment_auth(base);
    let origin = BaseOrigin::new(base)?;

    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .user_agent(crate::USER_AGENT)
        .build()
        .map_err(transport_error)?;

    let mut current = url.clone();
    let mut hops = 0usize;
    loop {
        let mut request = client.get(current.clone());
        if origin.contains(&current)
            && let Some(value) = auth.header_value()
        {
            let mut value = reqwest::header::HeaderValue::from_str(&value)
                .map_err(|_| ftl_eyre!("msg-pr-attachment-auth_invalid"))?;
            value.set_sensitive(true);
            request = request.header(reqwest::header::AUTHORIZATION, value);
        }
        let response = request.send().await.map_err(transport_error)?;
        let status = response.status();

        if matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308) {
            if hops >= MAX_REDIRECTS {
                ftl_bail!("msg-pr-attachment-too_many_redirects");
            }
            hops += 1;
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| ftl_eyre!("msg-pr-attachment-redirect_invalid"))?;
            current = resolve_redirect(&current, location)?;
            continue;
        }
        if !status.is_success() {
            return Err(ftl_eyre!(
                "msg-pr-attachment-http_status",
                status = status.as_u16()
            ));
        }

        let expected = response.content_length();
        let mut response = response;
        let mut file = create_new_file(&path).await?;
        let result = stream_to_file(&mut response, &mut file, expected).await;
        match result {
            Ok(written) => {
                drop(file);
                report_success(notice, &path, written)?;
                return Ok(());
            }
            Err(error) => {
                drop(file);
                let _ = tokio::fs::remove_file(&path).await;
                return Err(error);
            }
        }
    }
}

fn report_success(notice: &mut dyn std::io::Write, path: &Path, written: u64) -> eyre::Result<()> {
    {
        use std::fmt::Write as _;
        let mut writer = crate::localization::WriterCompat(&mut *notice);
        crate::ftl_write!(
            &mut writer,
            "msg-pr-attachment-download-success",
            file = path.to_string_lossy().into_owned(),
            size = written
        );
        writeln!(&mut writer).expect("failed to write newline");
    }
    notice.flush()?;
    Ok(())
}

fn display_name(name: &Option<String>) -> String {
    name.as_deref()
        .map(super::attachments::escape_control)
        .unwrap_or_default()
}

async fn stream_to_file(
    response: &mut reqwest::Response,
    file: &mut tokio::fs::File,
    expected: Option<u64>,
) -> eyre::Result<u64> {
    let mut written = 0u64;
    while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
        file.write_all(&chunk)
            .await
            .wrap_err_with(|| ftl_format!("msg-pr-attachment-write_failed").into_owned())?;
        written += chunk.len() as u64;
    }
    if let Some(expected) = expected
        && written != expected
    {
        ftl_bail!("msg-pr-attachment-truncated");
    }
    file.flush()
        .await
        .wrap_err_with(|| ftl_format!("msg-pr-attachment-write_failed").into_owned())?;
    Ok(written)
}

async fn create_new_file(path: &Path) -> eyre::Result<tokio::fs::File> {
    let file = path.to_string_lossy().into_owned();
    match tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await
    {
        Ok(file) => Ok(file),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(ftl_eyre!("msg-pr-attachment-output_exists", file = file))
        }
        Err(error) => Err(eyre::Report::new(error)
            .wrap_err(ftl_format!("msg-pr-attachment-create_failed", file = file).into_owned())),
    }
}

fn transport_error(error: reqwest::Error) -> eyre::Report {
    eyre::Report::new(error.without_url())
        .wrap_err(ftl_format!("msg-pr-attachment-transport").into_owned())
}

/// Reject a download URL unless it is an absolute HTTP(S) URL without userinfo.
fn validate_http_url(url: &Url) -> eyre::Result<()> {
    if !(url.scheme() == "http" || url.scheme() == "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        eyre::bail!("attachment URL must be an absolute HTTP(S) URL without userinfo");
    }
    Ok(())
}

/// Resolve and validate one manual redirect hop.
fn resolve_redirect(current: &Url, location: &str) -> eyre::Result<Url> {
    let next = current
        .join(location)
        .map_err(|_| ftl_eyre!("msg-pr-attachment-redirect_invalid"))?;
    validate_http_url(&next).map_err(|_| ftl_eyre!("msg-pr-attachment-redirect_invalid"))?;
    if current.scheme() == "https" && next.scheme() == "http" {
        ftl_bail!("msg-pr-attachment-redirect_downgrade");
    }
    Ok(next)
}

/// The configured Forgejo origin and base path that may receive the login.
struct BaseOrigin {
    scheme: String,
    host: String,
    port: Option<u16>,
    base_segments: Vec<String>,
}

impl BaseOrigin {
    fn new(base: &Url) -> eyre::Result<Self> {
        let host = base
            .host_str()
            .ok_or_else(|| ftl_eyre!("msg-pr-attachment-invalid_url"))?
            .to_ascii_lowercase();
        Ok(Self {
            scheme: base.scheme().to_owned(),
            host,
            port: base.port_or_known_default(),
            base_segments: path_segments(base),
        })
    }

    fn contains(&self, url: &Url) -> bool {
        url.scheme() == self.scheme
            && url
                .host_str()
                .is_some_and(|host| host.eq_ignore_ascii_case(&self.host))
            && url.port_or_known_default() == self.port
            && url.username().is_empty()
            && url.password().is_none()
            && path_segments(url).starts_with(&self.base_segments)
    }
}

fn path_segments(url: &Url) -> Vec<String> {
    url.path_segments()
        .map(|segments| {
            segments
                .filter(|segment| !segment.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Reject unsafe server-supplied default filenames instead of rewriting them.
fn safe_default_filename(name: &str) -> eyre::Result<String> {
    let unsafe_name = || {
        ftl_eyre!(
            "msg-pr-attachment-unsafe_name",
            name = super::attachments::escape_control(name)
        )
    };

    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.contains(':')
        || name.chars().any(char::is_control)
        || name.trim_end_matches(['.', ' ']) != name
    {
        return Err(unsafe_name());
    }

    let stem = name
        .split('.')
        .next()
        .unwrap_or(name)
        .trim_end_matches([' ', '.']);
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    if RESERVED
        .iter()
        .any(|reserved| stem.eq_ignore_ascii_case(reserved))
    {
        return Err(unsafe_name());
    }

    Ok(name.to_owned())
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use forgejo_api::Auth;
    use forgejo_api::structs::Attachment;

    use super::*;
    use crate::keys::{KeyInfo, LoginInfo};

    #[derive(Clone, Debug, Default)]
    struct Captured {
        path: String,
        authorization: Option<String>,
    }

    struct MockResponse {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl MockResponse {
        fn ok(body: impl Into<Vec<u8>>) -> Self {
            Self {
                status: 200,
                headers: Vec::new(),
                body: body.into(),
            }
        }

        fn status(status: u16) -> Self {
            Self {
                status,
                headers: Vec::new(),
                body: Vec::new(),
            }
        }

        fn redirect(location: &str) -> Self {
            Self {
                status: 302,
                headers: vec![("Location".to_owned(), location.to_owned())],
                body: Vec::new(),
            }
        }

        fn with_header(mut self, name: &str, value: &str) -> Self {
            self.headers.push((name.to_owned(), value.to_owned()));
            self
        }
    }

    type Router = Arc<dyn Fn(&Captured) -> MockResponse + Send + Sync + 'static>;

    struct Server {
        base: Url,
        captured: Arc<Mutex<Vec<Captured>>>,
        stop: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl Server {
        fn start(router: impl Fn(&Captured) -> MockResponse + Send + Sync + 'static) -> Self {
            Self::start_on("127.0.0.1", router)
        }

        fn start_on(
            ip: &str,
            router: impl Fn(&Captured) -> MockResponse + Send + Sync + 'static,
        ) -> Self {
            let listener = std::net::TcpListener::bind(format!("{ip}:0")).unwrap();
            listener.set_nonblocking(true).unwrap();
            let addr = listener.local_addr().unwrap();
            let captured: Arc<Mutex<Vec<Captured>>> = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let router: Router = Arc::new(router);
            let captured_clone = Arc::clone(&captured);
            let router_clone = Arc::clone(&router);
            let stop_clone = Arc::clone(&stop);
            let handle = std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(10);
                while Instant::now() < deadline && !stop_clone.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let _ = handle_one(stream, &captured_clone, &router_clone);
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            });
            Self {
                base: Url::parse(&format!("http://{ip}:{}/", addr.port())).unwrap(),
                captured,
                stop,
                handle: Some(handle),
            }
        }

        fn requests(&self) -> Vec<Captured> {
            self.captured.lock().unwrap().clone()
        }

        fn download_requests(&self) -> Vec<Captured> {
            self.requests()
                .into_iter()
                .filter(|request| request.path != "/api/v1/repos/o/r/issues/7/assets")
                .collect()
        }

        fn start_with_assets(
            router: impl Fn(&Captured) -> MockResponse + Send + Sync + 'static,
        ) -> (Self, Arc<Mutex<Vec<Attachment>>>) {
            let slot: Arc<Mutex<Vec<Attachment>>> = Arc::new(Mutex::new(Vec::new()));
            let slot_for_router = Arc::clone(&slot);
            let server = Self::start(move |request: &Captured| {
                if request.path == "/api/v1/repos/o/r/issues/7/assets" {
                    let body = serde_json::to_vec(&*slot_for_router.lock().unwrap()).unwrap();
                    MockResponse::ok(body)
                } else {
                    router(request)
                }
            });
            (server, slot)
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    fn handle_one(
        mut stream: std::net::TcpStream,
        captured: &Arc<Mutex<Vec<Captured>>>,
        router: &Router,
    ) -> std::io::Result<()> {
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let header_end = loop {
            let read = stream.read(&mut chunk)?;
            if read == 0 {
                return Ok(());
            }
            buf.extend_from_slice(&chunk[..read]);
            if let Some(position) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
                break position + 4;
            }
            if buf.len() > 64 * 1024 {
                return Ok(());
            }
        };
        let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
        let mut lines = head.lines();
        let request_line = lines.next().unwrap_or_default();
        let mut parts = request_line.split_whitespace();
        let _method = parts.next().unwrap_or_default();
        let target = parts.next().unwrap_or_default().to_owned();
        let path = Url::parse(&format!("http://localhost{target}"))
            .map(|url| url.path().to_owned())
            .unwrap_or(target);
        let mut authorization = None;
        for line in lines {
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("authorization")
            {
                authorization = Some(value.trim().to_owned());
            }
        }
        let request = Captured {
            path,
            authorization,
        };
        captured.lock().unwrap().push(request.clone());
        let response = router(&request);
        let mut head = format!(
            "HTTP/1.1 {} {}\r\nConnection: close\r\nContent-Type: application/octet-stream\r\n",
            response.status,
            reason(response.status),
        );
        let mut has_length = false;
        for (name, value) in &response.headers {
            if name.eq_ignore_ascii_case("content-length") {
                has_length = true;
            }
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        if !has_length {
            head.push_str(&format!("Content-Length: {}\r\n", response.body.len()));
        }
        head.push_str("\r\n");
        stream.write_all(head.as_bytes())?;
        stream.write_all(&response.body)?;
        stream.flush()?;
        Ok(())
    }

    fn reason(status: u16) -> &'static str {
        match status {
            200 => "OK",
            302 => "Found",
            404 => "Not Found",
            500 => "Internal Server Error",
            _ => "OK",
        }
    }

    fn attachment(id: i64, name: Option<&str>, url: Option<&str>, size: Option<i64>) -> Attachment {
        Attachment {
            browser_download_url: url.map(|url| Url::parse(url).unwrap()),
            created_at: None,
            download_count: None,
            id: Some(id),
            name: name.map(str::to_owned),
            size,
            r#type: None,
            uuid: None,
        }
    }

    fn test_api(base: &Url) -> Forgejo {
        Forgejo::with_user_agent(Auth::None, base.clone(), crate::USER_AGENT).unwrap()
    }

    fn test_keys(base: &Url, login: LoginInfo) -> KeyInfo {
        let mut keys = KeyInfo::default();
        keys.hosts.insert(crate::host_name(base).to_owned(), login);
        keys
    }

    fn application_login() -> LoginInfo {
        LoginInfo::Application {
            token: "sekret-token".to_owned(),
        }
    }

    fn oauth_login() -> LoginInfo {
        LoginInfo::OAuth {
            token: "oauth-token".to_owned(),
            refresh_token: "refresh".to_owned(),
            expires_at: time::OffsetDateTime::now_utc() + time::Duration::hours(1),
        }
    }

    fn scratch_dir() -> PathBuf {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join(".cache/ai/1198");
        std::fs::create_dir_all(&base).unwrap();
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        loop {
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let dir = base.join(format!("download-{}-{n}", std::process::id()));
            match std::fs::create_dir(&dir) {
                Ok(()) => return dir,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("cannot create scratch dir: {error:?}"),
            }
        }
    }

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            Self(scratch_dir())
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    async fn download(
        server: &Server,
        slot: &Arc<Mutex<Vec<Attachment>>>,
        attachments: Vec<Attachment>,
        selector: &str,
        output: Option<&Path>,
        login: LoginInfo,
    ) -> eyre::Result<()> {
        *slot.lock().unwrap() = attachments;
        let base = server.base.clone();
        let api = test_api(&base);
        let mut keys = test_keys(&base, login);
        let repo = RepoName::new("o".to_owned(), "r".to_owned());
        download_attachment(
            &repo,
            &api,
            &mut keys,
            &base,
            7,
            selector,
            output,
            &mut std::io::sink(),
        )
        .await
    }

    async fn download_from_origin(
        server: &Server,
        slot: &Arc<Mutex<Vec<Attachment>>>,
        origin: &Url,
        attachments: Vec<Attachment>,
        selector: &str,
        output: &Path,
        login: LoginInfo,
    ) -> eyre::Result<()> {
        *slot.lock().unwrap() = attachments;
        let api_base = server.base.clone();
        let api = test_api(&api_base);
        let mut keys = test_keys(origin, login);
        let repo = RepoName::new("o".to_owned(), "r".to_owned());
        download_attachment(
            &repo,
            &api,
            &mut keys,
            origin,
            7,
            selector,
            Some(output),
            &mut std::io::sink(),
        )
        .await
    }

    #[tokio::test]
    async fn application_token_downloads_exact_bytes() {
        let (server, slot) = Server::start_with_assets(|request| match request.path.as_str() {
            "/attachments/one" => MockResponse::ok(b"hello binary\0world".to_vec()),
            _ => MockResponse::status(404),
        });
        let server = Arc::new(server);
        let url = format!("{}attachments/one", server.base);
        let scratch = Scratch::new();
        let output = scratch.path().join("out.bin");

        let result = download(
            &server,
            &slot,
            vec![attachment(1, Some("out.bin"), Some(&url), Some(18))],
            "out.bin",
            Some(&output),
            application_login(),
        )
        .await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(std::fs::read(&output).unwrap(), b"hello binary\0world");
        let requests = server.download_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].authorization.as_deref(),
            Some("token sekret-token")
        );
    }

    #[tokio::test]
    async fn oauth_token_uses_bearer() {
        let (server, slot) = Server::start_with_assets(|request| match request.path.as_str() {
            "/attachments/one" => MockResponse::ok(b"body".to_vec()),
            _ => MockResponse::status(404),
        });
        let server = Arc::new(server);
        let url = format!("{}attachments/one", server.base);
        let scratch = Scratch::new();
        let output = scratch.path().join("out.bin");
        let result = download(
            &server,
            &slot,
            vec![attachment(1, Some("out.bin"), Some(&url), Some(4))],
            "out.bin",
            Some(&output),
            oauth_login(),
        )
        .await;
        assert!(result.is_ok(), "{result:?}");
        let requests = server.download_requests();
        assert_eq!(
            requests[0].authorization.as_deref(),
            Some("Bearer oauth-token")
        );
    }

    #[tokio::test]
    async fn external_url_gets_no_login() {
        let external = Arc::new(Server::start(|_| MockResponse::ok(b"signed".to_vec())));
        let url = format!("{}signed", external.base);
        let (configured, slot) = Server::start_with_assets(|_| MockResponse::status(404));
        let configured = Arc::new(configured);
        let scratch = Scratch::new();
        let output = scratch.path().join("out.bin");
        let result = download(
            &configured,
            &slot,
            vec![attachment(1, Some("out.bin"), Some(&url), Some(6))],
            "out.bin",
            Some(&output),
            application_login(),
        )
        .await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(std::fs::read(&output).unwrap(), b"signed");
        assert_eq!(external.requests().len(), 1);
        assert_eq!(external.requests()[0].authorization, None);
        assert!(configured.download_requests().is_empty());
    }

    #[tokio::test]
    async fn anonymous_download_sends_no_login() {
        let (server, slot) = Server::start_with_assets(|request| match request.path.as_str() {
            "/attachments/one" => MockResponse::ok(b"public".to_vec()),
            _ => MockResponse::status(404),
        });
        let server = Arc::new(server);
        let url = format!("{}attachments/one", server.base);
        *slot.lock().unwrap() = vec![attachment(1, Some("out.bin"), Some(&url), Some(6))];
        let scratch = Scratch::new();
        let output = scratch.path().join("out.bin");
        let base = server.base.clone();
        let api = test_api(&base);
        let mut keys = KeyInfo::default();
        let repo = RepoName::new("o".to_owned(), "r".to_owned());
        let result = download_attachment(
            &repo,
            &api,
            &mut keys,
            &base,
            7,
            "out.bin",
            Some(&output),
            &mut std::io::sink(),
        )
        .await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(std::fs::read(&output).unwrap(), b"public");
        assert_eq!(server.download_requests()[0].authorization, None);
    }

    #[tokio::test]
    async fn cross_host_redirect_drops_login() {
        // A different loopback address is a different host, so the login must
        // not follow the redirect even though both listeners are local.
        let target = Arc::new(Server::start_on("[::1]", |_| {
            MockResponse::ok(b"other-host".to_vec())
        }));
        let location = format!("{}elsewhere", target.base);
        let (source, slot) =
            Server::start_with_assets(move |request| match request.path.as_str() {
                "/attachments/one" => MockResponse::redirect(&location),
                _ => MockResponse::status(404),
            });
        let source = Arc::new(source);
        let url = format!("{}attachments/one", source.base);
        let scratch = Scratch::new();
        let output = scratch.path().join("out.bin");
        let result = download(
            &source,
            &slot,
            vec![attachment(1, Some("out.bin"), Some(&url), Some(10))],
            "out.bin",
            Some(&output),
            application_login(),
        )
        .await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(std::fs::read(&output).unwrap(), b"other-host");
        assert_eq!(
            source.download_requests()[0].authorization.as_deref(),
            Some("token sekret-token")
        );
        assert_eq!(target.requests()[0].authorization, None);
    }

    #[tokio::test]
    async fn out_of_base_path_hops_omit_login() {
        let (server, slot) = Server::start_with_assets(|request| match request.path.as_str() {
            "/forgejo/attachments/one" => MockResponse::redirect("/elsewhere"),
            "/elsewhere" => MockResponse::ok(b"out".to_vec()),
            "/attachments/outside" => MockResponse::ok(b"first".to_vec()),
            _ => MockResponse::status(404),
        });
        let server = Arc::new(server);
        let origin = Url::parse(&format!("{}forgejo/", server.base)).unwrap();
        let scratch = Scratch::new();

        // A redirect from the configured base path to the same origin outside
        // it keeps the request but drops the login.
        let inside = format!("{}forgejo/attachments/one", server.base);
        let output = scratch.path().join("inside.bin");
        let result = download_from_origin(
            &server,
            &slot,
            &origin,
            vec![attachment(1, Some("inside.bin"), Some(&inside), Some(3))],
            "inside.bin",
            &output,
            application_login(),
        )
        .await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(std::fs::read(&output).unwrap(), b"out");

        // A first-hop URL outside the base path is same-origin but must not
        // receive the login either.
        let outside = format!("{}attachments/outside", server.base);
        let output = scratch.path().join("outside.bin");
        let result = download_from_origin(
            &server,
            &slot,
            &origin,
            vec![attachment(1, Some("outside.bin"), Some(&outside), Some(5))],
            "outside.bin",
            &output,
            application_login(),
        )
        .await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(std::fs::read(&output).unwrap(), b"first");

        let requests = server.download_requests();
        let by_path = |path: &str| {
            requests
                .iter()
                .find(|request| request.path == path)
                .unwrap_or_else(|| panic!("no request for {path}: {requests:?}"))
        };
        assert_eq!(
            by_path("/forgejo/attachments/one").authorization.as_deref(),
            Some("token sekret-token")
        );
        assert_eq!(by_path("/elsewhere").authorization, None);
        assert_eq!(by_path("/attachments/outside").authorization, None);
    }

    #[tokio::test]
    async fn expired_oauth_fails_before_download_request() {
        let (server, slot) = Server::start_with_assets(|_| MockResponse::ok(b"body".to_vec()));
        let server = Arc::new(server);
        let url = format!("{}attachments/one", server.base);
        let scratch = Scratch::new();
        let output = scratch.path().join("out.bin");
        let expired = LoginInfo::OAuth {
            token: "stale".to_owned(),
            refresh_token: "refresh".to_owned(),
            expires_at: time::OffsetDateTime::now_utc() - time::Duration::hours(1),
        };
        let result = download(
            &server,
            &slot,
            vec![attachment(1, Some("out.bin"), Some(&url), Some(4))],
            "out.bin",
            Some(&output),
            expired,
        )
        .await;
        assert!(
            result.is_err(),
            "an expired OAuth login cannot refresh in this test"
        );
        assert!(!output.exists());
        assert!(
            server.download_requests().is_empty(),
            "credentials must be refreshed before any download request"
        );
        assert_eq!(
            server.requests().len(),
            1,
            "only the metadata request may happen before the refresh failure"
        );
    }

    #[tokio::test]
    async fn relative_redirect_keeps_login() {
        let (server, slot) = Server::start_with_assets(|request| match request.path.as_str() {
            "/attachments/one" => MockResponse::redirect("/attachments/two"),
            "/attachments/two" => MockResponse::ok(b"final".to_vec()),
            _ => MockResponse::status(404),
        });
        let server = Arc::new(server);
        let url = format!("{}attachments/one", server.base);
        let scratch = Scratch::new();
        let output = scratch.path().join("out.bin");
        let result = download(
            &server,
            &slot,
            vec![attachment(1, Some("out.bin"), Some(&url), Some(5))],
            "out.bin",
            Some(&output),
            application_login(),
        )
        .await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(std::fs::read(&output).unwrap(), b"final");
        let requests = server.download_requests();
        assert_eq!(requests.len(), 2);
        for request in requests {
            assert_eq!(request.authorization.as_deref(), Some("token sekret-token"));
        }
    }

    #[tokio::test]
    async fn cross_port_redirect_drops_login() {
        let target = Arc::new(Server::start(|_| MockResponse::ok(b"other".to_vec())));
        let location = format!("{}elsewhere", target.base);
        let (source, slot) =
            Server::start_with_assets(move |request| match request.path.as_str() {
                "/attachments/one" => MockResponse::redirect(&location),
                _ => MockResponse::status(404),
            });
        let source = Arc::new(source);
        let url = format!("{}attachments/one", source.base);
        let scratch = Scratch::new();
        let output = scratch.path().join("out.bin");
        let result = download(
            &source,
            &slot,
            vec![attachment(1, Some("out.bin"), Some(&url), Some(5))],
            "out.bin",
            Some(&output),
            application_login(),
        )
        .await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(std::fs::read(&output).unwrap(), b"other");
        assert_eq!(
            source.download_requests()[0].authorization.as_deref(),
            Some("token sekret-token")
        );
        assert_eq!(target.requests()[0].authorization, None);
    }

    #[tokio::test]
    async fn redirect_cap_is_enforced() {
        let (server, slot) = Server::start_with_assets(|request| {
            if request.path.starts_with("/loop") {
                MockResponse::redirect(&format!("{}loop", request.path))
            } else {
                MockResponse::status(404)
            }
        });
        let server = Arc::new(server);
        let url = format!("{}loop", server.base);
        let scratch = Scratch::new();
        let output = scratch.path().join("out.bin");
        let result = download(
            &server,
            &slot,
            vec![attachment(1, Some("out.bin"), Some(&url), None)],
            "out.bin",
            Some(&output),
            application_login(),
        )
        .await;
        assert!(result.is_err());
        assert_eq!(server.download_requests().len(), MAX_REDIRECTS + 1);
        assert!(!output.exists());
    }

    #[tokio::test]
    async fn http_error_status_leaves_no_file() {
        let (server, slot) = Server::start_with_assets(|_| MockResponse::status(500));
        let server = Arc::new(server);
        let url = format!("{}attachments/one", server.base);
        let scratch = Scratch::new();
        let output = scratch.path().join("out.bin");
        let result = download(
            &server,
            &slot,
            vec![attachment(1, Some("out.bin"), Some(&url), None)],
            "out.bin",
            Some(&output),
            application_login(),
        )
        .await;
        assert!(result.is_err());
        assert!(!output.exists());
    }

    #[tokio::test]
    async fn truncated_stream_cleans_up_partial_file() {
        let (server, slot) = Server::start_with_assets(|_| MockResponse {
            status: 200,
            headers: vec![("Content-Length".to_owned(), "100".to_owned())],
            body: b"short".to_vec(),
        });
        let server = Arc::new(server);
        let url = format!("{}attachments/one", server.base);
        let scratch = Scratch::new();
        let output = scratch.path().join("out.bin");
        let result = download(
            &server,
            &slot,
            vec![attachment(1, Some("out.bin"), Some(&url), None)],
            "out.bin",
            Some(&output),
            application_login(),
        )
        .await;
        assert!(result.is_err());
        assert!(!output.exists());
    }

    #[tokio::test]
    async fn zero_byte_download_succeeds() {
        let (server, slot) = Server::start_with_assets(|_| {
            MockResponse::ok(Vec::new()).with_header("Content-Length", "0")
        });
        let server = Arc::new(server);
        let url = format!("{}attachments/one", server.base);
        let scratch = Scratch::new();
        let output = scratch.path().join("out.bin");
        let result = download(
            &server,
            &slot,
            vec![attachment(1, Some("out.bin"), Some(&url), Some(0))],
            "out.bin",
            Some(&output),
            application_login(),
        )
        .await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(std::fs::read(&output).unwrap(), b"");
    }

    #[tokio::test]
    async fn existing_file_is_not_overwritten() {
        let (server, slot) = Server::start_with_assets(|_| MockResponse::ok(b"new".to_vec()));
        let server = Arc::new(server);
        let url = format!("{}attachments/one", server.base);
        let scratch = Scratch::new();
        let output = scratch.path().join("keep.bin");
        std::fs::write(&output, b"original").unwrap();
        let result = download(
            &server,
            &slot,
            vec![attachment(1, Some("keep.bin"), Some(&url), Some(3))],
            "keep.bin",
            Some(&output),
            application_login(),
        )
        .await;
        assert!(result.is_err());
        assert_eq!(std::fs::read(&output).unwrap(), b"original");
        assert_eq!(server.download_requests().len(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_is_not_followed_or_replaced() {
        let (server, slot) = Server::start_with_assets(|_| MockResponse::ok(b"new".to_vec()));
        let server = Arc::new(server);
        let url = format!("{}attachments/one", server.base);
        let scratch = Scratch::new();
        let target = scratch.path().join("target.bin");
        std::fs::write(&target, b"original").unwrap();
        let output = scratch.path().join("link.bin");
        std::os::unix::fs::symlink(&target, &output).unwrap();
        let result = download(
            &server,
            &slot,
            vec![attachment(1, Some("link.bin"), Some(&url), Some(3))],
            "link.bin",
            Some(&output),
            application_login(),
        )
        .await;
        assert!(result.is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"original");
        assert!(
            std::fs::symlink_metadata(&output)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[tokio::test]
    async fn unsafe_default_names_are_rejected_before_download() {
        for name in [
            "",
            ".",
            "..",
            "a/b",
            "a\\b",
            "C:\\evil",
            "C:evil",
            "CON",
            "con.txt",
            "CON .txt",
            "LPT1.tar.gz",
            "trailing.",
            "trailing ",
            "nul\0byte",
            "line\nbreak",
            "\u{7}bell",
        ] {
            let body = serde_json::to_vec(&vec![attachment(
                1,
                Some(name),
                Some("https://example.invalid/attachment"),
                Some(4),
            )])
            .unwrap();
            let server = Server::start(move |request: &Captured| {
                if request.path == "/api/v1/repos/o/r/issues/7/assets" {
                    MockResponse::ok(body.clone())
                } else {
                    MockResponse::status(404)
                }
            });
            let base = server.base.clone();
            let api = test_api(&base);
            let mut keys = test_keys(&base, application_login());
            let repo = RepoName::new("o".to_owned(), "r".to_owned());
            let result = download_attachment(
                &repo,
                &api,
                &mut keys,
                &base,
                7,
                name,
                None,
                &mut std::io::sink(),
            )
            .await;
            assert!(result.is_err(), "{name:?} should be rejected");
            assert_eq!(server.requests().len(), 1, "{name:?} must not be fetched");
        }
    }

    #[test]
    fn origin_check_compares_scheme_host_port_and_base_path() {
        let base = Url::parse("https://forgejo.example/forgejo/").unwrap();
        let origin = BaseOrigin::new(&base).unwrap();
        assert!(
            origin.contains(&Url::parse("https://forgejo.example/forgejo/attachments/x").unwrap())
        );
        assert!(origin.contains(&Url::parse("https://FORGEJO.example:443/forgejo/x").unwrap()));
        assert!(!origin.contains(&Url::parse("https://forgejo.example/forgejo-evil/x").unwrap()));
        assert!(!origin.contains(&Url::parse("http://forgejo.example/forgejo/x").unwrap()));
        assert!(!origin.contains(&Url::parse("https://forgejo.example:8443/forgejo/x").unwrap()));
        assert!(!origin.contains(&Url::parse("https://forgejo.example/other/forgejo/x").unwrap()));
        assert!(!origin.contains(&Url::parse("https://user@forgejo.example/forgejo/x").unwrap()));

        let root = BaseOrigin::new(&Url::parse("http://127.0.0.1:1234/").unwrap()).unwrap();
        assert!(root.contains(&Url::parse("http://127.0.0.1:1234/anything").unwrap()));
        assert!(!root.contains(&Url::parse("http://127.0.0.1:4321/anything").unwrap()));
    }

    #[test]
    fn redirects_are_validated() {
        let https = Url::parse("https://forgejo.example/a").unwrap();
        assert!(resolve_redirect(&https, "/b").is_ok());
        assert!(resolve_redirect(&https, "http://forgejo.example/b").is_err());
        assert!(resolve_redirect(&https, "file:///etc/passwd").is_err());
        assert!(resolve_redirect(&https, "https://user@forgejo.example/b").is_err());

        let http = Url::parse("http://127.0.0.1:1234/a").unwrap();
        let next = resolve_redirect(&http, "/b").unwrap();
        assert_eq!(next.as_str(), "http://127.0.0.1:1234/b");
    }

    #[test]
    fn safe_names_are_accepted() {
        for name in [
            "a.txt",
            "my file.pdf",
            "a.b.c",
            "COM1x",
            "console.log",
            ".hidden",
        ] {
            assert_eq!(safe_default_filename(name).unwrap(), name);
        }
    }
}
