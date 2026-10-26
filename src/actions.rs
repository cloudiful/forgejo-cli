use std::{collections::BTreeMap, fmt::Display};

use clap::{Args, Subcommand};
use eyre::OptionExt;
use forgejo_api::{
    Forgejo, ForgejoError,
    structs::{
        ActionVariable, CreateOrUpdateSecretOption, CreateVariableOption,
        ListActionRunsQueryStatus, UpdateVariableOption,
    },
};
use hyper::StatusCode;
use time::Duration;

use crate::{
    SpecialRender, ftl_bail, ftl_eprintln, ftl_println, h,
    repo::{RepoArg, RepoInfo, RepoName},
};

mod logs;
mod runs;

#[cfg(test)]
mod test_support;

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
            value_parser = clap::value_parser!(u32).range(1..=runs::RUNS_LIMIT_MAX)
        )]
        limit: u32,

        #[clap(help = h!("arg-actions-runs-status"))]
        #[clap(long, short = 's', value_name = "STATUS", value_parser = runs::parse_run_status)]
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
                let options = runs::RunsOptions {
                    limit,
                    status,
                    workflow,
                    branch,
                    commit,
                };
                let mut out = std::io::stdout();
                runs::view_runs(repo, &api, &options, &mut out).await?
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
                let options = logs::LogsOptions {
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
                logs::view_logs(repo, &api, &options, &mut out, &mut notice).await?
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
    use clap::Parser;

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
