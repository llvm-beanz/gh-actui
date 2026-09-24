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

Workflow discovery and run-history enrichment load separately in the
background. The workflow rows appear as soon as the repository's active
workflow list is available; status and 24-hour, 7-day, and 14-day metrics then
fill in incrementally as each workflow's run history finishes loading. Up to
eight histories are queried concurrently. Refreshes run every 15 seconds by
default and retain the previous status and metrics until each updated result
arrives; only workflows that have no data yet, such as ones discovered during
that refresh, show `loading...` placeholders.

The bottom bar always displays the configured refresh rate. While loading or
refreshing, it shows the current operation and run-status progress. The
borderless bar occupies one terminal row. Messages and command input stay
anchored at the left edge, while the refresh rate and the highlighted current
mode or operation stay anchored at the right edge. The interface remains
responsive during background work.

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
| `Ctrl-w`, then an arrow or `h`/`j`/`k`/`l` | Focus the split view in that direction |
| `Ctrl-w`, then `+` or `-` | Increase or decrease active view height by one row |
| `Ctrl-w`, then `>` or `<` | Increase or decrease active view width by one column |
| `Ctrl-w`, then `=` | Equalize all splits in the active tab |
| Left mouse click | Focus the split view under the pointer |
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

Command history is shared across `gh-actui` sessions for the current user.
Empty commands and consecutive duplicates are omitted, and the latest 1,000
commands are retained. The history file is stored at:

- `%LOCALAPPDATA%\gh-actui\history` on Windows
- `$XDG_STATE_HOME/gh-actui/history` when `XDG_STATE_HOME` is set
- `~/.local/state/gh-actui/history` otherwise

Set `GH_ACTUI_HISTORY` to use a different history file. History is loaded at
startup and updated after every executed command, including `:q`.

## Commands

| Command | Action |
| --- | --- |
| `:q` | Exit |
| `:w [path]` | Save the repository and displayed workflows as JSON |
| `:wq [path]` | Save the view, then exit if the write succeeds |
| `:e [path]` | Load a saved view and refresh its workflows from GitHub |
| `:refresh` | Refresh workflow/run data, or rerun the active triage view |
| `:refresh-rate N` | Refresh automatically every `N` seconds |
| `:filter EXPRESSION` | Apply a GitHub Projects-style workflow filter |
| `:filter` | Clear the active filter |
| `:sort FIELD[:asc\|desc]` | Sort workflows by a field; ascending by default |
| `:sort` | Clear the active sort |
| `:split horizontal` | Duplicate the active list view in a top/bottom split |
| `:split vertical` | Duplicate the active list view in a side-by-side split |
| `:resize height N` | Set the active view's approximate height to `N` rows |
| `:resize height +N` or `-N` | Increase or decrease the active view's height |
| `:resize width N` | Set the active view's approximate width to `N` columns |
| `:resize width +N` or `-N` | Increase or decrease the active view's width |
| `:resize equal` | Equalize all splits in the active tab |
| `:tabnew [name]` | Duplicate the active workflow view in an optionally named tab |
| `:tabsetname NAME` | Change the active tab's displayed name |
| `:tabnext` or `:tabn` | Switch to the next tab |
| `:tabprevious` or `:tabp` | Switch to the previous tab |
| `:tabclose` or `:tabc` | Close the active tab |
| `:triage` | Analyze visible failing workflows and open an enriched triage tab |
| `:d` | Delete the selected workflow from the view |
| `:dN` | Delete `N` consecutive workflows starting at the selection |

There is no single-key quit binding in Normal mode.

Deletion affects only the active view; the process-wide workflow data remains
available to other views and tabs. Use `:w` to persist the updated view. If the
requested count extends past the end of the table, all remaining rows are
deleted.

### Tabs

The process fetches and refreshes one global workflow collection for the
repository. Tabs are lightweight views over that collection. A tab contains
one or more list views, and each list view has its own workflow subset,
selected row, filter, and sort.

