//! Live updates from the beads events journal. A background thread reads the
//! journal's head, takes a fresh baseline with `bd list`, then follows
//! `bd events tail --follow` from that head, so no change between the two
//! reads is lost. Replaying a record twice is harmless: each one carries the
//! issue's full state after the change.

use crate::app::App;
use crate::model::{Bd, IssueGraph, JournalHead, JournalRecord};
use std::io::{self, BufRead, BufReader};
use std::process::Child;
use std::sync::{mpsc, Arc, Mutex, MutexGuard, PoisonError};
use std::thread;

const ENABLE_HINT: &str = "live updates off · enable with: bd config set events-journal true";

enum Update {
    /// A `bd list` taken after the journal head was read; the records that
    /// follow apply on top of it.
    Baseline(IssueGraph),
    Record(JournalRecord),
    /// Live updates ended; the message says why.
    Stopped(String),
}

/// The running `bd events tail --follow`, shared so `Drop` can kill it.
#[derive(Default)]
struct Follower {
    child: Option<Child>,
    stopped: bool,
}

pub struct LiveUpdates {
    updates: mpsc::Receiver<Update>,
    follower: Arc<Mutex<Follower>>,
    following: bool,
}

impl LiveUpdates {
    pub fn spawn(bd: &Bd) -> Self {
        let (sender, updates) = mpsc::channel();
        let follower = Arc::new(Mutex::new(Follower::default()));
        let bd = bd.clone();
        let shared = Arc::clone(&follower);
        thread::spawn(move || {
            if let Some(message) = follow(&bd, &sender, &shared) {
                let _ = sender.send(Update::Stopped(message));
            }
        });
        Self {
            updates,
            follower,
            following: true,
        }
    }

    /// Whether updates can still arrive, so the event loop must keep waking.
    pub fn is_following(&self) -> bool {
        self.following
    }

    /// Applies everything the follower sent since the last call; returns
    /// whether the app changed.
    pub fn poll(&mut self, app: &mut App) -> bool {
        let mut records = Vec::new();
        let mut changed = false;
        loop {
            match self.updates.try_recv() {
                Ok(Update::Baseline(graph)) => {
                    // A baseline is newer than any record queued before it.
                    records.clear();
                    app.refresh_graph(graph);
                    changed = true;
                }
                Ok(Update::Record(record)) => records.push(record),
                Ok(Update::Stopped(message)) => {
                    app.set_status(message);
                    self.following = false;
                    changed = true;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.following = false;
                    break;
                }
            }
        }
        if !records.is_empty() {
            let graph = app.graph.with_records(&records);
            app.refresh_graph(graph);
            changed = true;
        }
        changed
    }
}

