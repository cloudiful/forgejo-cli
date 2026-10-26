use std::io::Write;

use eyre::OptionExt;
use forgejo_api::{
    Forgejo, ForgejoError,
    structs::{
        ActionRun, ActionRunJob, ListActionRunsQuery, RepoGetActionJobLogsQuery,
        RepoGetActionJobLogsQueryFormat,
    },
};
use hyper::StatusCode;

use crate::{ftl_bail, ftl_format, repo::RepoName};

pub(super) struct LogsOptions {
    pub(super) run_id: Option<i64>,
    pub(super) run_number: Option<i64>,
    pub(super) job: Option<i64>,
    pub(super) step: Option<u32>,
    pub(super) attempt: Option<i64>,
    pub(super) failed: bool,
    pub(super) filter: Option<String>,
    pub(super) ignore_case: bool,
}

pub(super) async fn view_logs(
    repo: &RepoName,
    api: &Forgejo,
    options: &LogsOptions,
    out: &mut dyn Write,
    notice: &mut dyn Write,
) -> eyre::Result<()> {
    let run = match (options.run_id, options.run_number) {
        (Some(run_id), _) => fetch_run(repo, api, run_id).await?,
        (None, Some(run_number)) => resolve_run_by_number(repo, api, run_number).await?,
        (None, None) => ftl_bail!("msg-actions-logs-run-missing"),
    };
    let run_id = run.id.ok_or_eyre("action run without id")?;

    let jobs = api
        .list_action_run_jobs(repo.owner(), repo.name(), run_id)
        .await?;

    let selected: Vec<&ActionRunJob> = match options.job {
        Some(job_id) => match jobs.iter().find(|job| job.id == Some(job_id)) {
            Some(job) => vec![job],
            None => ftl_bail!("msg-actions-logs-job-not-in-run", job_id, run_id),
        },
        None => jobs.iter().collect(),
    };

    if options.failed {
        print_failed_logs(repo, api, &selected, options, out, notice).await?;
    } else {
        print_full_logs(repo, api, &selected, options, out).await?;
    }
    out.flush()?;

    Ok(())
}

async fn fetch_run(repo: &RepoName, api: &Forgejo, run_id: i64) -> eyre::Result<ActionRun> {
    match api.get_action_run(repo.owner(), repo.name(), run_id).await {
        Ok(run) => Ok(run),
        Err(ForgejoError::ApiError(err)) if is_not_found(&err) => {
            ftl_bail!("msg-actions-logs-run-not-found", run_id)
        }
        Err(err) => Err(err.into()),
    }
}

/// A run number is the number of a run within its repository, not the id of a
/// task, so it has to be looked up in the run listing.
async fn resolve_run_by_number(
    repo: &RepoName,
    api: &Forgejo,
    run_number: i64,
) -> eyre::Result<ActionRun> {
    let query = ListActionRunsQuery {
        run_number: Some(run_number),
        ..Default::default()
    };
    let runs =
        super::runs::list_runs(repo, api, query, super::runs::RUN_NUMBER_LOOKUP_LIMIT).await?;

    let matching: Vec<&ActionRun> = runs
        .iter()
        .filter(|run| run.index_in_repo == Some(run_number))
        .collect();

    match matching.len() {
        0 => ftl_bail!("msg-actions-logs-run-number-not-found", run_number),
        1 => Ok(matching[0].clone()),
        _ => ftl_bail!("msg-actions-logs-run-number-ambiguous", run_number),
    }
}

fn is_not_found(err: &forgejo_api::ApiError) -> bool {
    matches!(
        &err.kind,
        forgejo_api::ApiErrorKind::NotFound { .. }
            | forgejo_api::ApiErrorKind::Other(StatusCode::NOT_FOUND)
    )
}

