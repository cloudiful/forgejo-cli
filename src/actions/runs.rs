use std::io::Write;

use forgejo_api::{
    Forgejo,
    structs::{ActionRun, ListActionRunsQuery, ListActionRunsQueryStatus},
};

use crate::{SpecialRender, ftl_format, repo::RepoName};

/// The biggest page the runs listing asks the API for.
const RUNS_PAGE_SIZE: usize = 50;
/// The biggest number of runs a single listing may return.
pub(super) const RUNS_LIMIT_MAX: i64 = 1000;
/// How many runs are inspected when a run number is looked up.
pub(super) const RUN_NUMBER_LOOKUP_LIMIT: usize = 100;

pub(super) fn parse_run_status(value: &str) -> Result<ListActionRunsQueryStatus, String> {
    match value.to_ascii_lowercase().as_str() {
        "unknown" => Ok(ListActionRunsQueryStatus::Unknown),
        "waiting" => Ok(ListActionRunsQueryStatus::Waiting),
        "running" => Ok(ListActionRunsQueryStatus::Running),
        "success" => Ok(ListActionRunsQueryStatus::Success),
        "failure" => Ok(ListActionRunsQueryStatus::Failure),
        "cancelled" => Ok(ListActionRunsQueryStatus::Cancelled),
        "skipped" => Ok(ListActionRunsQueryStatus::Skipped),
        "blocked" => Ok(ListActionRunsQueryStatus::Blocked),
        other => Err(ftl_format!("msg-actions-runs-status-invalid", status = other).into_owned()),
    }
}

/// The API wants a full ref, but users think in branch names.
fn branch_to_ref(branch: &str) -> String {
    if branch.starts_with("refs/") {
        branch.to_owned()
    } else {
        format!("refs/heads/{branch}")
    }
}

pub(super) struct RunsOptions {
    pub(super) limit: u32,
    pub(super) status: Option<ListActionRunsQueryStatus>,
    pub(super) workflow: Option<String>,
    pub(super) branch: Option<String>,
    pub(super) commit: Option<String>,
}

pub(super) async fn view_runs(
    repo: &RepoName,
    api: &Forgejo,
    options: &RunsOptions,
    out: &mut dyn Write,
) -> eyre::Result<()> {
    let query = ListActionRunsQuery {
        event: None,
        status: options.status.map(|status| vec![status]),
        run_number: None,
        head_sha: options.commit.clone(),
        r#ref: options.branch.as_deref().map(branch_to_ref),
        workflow_id: options.workflow.clone(),
    };

    for run in list_runs(repo, api, query, options.limit as usize).await? {
        writeln!(out, "{}", format_run(&run))?;
    }
    out.flush()?;

    Ok(())
}

/// Collects runs until the limit, the total count or an empty page is reached.
pub(super) async fn list_runs(
    repo: &RepoName,
    api: &Forgejo,
    query: ListActionRunsQuery,
    limit: usize,
) -> eyre::Result<Vec<ActionRun>> {
    let page_size = limit.min(RUNS_PAGE_SIZE) as u32;
    let mut page = 1u32;
    let mut runs: Vec<ActionRun> = Vec::new();

    loop {
        let res = api
            .list_action_runs(repo.owner(), repo.name(), query.clone())
            .page(page)
            .page_size(page_size)
            .await?;

        let page_runs = res.workflow_runs.unwrap_or_default();
        if page_runs.is_empty() {
            break;
        }
        let total_count = res.total_count;
        runs.extend(page_runs);

        if runs.len() >= limit {
            break;
        }
        if let Some(total_count) = total_count
            && runs.len() >= total_count.max(0) as usize
        {
            break;
        }
        page = page.saturating_add(1);
    }

    runs.truncate(limit);
    Ok(runs)
}

fn format_run(run: &ActionRun) -> String {
    let id = run.id.unwrap_or_default();
    let number = run.index_in_repo.unwrap_or_default();
    let status = format_run_status(run.status.as_deref().unwrap_or("?"));
    let workflow = non_empty(run.workflow_id.as_deref());
    let sha = short_sha(run.commit_sha.as_deref());
    let event = non_empty(run.event.as_deref());
    let title = non_empty(run.title.as_deref());

    format!("{id} #{number} {status} {workflow} {sha} ({event}): {title}")
}

pub(super) fn non_empty(value: Option<&str>) -> &str {
    match value {
        Some(value) if !value.is_empty() => value,
        _ => "?",
    }
}

fn short_sha(sha: Option<&str>) -> &str {
    match sha {
        Some(sha) if !sha.is_empty() => sha.get(..10).unwrap_or(sha),
        _ => "?",
    }
}

