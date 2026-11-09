//! Live updates from the beads events journal. A background thread reads the
//! journal's head, takes a fresh baseline with `bd list`, then follows
//! `bd events tail --follow` from that head, so no change between the two
//! reads is lost. Replaying a record twice is harmless: each one carries the
//! issue's full state after the change. If the follower exits, the thread
//! waits and starts over from a fresh baseline.

use crate::app::App;
use crate::model::{Bd, IssueGraph, JournalHead, JournalRecord};
use std::io::{self, BufRead, BufReader};
use std::process::Child;
use std::sync::{mpsc, Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

const ENABLE_HINT: &str =
    "events journal off · reloading every 10s · for live updates: bd config set events-journal true";

/// The first wait before following again after the follower exits.
const FIRST_RETRY_DELAY: Duration = Duration::from_secs(1);
/// The longest wait between attempts.
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);
/// A follower that ran this long counts as healthy, so the wait resets.
const HEALTHY_RUN: Duration = Duration::from_secs(60);
/// How often a waiting thread checks whether bew has quit.
const STOP_CHECK_INTERVAL: Duration = Duration::from_millis(100);

enum Update {
    /// A `bd list` taken after the journal head was read; the records that
    /// follow apply on top of it.
    Baseline(IssueGraph),
    /// The follower is running, so records arrive as changes land.
    Following,
    Record(JournalRecord),
    /// The follower is not running; the message says why and whether it
    /// will start again.
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
            following: false,
        }
    }

    /// Whether the follower is running, so changes made elsewhere arrive on
    /// their own and periodic reloads are not needed.
    pub fn is_following(&self) -> bool {
        self.following
    }

    /// Applies everything the follower sent since the last call; returns
    /// whether the graph or the status line changed.
    pub fn poll(&mut self, app: &mut App) -> bool {
        let mut records = Vec::new();
        let mut changed = false;
        loop {
            match self.updates.try_recv() {
                Ok(Update::Baseline(graph)) => {
                    // A baseline is newer than any record queued before it.
                    records.clear();
                    if graph != app.graph {
                        app.refresh_graph(graph);
                        changed = true;
                    }
                }
                Ok(Update::Following) => self.following = true,
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
            if graph != app.graph {
                app.refresh_graph(graph);
                changed = true;
            }
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

/// How one attempt to follow the journal ended.
enum Run {
    /// bew quit.
    Quit,
    /// A prune passed our position; follow again from a fresh baseline.
    Truncated,
    /// The baseline or the follower failed; the message says why.
    Ended(String),
}

/// Follows the journal, starting over after every failure. Returns why
/// live updates are off for good, or `None` once `LiveUpdates` is dropped.
fn follow(bd: &Bd, updates: &mpsc::Sender<Update>, follower: &Mutex<Follower>) -> Option<String> {
    let mut delay = FIRST_RETRY_DELAY;
    // Before the journal answers once, a failure means this `bd` cannot
    // follow at all, such as one older than 1.3; afterwards it is transient.
    let mut answered = false;
    loop {
        let started = Instant::now();
        let reason = match bd.journal_head() {
            Ok(JournalHead::Disabled) => return Some(ENABLE_HINT.to_string()),
            Err(error) if !answered => {
                return Some(format!("live updates unavailable: {}", first_line(&error)));
            }
            Err(error) => first_line(&error),
            Ok(JournalHead::At(since)) => {
                answered = true;
                match follow_once(bd, updates, follower, since) {
                    Run::Quit => return None,
                    Run::Truncated => continue,
                    Run::Ended(reason) => reason,
                }
            }
        };
        if started.elapsed() >= HEALTHY_RUN {
            delay = FIRST_RETRY_DELAY;
        }
        let message = format!(
            "live updates interrupted: {reason} · retrying in {}s",
            delay.as_secs()
        );
        updates.send(Update::Stopped(message)).ok()?;
        if !sleep_unless_stopped(follower, delay) {
            return None;
        }
        delay = (delay * 2).min(MAX_RETRY_DELAY);
    }
}

/// Sends a fresh baseline, then follows the journal from `since` until the
/// follower exits.
fn follow_once(
    bd: &Bd,
    updates: &mpsc::Sender<Update>,
    follower: &Mutex<Follower>,
    since: i64,
) -> Run {
    let graph = match bd.load() {
        Ok(graph) => graph,
        Err(error) => return Run::Ended(first_line(&error)),
    };
    if updates.send(Update::Baseline(graph)).is_err() {
        return Run::Quit;
    }

    let mut child = match bd.follow_journal(since) {
        Ok(child) => child,
        Err(error) => return Run::Ended(first_line(&error)),
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Run::Ended("bd events tail has no output".to_string());
    };
    {
        let mut follower = lock(follower);
        if follower.stopped {
            let _ = child.kill();
            let _ = child.wait();
            return Run::Quit;
        }
        follower.child = Some(child);
    }
    if updates.send(Update::Following).is_err() {
        return Run::Quit;
    }

    let mut truncated = false;
    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else {
            break;
        };
        if let Ok(record) = serde_json::from_str::<JournalRecord>(&line) {
            if updates.send(Update::Record(record)).is_err() {
                return Run::Quit;
            }
        } else if line.contains("events_journal_truncated") {
            // `bd` exits after this line.
            truncated = true;
        }
    }

    let child = {
        let mut follower = lock(follower);
        if follower.stopped {
            return Run::Quit;
        }
        follower.child.take()
    };
    let status = child.and_then(|mut child| child.wait().ok());
    if truncated {
        return Run::Truncated;
    }
    Run::Ended(match status {
        Some(status) => format!("bd events tail exited ({status})"),
        None => "bd events tail exited".to_string(),
    })
}

/// Waits `delay`; returns `false` early if `LiveUpdates` was dropped.
fn sleep_unless_stopped(follower: &Mutex<Follower>, delay: Duration) -> bool {
    let deadline = Instant::now() + delay;
    while Instant::now() < deadline {
        if lock(follower).stopped {
            return false;
        }
        thread::sleep(STOP_CHECK_INTERVAL.min(deadline.saturating_duration_since(Instant::now())));
    }
    !lock(follower).stopped
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
        let status = app.status_message().unwrap();
        assert!(status.contains("live updates interrupted"), "{status}");
        assert!(status.contains("retrying in 1s"), "{status}");
        let calls = fake.arguments().join(" ");
        assert!(
            calls.starts_with(
                "events tail --since 0 --json list --all --json events tail --since 4 --follow --json"
            ),
            "{calls}"
        );
    }

    #[test]
    fn an_exited_follower_starts_again_from_a_fresh_baseline() {
        let fake = FakeBd::script(
            "live-retry",
            r#"case "$*" in
  "events tail --since 0 --json") : ;;
  "list --all --json") printf '%s\n' '[{"id":"a","title":"A","status":"open"}]' ;;
esac"#,
        );
        let mut app = App::new(IssueGraph::default());
        let mut live = LiveUpdates::spawn(&fake.bd(None));

        let deadline = Instant::now() + Duration::from_secs(5);
        let baselines = || {
            fake.arguments()
                .iter()
                .filter(|argument| *argument == "list")
                .count()
        };
        while baselines() < 2 {
            assert!(
                Instant::now() < deadline,
                "the follower never started again"
            );
            live.poll(&mut app);
            thread::sleep(Duration::from_millis(20));
        }
        assert!(app.graph.is_listed("a"));
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