async fn print_full_logs(
    repo: &RepoName,
    api: &Forgejo,
    jobs: &[&ActionRunJob],
    options: &LogsOptions,
    out: &mut dyn Write,
) -> eyre::Result<()> {
    for job in jobs {
        let job_id = job.id.ok_or_eyre("action run job without id")?;
        let query = RepoGetActionJobLogsQuery {
            attempt: options.attempt,
            step: options.step,
            q: options.filter.clone(),
            qi: ignore_case_value(options),
            format: Some(RepoGetActionJobLogsQueryFormat::Text),
        };
        let log = fetch_job_log(repo, api, job_id, query).await?;

        write_job_header(out, job_id, job.name.as_deref())?;
        out.write_all(&log)?;
        out.write_all(b"\n")?;
    }

    Ok(())
}

/// Prints the steps that failed. The step details of a job only describe its
/// latest attempt, so a job without step details falls back to its full log.
async fn print_failed_logs(
    repo: &RepoName,
    api: &Forgejo,
    jobs: &[&ActionRunJob],
    options: &LogsOptions,
    out: &mut dyn Write,
    notice: &mut dyn Write,
) -> eyre::Result<()> {
    for job in jobs {
        let job_id = job.id.ok_or_eyre("action run job without id")?;
        if let Some(status) = job.status.as_deref()
            && status != "failure"
        {
            continue;
        }

        let detail = api
            .repo_get_action_job(repo.owner(), repo.name(), job_id)
            .await?;
        let status = detail.status.as_deref().or(job.status.as_deref());
        if status != Some("failure") {
            continue;
        }
        let name = detail.name.as_deref().or(job.name.as_deref());

        let steps = detail.steps.unwrap_or_default();
        if steps.is_empty() {
            let query = RepoGetActionJobLogsQuery {
                attempt: None,
                step: None,
                q: options.filter.clone(),
                qi: ignore_case_value(options),
                format: Some(RepoGetActionJobLogsQueryFormat::Text),
            };
            let log = fetch_job_log(repo, api, job_id, query).await?;

            writeln!(
                notice,
                "{}",
                ftl_format!("msg-actions-logs-no-step-details", job_id)
            )?;
            write_job_header(out, job_id, name)?;
            out.write_all(&log)?;
            out.write_all(b"\n")?;
            continue;
        }

        let failed_steps: Vec<_> = steps
            .iter()
            .filter(|step| step.status.as_deref() == Some("failure"))
            .collect();
        if failed_steps.is_empty() {
            continue;
        }

        write_job_header(out, job_id, name)?;
        for step in failed_steps {
            let Some(number) = step.number.and_then(|number| u32::try_from(number).ok()) else {
                continue;
            };
            let query = RepoGetActionJobLogsQuery {
                attempt: None,
                step: Some(number),
                q: options.filter.clone(),
                qi: ignore_case_value(options),
                format: Some(RepoGetActionJobLogsQueryFormat::Text),
            };
            let log = fetch_job_log(repo, api, job_id, query).await?;

            write_step_header(out, number, step.name.as_deref())?;
            out.write_all(&log)?;
            out.write_all(b"\n")?;
        }
    }

    Ok(())
}

fn ignore_case_value(options: &LogsOptions) -> Option<bool> {
    if options.ignore_case && options.filter.is_some() {
        Some(true)
    } else {
        None
    }
}

async fn fetch_job_log(
    repo: &RepoName,
    api: &Forgejo,
    job_id: i64,
    query: RepoGetActionJobLogsQuery,
) -> eyre::Result<Vec<u8>> {
    match api
        .repo_get_action_job_logs(repo.owner(), repo.name(), job_id, query)
        .await
    {
        Ok(log) => Ok(log.to_vec()),
        Err(ForgejoError::ApiError(err)) if is_not_found(&err) => {
            ftl_bail!("msg-actions-logs-log-missing", job_id)
        }
        Err(err) => Err(err.into()),
    }
}

