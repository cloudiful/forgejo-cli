use std::path::{Component, Path, PathBuf};

use eyre::Context;
use forgejo_api::{Forgejo, structs::IssueCreateIssueAttachmentQuery};

use crate::{ftl_bail, ftl_format, repo::RepoName};

const IMAGE_EXTENSIONS: [&str; 9] = [
    "png", "jpg", "jpeg", "gif", "webp", "svg", "bmp", "avif", "apng",
];

struct UploadedAttachment {
    url: String,
    label: String,
}

/// An attachment argument: the file to upload and an optional alt text.
pub(super) struct AttachmentSpec {
    pub path: PathBuf,
    pub alt: Option<String>,
}

/// An attachment that passed the preflight.
pub(super) struct PreparedAttachment {
    pub path: PathBuf,
    pub basename: String,
    pub alt: Option<String>,
    pub identity: PathBuf,
}

/// A file that was uploaded successfully.
struct Upload {
    url: String,
    label: String,
    alt: Option<String>,
    is_image: bool,
    identity: PathBuf,
}

enum UploadError {
    Read(std::io::Error),
    Upload(forgejo_api::ForgejoError),
    MissingUrl,
    InvalidUrl,
}

/// The failures of a create-time attachment run.
///
/// Every detail keeps the error chain of the underlying failure, so the cause
/// of a failed upload or read is never dropped.
#[derive(Debug)]
struct AttachFailures {
    summary: String,
    details: Vec<eyre::Report>,
}

impl std::fmt::Display for AttachFailures {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut separator = "";
        if !self.summary.is_empty() {
            f.write_str(&self.summary)?;
            separator = "\n";
        }
        for detail in &self.details {
            f.write_str(separator)?;
            separator = "\n";
            write!(f, "{detail}")?;
            if let Some(cause) = error_causes(detail) {
                write!(
                    f,
                    "\n{}",
                    crate::ftl_format!("msg-pr-create-attach_cause", cause = cause)
                )?;
            }
        }
        Ok(())
    }
}

impl std::error::Error for AttachFailures {}

/// The causes of an error, without the message of the error itself.
fn error_causes(report: &eyre::Report) -> Option<String> {
    let mut causes = Vec::new();
    let mut source = report.source();
    while let Some(cause) = source {
        causes.push(cause.to_string());
        source = cause.source();
    }
    if causes.is_empty() {
        None
    } else {
        Some(causes.join(": "))
    }
}