fn format_run_status(status: &str) -> String {
    let SpecialRender {
        reset,
        bright_green,
        light_grey,
        bright_red,
        yellow,
        ..
    } = *crate::special_render();

    let color = match status {
        "success" => bright_green,
        "failure" | "blocked" => bright_red,
        "cancelled" | "skipped" | "waiting" => light_grey,
        "running" => yellow,
        _ => "",
    };

    if color.is_empty() {
        status.to_owned()
    } else {
        format!("{color}{status}{reset}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::actions::test_support::{
        MockResponse, MockServer, RUN_PATH, init_render, run_json, runs_response, test_api,
        test_repo,
    };

    fn runs_options(limit: u32) -> RunsOptions {
        RunsOptions {
            limit,
            status: None,
            workflow: None,
            branch: None,
            commit: None,
        }
    }

    #[tokio::test]
    async fn runs_lists_run_ids_and_numbers() {
        init_render();
        let body = runs_response(
            &[run_json(900, 7, "success"), run_json(901, 6, "failure")],
            2,
        );
        let server = MockServer::start(vec![(RUN_PATH, vec![MockResponse::json(200, body)])]);
        let api = test_api(server.addr);

        let mut out: Vec<u8> = Vec::new();
        view_runs(&test_repo(), &api, &runs_options(20), &mut out)
            .await
            .unwrap();

        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert!(lines[0].starts_with("900 #7 "), "{text}");
        assert!(lines[0].contains("success"), "{text}");
        assert!(lines[0].contains("ci.yml"), "{text}");
        assert!(lines[0].contains("0123456789"), "{text}");
        assert!(lines[0].contains("push"), "{text}");
        assert!(lines[0].contains("Some title"), "{text}");
        assert!(lines[1].starts_with("901 #6 "), "{text}");
        assert!(lines[1].contains("failure"), "{text}");
    }

    #[tokio::test]
    async fn runs_forwards_the_filters_to_the_api() {
        init_render();
        let server = MockServer::start(vec![(
            RUN_PATH,
            vec![MockResponse::json(200, runs_response(&[], 0))],
        )]);
        let api = test_api(server.addr);

        let mut out: Vec<u8> = Vec::new();
        let mut options = runs_options(20);
        options.status = Some(ListActionRunsQueryStatus::Failure);
        options.workflow = Some("ci.yml".to_owned());
        options.branch = Some("main".to_owned());
        options.commit = Some("abc123".to_owned());
        view_runs(&test_repo(), &api, &options, &mut out)
            .await
            .unwrap();

        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        let query = requests[0].2.clone().unwrap();
        assert!(query.contains("status=failure"), "{query}");
        assert!(query.contains("workflow_id=ci.yml"), "{query}");
        assert!(query.contains("ref=refs%2Fheads%2Fmain"), "{query}");
        assert!(query.contains("head_sha=abc123"), "{query}");
    }

    #[tokio::test]
    async fn runs_paginates_until_the_limit_is_reached() {
        init_render();
        let first: Vec<String> = (0..50).map(|i| run_json(1000 + i, i, "success")).collect();
        let second: Vec<String> = (50..60).map(|i| run_json(1000 + i, i, "success")).collect();
        let server = MockServer::start(vec![(
            RUN_PATH,
            vec![
                MockResponse::json(200, runs_response(&first, 60)),
                MockResponse::json(200, runs_response(&second, 60)),
            ],
        )]);
        let api = test_api(server.addr);

        let mut out: Vec<u8> = Vec::new();
        view_runs(&test_repo(), &api, &runs_options(60), &mut out)
            .await
            .unwrap();

        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.lines().count(), 60, "{text}");
        let requests = server.requests();
        assert_eq!(requests.len(), 2, "{requests:?}");
        let first_query = requests[0].2.clone().unwrap();
        let second_query = requests[1].2.clone().unwrap();
        assert!(first_query.contains("page=1"), "{first_query}");
        assert!(first_query.contains("limit=50"), "{first_query}");
        assert!(second_query.contains("page=2"), "{second_query}");
    }

    #[tokio::test]
    async fn runs_stops_at_the_total_count() {
        init_render();
        let server = MockServer::start(vec![(
            RUN_PATH,
            vec![
                MockResponse::json(200, runs_response(&[run_json(900, 7, "success")], 1)),
                MockResponse::json(200, runs_response(&[], 1)),
            ],
        )]);
        let api = test_api(server.addr);

        let mut out: Vec<u8> = Vec::new();
        view_runs(&test_repo(), &api, &runs_options(20), &mut out)
            .await
            .unwrap();

        assert_eq!(String::from_utf8(out).unwrap().lines().count(), 1);
        assert_eq!(server.requests().len(), 1);
    }
}
