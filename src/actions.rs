use std::{collections::BTreeMap, fmt::Display, io::Write};

use clap::{Args, Subcommand};
use eyre::OptionExt;
use forgejo_api::{
    Forgejo, ForgejoError,
    structs::{
        ActionRun, ActionRunJob, ActionVariable, CreateOrUpdateSecretOption, CreateVariableOption,
        ListActionRunsQuery, ListActionRunsQueryStatus, RepoGetActionJobLogsQuery,
        RepoGetActionJobLogsQueryFormat, UpdateVariableOption,
    },
};
use hyper::StatusCode;
use time::Duration;

use crate::{
    SpecialRender, ftl_bail, ftl_eprintln, ftl_format, ftl_println, h,
    repo::{RepoArg, RepoInfo, RepoName},
};

#[derive(Args, Clone, Debug)]
pub struct ActionsCommand {
    #[clap(help = h!("arg-remote"))]
    #[clap(long, short = 'R', global = true)]
    remote: Option<String>,

    #[clap(help = h!("arg-repo"))]
    #[clap(long, short, global = true)]
    repo: Option<RepoArg>,

    #[clap(subcommand)]
    command: ActionsSubcommand,
}

#[derive(Subcommand, Clone, Debug)]
pub enum ActionsSubcommand {
    #[clap(about = h!("cmd-actions-tasks"))]
    Tasks {
        #[clap(help = h!("arg-actions-tasks-page"))]
        #[clap(long, short, default_value = "1")]
        page: u32,
    },

    #[clap(about = h!("cmd-actions-runs"))]
    Runs {
        #[clap(help = h!("arg-actions-runs-limit"))]
        #[clap(
            long,
            short = 'L',
            default_value = "20",
            value_parser = clap::value_parser!(u32).range(1..=RUNS_LIMIT_MAX)
        )]
        limit: u32,

        #[clap(help = h!("arg-actions-runs-status"))]
        #[clap(long, short = 's', value_name = "STATUS", value_parser = parse_run_status)]
        status: Option<ListActionRunsQueryStatus>,

        #[clap(help = h!("arg-actions-runs-workflow"))]
        #[clap(long, short = 'w', value_name = "ID")]
        workflow: Option<String>,

        #[clap(help = h!("arg-actions-runs-branch"))]
        #[clap(long, short = 'b', value_name = "BRANCH")]
        branch: Option<String>,

        #[clap(help = h!("arg-actions-runs-commit"))]
        #[clap(long, short = 'c', value_name = "SHA")]
        commit: Option<String>,
    },

    #[clap(about = h!("cmd-actions-logs"))]
    Logs {
        #[clap(help = h!("arg-actions-logs-run-id"))]
        #[clap(
            value_name = "RUN_ID",
            required_unless_present = "run_number",
            conflicts_with = "run_number",
            value_parser = clap::value_parser!(i64).range(1..)
        )]
        run_id: Option<i64>,

        #[clap(help = h!("arg-actions-logs-run-number"))]
        #[clap(long, value_name = "N", value_parser = clap::value_parser!(i64).range(1..))]
        run_number: Option<i64>,

        #[clap(help = h!("arg-actions-logs-job"))]
        #[clap(
            long,
            short = 'j',
            value_name = "JOB_ID",
            value_parser = clap::value_parser!(i64).range(1..)
        )]
        job: Option<i64>,

        #[clap(help = h!("arg-actions-logs-step"))]
        #[clap(
            long,
            value_name = "N",
            requires = "job",
            value_parser = clap::value_parser!(u32).range(0..)
        )]
        step: Option<u32>,

        #[clap(help = h!("arg-actions-logs-attempt"))]
        #[clap(
            long,
            value_name = "N",
            requires = "job",
            value_parser = clap::value_parser!(i64).range(1..)
        )]
        attempt: Option<i64>,

        #[clap(help = h!("arg-actions-logs-failed"))]
        #[clap(long, conflicts_with_all = ["step", "attempt"])]
        failed: bool,

        #[clap(help = h!("arg-actions-logs-filter"))]
        #[clap(long, value_name = "TEXT")]
        filter: Option<String>,

        #[clap(help = h!("arg-actions-logs-ignore-case"))]
        #[clap(short = 'i', requires = "filter")]
        ignore_case: bool,
    },

    #[clap(about = h!("cmd-actions-variables"))]
    Variables {
        #[clap(subcommand)]
        command: ActionsVariablesSubcommmand,
    },

    #[clap(about = h!("cmd-actions-secrets"))]
    Secrets {
        #[clap(subcommand)]
        command: ActionsSecretsSubcommmand,
    },

    #[clap(about = h!("cmd-actions-dispatch"))]
    Dispatch {
        #[clap(help = h!("arg-actions-dispatch-name"))]
        name: String,

        #[clap(help = h!("arg-actions-dispatch-ref"))]
        r#ref: String,

        #[clap(help = h!("arg-actions-dispatch-inputs"))]
        #[clap(long, short = 'I', value_parser = parse_dispatch_kvs)]
        inputs: Vec<(String, String)>,
    },
}

