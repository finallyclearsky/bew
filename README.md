# bew

`bew` is a terminal workspace for [beads](https://github.com/gastownhall/beads) issues. It shows the issue tree, edits issues through `bd`, and hands an issue to an agent in a tmux pane.

https://github.com/user-attachments/assets/206a634c-8ced-4588-ae28-14b8174a9a3a

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/finallyblueskies/beads-explorer/main/install.sh | sh
```

This installs `bew` into `~/.local/bin`. To use another directory, run `curl ... | BIN_DIR=/some/path sh`. To uninstall, pipe `uninstall.sh` through `sh` the same way.

Run `bew` anywhere that `bd list` works. Options: `--db path/to/.beads` selects a database; `--bd path/to/bd` (env: `BEW_BD`) selects the `bd` executable.

## Keys

| Key | Tree | Task view |
| --- | --- | --- |
| `j` / `k`, arrows | Move | Select dependency |
| `h` / `l`, Left / Right | Fold / expand | — |
| `Tab` | Toggle fold | — |
| `Enter` | Open task (`+ Create New`: add a top-level issue) | Open dependency |
| `+` | Add a child issue | Add a child issue |
| `e` / `ee` | Edit description in `$EDITOR` | Edit description in `$EDITOR` |
| `et` | Edit title in `$EDITOR` | Edit title in `$EDITOR` |
| `s` | Set status | Set status |
| `p` | Set priority | Set priority |
| `x`, then `y` | Close issue | Close issue |
| `a` | Start an agent pane | Start an agent pane |
| `f` | Focus the agent pane | Focus the agent pane |
| `/` | Go to an issue ID (fuzzy) | — |
| `Backspace` | — | Back |
| `Esc` | Quit | Return to tree |
| `q` | Quit | Quit |

In a step (status, priority, close, add issue, agent command), `Enter` applies and `Esc` cancels. In a text field, press `e` while it is empty to use `$EDITOR`.

## Agents

bew must run inside tmux. Press `a` to get the command `claude --effort medium "/ship <short-id>"`. Edit it or press Ctrl-U to clear it, then press Enter. bew runs it in a new pane to the right and binds the pane to the issue. bew does not change the issue.

Press `f` to show the issue's pane again. A `●` marks issues with a live pane.

## Live updates

To see changes from other terminals and agents within a second, turn on the events journal:

```sh
bd config set events-journal true
```

Without it, bew reloads every 10 seconds. bew also reloads when the terminal gains focus; in tmux, this needs `set -g focus-events on`.
