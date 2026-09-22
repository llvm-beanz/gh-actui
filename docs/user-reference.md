# User reference

## Usage

```console
gh actui <REPOSITORY>
```

`REPOSITORY` can be an HTTPS GitHub URL or an `OWNER/REPO` name.

The main table lists each workflow's name, state, and path. The highlighted row
is the current selection, and the table scrolls automatically as the selection
moves beyond the visible area.

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

There is no single-key quit binding in Normal mode.