#[derive(Subcommand, Clone, Debug)]
pub enum ActionsVariablesSubcommmand {
    #[clap(about = h!("cmd-actions-variables-list"))]
    List {
        #[clap(help = h!("arg-actions-variables-list-verbose"))]
        #[clap(long, short)]
        verbose: bool,
    },

    #[clap(about = h!("cmd-actions-variables-create"))]
    Create {
        #[clap(help = h!("arg-actions-variables-create-name"))]
        name: String,

        #[clap(help = h!("arg-actions-variables-create-data"))]
        data: Option<String>,

        #[clap(help = h!("arg-actions-variables-create-force"))]
        #[clap(long, short)]
        force: bool,
    },

    #[clap(about = h!("cmd-actions-variables-delete"))]
    Delete {
        #[clap(help = h!("arg-actions-variables-delete-name"))]
        name: String,
    },
}

#[derive(Subcommand, Clone, Debug)]
pub enum ActionsSecretsSubcommmand {
    #[clap(about = h!("cmd-actions-secrets-list"))]
    List,

    #[clap(about = h!("cmd-actions-secrets-create"))]
    Create {
        #[clap(help = h!("arg-actions-secrets-create-name"))]
        name: String,

        #[clap(help = h!("arg-actions-secrets-create-data"))]
        data: String,
    },

    #[clap(about = h!("cmd-actions-secrets-delete"))]
    Delete {
        #[clap(help = h!("arg-actions-secrets-delete-name"))]
        name: String,
    },
}