impl Drop for LiveUpdates {
    fn drop(&mut self) {
        let mut follower = lock(&self.follower);
        follower.stopped = true;
        if let Some(mut child) = follower.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn lock(follower: &Mutex<Follower>) -> MutexGuard<'_, Follower> {
    follower.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Follows the journal until it fails; returns why it stopped, or `None`
/// once `LiveUpdates` has been dropped.
fn follow(bd: &Bd, updates: &mpsc::Sender<Update>, follower: &Mutex<Follower>) -> Option<String> {
    loop {
        let since = match bd.journal_head() {
            Ok(JournalHead::At(seq)) => seq,
            Ok(JournalHead::Disabled) => return Some(ENABLE_HINT.to_string()),
            Err(error) => return Some(format!("live updates unavailable: {}", first_line(&error))),
        };
        let graph = match bd.load() {
            Ok(graph) => graph,
            Err(error) => return Some(format!("live updates stopped: {}", first_line(&error))),
        };
        updates.send(Update::Baseline(graph)).ok()?;

        let mut child = match bd.follow_journal(since) {
            Ok(child) => child,
            Err(error) => return Some(format!("live updates stopped: {}", first_line(&error))),
        };
        let stdout = child.stdout.take()?;
        {
            let mut follower = lock(follower);
            if follower.stopped {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            follower.child = Some(child);
        }

        let mut truncated = false;
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else {
                break;
            };
            if let Ok(record) = serde_json::from_str::<JournalRecord>(&line) {
                updates.send(Update::Record(record)).ok()?;
            } else if line.contains("events_journal_truncated") {
                // A prune passed our position; `bd` exits after this line.
                truncated = true;
            }
        }

        let child = {
            let mut follower = lock(follower);
            if follower.stopped {
                return None;
            }
            follower.child.take()
        };
        if !truncated {
            let status = child.and_then(|mut child| child.wait().ok());
            return Some(match status {
                Some(status) => format!("live updates stopped: bd events tail exited ({status})"),
                None => "live updates stopped".to_string(),
            });
        }
        // Re-baseline and follow again from the new head.
        if let Some(mut child) = child {
            let _ = child.wait();
        }
    }
}

/// The status line has one row, and `bd` errors can span several.
fn first_line(error: &io::Error) -> String {
    error
        .to_string()
        .lines()
        .next()
        .unwrap_or_default()
        .to_string()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::test_support::FakeBd;
    use std::time::{Duration, Instant};

    /// Polls until `done` holds, failing the test after five seconds.
    fn poll_until(live: &mut LiveUpdates, app: &mut App, done: impl Fn(&App) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done(app) {
            assert!(Instant::now() < deadline, "live updates never arrived");
            live.poll(app);
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn follows_from_the_journal_head_after_a_fresh_baseline() {
        let fake = FakeBd::script(
            "live",
            r#"case "$*" in
  "events tail --since 0 --json")
    printf '%s\n' '{"seq":3,"op":"create","issue_id":"a","issue":{"id":"a","status":"open"}}'
    printf '%s\n' '{"seq":4,"op":"update","issue_id":"a","issue":{"id":"a","status":"open"}}' ;;
  "list --all --json") printf '%s\n' '[{"id":"a","title":"A","status":"open"}]' ;;
  "events tail --since 4 --follow --json")
    printf '%s\n' '{"seq":5,"op":"create","issue_id":"b","issue":{"id":"b","title":"B","status":"open"}}' ;;
esac"#,
        );
        let mut app = App::new(IssueGraph::default());
        let mut live = LiveUpdates::spawn(&fake.bd(None));

        poll_until(&mut live, &mut app, |app| app.status_message().is_some());

        assert!(app.graph.is_listed("a"), "baseline applied");
        assert!(app.graph.is_listed("b"), "followed record applied");
        assert!(!live.is_following());
        assert!(app
            .status_message()
            .unwrap()
            .contains("live updates stopped"));
        let calls = fake.arguments().join(" ");
        assert!(
            calls.starts_with(
                "events tail --since 0 --json list --all --json events tail --since 4 --follow --json"
            ),
            "{calls}"
        );
    }

    #[test]
    fn a_disabled_journal_stops_with_the_enable_hint() {
        let fake = FakeBd::script(
            "live-off",
            r#"echo "note: the events journal is disabled for this workspace" >&2"#,
        );
        let mut app = App::new(IssueGraph::default());
        let mut live = LiveUpdates::spawn(&fake.bd(None));

        poll_until(&mut live, &mut app, |app| app.status_message().is_some());

        assert_eq!(app.status_message(), Some(ENABLE_HINT));
        assert!(!fake.arguments().iter().any(|argument| argument == "list"));
    }

    #[test]
    fn dropping_live_updates_kills_the_follower() {
        let pid_file = std::env::temp_dir().join(format!("be-live-pid-{}", std::process::id()));
        let fake = FakeBd::script(
            "live-drop",
            &format!(
                r#"case "$*" in
  "list --all --json") printf '%s\n' '[]' ;;
  *--follow*)
    echo $$ > '{}'
    printf '%s\n' '{{"seq":1,"op":"create","issue_id":"a","issue":{{"id":"a","status":"open"}}}}'
    exec sleep 30 ;;
esac"#,
                pid_file.display()
            ),
        );
        let mut app = App::new(IssueGraph::default());
        let mut live = LiveUpdates::spawn(&fake.bd(None));
        poll_until(&mut live, &mut app, |app| app.graph.is_listed("a"));
        let pid = std::fs::read_to_string(&pid_file).unwrap();

        drop(live);

        let alive = std::process::Command::new("kill")
            .args(["-0", pid.trim()])
            .status()
            .unwrap()
            .success();
        let _ = std::fs::remove_file(&pid_file);
        assert!(!alive, "the follower must not outlive be");
    }
}
