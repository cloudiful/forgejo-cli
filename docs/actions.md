# Actions

Discover workflow runs of a repository and read the CI logs of a run.

```sh
fj actions runs [-r owner/repo] [-L|--limit N] [-s|--status STATUS] [-w|--workflow ID] [-b|--branch BRANCH] [-c|--commit SHA]
fj actions logs <RUN_ID> [-r owner/repo] [-j|--job JOB_ID] [--step N] [--attempt N] [--failed] [--filter TEXT] [-i]
fj actions logs --run-number N [-r owner/repo] [-j|--job JOB_ID] [--step N] [--attempt N] [--failed] [--filter TEXT] [-i]
```

The repository is taken from the current checkout unless `-r owner/repo` is given.

## Runs

`fj actions runs` lists the workflow runs of a repository, newest first. Every
line holds the run ID, the run number, the status, the workflow, the short
commit SHA, the event and the title:

```text
900 #7 success ci.yml 0123456789 (push): Some title
```

- `--limit` is the maximum number of runs to print. It defaults to 20 and must
  be between 1 and 1000. The API is asked for at most 50 runs per page, and
  pages are requested until the limit, the total number of runs or an empty
  page is reached.
- `--status` only lists runs with that status. The accepted values are
  `unknown`, `waiting`, `running`, `success`, `failure`, `cancelled`,
  `skipped` and `blocked`.
- `--workflow` only lists runs of that workflow file, for example `ci.yml`.
- `--branch` only lists runs of that branch. A plain branch name is sent as
  `refs/heads/<name>`, a full ref is sent unchanged.
- `--commit` only lists runs of that commit SHA.

## Run IDs and run numbers

A run has two identifiers and they are not interchangeable:

- The **run ID** is the identifier of the run itself. It is unique on the
  server and is what the API expects.
- The **run number** is the position of the run in the numbering of its
  repository, the number the web interface shows. It is unique per repository
  only.

`fj actions runs` prints both. `fj actions logs` accepts either one, but not
both at the same time: a run number is resolved through the run listing of the
repository and has to match exactly one run. An unknown number, or a number that
matches more than one run, is an error.

A run ID, a run number, a job ID and an attempt number all have to be positive.
Only a step number may be `0`, because that is the "Set up job" step.

## Logs

`fj actions logs <RUN_ID>` prints the log of every job of the run. Each job is
introduced by a header and followed by its log, unchanged:

```text
=== job 12 (build) ===
2024-05-05T12:00:00Z Starting build
```

- `--job` restricts the output to a single job. The job has to belong to the
  selected run, a job of another run is an error.
- `--step` restricts the output to a single step of the job. The step numbers
  are the ones of the job details, `0` is the "Set up job" step. `--step`
  requires `--job`.
- `--attempt` selects an attempt of the job. Attempts are numbered from 1.
  `--attempt` requires `--job`.
- `--failed` prints only the steps that failed. It cannot be combined with
  `--step` or `--attempt`, because the step details of a job only describe its
  latest attempt.
- `--filter` keeps only the log lines that contain the given text, `-i` makes
  the match case insensitive. `-i` requires `--filter`.

The bytes of a log are written as they were received. Line endings, escape
sequences and content that is not valid UTF-8 are passed through unchanged.

### Failed steps

With `--failed`, every failed job of the run is inspected:

- If the job has step details, only the steps that failed are printed. A failed
  "Set up job" or "Complete job" step counts as a failed step.
- If the job has no step details, the complete log of the job is printed and a
  notice about the missing details is written to standard error.
- If the job has step details but none of them failed, nothing is printed for
  that job. A run without failed steps prints nothing at all and still
  succeeds.

### Errors

A missing log, an unknown run and a job that does not belong to the run are
errors with a non-zero exit code, they are not reported as an empty success.
Authentication and other API errors are passed on unchanged.

## Requirements and limitations

- Reading runs and logs needs read access to the repository and to its
  Actions. A token without access to the Actions of the repository answers
  with an authorization error.
- The commands use the Actions API of the Forgejo server. A server that does
  not provide the run listing or the log endpoints answers with an API error
  instead of a listing.
- Logs are only available while the server keeps them. Once they are gone, the
  log request fails instead of returning an empty log.
- The log of a job is fetched completely and kept in memory before it is
  written, so the memory needed for a log grows with the size of that log.
- There is no live following. A log is fetched once, and the log of a run that
  is still going on can be incomplete.