`:tabnew [name]` duplicates the active tab and switches to the duplicate
without performing another network request. The optional argument, including
spaces, becomes the tab's displayed name; unnamed tabs use `Tab N`.
`:tabsetname NAME` changes the active tab's name and also accepts spaces.
Unlike `ghui`, the `:tabnew` argument is not interpreted as a URL because one
`gh-actui` process monitors only one repository. Tab navigation wraps at either
end. Closing the only tab resets it to an empty view instead of exiting the
application.

`:split horizontal` duplicates the active list view below it.
`:split vertical` duplicates it to the right. Splitting can be repeated to
create nested layouts. The active view has a cyan border; inactive views have
dim borders. Use `Ctrl-w` followed by an arrow key or `h`, `j`, `k`, or `l` to
move focus geometrically, or click a view with the left mouse button. Commands
such as `:filter`, `:sort`, `:d`, and `:triage` operate on the active view.
Triage result tabs cannot be split.

Resizing changes the nearest split ancestor with the requested orientation:
height changes use horizontal splits and width changes use vertical splits.
Each side is kept at a minimum of 5 rows for horizontal splits or 20 columns
for vertical splits. If the active view has no matching split, the command
reports that it cannot be resized in that dimension. Split sizes are saved as
relative weights, so their proportions survive terminal resizing and saved
session reloads. `:resize equal` sizes the split subtrees according to how many
views they contain, making the final views equal in size rather than merely
setting every nested split to 50/50. It does not change the split arrangement.

### Failure triage

`:triage` starts a background analysis of only the visible workflows whose
latest completed run status is failing. It does not change the source tab.
It can start while a workflow refresh is already running; in that case it
uses the currently displayed failing-workflow snapshot while the refresh
continues independently.
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
of the affected workflows under each test. Correlation uses the same structured
per-workflow test lists displayed in the individual panels, with raw lit
summary parsing retained as a fallback. If nothing is shared, it states that
explicitly.

Each following workflow panel contains the failed jobs, first failed step for
each job, and the extracted lit summary for failures in the `Run HLSL Tests`
step. Each summary includes the individual tests listed under both `Failed
Tests` and `Unexpectedly Passed Tests`, when present. Each workflow panel also
shows explicit `Failed tests` and `Unexpectedly passed tests` lists parsed
from those summary sections or, as a fallback, the detailed `FAIL:` and
`XPASS:` result lines. Lit summaries retain their original line structure and
wrap to the terminal width. Use the normal `j`/`k`, arrow, paging, and jump
bindings to move between workflow panels; the selected panel has a highlighted
border.
Workflows whose latest scheduled result is not a failure are omitted. Run,
job, and log requests execute in the background and the footer displays
`TRIAGING` while they are in progress.

Triage tabs and their fetched details are intentionally not saved because the
analysis may become stale. Saving persists only normal workflow tabs and
remaps the saved active tab to the nearest preceding normal tab. If only
triage tabs remain open, the saved session contains one normal tab with the
complete global monitored workflow list. Run `:triage` after loading to create
a fresh analysis tab.

Running `:refresh` while a triage tab is active reruns the analysis for that
tab's workflow subset and replaces its results in place. It does not create
another tab. Automatic interval refreshes continue to update the shared
workflow and run data only; they do not repeatedly download triage jobs and
logs.

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
supported. Filters and sorts apply only to the active view, survive refreshes,
and are included in saved view files. `:dN` deletes rows in the current
filtered and sorted order.

After `:w path`, `:wq path`, or `:e path` succeeds, that path is remembered.
Later write or edit commands without a path reuse it. A state file supplied on
the command line is also remembered. Using `:w`, `:wq`, or `:e` without a
remembered path reports an error. `:wq` does not exit if saving fails.

State files contain the repository identity, the global monitored workflow
IDs, every tab in tab-bar order, each tab's name, split layout, active view,
and each view's workflow IDs, filter, sort, and selected workflow. Workflow
names, paths, enablement state, and run status are not saved because they may
change; they are queried from GitHub whenever a state file is loaded. Existing
single-view state files load as one unnamed tab with one view.