fn write_job_header(out: &mut dyn Write, job_id: i64, name: Option<&str>) -> eyre::Result<()> {
    let name = super::runs::non_empty(name);
    writeln!(
        out,
        "{}",
        ftl_format!("msg-actions-logs-job-header", job_id, name)
    )?;
    Ok(())
}

fn write_step_header(out: &mut dyn Write, step: u32, name: Option<&str>) -> eyre::Result<()> {
    let name = super::runs::non_empty(name);
    writeln!(
        out,
        "{}",
        ftl_format!("msg-actions-logs-step-header", step, name)
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::actions::test_support::{
        JOB_5_LOGS_PATH, JOB_5_PATH, MockResponse, MockServer, RUN_900_JOBS_PATH, RUN_900_PATH,
        RUN_PATH, init_render, job_json, jobs_response, run_json, runs_response, step_json,
        test_api, test_repo,
    };

    fn logs_options() -> LogsOptions {
        LogsOptions {
            run_id: None,
            run_number: None,
            job: None,
            step: None,
            attempt: None,
            failed: false,
            filter: None,
            ignore_case: false,
        }
    }

    #[tokio::test]
    async fn logs_resolves_a_run_number_to_the_run_id() {
        init_render();
        let server = MockServer::start(vec![
            (
                RUN_PATH,
                vec![MockResponse::json(
                    200,
                    runs_response(&[run_json(900, 7, "failure")], 1),
                )],
            ),
            (
                RUN_900_JOBS_PATH,
                vec![MockResponse::json(200, b"[]".to_vec())],
            ),
        ]);
        let api = test_api(server.addr);

        let mut options = logs_options();
        options.run_number = Some(7);
        let mut out: Vec<u8> = Vec::new();
        let mut notice: Vec<u8> = Vec::new();
        view_logs(&test_repo(), &api, &options, &mut out, &mut notice)
            .await
            .unwrap();

        let requests = server.requests();
        assert_eq!(requests.len(), 2, "{requests:?}");
        assert!(
            requests[0].2.as_deref().unwrap().contains("run_number=7"),
            "{:?}",
            requests[0]
        );
        assert_eq!(requests[1].1, RUN_900_JOBS_PATH);
    }

    #[tokio::test]
    async fn logs_rejects_an_ambiguous_run_number() {
        init_render();
        let server = MockServer::start(vec![(
            RUN_PATH,
            vec![MockResponse::json(
                200,
                runs_response(
                    &[run_json(900, 7, "failure"), run_json(901, 7, "success")],
                    2,
                ),
            )],
        )]);
        let api = test_api(server.addr);

        let mut options = logs_options();
        options.run_number = Some(7);
        let mut out: Vec<u8> = Vec::new();
        let mut notice: Vec<u8> = Vec::new();
        let err = view_logs(&test_repo(), &api, &options, &mut out, &mut notice)
            .await
            .expect_err("ambiguous run number must fail");
        assert!(err.to_string().contains('7'), "{err}");
    }

    #[tokio::test]
    async fn logs_rejects_a_run_number_without_a_match() {
        init_render();
        let server = MockServer::start(vec![(
            RUN_PATH,
            vec![MockResponse::json(200, runs_response(&[], 0))],
        )]);
        let api = test_api(server.addr);

        let mut options = logs_options();
        options.run_number = Some(7);
        let mut out: Vec<u8> = Vec::new();
        let mut notice: Vec<u8> = Vec::new();
        let err = view_logs(&test_repo(), &api, &options, &mut out, &mut notice)
            .await
            .expect_err("unknown run number must fail");
        assert!(err.to_string().contains('7'), "{err}");
    }

    #[tokio::test]
    async fn logs_rejects_a_job_of_another_run() {
        init_render();
        let server = MockServer::start(vec![
            (
                RUN_900_PATH,
                vec![MockResponse::json(200, run_json(900, 7, "failure"))],
            ),
            (
                RUN_900_JOBS_PATH,
                vec![MockResponse::json(
                    200,
                    jobs_response(&[job_json(5, "build", "failure", "null")]),
                )],
            ),
        ]);
        let api = test_api(server.addr);

        let mut options = logs_options();
        options.run_id = Some(900);
        options.job = Some(99);
        let mut out: Vec<u8> = Vec::new();
        let mut notice: Vec<u8> = Vec::new();
        let err = view_logs(&test_repo(), &api, &options, &mut out, &mut notice)
            .await
            .expect_err("a foreign job must be rejected");
        assert!(err.to_string().contains("99"), "{err}");
    }

    #[tokio::test]
    async fn logs_full_mode_preserves_the_raw_log_bytes() {
        init_render();
        let raw: Vec<u8> = vec![0xff, 0xfe, b'a', b'\r', b'\n', 0x1b, b'[', b'3', b'1', b'm'];
        let server = MockServer::start(vec![
            (
                RUN_900_PATH,
                vec![MockResponse::json(200, run_json(900, 7, "failure"))],
            ),
            (
                RUN_900_JOBS_PATH,
                vec![MockResponse::json(
                    200,
                    jobs_response(&[job_json(5, "build", "failure", "null")]),
                )],
            ),
            (JOB_5_LOGS_PATH, vec![MockResponse::text(200, raw.clone())]),
        ]);
        let api = test_api(server.addr);

        let mut options = logs_options();
        options.run_id = Some(900);
        let mut out: Vec<u8> = Vec::new();
        let mut notice: Vec<u8> = Vec::new();
        view_logs(&test_repo(), &api, &options, &mut out, &mut notice)
            .await
            .unwrap();

        assert!(
            out.windows(raw.len())
                .any(|window| window == raw.as_slice()),
            "{out:?}"
        );
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("5"), "{text}");
        assert!(text.contains("build"), "{text}");
    }

    #[tokio::test]
    async fn logs_forwards_the_query_parameters() {
        init_render();
        let server = MockServer::start(vec![
            (
                RUN_900_PATH,
                vec![MockResponse::json(200, run_json(900, 7, "failure"))],
            ),
            (
                RUN_900_JOBS_PATH,
                vec![MockResponse::json(
                    200,
                    jobs_response(&[job_json(5, "build", "failure", "null")]),
                )],
            ),
            (
                JOB_5_LOGS_PATH,
                vec![MockResponse::text(200, b"log".to_vec())],
            ),
        ]);
        let api = test_api(server.addr);

        let mut options = logs_options();
        options.run_id = Some(900);
        options.job = Some(5);
        options.step = Some(2);
        options.attempt = Some(3);
        options.filter = Some("boom".to_owned());
        options.ignore_case = true;
        let mut out: Vec<u8> = Vec::new();
        let mut notice: Vec<u8> = Vec::new();
        view_logs(&test_repo(), &api, &options, &mut out, &mut notice)
            .await
            .unwrap();

        let requests = server.requests();
        let query = requests.last().unwrap().2.clone().unwrap();
        assert!(query.contains("format=text"), "{query}");
        assert!(query.contains("step=2"), "{query}");
        assert!(query.contains("attempt=3"), "{query}");
        assert!(query.contains("q=boom"), "{query}");
        assert!(query.contains("qi=true"), "{query}");
    }

    #[tokio::test]
    async fn logs_failed_mode_prints_only_the_failed_steps() {
        init_render();
        let steps = format!(
            "[{},{},{}]",
            step_json(0, "Set up job", "success"),
            step_json(1, "run tests", "failure"),
            step_json(2, "Complete job", "success")
        );
        let server = MockServer::start(vec![
            (
                RUN_900_PATH,
                vec![MockResponse::json(200, run_json(900, 7, "failure"))],
            ),
            (
                RUN_900_JOBS_PATH,
                vec![MockResponse::json(
                    200,
                    jobs_response(&[job_json(5, "build", "failure", "null")]),
                )],
            ),
            (
                JOB_5_PATH,
                vec![MockResponse::json(
                    200,
                    job_json(5, "build", "failure", &steps).into_bytes(),
                )],
            ),
            (
                JOB_5_LOGS_PATH,
                vec![MockResponse::text(200, b"failed log".to_vec())],
            ),
        ]);
        let api = test_api(server.addr);

        let mut options = logs_options();
        options.run_id = Some(900);
        options.failed = true;
        let mut out: Vec<u8> = Vec::new();
        let mut notice: Vec<u8> = Vec::new();
        view_logs(&test_repo(), &api, &options, &mut out, &mut notice)
            .await
            .unwrap();

        let requests = server.requests();
        let log_requests: Vec<_> = requests
            .iter()
            .filter(|(_, path, _)| path == JOB_5_LOGS_PATH)
            .collect();
        assert_eq!(log_requests.len(), 1, "{requests:?}");
        let query = log_requests[0].2.clone().unwrap();
        assert!(query.contains("step=1"), "{query}");
        assert!(!query.contains("step=0"), "{query}");
        assert!(!query.contains("step=2"), "{query}");

        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("failed log"), "{text}");
        assert!(text.contains("run tests"), "{text}");
        assert!(notice.is_empty(), "{notice:?}");
    }

    #[tokio::test]
    async fn logs_failed_mode_includes_failed_setup_steps() {
        init_render();
        let steps = format!(
            "[{},{}]",
            step_json(0, "Set up job", "failure"),
            step_json(1, "run tests", "success")
        );
        let server = MockServer::start(vec![
            (
                RUN_900_PATH,
                vec![MockResponse::json(200, run_json(900, 7, "failure"))],
            ),
            (
                RUN_900_JOBS_PATH,
                vec![MockResponse::json(
                    200,
                    jobs_response(&[job_json(5, "build", "failure", "null")]),
                )],
            ),
            (
                JOB_5_PATH,
                vec![MockResponse::json(
                    200,
                    job_json(5, "build", "failure", &steps).into_bytes(),
                )],
            ),
            (
                JOB_5_LOGS_PATH,
                vec![MockResponse::text(200, b"setup log".to_vec())],
            ),
        ]);
        let api = test_api(server.addr);

        let mut options = logs_options();
        options.run_id = Some(900);
        options.failed = true;
        let mut out: Vec<u8> = Vec::new();
        let mut notice: Vec<u8> = Vec::new();
        view_logs(&test_repo(), &api, &options, &mut out, &mut notice)
            .await
            .unwrap();

        let requests = server.requests();
        let query = requests
            .iter()
            .find(|(_, path, _)| path == JOB_5_LOGS_PATH)
            .unwrap()
            .2
            .clone()
            .unwrap();
        assert!(query.contains("step=0"), "{query}");
    }

    #[tokio::test]
    async fn logs_failed_mode_falls_back_to_the_full_log_without_step_details() {
        init_render();
        let server = MockServer::start(vec![
            (
                RUN_900_PATH,
                vec![MockResponse::json(200, run_json(900, 7, "failure"))],
            ),
            (
                RUN_900_JOBS_PATH,
                vec![MockResponse::json(
                    200,
                    jobs_response(&[job_json(5, "build", "failure", "null")]),
                )],
            ),
            (
                JOB_5_PATH,
                vec![MockResponse::json(
                    200,
                    job_json(5, "build", "failure", "null").into_bytes(),
                )],
            ),
            (
                JOB_5_LOGS_PATH,
                vec![MockResponse::text(200, b"whole log".to_vec())],
            ),
        ]);
        let api = test_api(server.addr);

        let mut options = logs_options();
        options.run_id = Some(900);
        options.failed = true;
        let mut out: Vec<u8> = Vec::new();
        let mut notice: Vec<u8> = Vec::new();
        view_logs(&test_repo(), &api, &options, &mut out, &mut notice)
            .await
            .unwrap();

        let requests = server.requests();
        let query = requests
            .iter()
            .find(|(_, path, _)| path == JOB_5_LOGS_PATH)
            .unwrap()
            .2
            .clone()
            .unwrap();
        assert!(!query.contains("step="), "{query}");
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("whole log"), "{text}");
        let notice_text = String::from_utf8_lossy(&notice);
        assert!(notice_text.contains('5'), "{notice_text}");
    }

    #[tokio::test]
    async fn logs_failed_mode_without_failures_stays_silent() {
        init_render();
        let steps = format!(
            "[{},{}]",
            step_json(0, "Set up job", "success"),
            step_json(1, "run tests", "success")
        );
        let server = MockServer::start(vec![
            (
                RUN_900_PATH,
                vec![MockResponse::json(200, run_json(900, 7, "failure"))],
            ),
            (
                RUN_900_JOBS_PATH,
                vec![MockResponse::json(
                    200,
                    jobs_response(&[job_json(5, "build", "failure", "null")]),
                )],
            ),
            (
                JOB_5_PATH,
                vec![MockResponse::json(
                    200,
                    job_json(5, "build", "failure", &steps).into_bytes(),
                )],
            ),
        ]);
        let api = test_api(server.addr);

        let mut options = logs_options();
        options.run_id = Some(900);
        options.failed = true;
        let mut out: Vec<u8> = Vec::new();
        let mut notice: Vec<u8> = Vec::new();
        view_logs(&test_repo(), &api, &options, &mut out, &mut notice)
            .await
            .unwrap();

        assert!(out.is_empty(), "{out:?}");
        assert!(notice.is_empty(), "{notice:?}");
        assert!(
            !server
                .requests()
                .iter()
                .any(|(_, path, _)| path == JOB_5_LOGS_PATH)
        );
    }

    #[tokio::test]
    async fn logs_reports_missing_logs_as_an_error() {
        init_render();
        let server = MockServer::start(vec![
            (
                RUN_900_PATH,
                vec![MockResponse::json(200, run_json(900, 7, "failure"))],
            ),
            (
                RUN_900_JOBS_PATH,
                vec![MockResponse::json(
                    200,
                    jobs_response(&[job_json(5, "build", "failure", "null")]),
                )],
            ),
            (
                JOB_5_LOGS_PATH,
                vec![MockResponse::json(
                    404,
                    br#"{"message":"log not found"}"#.to_vec(),
                )],
            ),
        ]);
        let api = test_api(server.addr);

        let mut options = logs_options();
        options.run_id = Some(900);
        let mut out: Vec<u8> = Vec::new();
        let mut notice: Vec<u8> = Vec::new();
        let err = view_logs(&test_repo(), &api, &options, &mut out, &mut notice)
            .await
            .expect_err("missing logs must fail");
        assert!(err.to_string().contains('5'), "{err}");
    }

    #[tokio::test]
    async fn logs_keeps_authentication_errors() {
        init_render();
        let server = MockServer::start(vec![
            (
                RUN_900_PATH,
                vec![MockResponse::json(200, run_json(900, 7, "failure"))],
            ),
            (
                RUN_900_JOBS_PATH,
                vec![MockResponse::json(
                    200,
                    jobs_response(&[job_json(5, "build", "failure", "null")]),
                )],
            ),
            (
                JOB_5_LOGS_PATH,
                vec![MockResponse::json(
                    401,
                    br#"{"message":"token is required"}"#.to_vec(),
                )],
            ),
        ]);
        let api = test_api(server.addr);

        let mut options = logs_options();
        options.run_id = Some(900);
        let mut out: Vec<u8> = Vec::new();
        let mut notice: Vec<u8> = Vec::new();
        let err = view_logs(&test_repo(), &api, &options, &mut out, &mut notice)
            .await
            .expect_err("an unauthorized request must fail");
        assert!(err.to_string().contains("token is required"), "{err}");
    }
}
