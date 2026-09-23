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
| `:` | Enter Command mode |

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
| `:e [path]` | Load a saved view and refresh its workflows from GitHub |

There is no single-key quit binding in Normal mode.

After `:w path` or `:e path` succeeds, that path is remembered. Later `:w` or
`:e` commands without a path reuse it. A state file supplied on the command
line is also remembered. Using `:w` or `:e` without a remembered path reports
an error.

State files contain only the repository identity and the IDs of the workflows
in the view. Workflow names, paths, enablement state, and run status are not
saved because they may change; they are queried from GitHub whenever a state
file is loaded.
