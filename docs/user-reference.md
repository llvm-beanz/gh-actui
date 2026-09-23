# User reference

## Usage

```console
gh actui <REPOSITORY_OR_STATE_FILE>
```

The argument can be an HTTPS GitHub URL, an `OWNER/REPO` name, or the path to
an existing JSON view state file.

The main table lists the names and run status of active workflows. Workflows in
any disabled or deleted state are omitted. The highlighted row is the current
selection, and the table scrolls automatically as the selection moves beyond
the visible area.

| Status | Meaning |
| --- | --- |
| 🟢 | The latest completed run succeeded |
| 🔴 | The latest completed run failed |
| ⚪ | There is no completed run, or its conclusion was neither success nor failure |

The status indicator flashes every half second while the workflow has a run in
progress.

The 24 Hours, 7 Days, and 14 Days columns summarize completed runs as:

```text
passed/failed/total pass%
10/15/28 35.7%
```

The total includes every completed run conclusion, while passed and failed
count only `success` and `failure`. In-progress runs are not included. Rates
below 70% are red, rates below 90% are yellow, and rates of 90% or higher are
green.

GitHub's server-side `created` filter for workflow runs can return stale
results for workflows with large run histories. `gh-actui` therefore fetches
the unfiltered workflow-run endpoint in newest-first pages and applies the
14-day cutoff locally. Pagination stops after passing that cutoff and finding
the latest completed run.

Workflow and run data load in the background and refresh every 15 seconds by
default. The bottom bar always displays the configured refresh rate. While
loading or refreshing, it replaces the normal keybinding summary with the
current operation. The interface remains responsive during background work.

## Keyboard shortcuts

The interface starts in Normal mode. Press `:` to enter Command mode. Command
mode displays an editable command line at the bottom of the screen; press Enter
to execute a command or Escape to return to Normal mode.

| Key | Action |
| --- | --- |
| `j` or Down | Select the next workflow |
| `k` or Up | Select the previous workflow |
| `Ctrl-d` or Page Down | Move forward up to 10 workflows |
| `Ctrl-u` or Page Up | Move backward up to 10 workflows |
| `gg` or Home | Select the first workflow |
| `G` or End | Select the last workflow |
| `Ctrl+Tab` | Switch to the next tab |
| `Ctrl+Shift+Tab` | Switch to the previous tab |
| `:` | Enter Command mode |

The tab shortcuts work in both Normal and Command modes.

### Command-line editing

| Key | Action |
| --- | --- |
| Left or Right | Move the command cursor |
| Home or End | Move to the start or end of the command |
| Up or Down | Move backward or forward through command history |
| Backspace or Delete | Delete before or under the cursor |
| Enter | Execute the command |
| Escape | Cancel the command |

## Commands

| Command | Action |
| --- | --- |
| `:q` | Exit |
| `:w [path]` | Save the repository and displayed workflows as JSON |
| `:wq [path]` | Save the view, then exit if the write succeeds |
| `:e [path]` | Load a saved view and refresh its workflows from GitHub |
| `:refresh` | Refresh workflow and run data immediately |
| `:refresh-rate N` | Refresh automatically every `N` seconds |
| `:filter EXPRESSION` | Apply a GitHub Projects-style workflow filter |
| `:filter` | Clear the active filter |
| `:sort FIELD[:asc\|desc]` | Sort workflows by a field; ascending by default |
| `:sort` | Clear the active sort |
| `:tabnew [name]` | Duplicate the active workflow view in an optionally named tab |
| `:tabsetname NAME` | Change the active tab's displayed name |
| `:tabnext` or `:tabn` | Switch to the next tab |
| `:tabprevious` or `:tabp` | Switch to the previous tab |
| `:tabclose` or `:tabc` | Close the active tab |
| `:triage` | Analyze visible failing workflows and open an enriched triage tab |
| `:d` | Delete the selected workflow from the view |
| `:dN` | Delete `N` consecutive workflows starting at the selection |

There is no single-key quit binding in Normal mode.

