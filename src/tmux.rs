//! The agent handoff: run a command in a new tmux pane, tag the pane with
//! the bead it belongs to, and find or focus that pane later. The tag lives
//! in tmux as a pane option, so it outlives a bew restart and needs no state
//! file of bew's own.

use std::collections::HashMap;
use std::env;
use std::io;
use std::path::Path;
use std::process::Command;

/// The pane option that holds the bead ID of the pane's agent.
const BEAD_OPTION: &str = "@bew_bead";

/// Whether bew runs inside tmux, so panes can be created and focused.
pub fn available() -> bool {
    env::var_os("TMUX").is_some_and(|value| !value.is_empty())
}

/// Runs `command` in a new pane split to the right of bew's own pane, in
/// `cwd`, and tags it with `bead_id`. Returns the new pane's ID.
pub fn start(command: &str, cwd: &Path, bead_id: &str) -> io::Result<String> {
    let mut split = Command::new("tmux");
    split.args(["split-window", "-h", "-d", "-P", "-F", "#{pane_id}", "-c"]);
    split.arg(cwd);
    // Split bew's pane even if another pane became active meanwhile.
    if let Some(own_pane) = env::var_os("TMUX_PANE").filter(|pane| !pane.is_empty()) {
        split.arg("-t").arg(own_pane);
    }
    // One argument: tmux runs it through the default shell, as typed.
    split.arg(command);
    let pane_id = run(&mut split, "split-window")?.trim().to_string();
    if pane_id.is_empty() {
        return Err(io::Error::other("tmux split-window printed no pane ID"));
    }
    run(
        Command::new("tmux").args(["set-option", "-p", "-t", &pane_id, BEAD_OPTION, bead_id]),
        "set-option",
    )?;
    Ok(pane_id)
}

/// Live panes that carry a bead tag, as bead ID → pane ID.
pub fn panes() -> io::Result<HashMap<String, String>> {
    let format = format!("#{{pane_dead}}\t#{{pane_id}}\t#{{{BEAD_OPTION}}}");
    let output = run(
        Command::new("tmux").args(["list-panes", "-a", "-F", &format]),
        "list-panes",
    )?;
    Ok(parse_panes(&output))
}

/// Shows `pane_id`: selects its window and the pane, then switches the
/// client to its session, which may be another one.
pub fn focus(pane_id: &str) -> io::Result<()> {
    let session = run(
        Command::new("tmux").args(["display-message", "-p", "-t", pane_id, "#{session_id}"]),
        "display-message",
    )?;
    run(
        Command::new("tmux").args(["select-window", "-t", pane_id]),
        "select-window",
    )?;
    run(
        Command::new("tmux").args(["select-pane", "-t", pane_id]),
        "select-pane",
    )?;
    run(
        Command::new("tmux").args(["switch-client", "-t", session.trim()]),
        "switch-client",
    )?;
    Ok(())
}

fn parse_panes(output: &str) -> HashMap<String, String> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(3, '\t');
            let dead = fields.next()?;
            let pane_id = fields.next()?;
            let bead_id = fields.next()?.trim();
            (dead != "1" && !bead_id.is_empty()).then(|| (bead_id.to_string(), pane_id.to_string()))
        })
        .collect()
}

fn run(command: &mut Command, verb: &str) -> io::Result<String> {
    let output = command.output().map_err(|error| {
        io::Error::new(error.kind(), format!("could not run tmux {verb}: {error}"))
    })?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let message = stderr.trim();
    Err(io::Error::other(if message.is_empty() {
        format!("tmux {verb} failed: {}", output.status)
    } else {
        format!("tmux {verb} failed: {message}")
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panes_maps_tagged_live_panes_to_their_beads() {
        let output = "0\t%1\t\n0\t%2\tbew-1\n1\t%3\tbew-2\n0\t%4\tbew-3\n";
        let panes = parse_panes(output);
        assert_eq!(panes.len(), 2);
        assert_eq!(panes["bew-1"], "%2");
        assert_eq!(panes["bew-3"], "%4");
        assert!(!panes.contains_key("bew-2"), "dead panes do not count");
    }
}
