# bew

`bew` is a minimal terminal workspace for [beads](https://github.com/gastownhall/beads) issues. Browse the issue tree, edit issues, and hand one to a coding agent in a tmux pane.

https://github.com/user-attachments/assets/a9cc007e-1222-4e12-adbf-b5b5a20b7ee4

## Install

You need [Rust](https://rustup.rs) to build it and [`bd`](https://github.com/gastownhall/beads) to run it. Agent panes also need tmux.

```sh
curl -fsSL https://raw.githubusercontent.com/finallyclearsky/bew/main/install.sh | sh
```

This builds `bew` and puts it in `~/.local/bin`. Set `BIN_DIR` to install somewhere else. To remove it, delete the binary.

## Use

Run `bew` in any directory where `bd list` works.

| Key | Action |
| --- | --- |
| `j` / `k` | Move |
| `h` / `l` | Fold / expand |
| `Enter` | Open the issue |
| `Backspace` / `Esc` | Back / return to the tree |
| `/` | Go to an issue by ID |
| `+` | Add a child issue |
| `e` / `et` | Edit the description / title in `$EDITOR` |
| `s` / `p` | Set status / priority |
| `x` | Close the issue |
| `a` | Start an agent on the issue |
| `f` | Focus the issue's agent pane |
| `q` | Quit |

`bew --help` lists every key and option.

## Agents

Run `bew` inside tmux. Press `a` on an issue and bew opens the agent command in `$EDITOR`, by default `claude --effort medium "/ship <id>"`. Change it to any command you like. Save to run it in a new pane on the right, or quit without saving to cancel.

A `●` marks issues with a live agent pane. Press `f` to jump to it.

## Live updates

bew reloads every 10 seconds. To see changes from agents and other terminals as they land, turn on the events journal:

```sh
bd config set events-journal true
```