Deletion affects only the active tab; the process-wide workflow data remains
available to other tabs. Use `:w` to persist the updated tab. If the requested
count extends past the end of the table, all remaining rows are deleted.

### Tabs

The process fetches and refreshes one global workflow collection for the
repository. Tabs are lightweight views over that collection. Each tab has its
own workflow subset, selected row, filter, and sort.

`:tabnew [name]` duplicates the active tab and switches to the duplicate
without performing another network request. The optional argument, including
spaces, becomes the tab's displayed name; unnamed tabs use `Tab N`.
`:tabsetname NAME` changes the active tab's name and also accepts spaces.
Unlike `ghui`, the `:tabnew` argument is not interpreted as a URL because one
`gh-actui` process monitors only one repository. Tab navigation wraps at either
end. Closing the only tab resets it to an empty view instead of exiting the
application.

### Failure triage

`:triage` starts a background analysis of only the visible workflows whose
latest completed run status is failing. It does not change the source tab.
For each candidate, `gh-actui` first fetches its workflow YAML and excludes
workflows without a top-level `on.schedule` trigger. This prevents
non-scheduled workflows with large histories from forcing a scan of their
entire run history. It then scans the unfiltered newest-first run history for
scheduled runs. A completed scheduled failure is analyzed directly; when the
newest scheduled run is still queued or in progress, the most recent completed
scheduled failure is analyzed instead.

The resulting `Triage` tab displays a vertically scrollable collection of
bordered panels. The first panel correlates tests reported by more than one
workflow, with separate sections for failures and unexpected passes and a list
of the affected workflows under each test. If nothing is shared, it states
that explicitly.

Each following workflow panel contains the failed jobs, first failed step for
each job, and the extracted lit summary for failures in the `Run HLSL Tests`
step. Lit summaries retain their original line structure and wrap to the
terminal width. Use the normal `j`/`k`, arrow, paging, and jump bindings to
move between workflow panels; the selected panel has a highlighted border.
Workflows whose latest scheduled result is not a failure are omitted. Run,
job, and log requests execute in the background and the footer displays
`TRIAGING` while they are in progress.

The triage tab's view type, name, workflow subset, and selection are saved, so
it reloads using the bordered triage layout rather than the normal workflow
table. Fetched job, step, and log details are intentionally not saved because
they may become stale; a reloaded triage view shows unavailable details until
`:triage` is run again to create a fresh analysis tab.

### Filtering and sorting

Filters support quoted values, comma-separated alternatives, negation,
`has:`, `no:`, `is:`, general name text, `*` wildcards, comparisons, and
inclusive ranges. Multiple clauses are combined with AND.

Examples:

```vim
:filter status:failure
:filter -status:success name:*Linux*
:filter 24h.rate:<70 24h.total:>0
:filter 7d.fail:1..*
:sort name
:sort 24h.rate:desc
```

Available fields are:

| Field | Value |
| --- | --- |
| `name` or `workflow` | Workflow name |
| `status` or `state` | `success`, `failure`, `other`, or `in_progress` |
| `24h`, `7d`, `14d` | Pass percentage for that period |
| `<period>.pass` | Passed run count |
| `<period>.fail` | Failed run count |
| `<period>.total` | Completed run count |
| `<period>.rate` | Pass percentage |

Periods accept `24h`, `7d`, or `14d`. Relative keywords such as `@me` are not
supported. Filters and sorts apply only to the active tab, survive refreshes,
and are included in saved view files. `:dN` deletes rows in the current
filtered and sorted order.

After `:w path`, `:wq path`, or `:e path` succeeds, that path is remembered.
Later write or edit commands without a path reuse it. A state file supplied on
the command line is also remembered. Using `:w`, `:wq`, or `:e` without a
remembered path reports an error. `:wq` does not exit if saving fails.

State files contain the repository identity, the global monitored workflow
IDs, every tab in tab-bar order, each tab's name, workflow IDs, filter, sort,
and selected workflow, and the active tab. Workflow names, paths, enablement
state, and run status are not saved because they may change; they are queried
from GitHub whenever a state file is loaded. Existing single-view state files
load as one unnamed tab.
