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