pub(super) async fn attach_files(
    repo: &RepoName,
    api: &Forgejo,
    pr_number: i64,
    files: &[PathBuf],
    out: &mut dyn std::io::Write,
) -> eyre::Result<()> {
    let basenames = validate_files(files).await?;
    api.repo_get_pull_request(repo.owner(), repo.name(), pr_number)
        .await
        .wrap_err_with(|| {
            crate::ftl_format!("msg-pr-attach-pr_verify_failed", number = pr_number).into_owned()
        })?;
    let mut succeeded: usize = 0;
    for (path, basename) in files.iter().zip(basenames.iter()) {
        let step: eyre::Result<()> = async {
            let uploaded =
                upload_attachment(api, repo.owner(), repo.name(), pr_number, path, basename)
                    .await
                    .map_err(|error| upload_error_report(error, path))?;
            writeln!(out, "[{}](<{}>)", uploaded.label, uploaded.url).wrap_err_with(|| {
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

/// Upload files to a pull request that was just created and add Markdown
/// references for them to the pull request body.
///
/// The pull request exists already, so every failure is reported with the pull
/// request identity and the successfully uploaded links are never hidden.
#[allow(clippy::too_many_arguments)]
pub(super) async fn attach_to_created_pr(
    api: &Forgejo,
    owner: &str,
    repo: &str,
    pr_number: i64,
    pr_url: Option<&str>,
    attachments: &[PreparedAttachment],
    body_dir: Option<&Path>,
    out: &mut dyn std::io::Write,
) -> eyre::Result<()> {
    writeln!(
        out,
        "{}",
        crate::ftl_format!(
            "msg-pr-create-attach_pr",
            number = pr_number,
            repo = format!("{owner}/{repo}")
        )
    )
    .wrap_err("failed to write output")?;
    if let Some(url) = pr_url {
        writeln!(
            out,
            "{}",
            crate::ftl_format!("msg-pr-create-attach_pr_url", url = url)
        )
        .wrap_err("failed to write output")?;
    }

    let mut uploads: Vec<Upload> = Vec::new();
    let mut failed_files: Vec<String> = Vec::new();
    let mut failures: Vec<eyre::Report> = Vec::new();
    for attachment in attachments {
        match upload_attachment(
            api,
            owner,
            repo,
            pr_number,
            &attachment.path,
            &attachment.basename,
        )
        .await
        {
            Ok(uploaded) => uploads.push(Upload {
                url: uploaded.url,
                label: uploaded.label,
                alt: attachment.alt.clone(),
                is_image: is_image_name(&attachment.basename),
                identity: attachment.identity.clone(),
            }),
            Err(error) => {
                failed_files.push(attachment.path.to_string_lossy().into_owned());
                failures.push(created_upload_error(error, &attachment.path, pr_number));
            }
        }
    }

    if !uploads.is_empty() {
        let links: Vec<String> = uploads.iter().map(upload_markdown).collect();
        let body = match api.repo_get_pull_request(owner, repo, pr_number).await {
            Ok(pr) => pr.body.unwrap_or_default(),
            Err(error) => {
                print_references(out, &links)?;
                let mut details = vec![
                    eyre::Report::new(error).wrap_err(
                        crate::ftl_format!(
                            "msg-pr-create-attach_body_read_failed",
                            number = pr_number
                        )
                        .into_owned(),
                    ),
                ];
                details.extend(failures);
                return Err(eyre::Report::new(AttachFailures {
                    summary: String::new(),
                    details,
                }));
            }
        };
        let (body, referenced) = rewrite_references(&body, &uploads, body_dir);
        let appended: Vec<String> = uploads
            .iter()
            .enumerate()
            .filter(|(index, _)| !referenced[*index])
            .map(|(_, upload)| upload_markdown(upload))
            .collect();
        let edit = forgejo_api::structs::EditIssueOption {
            body: Some(append_references(&body, &appended)),
            #[allow(deprecated)]
            assignee: None,
            assignees: None,
            due_date: None,
            milestone: None,
            r#ref: None,
            state: None,
            title: None,
            unset_due_date: None,
            updated_at: None,
        };
        if let Err(error) = api.issue_edit_issue(owner, repo, pr_number, edit).await {
            print_references(out, &links)?;
            let mut details = vec![
                eyre::Report::new(error).wrap_err(
                    crate::ftl_format!(
                        "msg-pr-create-attach_body_update_failed",
                        number = pr_number
                    )
                    .into_owned(),
                ),
            ];
            details.extend(failures);
            return Err(eyre::Report::new(AttachFailures {
                summary: String::new(),
                details,
            }));
        }
        writeln!(
            out,
            "{}",
            crate::ftl_format!(
                "msg-pr-create-attach_done",
                count = uploads.len() as i64,
                number = pr_number
            )
        )
        .wrap_err("failed to write output")?;
    }

    if !failed_files.is_empty() {
        return Err(eyre::Report::new(AttachFailures {
            summary: crate::ftl_format!(
                "msg-pr-create-attach_partial",
                failed = failed_files.len() as i64,
                total = attachments.len() as i64,
                number = pr_number,
                files = failed_files.join(", ")
            )
            .into_owned(),
            details: failures,
        }));
    }
    Ok(())
}

/// Check every file before anything is sent to the server.
pub(super) async fn validate_files(files: &[PathBuf]) -> eyre::Result<Vec<String>> {
    let mut basenames = Vec::with_capacity(files.len());
    for path in files {
        basenames.push(validate_file(path).await?);
    }
    Ok(basenames)
}

/// Split every attachment argument into the file and its optional alt text.
pub(super) fn parse_attachment_args(args: &[String]) -> Vec<AttachmentSpec> {
    args.iter().map(|arg| parse_attachment_arg(arg)).collect()
}

/// Split an attachment argument into the file and its optional alt text.
///
/// An argument that is an existing path is used as it is, so a file name with
/// a `#` in it keeps working. Otherwise the longest existing prefix before a
/// `#` is the file and the rest is the alt text.
pub(super) fn parse_attachment_arg(arg: &str) -> AttachmentSpec {
    let path = Path::new(arg);
    if path.exists() {
        return AttachmentSpec {
            path: path.to_owned(),
            alt: None,
        };
    }
    for (index, c) in arg.char_indices().rev() {
        if c != '#' {
            continue;
        }
        let prefix = &arg[..index];
        if Path::new(prefix).exists() {
            return AttachmentSpec {
                path: PathBuf::from(prefix),
                alt: alt_text(&arg[index + c.len_utf8()..]),
            };
        }
    }
    match arg.rfind('#') {
        Some(index) => AttachmentSpec {
            path: PathBuf::from(&arg[..index]),
            alt: alt_text(&arg[index + 1..]),
        },
        None => AttachmentSpec {
            path: PathBuf::from(arg),
            alt: None,
        },
    }
}

fn alt_text(text: &str) -> Option<String> {
    if text.is_empty() {
        None
    } else {
        Some(text.to_owned())
    }
}

/// Check every attachment before anything is sent to the server.
pub(super) async fn validate_attachments(
    specs: &[AttachmentSpec],
) -> eyre::Result<Vec<PreparedAttachment>> {
    let mut prepared = Vec::with_capacity(specs.len());
    for spec in specs {
        let basename = validate_file(&spec.path).await?;
        prepared.push(PreparedAttachment {
            path: spec.path.clone(),
            basename,
            alt: spec.alt.as_deref().map(escape_label),
            identity: file_identity(&spec.path),
        });
    }
    Ok(prepared)
}

async fn validate_file(path: &Path) -> eyre::Result<String> {
    let name = basename_of(path)?;
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
    Ok(name)
}

/// The identity of a file, so that two spellings of the same file match.
fn file_identity(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| absolute_path(path))
}

fn absolute_path(path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            component => out.push(component.as_os_str()),
        }
    }
    out
}

fn basename_of(path: &Path) -> eyre::Result<String> {
    if path == Path::new("-") {
        ftl_bail!("msg-pr-attach-stdin_unsupported");
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
    Ok(name.to_owned())
}

async fn upload_attachment(
    api: &Forgejo,
    owner: &str,
    repo: &str,
    pr_number: i64,
    path: &Path,
    basename: &str,
) -> Result<UploadedAttachment, UploadError> {
    let bytes = tokio::fs::read(path).await.map_err(UploadError::Read)?;
    let query = IssueCreateIssueAttachmentQuery {
        name: Some(basename.to_owned()),
        updated_at: None,
    };
    let attachment = api
        .issue_create_issue_attachment(owner, repo, pr_number, &bytes, query)
        .await
        .map_err(UploadError::Upload)?;
    let url = attachment
        .browser_download_url
        .as_ref()
        .filter(|u| !u.as_str().is_empty())
        .ok_or(UploadError::MissingUrl)?;
    if !(url.scheme() == "http" || url.scheme() == "https")
        || url.host_str().is_none()
        || url.as_str().contains('<')
        || url.as_str().contains('>')
        || url.as_str().contains('\n')
        || url.as_str().contains('\r')
    {
        return Err(UploadError::InvalidUrl);
    }
    let raw_label = attachment
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(basename);
    Ok(UploadedAttachment {
        url: url.as_str().to_owned(),
        label: escape_label(raw_label),
    })
}

fn upload_error_report(error: UploadError, path: &Path) -> eyre::Report {
    let file = path.to_string_lossy().into_owned();
    match error {
        UploadError::Read(error) => eyre::Report::new(error)
            .wrap_err(crate::ftl_format!("msg-pr-attach-read_failed", file = file).into_owned()),
        UploadError::Upload(error) => eyre::Report::new(error)
            .wrap_err(crate::ftl_format!("msg-pr-attach-upload_failed", file = file).into_owned()),
        UploadError::MissingUrl => crate::ftl_eyre!("msg-pr-attach-missing_url", file = file),
        UploadError::InvalidUrl => crate::ftl_eyre!("msg-pr-attach-invalid_url", file = file),
    }
}

fn created_upload_error(error: UploadError, path: &Path, pr_number: i64) -> eyre::Report {
    let file = path.to_string_lossy().into_owned();
    match error {
        UploadError::Read(error) => eyre::Report::new(error).wrap_err(
            crate::ftl_format!(
                "msg-pr-create-attach_read_failed",
                file = file,
                number = pr_number
            )
            .into_owned(),
        ),
        UploadError::Upload(error) => eyre::Report::new(error).wrap_err(
            crate::ftl_format!(
                "msg-pr-create-attach_upload_failed",
                file = file,
                number = pr_number
            )
            .into_owned(),
        ),
        UploadError::MissingUrl => crate::ftl_eyre!(
            "msg-pr-create-attach_missing_url",
            file = file,
            number = pr_number
        ),
        UploadError::InvalidUrl => crate::ftl_eyre!(
            "msg-pr-create-attach_invalid_url",
            file = file,
            number = pr_number
        ),
    }
}

fn is_image_name(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .is_some_and(|ext| IMAGE_EXTENSIONS.contains(&ext.as_str()))
}

/// The Markdown reference of an uploaded file.
fn upload_markdown(upload: &Upload) -> String {
    let label = upload.alt.as_deref().unwrap_or(&upload.label);
    if upload.is_image {
        format!("![{label}](<{}>)", upload.url)
    } else {
        format!("[{label}](<{}>)", upload.url)
    }
}

/// Replace the destinations of the body that point to an uploaded file.
///
/// Only the destination of a reference is replaced, the rest of the body is
/// kept as it is. Every upload that is used by a reference is not appended
/// again.
fn rewrite_references(
    body: &str,
    uploads: &[Upload],
    body_dir: Option<&Path>,
) -> (String, Vec<bool>) {
    let mut referenced = vec![false; uploads.len()];
    let mut edits: Vec<(std::ops::Range<usize>, String)> = Vec::new();
    for body_ref in super::markdown_refs::scan_body_refs(body) {
        let Some(index) = find_upload(&body_ref.dest, uploads, body_dir) else {
            continue;
        };
        referenced[index] = true;
        if edits
            .iter()
            .any(|(range, _)| range.start == body_ref.dest_range.start)
        {
            continue;
        }
        edits.push((
            body_ref.dest_range.clone(),
            format!("<{}>", uploads[index].url),
        ));
    }
    edits.sort_by_key(|(range, _)| range.start);
    let mut body = body.to_owned();
    for (range, url) in edits.iter().rev() {
        body.replace_range(range.clone(), url);
    }
    (body, referenced)
}

/// The upload a destination of the body points to.
fn find_upload(dest: &str, uploads: &[Upload], body_dir: Option<&Path>) -> Option<usize> {
    for spelling in super::markdown_refs::destination_spellings(dest) {
        if super::markdown_refs::is_remote_destination(&spelling) {
            continue;
        }
        let Some(path) = resolve_destination(&spelling, body_dir) else {
            continue;
        };
        let identity = file_identity(&path);
        if let Some(index) = uploads
            .iter()
            .position(|upload| upload.identity == identity)
        {
            return Some(index);
        }
    }
    None
}

/// The file a destination of the body points to.
///
/// A relative destination is looked up in the directory of the body file
/// first and in the working directory afterwards.
fn resolve_destination(dest: &str, body_dir: Option<&Path>) -> Option<PathBuf> {
    let path = Path::new(dest);
    if path.is_absolute() {
        return path.exists().then(|| path.to_owned());
    }
    if let Some(dir) = body_dir {
        let candidate = dir.join(path);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    let candidate = PathBuf::from(dest);
    candidate.exists().then_some(candidate)
}

/// Append the references to the body, keeping the existing content as it is.
fn append_references(body: &str, references: &[String]) -> String {
    if references.is_empty() {
        return body.to_owned();
    }
    let references = references.join("\n");
    if body.is_empty() {
        references
    } else if body.ends_with("\n\n") {
        format!("{body}{references}")
    } else if body.ends_with('\n') {
        format!("{body}\n{references}")
    } else {
        format!("{body}\n\n{references}")
    }
}

fn print_references(out: &mut dyn std::io::Write, references: &[String]) -> eyre::Result<()> {
    for reference in references {
        writeln!(out, "{reference}").wrap_err("failed to write output")?;
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

    #[derive(Clone)]
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
    type Router = Arc<dyn Fn(&Captured) -> (u16, Vec<u8>) + Send + Sync + 'static>;

    struct Server {
        addr: std::net::SocketAddr,
        captured: Arc<Mutex<Vec<Captured>>>,
        stop: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl Server {
        fn start(pr_status: u16, pr_body: Vec<u8>, posts: Vec<PostResponse>) -> Self {
            let queue: PostQueue = Arc::new(Mutex::new(posts.into_iter().collect()));
            let pr = Arc::new((pr_status, pr_body));
            Self::start_with(move |captured: &Captured| {
                if captured.method == "GET" && captured.path.contains("/pulls/") {
                    (pr.0, pr.1.clone())
                } else if captured.method == "POST"
                    && captured.path.contains("/issues/")
                    && captured.path.ends_with("/assets")
                {
                    queue
                        .lock()
                        .unwrap()
                        .pop_front()
                        .unwrap_or((500, br#"{"message":"no more mocked responses"}"#.to_vec()))
                } else {
                    (404, br#"{"message":"not found"}"#.to_vec())
                }
            })
        }

        fn start_with(
            router: impl Fn(&Captured) -> (u16, Vec<u8>) + Send + Sync + 'static,
        ) -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let addr = listener.local_addr().unwrap();
            let captured: Arc<Mutex<Vec<Captured>>> = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let router: Router = Arc::new(router);
            let captured_clone = Arc::clone(&captured);
            let router_clone = Arc::clone(&router);
            let stop_clone = Arc::clone(&stop);
            let handle = std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(5);
                while Instant::now() < deadline && !stop_clone.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let _ = handle_one(stream, &captured_clone, &router_clone);
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
        router: &Router,
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
        let request = Captured {
            method: method.clone(),
            path: path.clone(),
            query: query.clone(),
            content_type: content_type.clone(),
            body: body.clone(),
        };
        captured.lock().unwrap().push(request.clone());
        let (status, resp_body) = router(&request);
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

    fn arg(path: &Path) -> String {
        path.to_string_lossy().into_owned()
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

    const REPO_BODY: &[u8] = br#"{"default_branch":"main","name":"r","owner":{"login":"o"}}"#;
    const COMPARE_BODY: &[u8] = br#"{"commits":[]}"#;
    const ISSUE_BODY: &[u8] = br#"{"number":7,"title":"test title","requested_reviewers":null}"#;
    const NOT_FOUND_BODY: &[u8] = br#"{"message":"not found"}"#;

    struct CreateMock {
        repo: String,
        base: String,
        repo_body: Vec<u8>,
        created: Vec<u8>,
        pr_body: Vec<u8>,
        pr_get_status: u16,
        patch_status: u16,
        assets: Mutex<VecDeque<(u16, Vec<u8>)>>,
    }

    impl CreateMock {
        fn new(repo: &str, base: &str) -> Self {
            Self {
                repo: repo.to_owned(),
                base: base.to_owned(),
                repo_body: REPO_BODY.to_vec(),
                created: br#"{"number":7,"title":"test title","requested_reviewers":null}"#.to_vec(),
                pr_body: br#"{"number":7,"title":"test title","body":"original body","requested_reviewers":null}"#
                    .to_vec(),
                pr_get_status: 200,
                patch_status: 201,
                assets: Mutex::new(VecDeque::new()),
            }
        }

        fn route(&self, request: &Captured) -> (u16, Vec<u8>) {
            let repo_path = format!("/api/v1/repos/{}", self.repo);
            let base_path = format!("/api/v1/repos/{}", self.base);
            if request.method == "GET" && request.path == repo_path {
                return (200, self.repo_body.clone());
            }
            if request.method == "GET" && request.path.starts_with(&format!("{repo_path}/raw/")) {
                return (404, NOT_FOUND_BODY.to_vec());
            }
            if request.method == "GET" && request.path.starts_with(&format!("{base_path}/compare/"))
            {
                return (200, COMPARE_BODY.to_vec());
            }
            if request.method == "POST" && request.path == format!("{base_path}/pulls") {
                return (201, self.created.clone());
            }
            if request.method == "POST" && request.path == format!("{base_path}/issues/7/assets") {
                return self
                    .assets
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or((500, br#"{"message":"no more mocked responses"}"#.to_vec()));
            }
            if request.method == "GET" && request.path == format!("{base_path}/pulls/7") {
                return (self.pr_get_status, self.pr_body.clone());
            }
            if request.method == "PATCH" && request.path == format!("{base_path}/issues/7") {
                return (self.patch_status, ISSUE_BODY.to_vec());
            }
            (404, NOT_FOUND_BODY.to_vec())
        }
    }

    fn create_server_for(mock: CreateMock) -> Server {
        let mock = Arc::new(mock);
        Server::start_with(move |request| mock.route(request))
    }

    fn create_server_with_body(body: &str, assets: Vec<(u16, Vec<u8>)>) -> Server {
        let mut mock = CreateMock::new("o/r", "o/r");
        mock.pr_body = serde_json::json!({
            "number": 7,
            "title": "test title",
            "body": body,
            "requested_reviewers": null,
        })
        .to_string()
        .into_bytes();
        mock.assets = Mutex::new(assets.into_iter().collect());
        create_server_for(mock)
    }

    fn asset(url: &str, name: &str) -> (u16, Vec<u8>) {
        (
            201,
            serde_json::json!({"browser_download_url": url, "name": name})
                .to_string()
                .into_bytes(),
        )
    }

    #[test]
    fn attachment_messages_format_in_every_locale() {
        use fluent_bundle::FluentValue;

        type TestBundle = fluent_bundle::concurrent::FluentBundle<fluent_bundle::FluentResource>;

        let cases: Vec<(&str, Vec<(&str, FluentValue)>)> = vec![
            (
                "msg-pr-create-attach_pr",
                vec![("number", 7i64.into()), ("repo", "o/r".into())],
            ),
            (
                "msg-pr-create-attach_pr_url",
                vec![("url", "https://example.com/o/r/pulls/7".into())],
            ),
            (
                "msg-pr-create-attach_read_failed",
                vec![("file", "a.txt".into()), ("number", 7i64.into())],
            ),
            (
                "msg-pr-create-attach_upload_failed",
                vec![("file", "a.txt".into()), ("number", 7i64.into())],
            ),
            (
                "msg-pr-create-attach_missing_url",
                vec![("file", "a.txt".into()), ("number", 7i64.into())],
            ),
            (
                "msg-pr-create-attach_invalid_url",
                vec![("file", "a.txt".into()), ("number", 7i64.into())],
            ),
            (
                "msg-pr-create-attach_body_read_failed",
                vec![("number", 7i64.into())],
            ),
            (
                "msg-pr-create-attach_body_update_failed",
                vec![("number", 7i64.into())],
            ),
            ("msg-pr-create-attach_cause", vec![("cause", "boom".into())]),
            (
                "msg-pr-create-attach_partial",
                vec![
                    ("failed", 1i64.into()),
                    ("total", 2i64.into()),
                    ("number", 7i64.into()),
                    ("files", "a.txt".into()),
                ],
            ),
            (
                "msg-pr-create-attach_done",
                vec![("count", 1i64.into()), ("number", 7i64.into())],
            ),
            ("msg-pr-attach-stdin_unsupported", vec![]),
            (
                "msg-pr-attach-invalid_filename",
                vec![("file", "a.txt".into())],
            ),
            ("msg-pr-attach-not_regular", vec![("file", "a.txt".into())]),
            (
                "msg-pr-attach-access_failed",
                vec![("file", "a.txt".into())],
            ),
            ("msg-pr-attach-read_failed", vec![("file", "a.txt".into())]),
            (
                "msg-pr-attach-pr_verify_failed",
                vec![("number", 7i64.into())],
            ),
            (
                "msg-pr-attach-upload_failed",
                vec![("file", "a.txt".into())],
            ),
            ("msg-pr-attach-missing_url", vec![("file", "a.txt".into())]),
            ("msg-pr-attach-invalid_url", vec![("file", "a.txt".into())]),
            ("msg-pr-attach-write_failed", vec![("file", "a.txt".into())]),
            ("msg-pr-attach-partial", vec![("succeeded", 1i64.into())]),
        ];

        let bundles: Vec<&TestBundle> = vec![
            &crate::localization::bundles::EN_US,
            #[cfg(feature = "locale.zh-Hans")]
            &crate::localization::bundles::ZH_HANS,
        ];

        for bundle in bundles {
            for (id, args) in &cases {
                let message = bundle
                    .get_message(id)
                    .unwrap_or_else(|| panic!("missing message {id}"));
                let pattern = message
                    .value()
                    .unwrap_or_else(|| panic!("message {id} has no value"));
                let mut fluent_args = fluent_bundle::FluentArgs::new();
                for (name, value) in args {
                    fluent_args.set(*name, value.clone());
                }
                let formatted =
                    crate::localization::format_pattern(bundle, pattern, Some(&fluent_args));
                assert!(!formatted.is_empty(), "message {id} formatted empty");
            }
        }
    }

    fn create_server(
        created: Vec<u8>,
        pr_body: Vec<u8>,
        pr_get_status: u16,
        patch_status: u16,
        assets: Vec<(u16, Vec<u8>)>,
    ) -> Server {
        let mut mock = CreateMock::new("o/r", "o/r");
        mock.created = created;
        mock.pr_body = pr_body;
        mock.pr_get_status = pr_get_status;
        mock.patch_status = patch_status;
        mock.assets = Mutex::new(assets.into_iter().collect());
        create_server_for(mock)
    }

    async fn run_create(api: &Forgejo, attach: Vec<String>, out: &mut Vec<u8>) -> eyre::Result<()> {
        run_create_in(api, &test_repo(), None, attach, out).await
    }

    async fn run_create_in(
        api: &Forgejo,
        repo: &RepoName,
        base: Option<&str>,
        attach: Vec<String>,
        out: &mut Vec<u8>,
    ) -> eyre::Result<()> {
        run_create_with(
            api,
            repo,
            base,
            Some("original body".to_owned()),
            None,
            attach,
            out,
        )
        .await
    }

    async fn run_create_with(
        api: &Forgejo,
        repo: &RepoName,
        base: Option<&str>,
        body: Option<String>,
        body_file: Option<PathBuf>,
        attach: Vec<String>,
        out: &mut Vec<u8>,
    ) -> eyre::Result<()> {
        super::super::create_pr(
            repo,
            api,
            Some("test title".to_owned()),
            base.map(str::to_owned),
            Some("feature".to_owned()),
            body,
            body_file,
            false,
            false,
            false,
            None,
            attach,
            None,
            out,
        )
        .await
    }

    fn find_request(requests: &[RequestSummary], method: &str, path: &str) -> usize {
        requests
            .iter()
            .position(|(m, p, _, _, _)| m == method && p == path)
            .unwrap_or_else(|| panic!("no {method} request for {path}"))
    }

    fn patch_body(requests: &[RequestSummary]) -> String {
        patch_body_in(requests, "o/r")
    }

    fn patch_body_in(requests: &[RequestSummary], base: &str) -> String {
        let index = find_request(requests, "PATCH", &format!("/api/v1/repos/{base}/issues/7"));
        let json: serde_json::Value =
            serde_json::from_slice(&requests[index].3).expect("patch body is json");
        json["body"]
            .as_str()
            .expect("patch body is a string")
            .to_owned()
    }

    #[tokio::test]
    async fn create_with_attach_uploads_and_appends_references() {
        let image = TempFile::new("shot.PNG", b"\x89PNG\r\n\x1a\nbinary");
        let doc = TempFile::new("notes.txt", b"plain text");
        let server = create_server(
            br#"{"number":7,"title":"test title","html_url":"https://example.com/o/r/pulls/7","requested_reviewers":null}"#
                .to_vec(),
            br#"{"number":7,"title":"test title","body":"original body","requested_reviewers":null}"#.to_vec(),
            200,
            201,
            vec![
                (
                    201,
                    br#"{"browser_download_url":"https://example.com/assets/1","name":"shot.PNG"}"#
                        .to_vec(),
                ),
                (
                    201,
                    br#"{"browser_download_url":"https://example.com/assets/2","name":"notes.txt"}"#
                        .to_vec(),
                ),
            ],
        );
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let res = tokio::time::timeout(
            Duration::from_secs(10),
            run_create(&api, vec![arg(image.path()), arg(doc.path())], &mut out),
        )
        .await
        .expect("timed out");
        assert!(res.is_ok(), "{res:?}");

        let requests = server.requests();
        let create = find_request(&requests, "POST", "/api/v1/repos/o/r/pulls");
        let first_asset = find_request(&requests, "POST", "/api/v1/repos/o/r/issues/7/assets");
        let second_asset = requests
            .iter()
            .enumerate()
            .filter(|(_, (m, p, _, _, _))| m == "POST" && p == "/api/v1/repos/o/r/issues/7/assets")
            .map(|(i, _)| i)
            .nth(1)
            .expect("second upload");
        let read_pr = find_request(&requests, "GET", "/api/v1/repos/o/r/pulls/7");
        let patch = find_request(&requests, "PATCH", "/api/v1/repos/o/r/issues/7");
        assert!(create < first_asset, "the pull request is created first");
        assert!(first_asset < second_asset, "uploads keep their order");
        assert!(second_asset < read_pr, "the body is read after the uploads");
        assert!(read_pr < patch, "the body is updated last");

        assert_eq!(
            query_name(requests[first_asset].2.clone()),
            Some("shot.PNG".to_owned())
        );
        assert_eq!(
            query_name(requests[second_asset].2.clone()),
            Some("notes.txt".to_owned())
        );
        assert!(contains_subslice(&requests[first_asset].3, b"\x89PNG"));
        assert!(contains_subslice(&requests[second_asset].3, b"plain text"));
        assert!(
            !requests
                .iter()
                .any(|(_, p, _, _, _)| p.contains("/comments"))
        );

        assert_eq!(
            patch_body(&requests),
            "original body\n\n![shot.PNG](<https://example.com/assets/1>)\n[notes.txt](<https://example.com/assets/2>)"
        );

        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("#7"), "{text}");
        assert!(text.contains("o/r"), "{text}");
        assert!(text.contains("https://example.com/o/r/pulls/7"), "{text}");
    }

    #[tokio::test]
    async fn create_attach_with_base_parent_targets_the_parent_repository() {
        let doc = TempFile::new("notes.txt", b"plain text");
        let mut mock = CreateMock::new("o/r", "p/upstream");
        mock.repo_body = br#"{"default_branch":"main","name":"r","owner":{"login":"o"},"parent":{"default_branch":"upstream-main","name":"upstream","owner":{"login":"p"}}}"#.to_vec();
        mock.assets.lock().unwrap().push_back((
            201,
            br#"{"browser_download_url":"https://example.com/assets/1","name":"notes.txt"}"#
                .to_vec(),
        ));
        let server = create_server_for(mock);
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let res = tokio::time::timeout(
            Duration::from_secs(10),
            run_create_in(
                &api,
                &test_repo(),
                Some("^"),
                vec![arg(doc.path())],
                &mut out,
            ),
        )
        .await
        .expect("timed out");
        assert!(res.is_ok(), "{res:?}");

        let requests = server.requests();
        let create = find_request(&requests, "POST", "/api/v1/repos/p/upstream/pulls");
        let create_body: serde_json::Value =
            serde_json::from_slice(&requests[create].3).expect("create body is json");
        assert_eq!(create_body["head"], "o:feature");
        assert_eq!(create_body["base"], "upstream-main");
        find_request(
            &requests,
            "POST",
            "/api/v1/repos/p/upstream/issues/7/assets",
        );
        find_request(&requests, "GET", "/api/v1/repos/p/upstream/pulls/7");
        find_request(&requests, "PATCH", "/api/v1/repos/p/upstream/issues/7");
        let compare = requests
            .iter()
            .find(|(m, p, _, _, _)| {
                m == "GET" && p.starts_with("/api/v1/repos/p/upstream/compare/")
            })
            .expect("compare request");
        assert!(
            compare.1.contains("upstream-main...o%3Afeature"),
            "the head is qualified with its owner: {}",
            compare.1
        );
        assert!(
            !requests
                .iter()
                .any(|(_, p, _, _, _)| p.starts_with("/api/v1/repos/o/r/issues")
                    || p.starts_with("/api/v1/repos/o/r/pulls")),
            "nothing is sent to the head repository: {requests:?}"
        );
        assert_eq!(
            patch_body_in(&requests, "p/upstream"),
            "original body\n\n[notes.txt](<https://example.com/assets/1>)"
        );
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("p/upstream"), "{text}");
    }

    #[tokio::test]
    async fn create_with_explicit_repo_targets_that_repository() {
        let doc = TempFile::new("notes.txt", b"plain text");
        let mut mock = CreateMock::new("x/y", "x/y");
        mock.repo_body = br#"{"default_branch":"main","name":"y","owner":{"login":"x"}}"#.to_vec();
        mock.assets.lock().unwrap().push_back((
            201,
            br#"{"browser_download_url":"https://example.com/assets/1","name":"notes.txt"}"#
                .to_vec(),
        ));
        let server = create_server_for(mock);
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let res = tokio::time::timeout(
            Duration::from_secs(10),
            run_create_in(
                &api,
                &RepoName::new("x".to_owned(), "y".to_owned()),
                None,
                vec![arg(doc.path())],
                &mut out,
            ),
        )
        .await
        .expect("timed out");
        assert!(res.is_ok(), "{res:?}");

        let requests = server.requests();
        find_request(&requests, "POST", "/api/v1/repos/x/y/pulls");
        find_request(&requests, "POST", "/api/v1/repos/x/y/issues/7/assets");
        find_request(&requests, "GET", "/api/v1/repos/x/y/pulls/7");
        find_request(&requests, "PATCH", "/api/v1/repos/x/y/issues/7");
        assert!(
            !requests
                .iter()
                .any(|(_, p, _, _, _)| p.starts_with("/api/v1/repos/o/r")),
            "the selected repository is used: {requests:?}"
        );
        assert_eq!(
            patch_body_in(&requests, "x/y"),
            "original body\n\n[notes.txt](<https://example.com/assets/1>)"
        );
    }

    #[tokio::test]
    async fn create_without_attach_makes_no_attachment_request() {
        let server = create_server(
            br#"{"number":7,"title":"test title","requested_reviewers":null}"#.to_vec(),
            br#"{"number":7,"title":"test title","body":"original body","requested_reviewers":null}"#.to_vec(),
            200,
            201,
            vec![],
        );
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let res = tokio::time::timeout(Duration::from_secs(10), run_create(&api, vec![], &mut out))
            .await
            .expect("timed out");
        assert!(res.is_ok(), "{res:?}");
        let requests = server.requests();
        assert!(find_request(&requests, "POST", "/api/v1/repos/o/r/pulls") < requests.len());
        assert!(
            !requests
                .iter()
                .any(|(_, p, _, _, _)| p.ends_with("/assets") || p == "/api/v1/repos/o/r/issues/7")
        );
        assert!(out.is_empty(), "{:?}", String::from_utf8_lossy(&out));
    }

    #[tokio::test]
    async fn create_attach_preflight_failure_makes_no_request() {
        let missing = MissingPath::new("missing.txt");
        let server = create_server(
            br#"{"number":7,"title":"test title","requested_reviewers":null}"#.to_vec(),
            br#"{"number":7,"title":"test title","requested_reviewers":null}"#.to_vec(),
            200,
            201,
            vec![],
        );
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let res = tokio::time::timeout(
            Duration::from_secs(10),
            run_create(&api, vec![arg(missing.path())], &mut out),
        )
        .await
        .expect("timed out");
        assert!(res.is_err());
        assert!(server.requests().is_empty());
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn create_attach_appends_to_empty_body() {
        let doc = TempFile::new("notes.txt", b"plain text");
        let server = create_server(
            br#"{"number":7,"title":"test title","requested_reviewers":null}"#.to_vec(),
            br#"{"number":7,"title":"test title","body":null,"requested_reviewers":null}"#.to_vec(),
            200,
            201,
            vec![(
                201,
                br#"{"browser_download_url":"https://example.com/assets/1","name":"notes.txt"}"#
                    .to_vec(),
            )],
        );
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let res = tokio::time::timeout(
            Duration::from_secs(10),
            run_create(&api, vec![arg(doc.path())], &mut out),
        )
        .await
        .expect("timed out");
        assert!(res.is_ok(), "{res:?}");
        let requests = server.requests();
        assert_eq!(
            patch_body(&requests),
            "[notes.txt](<https://example.com/assets/1>)"
        );
    }

    #[tokio::test]
    async fn create_attach_continues_after_upload_failure() {
        let first = TempFile::new("first.txt", b"one");
        let second = TempFile::new("second.png", b"two");
        let server = create_server(
            br#"{"number":7,"title":"test title","requested_reviewers":null}"#.to_vec(),
            br#"{"number":7,"title":"test title","body":"original body","requested_reviewers":null}"#.to_vec(),
            200,
            201,
            vec![
                (500, br#"{"message":"boom"}"#.to_vec()),
                (
                    201,
                    br#"{"browser_download_url":"https://example.com/assets/2","name":"second.png"}"#
                        .to_vec(),
                ),
            ],
        );
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let res = tokio::time::timeout(
            Duration::from_secs(10),
            run_create(&api, vec![arg(first.path()), arg(second.path())], &mut out),
        )
        .await
        .expect("timed out");
        let err = res.unwrap_err();
        let message = format!("{err:?}");
        assert!(message.contains("first.txt"), "{message}");
        assert!(message.contains("#7"), "{message}");
        assert!(
            message.contains("unexpected status code 500"),
            "the underlying cause is kept: {message}"
        );

        let requests = server.requests();
        let uploads = requests
            .iter()
            .filter(|(_, p, _, _, _)| p == "/api/v1/repos/o/r/issues/7/assets")
            .count();
        assert_eq!(uploads, 2, "the second file is uploaded anyway");
        assert_eq!(
            patch_body(&requests),
            "original body\n\n![second.png](<https://example.com/assets/2>)"
        );
    }

    #[tokio::test]
    async fn create_attach_all_uploads_failing_skips_the_body_update() {
        let first = TempFile::new("first.txt", b"one");
        let second = TempFile::new("second.txt", b"two");
        let server = create_server(
            br#"{"number":7,"title":"test title","requested_reviewers":null}"#.to_vec(),
            br#"{"number":7,"title":"test title","body":"original body","requested_reviewers":null}"#
                .to_vec(),
            200,
            201,
            vec![
                (500, br#"{"message":"boom"}"#.to_vec()),
                (500, br#"{"message":"boom"}"#.to_vec()),
            ],
        );
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let res = tokio::time::timeout(
            Duration::from_secs(10),
            run_create(&api, vec![arg(first.path()), arg(second.path())], &mut out),
        )
        .await
        .expect("timed out");
        let err = res.unwrap_err();
        let message = format!("{err:?}");
        assert!(message.contains("first.txt"), "{message}");
        assert!(message.contains("second.txt"), "{message}");
        assert!(message.contains("#7"), "{message}");

        let requests = server.requests();
        assert!(
            !requests
                .iter()
                .any(|(_, p, _, _, _)| p == "/api/v1/repos/o/r/issues/7"),
            "the body is not updated without any reference"
        );
    }

    #[tokio::test]
    async fn create_attach_body_update_failure_prints_fallback_links() {
        let doc = TempFile::new("notes.txt", b"plain text");
        let server = create_server(
            br#"{"number":7,"title":"test title","requested_reviewers":null}"#.to_vec(),
            br#"{"number":7,"title":"test title","body":"original body","requested_reviewers":null}"#.to_vec(),
            200,
            500,
            vec![(
                201,
                br#"{"browser_download_url":"https://example.com/assets/1","name":"notes.txt"}"#
                    .to_vec(),
            )],
        );
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let res = tokio::time::timeout(
            Duration::from_secs(10),
            run_create(&api, vec![arg(doc.path())], &mut out),
        )
        .await
        .expect("timed out");
        let err = res.unwrap_err();
        let message = format!("{err:?}");
        assert!(message.contains("may already have succeeded"), "{message}");
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("[notes.txt](<https://example.com/assets/1>)"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn create_attach_body_read_failure_prints_fallback_links() {
        let doc = TempFile::new("notes.txt", b"plain text");
        let server = create_server(
            br#"{"number":7,"title":"test title","requested_reviewers":null}"#.to_vec(),
            br#"{"message":"boom"}"#.to_vec(),
            500,
            201,
            vec![(
                201,
                br#"{"browser_download_url":"https://example.com/assets/1","name":"notes.txt"}"#
                    .to_vec(),
            )],
        );
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let res = tokio::time::timeout(
            Duration::from_secs(10),
            run_create(&api, vec![arg(doc.path())], &mut out),
        )
        .await
        .expect("timed out");
        let err = res.unwrap_err();
        let message = format!("{err:?}");
        assert!(message.contains("could not read the body"), "{message}");
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("[notes.txt](<https://example.com/assets/1>)"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn create_attach_without_html_url_reports_the_number() {
        let doc = TempFile::new("notes.txt", b"plain text");
        let server = create_server(
            br#"{"number":7,"title":"test title","requested_reviewers":null}"#.to_vec(),
            br#"{"number":7,"title":"test title","body":null,"requested_reviewers":null}"#.to_vec(),
            200,
            201,
            vec![(
                201,
                br#"{"browser_download_url":"https://example.com/assets/1","name":"notes.txt"}"#
                    .to_vec(),
            )],
        );
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let res = tokio::time::timeout(
            Duration::from_secs(10),
            run_create(&api, vec![arg(doc.path())], &mut out),
        )
        .await
        .expect("timed out");
        assert!(res.is_ok(), "{res:?}");
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("#7"), "{text}");
        assert!(text.contains("o/r"), "{text}");
        assert!(!text.contains("pull request URL"), "{text}");
    }

    #[tokio::test]
    async fn create_attach_rewrites_an_inline_image_in_place() {
        let image = TempFile::new("shot.png", b"\x89PNG");
        let body = format!("before\n\n![my alt]({})\n\nafter\n", image.path().display());
        let server = create_server_with_body(
            &body,
            vec![asset("https://example.com/assets/1", "shot.png")],
        );
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let res = tokio::time::timeout(
            Duration::from_secs(10),
            run_create_with(
                &api,
                &test_repo(),
                None,
                Some(body.clone()),
                None,
                vec![arg(image.path())],
                &mut out,
            ),
        )
        .await
        .expect("timed out");
        assert!(res.is_ok(), "{res:?}");
        let requests = server.requests();
        assert_eq!(
            patch_body(&requests),
            "before\n\n![my alt](<https://example.com/assets/1>)\n\nafter\n"
        );
    }

    #[tokio::test]
    async fn create_attach_rewrites_every_occurrence_of_a_reference() {
        let image = TempFile::new("shot.png", b"\x89PNG");
        let body = format!(
            "![first]({})\n\ntext\n\n[second]({})\n",
            image.path().display(),
            image.path().display()
        );
        let server = create_server_with_body(
            &body,
            vec![asset("https://example.com/assets/1", "shot.png")],
        );
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let res = tokio::time::timeout(
            Duration::from_secs(10),
            run_create_with(
                &api,
                &test_repo(),
                None,
                Some(body.clone()),
                None,
                vec![arg(image.path())],
                &mut out,
            ),
        )
        .await
        .expect("timed out");
        assert!(res.is_ok(), "{res:?}");
        let requests = server.requests();
        assert_eq!(
            patch_body(&requests),
            "![first](<https://example.com/assets/1>)\n\ntext\n\n[second](<https://example.com/assets/1>)\n"
        );
    }

    #[tokio::test]
    async fn create_attach_keeps_the_alt_of_the_body_and_does_not_append() {
        let image = TempFile::new("shot.png", b"\x89PNG");
        let body = format!("![body alt]({})\n", image.path().display());
        let server = create_server_with_body(
            &body,
            vec![asset("https://example.com/assets/1", "shot.png")],
        );
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let res = tokio::time::timeout(
            Duration::from_secs(10),
            run_create_with(
                &api,
                &test_repo(),
                None,
                Some(body.clone()),
                None,
                vec![format!("{}#flag alt", arg(image.path()))],
                &mut out,
            ),
        )
        .await
        .expect("timed out");
        assert!(res.is_ok(), "{res:?}");
        let requests = server.requests();
        assert_eq!(
            patch_body(&requests),
            "![body alt](<https://example.com/assets/1>)\n"
        );
    }

    #[tokio::test]
    async fn create_attach_uses_the_explicit_alt_only_when_appending() {
        let image = TempFile::new("shot.png", b"\x89PNG");
        let body = "no reference here\n".to_owned();
        let server = create_server_with_body(
            &body,
            vec![asset("https://example.com/assets/1", "shot.png")],
        );
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let res = tokio::time::timeout(
            Duration::from_secs(10),
            run_create_with(
                &api,
                &test_repo(),
                None,
                Some(body.clone()),
                None,
                vec![format!("{}#my alt", arg(image.path()))],
                &mut out,
            ),
        )
        .await
        .expect("timed out");
        assert!(res.is_ok(), "{res:?}");
        let requests = server.requests();
        assert_eq!(
            patch_body(&requests),
            "no reference here\n\n![my alt](<https://example.com/assets/1>)"
        );
    }

    #[tokio::test]
    async fn create_attach_leaves_code_and_prose_untouched() {
        let image = TempFile::new("shot.png", b"\x89PNG");
        let path = image.path().display().to_string();
        let body = format!(
            "```\n![code]({path})\n```\n\n    ![indented]({path})\n\ninline `![code]({path})` and prose {path}\n",
            path = path
        );
        let server = create_server_with_body(
            &body,
            vec![asset("https://example.com/assets/1", "shot.png")],
        );
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let res = tokio::time::timeout(
            Duration::from_secs(10),
            run_create_with(
                &api,
                &test_repo(),
                None,
                Some(body.clone()),
                None,
                vec![arg(image.path())],
                &mut out,
            ),
        )
        .await
        .expect("timed out");
        assert!(res.is_ok(), "{res:?}");
        let requests = server.requests();
        assert_eq!(
            patch_body(&requests),
            format!("{body}\n![shot.png](<https://example.com/assets/1>)")
        );
    }

    #[tokio::test]
    async fn create_attach_leaves_remote_and_anchor_destinations_untouched() {
        let image = TempFile::new("shot.png", b"\x89PNG");
        let body = "[remote](https://example.com/x.png) and [anchor](#section)\n".to_owned();
        let server = create_server_with_body(
            &body,
            vec![asset("https://example.com/assets/1", "shot.png")],
        );
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let res = tokio::time::timeout(
            Duration::from_secs(10),
            run_create_with(
                &api,
                &test_repo(),
                None,
                Some(body.clone()),
                None,
                vec![arg(image.path())],
                &mut out,
            ),
        )
        .await
        .expect("timed out");
        assert!(res.is_ok(), "{res:?}");
        let requests = server.requests();
        assert_eq!(
            patch_body(&requests),
            "[remote](https://example.com/x.png) and [anchor](#section)\n\n![shot.png](<https://example.com/assets/1>)"
        );
    }

    #[tokio::test]
    async fn create_attach_keeps_the_title_and_the_angle_syntax() {
        let image = TempFile::new("shot.png", b"\x89PNG");
        let body = format!("[label](<{}> \"the title\")\n", image.path().display());
        let server = create_server_with_body(
            &body,
            vec![asset("https://example.com/assets/1", "shot.png")],
        );
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let res = tokio::time::timeout(
            Duration::from_secs(10),
            run_create_with(
                &api,
                &test_repo(),
                None,
                Some(body.clone()),
                None,
                vec![arg(image.path())],
                &mut out,
            ),
        )
        .await
        .expect("timed out");
        assert!(res.is_ok(), "{res:?}");
        let requests = server.requests();
        assert_eq!(
            patch_body(&requests),
            "[label](<https://example.com/assets/1> \"the title\")\n"
        );
    }

    #[tokio::test]
    async fn create_attach_rewrites_a_reference_definition() {
        let image = TempFile::new("shot.png", b"\x89PNG");
        let body = format!(
            "![alt][ref] and [other][ref]\n\n[ref]: {} \"title\"\n",
            image.path().display()
        );
        let server = create_server_with_body(
            &body,
            vec![asset("https://example.com/assets/1", "shot.png")],
        );
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let res = tokio::time::timeout(
            Duration::from_secs(10),
            run_create_with(
                &api,
                &test_repo(),
                None,
                Some(body.clone()),
                None,
                vec![arg(image.path())],
                &mut out,
            ),
        )
        .await
        .expect("timed out");
        assert!(res.is_ok(), "{res:?}");
        let requests = server.requests();
        assert_eq!(
            patch_body(&requests),
            "![alt][ref] and [other][ref]\n\n[ref]: <https://example.com/assets/1> \"title\"\n"
        );
    }

    #[tokio::test]
    async fn create_attach_resolves_a_reference_next_to_the_body_file() {
        let dir = ScratchDir::new();
        // The file has the same name as a file of the working directory, so
        // only the file next to the body file can match the reference.
        let image = dir.path().join("Cargo.toml");
        std::fs::write(&image, b"not the manifest").unwrap();
        let body_file = dir.path().join("body.md");
        let body = "![alt](./Cargo.toml)\n".to_owned();
        std::fs::write(&body_file, &body).unwrap();
        let server = create_server_with_body(
            &body,
            vec![asset("https://example.com/assets/1", "Cargo.toml")],
        );
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let res = tokio::time::timeout(
            Duration::from_secs(10),
            run_create_with(
                &api,
                &test_repo(),
                None,
                None,
                Some(body_file),
                vec![arg(&image)],
                &mut out,
            ),
        )
        .await
        .expect("timed out");
        assert!(res.is_ok(), "{res:?}");
        let requests = server.requests();
        assert_eq!(
            patch_body(&requests),
            "![alt](<https://example.com/assets/1>)\n"
        );
    }

    #[tokio::test]
    async fn create_attach_falls_back_to_the_working_directory() {
        let dir = ScratchDir::new();
        let body_file = dir.path().join("body.md");
        let body = "![alt](Cargo.toml)\n".to_owned();
        std::fs::write(&body_file, &body).unwrap();
        let server = create_server_with_body(
            &body,
            vec![asset("https://example.com/assets/1", "Cargo.toml")],
        );
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let res = tokio::time::timeout(
            Duration::from_secs(10),
            run_create_with(
                &api,
                &test_repo(),
                None,
                None,
                Some(body_file),
                vec!["Cargo.toml".to_owned()],
                &mut out,
            ),
        )
        .await
        .expect("timed out");
        assert!(res.is_ok(), "{res:?}");
        let requests = server.requests();
        assert_eq!(
            patch_body(&requests),
            "![alt](<https://example.com/assets/1>)\n"
        );
    }

    #[tokio::test]
    async fn create_attach_appends_only_the_unreferenced_files_in_order() {
        let referenced = TempFile::new("referenced.png", b"\x89PNG");
        let first = TempFile::new("first.txt", b"one");
        let second = TempFile::new("second.txt", b"two");
        let body = format!("![alt]({})\n", referenced.path().display());
        let server = create_server_with_body(
            &body,
            vec![
                asset("https://example.com/assets/1", "referenced.png"),
                asset("https://example.com/assets/2", "first.txt"),
                asset("https://example.com/assets/3", "second.txt"),
            ],
        );
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let res = tokio::time::timeout(
            Duration::from_secs(10),
            run_create_with(
                &api,
                &test_repo(),
                None,
                Some(body.clone()),
                None,
                vec![
                    arg(referenced.path()),
                    arg(first.path()),
                    arg(second.path()),
                ],
                &mut out,
            ),
        )
        .await
        .expect("timed out");
        assert!(res.is_ok(), "{res:?}");
        let requests = server.requests();
        assert_eq!(
            patch_body(&requests),
            "![alt](<https://example.com/assets/1>)\n\n[first.txt](<https://example.com/assets/2>)\n[second.txt](<https://example.com/assets/3>)"
        );
    }

    #[tokio::test]
    async fn create_attach_resolves_a_reference_in_the_working_directory() {
        let body = "![alt](Cargo.toml)\n".to_owned();
        let server = create_server_with_body(
            &body,
            vec![asset("https://example.com/assets/1", "Cargo.toml")],
        );
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let res = tokio::time::timeout(
            Duration::from_secs(10),
            run_create_with(
                &api,
                &test_repo(),
                None,
                Some(body.clone()),
                None,
                vec!["Cargo.toml".to_owned()],
                &mut out,
            ),
        )
        .await
        .expect("timed out");
        assert!(res.is_ok(), "{res:?}");
        let requests = server.requests();
        assert_eq!(
            patch_body(&requests),
            "![alt](<https://example.com/assets/1>)\n"
        );
    }

    #[tokio::test]
    async fn create_attach_matches_a_percent_encoded_destination() {
        let image = TempFile::new("my file.png", b"\x89PNG");
        let encoded = image.path().to_string_lossy().replace(' ', "%20");
        let body = format!("![alt]({encoded})\n");
        let server = create_server_with_body(
            &body,
            vec![asset("https://example.com/assets/1", "my file.png")],
        );
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let res = tokio::time::timeout(
            Duration::from_secs(10),
            run_create_with(
                &api,
                &test_repo(),
                None,
                Some(body.clone()),
                None,
                vec![arg(image.path())],
                &mut out,
            ),
        )
        .await
        .expect("timed out");
        assert!(res.is_ok(), "{res:?}");
        let requests = server.requests();
        assert_eq!(
            patch_body(&requests),
            "![alt](<https://example.com/assets/1>)\n"
        );
    }

    #[tokio::test]
    async fn create_attach_matches_a_utf8_destination() {
        let image = TempFile::new("café.png", b"\x89PNG");
        let body = format!("![alt]({})\n", image.path().display());
        let server = create_server_with_body(
            &body,
            vec![asset("https://example.com/assets/1", "café.png")],
        );
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let res = tokio::time::timeout(
            Duration::from_secs(10),
            run_create_with(
                &api,
                &test_repo(),
                None,
                Some(body.clone()),
                None,
                vec![arg(image.path())],
                &mut out,
            ),
        )
        .await
        .expect("timed out");
        assert!(res.is_ok(), "{res:?}");
        let requests = server.requests();
        assert_eq!(
            patch_body(&requests),
            "![alt](<https://example.com/assets/1>)\n"
        );
    }

    #[tokio::test]
    async fn create_attach_keeps_the_reference_of_a_failed_upload() {
        let failed = TempFile::new("failed.png", b"\x89PNG");
        let uploaded = TempFile::new("ok.png", b"\x89PNG");
        let body = format!(
            "![failed]({})\n\n![ok]({})\n",
            failed.path().display(),
            uploaded.path().display()
        );
        let server = create_server_with_body(
            &body,
            vec![
                (500, br#"{"message":"boom"}"#.to_vec()),
                asset("https://example.com/assets/2", "ok.png"),
            ],
        );
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let res = tokio::time::timeout(
            Duration::from_secs(10),
            run_create_with(
                &api,
                &test_repo(),
                None,
                Some(body.clone()),
                None,
                vec![arg(failed.path()), arg(uploaded.path())],
                &mut out,
            ),
        )
        .await
        .expect("timed out");
        let err = res.unwrap_err();
        let message = format!("{err:?}");
        assert!(message.contains("failed.png"), "{message}");
        let requests = server.requests();
        assert_eq!(
            patch_body(&requests),
            format!(
                "![failed]({})\n\n![ok](<https://example.com/assets/2>)\n",
                failed.path().display()
            )
        );
    }

    #[tokio::test]
    async fn create_attach_appends_a_second_upload_of_the_same_file() {
        let image = TempFile::new("shot.png", b"\x89PNG");
        let body = format!("![alt]({})\n", image.path().display());
        let server = create_server_with_body(
            &body,
            vec![
                asset("https://example.com/assets/1", "shot.png"),
                asset("https://example.com/assets/2", "shot.png"),
            ],
        );
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let res = tokio::time::timeout(
            Duration::from_secs(10),
            run_create_with(
                &api,
                &test_repo(),
                None,
                Some(body.clone()),
                None,
                vec![arg(image.path()), arg(image.path())],
                &mut out,
            ),
        )
        .await
        .expect("timed out");
        assert!(res.is_ok(), "{res:?}");
        let requests = server.requests();
        assert_eq!(
            patch_body(&requests),
            "![alt](<https://example.com/assets/1>)\n\n![shot.png](<https://example.com/assets/2>)"
        );
    }

    #[test]
    fn attachment_arguments_are_split_into_file_and_alt() {
        let dir = ScratchDir::new();
        let image = dir.path().join("shot.png");
        std::fs::write(&image, b"\x89PNG").unwrap();
        let hash = dir.path().join("we#ird.png");
        std::fs::write(&hash, b"\x89PNG").unwrap();

        let spec = parse_attachment_arg(&arg(&image));
        assert_eq!(spec.path, image);
        assert_eq!(spec.alt, None);

        let spec = parse_attachment_arg(&format!("{}#my alt", arg(&image)));
        assert_eq!(spec.path, image);
        assert_eq!(spec.alt, Some("my alt".to_owned()));

        let spec = parse_attachment_arg(&format!("{}#", arg(&image)));
        assert_eq!(spec.path, image);
        assert_eq!(spec.alt, None);

        let spec = parse_attachment_arg(&arg(&hash));
        assert_eq!(spec.path, hash);
        assert_eq!(spec.alt, None);

        let spec = parse_attachment_arg("missing.png#my alt");
        assert_eq!(spec.path, PathBuf::from("missing.png"));
        assert_eq!(spec.alt, Some("my alt".to_owned()));

        let spec = parse_attachment_arg("missing.png");
        assert_eq!(spec.path, PathBuf::from("missing.png"));
        assert_eq!(spec.alt, None);

        // The longest existing path before the "#" is the file.
        let spec = parse_attachment_arg(&format!("{}#alt", arg(&hash)));
        assert_eq!(spec.path, hash);
        assert_eq!(spec.alt, Some("alt".to_owned()));
    }

    #[tokio::test]
    async fn a_missing_attachment_is_reported_with_its_file_name() {
        let server = create_server(
            br#"{"number":7,"title":"test title","requested_reviewers":null}"#.to_vec(),
            br#"{"number":7,"title":"test title"}"#.to_vec(),
            200,
            201,
            vec![],
        );
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let res = tokio::time::timeout(
            Duration::from_secs(10),
            run_create(&api, vec!["missing.png#my alt".to_owned()], &mut out),
        )
        .await
        .expect("timed out");
        let err = res.unwrap_err();
        let message = format!("{err:?}");
        assert!(message.contains("missing.png"), "{message}");
        assert!(server.requests().is_empty());
    }

    #[tokio::test]
    async fn create_attach_ignores_a_definition_inside_a_fence() {
        let image = TempFile::new("shot.png", b"\x89PNG");
        let path = image.path().display().to_string();
        let body = format!(
            "```\n[shot]: {path}\n```\n\n![shot][shot]\n\n[shot]: {path}\n",
            path = path
        );
        let server = create_server_with_body(
            &body,
            vec![asset("https://example.com/assets/1", "shot.png")],
        );
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let res = tokio::time::timeout(
            Duration::from_secs(10),
            run_create_with(
                &api,
                &test_repo(),
                None,
                Some(body.clone()),
                None,
                vec![arg(image.path())],
                &mut out,
            ),
        )
        .await
        .expect("timed out");
        assert!(res.is_ok(), "{res:?}");
        let requests = server.requests();
        assert_eq!(
            patch_body(&requests),
            format!(
                "```\n[shot]: {path}\n```\n\n![shot][shot]\n\n[shot]: <https://example.com/assets/1>\n",
                path = path
            )
        );
    }

    #[tokio::test]
    async fn create_attach_ignores_a_definition_inside_an_html_block() {
        let image = TempFile::new("shot.png", b"\x89PNG");
        let path = image.path().display().to_string();
        let body = format!(
            "<div>\n[shot]: {path}\n</div>\n\n![shot][shot]\n\n[shot]: {path}\n",
            path = path
        );
        let server = create_server_with_body(
            &body,
            vec![asset("https://example.com/assets/1", "shot.png")],
        );
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let res = tokio::time::timeout(
            Duration::from_secs(10),
            run_create_with(
                &api,
                &test_repo(),
                None,
                Some(body.clone()),
                None,
                vec![arg(image.path())],
                &mut out,
            ),
        )
        .await
        .expect("timed out");
        assert!(res.is_ok(), "{res:?}");
        let requests = server.requests();
        assert_eq!(
            patch_body(&requests),
            format!(
                "<div>\n[shot]: {path}\n</div>\n\n![shot][shot]\n\n[shot]: <https://example.com/assets/1>\n",
                path = path
            )
        );
    }

    #[tokio::test]
    async fn create_attach_matches_an_escaped_destination() {
        let image = TempFile::new("my(1).png", b"\x89PNG");
        let escaped = image
            .path()
            .to_string_lossy()
            .replace('(', "\\(")
            .replace(')', "\\)");
        let body = format!("![alt]({escaped})\n");
        let server = create_server_with_body(
            &body,
            vec![asset("https://example.com/assets/1", "my(1).png")],
        );
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let res = tokio::time::timeout(
            Duration::from_secs(10),
            run_create_with(
                &api,
                &test_repo(),
                None,
                Some(body.clone()),
                None,
                vec![arg(image.path())],
                &mut out,
            ),
        )
        .await
        .expect("timed out");
        assert!(res.is_ok(), "{res:?}");
        let requests = server.requests();
        assert_eq!(
            patch_body(&requests),
            "![alt](<https://example.com/assets/1>)\n"
        );
    }

    #[tokio::test]
    async fn create_attach_ignores_an_invalid_definition_prefix() {
        let image = TempFile::new("shot.png", b"\x89PNG");
        let path = image.path().display().to_string();
        let body = format!(
            "[foo]:\n[shot]: {path}\n\nsee [shot]\n\n[shot]: {path}\n",
            path = path
        );
        let server = create_server_with_body(
            &body,
            vec![asset("https://example.com/assets/1", "shot.png")],
        );
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let res = tokio::time::timeout(
            Duration::from_secs(10),
            run_create_with(
                &api,
                &test_repo(),
                None,
                Some(body.clone()),
                None,
                vec![arg(image.path())],
                &mut out,
            ),
        )
        .await
        .expect("timed out");
        assert!(res.is_ok(), "{res:?}");
        let requests = server.requests();
        assert_eq!(
            patch_body(&requests),
            format!(
                "[foo]:\n[shot]: {path}\n\nsee [shot]\n\n[shot]: <https://example.com/assets/1>\n",
                path = path
            )
        );
    }

    #[tokio::test]
    async fn create_attach_ignores_a_duplicate_after_a_container_definition() {
        let image = TempFile::new("shot.png", b"\x89PNG");
        let path = image.path().display().to_string();
        let body = format!("- [shot]: {path}\ntext\n[shot]: {path}\n\n![x][shot]\n");
        let server = create_server_with_body(
            &body,
            vec![asset("https://example.com/assets/1", "shot.png")],
        );
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let res = tokio::time::timeout(
            Duration::from_secs(10),
            run_create_with(
                &api,
                &test_repo(),
                None,
                Some(body.clone()),
                None,
                vec![arg(image.path())],
                &mut out,
            ),
        )
        .await
        .expect("timed out");
        assert!(res.is_ok(), "{res:?}");
        let requests = server.requests();
        assert_eq!(
            patch_body(&requests),
            format!("{body}\n![shot.png](<https://example.com/assets/1>)"),
            "the unrelated paragraph line stays as it is"
        );
    }

    #[tokio::test]
    async fn create_attach_ignores_a_duplicate_in_another_block() {
        let image = TempFile::new("shot.png", b"\x89PNG");
        let path = image.path().display().to_string();
        let body = format!("- [shot]: {path}\n\nsee [shot]\n\n[shot]: {path}\n\n![x][shot]\n");
        let server = create_server_with_body(
            &body,
            vec![asset("https://example.com/assets/1", "shot.png")],
        );
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let res = tokio::time::timeout(
            Duration::from_secs(10),
            run_create_with(
                &api,
                &test_repo(),
                None,
                Some(body.clone()),
                None,
                vec![arg(image.path())],
                &mut out,
            ),
        )
        .await
        .expect("timed out");
        assert!(res.is_ok(), "{res:?}");
        let requests = server.requests();
        assert_eq!(
            patch_body(&requests),
            format!("{body}\n![shot.png](<https://example.com/assets/1>)"),
            "the later duplicate stays as it is and the file is appended"
        );
    }

    #[tokio::test]
    async fn create_attach_appends_when_the_definition_probe_is_undecidable() {
        let image = TempFile::new("shot.png", b"\x89PNG");
        let path = image.path().display().to_string();
        // The earlier definition is large enough that its uses could use up the
        // parser reference budget, so the later definition cannot be proven to
        // be the winning one and the file is appended instead.
        let long_url = format!("./{}", "a".repeat(60_000));
        let body = format!(
            "[other]: {long_url}\n\n![a][other]\n\n![b][other]\n\n[shot]: {path}\n\n![c][shot]\n"
        );
        let server = create_server_with_body(
            &body,
            vec![asset("https://example.com/assets/1", "shot.png")],
        );
        let api = test_api(server.addr);
        let mut out: Vec<u8> = Vec::new();
        let res = tokio::time::timeout(
            Duration::from_secs(20),
            run_create_with(
                &api,
                &test_repo(),
                None,
                Some(body.clone()),
                None,
                vec![arg(image.path())],
                &mut out,
            ),
        )
        .await
        .expect("timed out");
        assert!(res.is_ok(), "{res:?}");
        let requests = server.requests();
        assert_eq!(
            patch_body(&requests),
            format!("{body}\n![shot.png](<https://example.com/assets/1>)"),
            "the undecidable candidate keeps its bytes and the file is appended"
        );
    }
}