impl ActionsCommand {
    pub async fn run(self, keys: &mut crate::KeyInfo, host_name: Option<&str>) -> eyre::Result<()> {
        let repo =
            RepoInfo::get_current(host_name, self.repo.as_ref(), self.remote.as_deref(), keys)?;

        let api = keys.get_api(repo.host_url()).await?;
        let repo = repo
            .name()
            .ok_or_eyre("can't figure what repo to access, try specifying with `--repo`")?;
        match self.command {
            ActionsSubcommand::Tasks { page } => view_tasks(repo, &api, page).await?,

            ActionsSubcommand::Runs {
                limit,
                status,
                workflow,
                branch,
                commit,
            } => {
                let options = RunsOptions {
                    limit,
                    status,
                    workflow,
                    branch,
                    commit,
                };
                let mut out = std::io::stdout();
                view_runs(repo, &api, &options, &mut out).await?
            }

            ActionsSubcommand::Logs {
                run_id,
                run_number,
                job,
                step,
                attempt,
                failed,
                filter,
                ignore_case,
            } => {
                let options = LogsOptions {
                    run_id,
                    run_number,
                    job,
                    step,
                    attempt,
                    failed,
                    filter,
                    ignore_case,
                };
                let mut out = std::io::stdout();
                let mut notice = std::io::stderr();
                view_logs(repo, &api, &options, &mut out, &mut notice).await?
            }

            ActionsSubcommand::Variables { command } => match command {
                ActionsVariablesSubcommmand::List { verbose } => {
                    list_variables(repo, &api, verbose).await?
                }
                ActionsVariablesSubcommmand::Create { name, data, force } => {
                    create_variable(repo, &api, name, data, force).await?
                }
                ActionsVariablesSubcommmand::Delete { name } => {
                    delete_variable(repo, &api, name).await?
                }
            },

            ActionsSubcommand::Secrets { command } => match command {
                ActionsSecretsSubcommmand::List => list_secrets(repo, &api).await?,
                ActionsSecretsSubcommmand::Create { name, data } => {
                    create_secret(repo, &api, name, data).await?
                }
                ActionsSecretsSubcommmand::Delete { name } => {
                    delete_secret(repo, &api, name).await?
                }
            },

            ActionsSubcommand::Dispatch {
                name,
                r#ref,
                inputs,
            } => dispatch(repo, &api, name, r#ref, inputs.into_iter().collect()).await?,
        }

        Ok(())
    }
}

async fn view_tasks(repo: &RepoName, api: &Forgejo, page: u32) -> eyre::Result<()> {
    // We don't iterate this to collect all tasks (not just the ones on the first page) like the
    // issue search subcommand will do, because it's unlikely someone wants to see *all* tasks.
    let res = api
        .list_action_tasks(
            repo.owner(),
            repo.name(),
            forgejo_api::structs::ListActionTasksQuery::default(),
        )
        .page(page)
        .page_size(20)
        .await?;

    if res.total_count == Some(1) {
        println!("1 task");
    } else {
        println!("{} tasks", res.total_count.unwrap_or(0));
    }

    let SpecialRender {
        fancy,
        reset,

        bold,
        bright_green,
        light_grey,
        bright_red,
        yellow,
        bright_blue,
        ..
    } = *crate::special_render();

    for task in res.workflow_runs.unwrap_or_default() {
        let task_sym = match task.status.as_deref() {
            // Don't use symbols when we're not in fancy mode.
            x if !fancy => x.unwrap_or("?"),

            // See: https://codeberg.org/forgejo/forgejo/src/commit/5380f23daba969057d9afc53c3dc746eca95188c/models/actions/status.go#L26
            Some("success") => &format!("{bright_green}✓{reset}"),
            Some("cancelled") => &format!("{light_grey}!{reset}"),
            Some("failure") => &format!("{bright_red}×{reset}"),
            Some("waiting") => &format!("{light_grey}{reset}"),
            Some("running") => &format!("{yellow}●{reset}"),
            Some("skipped") => &format!("{light_grey}{reset}"),
            Some("blocked") => &format!("{bright_red}{reset}"),
            Some(x) => x,
            None => "?",
        };

        let sha = task.head_sha.unwrap_or_default();
        let sha = if sha.len() > 10 { &sha[0..10] } else { &sha };

        let time = if let (Some(end), Some(start)) = (task.updated_at, task.run_started_at) {
            end - start
        } else {
            Duration::default()
        };

        println!(
            "#{bold}{}{reset} ({bright_blue}{}{reset}) {} {} {bright_green}{}{reset} ({}): {yellow}{}{reset}",
            task.run_number.unwrap_or(0),
            sha,
            task_sym,
            task.name.unwrap_or_default(),
            time,
            task.event.unwrap_or_default(),
            task.display_title.unwrap_or_default(),
        );
    }

    Ok(())
}

/// The biggest page the runs listing asks the API for.
const RUNS_PAGE_SIZE: usize = 50;
/// The biggest number of runs a single listing may return.
const RUNS_LIMIT_MAX: i64 = 1000;
/// How many runs are inspected when a run number is looked up.
const RUN_NUMBER_LOOKUP_LIMIT: usize = 100;

fn parse_run_status(value: &str) -> Result<ListActionRunsQueryStatus, String> {
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

struct RunsOptions {
    limit: u32,
    status: Option<ListActionRunsQueryStatus>,
    workflow: Option<String>,
    branch: Option<String>,
    commit: Option<String>,
}

async fn view_runs(
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
async fn list_runs(
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

fn non_empty(value: Option<&str>) -> &str {
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

struct LogsOptions {
    run_id: Option<i64>,
    run_number: Option<i64>,
    job: Option<i64>,
    step: Option<u32>,
    attempt: Option<i64>,
    failed: bool,
    filter: Option<String>,
    ignore_case: bool,
}

async fn view_logs(
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
    let runs = list_runs(repo, api, query, RUN_NUMBER_LOOKUP_LIMIT).await?;

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
    let name = non_empty(name);
    writeln!(
        out,
        "{}",
        ftl_format!("msg-actions-logs-job-header", job_id, name)
    )?;
    Ok(())
}

fn write_step_header(out: &mut dyn Write, step: u32, name: Option<&str>) -> eyre::Result<()> {
    let name = non_empty(name);
    writeln!(
        out,
        "{}",
        ftl_format!("msg-actions-logs-step-header", step, name)
    )?;
    Ok(())
}

async fn list_variables(repo: &RepoName, api: &Forgejo, verbose: bool) -> eyre::Result<()> {
    let variables = api
        .get_repo_variables_list(repo.owner(), repo.name())
        .all()
        .await?;

    for var in variables {
        println!("{}", DisplayActionVariable::new(var, verbose)?);
    }

    Ok(())
}

async fn create_variable(
    repo: &RepoName,
    api: &Forgejo,
    name: String,
    data: Option<String>,
    force: bool,
) -> eyre::Result<()> {
    let mut data = if let Some(data) = data {
        data
    } else {
        let mut data = String::new();
        crate::editor(&mut data, Some("variable_content.txt")).await?;
        data
    };

    match api
        .create_repo_variable(
            repo.owner(),
            repo.name(),
            &name,
            CreateVariableOption {
                // If we don't have force enabled, we will not need the data again to (potentially)
                // make another request. To avoid a clone in this case, we take the string here,
                // replacing it with an empty one.
                value: if force {
                    data.clone()
                } else {
                    std::mem::take(&mut data)
                },
            },
        )
        .await
    {
        Err(ForgejoError::ApiError(forgejo_api::ApiError {
            kind: forgejo_api::ApiErrorKind::Other(StatusCode::CONFLICT),
            ..
        })) => {
            if !force {
                ftl_bail!("msg-actions-variable-create-already_exists");
            }

            ftl_eprintln!("msg-actions-variable-create-already_exists_forced");
            api.update_repo_variable(
                repo.owner(),
                repo.name(),
                &name,
                UpdateVariableOption {
                    name: None,
                    value: data,
                },
            )
            .await?;
        }
        Err(e) => return Err(e.into()),
        Ok(()) => {}
    }

    Ok(())
}

async fn delete_variable(repo: &RepoName, api: &Forgejo, name: String) -> eyre::Result<()> {
    api.delete_repo_variable(repo.owner(), repo.name(), &name)
        .await?;
    ftl_println!("msg-actions-variable-delete-success", name);

    Ok(())
}

async fn list_secrets(repo: &RepoName, api: &Forgejo) -> eyre::Result<()> {
    let secrets = api
        .repo_list_actions_secrets(repo.owner(), repo.name())
        .all()
        .await?;

    for secret in secrets {
        println!(
            "({}) {}",
            crate::DisplayOptional(secret.created_at, "?"),
            crate::DisplayOptional(secret.name, "?")
        );
    }

    Ok(())
}

async fn create_secret(
    repo: &RepoName,
    api: &Forgejo,
    name: String,
    data: String,
) -> eyre::Result<()> {
    api.update_repo_secret(
        repo.owner(),
        repo.name(),
        &name,
        CreateOrUpdateSecretOption { data },
    )
    .await?;

    Ok(())
}

async fn delete_secret(repo: &RepoName, api: &Forgejo, name: String) -> eyre::Result<()> {
    api.delete_repo_secret(repo.owner(), repo.name(), &name)
        .await?;

    Ok(())
}

async fn dispatch(
    repo: &RepoName,
    api: &Forgejo,
    name: String,
    r#ref: String,
    inputs: BTreeMap<String, String>,
) -> eyre::Result<()> {
    let n_inputs = inputs.len();
    api.dispatch_workflow(
        repo.owner(),
        repo.name(),
        &name,
        forgejo_api::structs::DispatchWorkflowOption {
            inputs: Some(inputs),
            return_run_info: Some(false),
            r#ref: r#ref.clone(),
        },
    )
    .await?;

    ftl_println!("msg-actions-dispatch-success", name, r#ref, n_inputs);

    Ok(())
}

struct DisplayActionVariable {
    name: String,
    data: String,
    owner_id: Option<i64>,
    repo_id: Option<i64>,
    verbose: bool,
}

impl DisplayActionVariable {
    fn new(value: ActionVariable, verbose: bool) -> eyre::Result<Self> {
        Ok(Self {
            name: value
                .name
                .ok_or_eyre("Server returned ActionVariable without name?!")?,
            // The API usually (always?) returns Some("") here. The page on variables also notes
            // that their value cannot be read by other means than being passed to a CI job.
            data: value.data.unwrap_or_default(),
            owner_id: value.owner_id,
            repo_id: value.repo_id,
            verbose,
        })
    }
}

impl Display for DisplayActionVariable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.verbose {
            write!(
                f,
                "({}, {}) ",
                crate::DisplayOptional(self.owner_id, "?"),
                crate::DisplayOptional(self.repo_id, "?"),
            )?;
        }

        write!(f, "{}", self.name)?;

        if !self.data.is_empty() {
            write!(f, " = {}", self.data)?;
        }

        Ok(())
    }
}

fn parse_dispatch_kvs(s: &str) -> eyre::Result<(String, String)> {
    let eq_idx = s
        .find('=')
        .ok_or_eyre("Input argument does not contain a '=' character!")?;

    Ok((s[..eq_idx].to_string(), s[eq_idx + 1..].to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::{Duration, Instant};

    #[derive(Clone)]
    struct MockResponse {
        status: u16,
        body: Vec<u8>,
        content_type: &'static str,
    }

    impl MockResponse {
        fn json(status: u16, body: impl Into<Vec<u8>>) -> Self {
            Self {
                status,
                body: body.into(),
                content_type: "application/json",
            }
        }

        fn text(status: u16, body: impl Into<Vec<u8>>) -> Self {
            Self {
                status,
                body: body.into(),
                content_type: "text/plain",
            }
        }
    }

    struct Captured {
        method: String,
        path: String,
        query: Option<String>,
    }

    type CapturedList = Arc<Mutex<Vec<Captured>>>;
    type Routes = Arc<Mutex<HashMap<String, Vec<MockResponse>>>>;

    struct MockServer {
        addr: std::net::SocketAddr,
        captured: CapturedList,
        stop: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl MockServer {
        fn start(routes: Vec<(&'static str, Vec<MockResponse>)>) -> Self {
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

        fn requests(&self) -> Vec<(String, String, Option<String>)> {
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

    fn handle_one(
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

    fn test_api(addr: std::net::SocketAddr) -> Forgejo {
        let url = url::Url::parse(&format!("http://{addr}/")).unwrap();
        Forgejo::with_user_agent(forgejo_api::Auth::None, url, crate::USER_AGENT).unwrap()
    }

    fn test_repo() -> RepoName {
        RepoName::new("o".to_owned(), "r".to_owned())
    }

    fn init_render() {
        let _ = crate::SPECIAL_RENDER.set(crate::SpecialRender::new(crate::Style::Minimal));
    }

    fn run_json(id: i64, number: i64, status: &str) -> String {
        format!(
            r#"{{"id":{id},"index_in_repo":{number},"status":"{status}","workflow_id":"ci.yml","commit_sha":"0123456789abcdef","event":"push","title":"Some title"}}"#
        )
    }

    fn runs_response(runs: &[String], total: i64) -> Vec<u8> {
        format!(
            r#"{{"total_count":{total},"workflow_runs":[{}]}}"#,
            runs.join(",")
        )
        .into_bytes()
    }

    fn job_json(id: i64, name: &str, status: &str, steps: &str) -> String {
        format!(r#"{{"id":{id},"name":"{name}","status":"{status}","run_id":900,"steps":{steps}}}"#)
    }

    fn jobs_response(jobs: &[String]) -> Vec<u8> {
        format!("[{}]", jobs.join(",")).into_bytes()
    }

    fn step_json(number: i64, name: &str, status: &str) -> String {
        format!(r#"{{"number":{number},"name":"{name}","status":"{status}"}}"#)
    }

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

    fn runs_options(limit: u32) -> RunsOptions {
        RunsOptions {
            limit,
            status: None,
            workflow: None,
            branch: None,
            commit: None,
        }
    }

    const RUN_PATH: &str = "/api/v1/repos/o/r/actions/runs";
    const RUN_900_PATH: &str = "/api/v1/repos/o/r/actions/runs/900";
    const RUN_900_JOBS_PATH: &str = "/api/v1/repos/o/r/actions/runs/900/jobs";
    const JOB_5_PATH: &str = "/api/v1/repos/o/r/actions/jobs/5";
    const JOB_5_LOGS_PATH: &str = "/api/v1/repos/o/r/actions/jobs/5/logs";

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

    #[test]
    fn runs_rejects_limits_outside_the_supported_range() {
        assert!(crate::App::try_parse_from(["fj", "actions", "runs", "--limit", "0"]).is_err());
        assert!(crate::App::try_parse_from(["fj", "actions", "runs", "--limit", "1001"]).is_err());
        assert!(crate::App::try_parse_from(["fj", "actions", "runs", "--limit", "20"]).is_ok());
    }

    #[test]
    fn runs_rejects_an_unknown_status() {
        assert!(crate::App::try_parse_from(["fj", "actions", "runs", "--status", "nope"]).is_err());
        assert!(
            crate::App::try_parse_from(["fj", "actions", "runs", "--status", "failure"]).is_ok()
        );
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

    #[test]
    fn logs_rejects_invalid_argument_combinations() {
        assert!(crate::App::try_parse_from(["fj", "actions", "logs"]).is_err());
        assert!(
            crate::App::try_parse_from(["fj", "actions", "logs", "900", "--run-number", "7"])
                .is_err()
        );
        assert!(
            crate::App::try_parse_from(["fj", "actions", "logs", "900", "--step", "1"]).is_err()
        );
        assert!(
            crate::App::try_parse_from(["fj", "actions", "logs", "900", "--attempt", "1"]).is_err()
        );
        assert!(
            crate::App::try_parse_from(["fj", "actions", "logs", "900", "--failed", "--step", "1"])
                .is_err()
        );
        assert!(
            crate::App::try_parse_from([
                "fj",
                "actions",
                "logs",
                "900",
                "--failed",
                "--attempt",
                "1"
            ])
            .is_err()
        );
        assert!(crate::App::try_parse_from(["fj", "actions", "logs", "900", "-i"]).is_err());
        assert!(
            crate::App::try_parse_from(["fj", "actions", "logs", "900", "--job", "0"]).is_err()
        );
        assert!(
            crate::App::try_parse_from([
                "fj",
                "actions",
                "logs",
                "900",
                "--job",
                "5",
                "--attempt",
                "0"
            ])
            .is_err()
        );
        assert!(
            crate::App::try_parse_from([
                "fj", "actions", "logs", "900", "--job", "5", "--step", "-1"
            ])
            .is_err()
        );
    }

    #[test]
    fn logs_rejects_zero_run_id() {
        assert!(crate::App::try_parse_from(["fj", "actions", "logs", "0"]).is_err());
        assert!(crate::App::try_parse_from(["fj", "actions", "logs", "--", "-1"]).is_err());
        assert!(crate::App::try_parse_from(["fj", "actions", "logs", "900"]).is_ok());
    }

    #[test]
    fn logs_accepts_the_supported_argument_combinations() {
        assert!(crate::App::try_parse_from(["fj", "actions", "logs", "900"]).is_ok());
        assert!(crate::App::try_parse_from(["fj", "actions", "logs", "--run-number", "7"]).is_ok());
        assert!(crate::App::try_parse_from(["fj", "actions", "logs", "900", "--job", "5"]).is_ok());
        assert!(
            crate::App::try_parse_from([
                "fj", "actions", "logs", "900", "--job", "5", "--step", "0"
            ])
            .is_ok()
        );
        assert!(
            crate::App::try_parse_from([
                "fj",
                "actions",
                "logs",
                "900",
                "--job",
                "5",
                "--attempt",
                "2"
            ])
            .is_ok()
        );
        assert!(crate::App::try_parse_from(["fj", "actions", "logs", "900", "--failed"]).is_ok());
        assert!(
            crate::App::try_parse_from(["fj", "actions", "logs", "900", "--filter", "x", "-i"])
                .is_ok()
        );
    }
}
