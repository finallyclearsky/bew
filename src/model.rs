use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Output, Stdio};

// `bd show` hydrates dependencies into issue summaries (`id`, `title`, ...),
// while `bd list` emits raw edge records (`depends_on_id`, `type`, ...). The
// aliases let one shape cover both.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct Dependency {
    #[serde(alias = "depends_on_id")]
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub priority: i32,
    #[serde(default, alias = "type")]
    pub dependency_type: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct Issue {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub priority: i32,
    #[serde(default)]
    pub issue_type: String,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub dependencies: Vec<Dependency>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditField {
    Title,
    Description,
}

/// Everything `bd create` needs for a new issue; `parent_id` is `None` for a
/// top-level issue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddIssueDraft {
    pub parent_id: Option<String>,
    pub title: String,
    pub description: String,
    pub issue_type: String,
    pub priority: i32,
}

/// One record from `bd events tail`. `issue` is the full state after the
/// change, `null` on a delete, and never carries dependencies: those change
/// through `dep_add` / `dep_remove` records, which name the edge in `dep`.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct JournalRecord {
    pub op: String,
    pub issue_id: String,
    pub issue: Option<Issue>,
    pub dep: Option<JournalDep>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct JournalDep {
    pub kind: String,
    pub target: String,
}

/// Where to start following the events journal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JournalHead {
    /// The workspace does not record changes (`events-journal` is off).
    Disabled,
    /// The highest `seq` assigned so far; 0 for an empty journal.
    At(i64),
}

/// The tree shows open and in-progress issues; the rest are context.
fn is_listed_status(status: &str) -> bool {
    matches!(status, "open" | "in_progress")
}

#[derive(Clone, Debug, Default)]
pub struct IssueGraph {
    issues: HashMap<String, Issue>,
    order: Vec<String>,
    listed: HashSet<String>,
    tree_children: HashMap<String, Vec<String>>,
    roots: Vec<String>,
}

impl IssueGraph {
    /// `listed` issues form the tree; `context` issues (e.g. closed ones) only
    /// hydrate dependency targets reached through Task View.
    pub fn new(listed_issues: Vec<Issue>, context: Vec<Issue>) -> Self {
        let mut order = Vec::with_capacity(listed_issues.len());
        let mut map = HashMap::with_capacity(listed_issues.len() + context.len());
        for issue in listed_issues {
            if !map.contains_key(&issue.id) {
                order.push(issue.id.clone());
            }
            map.insert(issue.id.clone(), issue);
        }
        let listed: HashSet<String> = order.iter().cloned().collect();
        for issue in context {
            map.entry(issue.id.clone()).or_insert(issue);
        }

        // A dependency can point to a deleted or externally sourced issue. Keep a
        // navigable placeholder rather than silently dropping that graph edge.
        let missing: Vec<Issue> = map
            .values()
            .flat_map(|issue| issue.dependencies.iter())
            .filter(|dep| !map.contains_key(&dep.id))
            .map(|dep| Issue {
                id: dep.id.clone(),
                title: if dep.title.is_empty() {
                    "(not found)".to_string()
                } else {
                    dep.title.clone()
                },
                status: if dep.status.is_empty() {
                    "?".to_string()
                } else {
                    dep.status.clone()
                },
                priority: dep.priority,
                ..Issue::default()
            })
            .collect();
        for issue in missing {
            map.entry(issue.id.clone()).or_insert(issue);
        }

        // `bd` stores parent-child on the child as a dependency on its parent.
        // Reverse that relationship for the tree while leaving other dependency
        // types pointing from an issue to its prerequisite. Only listed issues
        // shape the tree, so edges to or from context issues are omitted.
        let mut tree_children: HashMap<String, Vec<String>> = HashMap::new();
        let mut targets = HashSet::new();
        for issue_id in &order {
            let Some(issue) = map.get(issue_id) else {
                continue;
            };
            for dependency in &issue.dependencies {
                if !listed.contains(&dependency.id) {
                    continue;
                }
                let (parent, child) = if dependency.dependency_type == "parent-child" {
                    (dependency.id.clone(), issue.id.clone())
                } else {
                    (issue.id.clone(), dependency.id.clone())
                };
                let children = tree_children.entry(parent).or_default();
                if !children.contains(&child) {
                    children.push(child.clone());
                }
                targets.insert(child);
            }
        }
        let mut roots: Vec<String> = order
            .iter()
            .filter(|id| !targets.contains(id.as_str()))
            .cloned()
            .collect();

        // A graph made solely of cycles has no mathematical root. Showing each
        // component entry is more useful than an empty explorer.
        if roots.is_empty() {
            roots.clone_from(&order);
        }

        Self {
            issues: map,
            order,
            listed,
            tree_children,
            roots,
        }
    }

    pub fn issue(&self, id: &str) -> Option<&Issue> {
        self.issues.get(id)
    }

    pub fn roots(&self) -> &[String] {
        &self.roots
    }

    pub fn is_listed(&self, id: &str) -> bool {
        self.listed.contains(id)
    }

    pub fn tree_children(&self, id: &str) -> &[String] {
        self.tree_children.get(id).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn replace_issue(&mut self, issue: Issue) {
        self.issues.insert(issue.id.clone(), issue);
    }

    /// Optimistic local mirror of `bd close`: the issue leaves the listed set
    /// and joins the context issues, so the UI can update before `bd` confirms.
    pub fn with_issue_closed(&self, issue_id: &str) -> IssueGraph {
        let closed = self.issues.get(issue_id).map(|issue| Issue {
            status: "closed".to_string(),
            ..issue.clone()
        });
        self.with_records(&[JournalRecord {
            op: "close".to_string(),
            issue_id: issue_id.to_string(),
            issue: closed,
            dep: None,
        }])
    }

    /// Replays journal records onto a copy of this graph. Snapshots carry no
    /// dependencies, so an issue keeps the edges already known for it and
    /// `dep_*` records edit them. The tree keeps its order; issues that join
    /// it go last.
    pub fn with_records(&self, records: &[JournalRecord]) -> IssueGraph {
        let mut issues = self.issues.clone();
        let mut touched = Vec::new();
        for record in records {
            if record.op == "delete" {
                issues.remove(&record.issue_id);
                continue;
            }
            if let Some(snapshot) = &record.issue {
                let dependencies = issues
                    .remove(&snapshot.id)
                    .map(|known| known.dependencies)
                    .unwrap_or_default();
                touched.push(snapshot.id.clone());
                issues.insert(
                    snapshot.id.clone(),
                    Issue {
                        dependencies,
                        ..snapshot.clone()
                    },
                );
            }
            if let (Some(dep), Some(issue)) = (&record.dep, issues.get_mut(&record.issue_id)) {
                // `dep_add` is an upsert: a re-add only refreshes the edge.
                issue
                    .dependencies
                    .retain(|known| known.id != dep.target || known.dependency_type != dep.kind);
                if record.op == "dep_add" {
                    issue.dependencies.push(Dependency {
                        id: dep.target.clone(),
                        dependency_type: dep.kind.clone(),
                        ..Dependency::default()
                    });
                }
            }
        }

        // A record decides whether its issue is listed; others keep their place.
        let touched_ids: HashSet<&str> = touched.iter().map(String::as_str).collect();
        let mut seen = HashSet::new();
        let listed: Vec<Issue> = self
            .order
            .iter()
            .chain(&touched)
            .filter(|id| seen.insert(id.as_str()))
            .filter_map(|id| issues.get(id))
            .filter(|issue| {
                !touched_ids.contains(issue.id.as_str()) || is_listed_status(&issue.status)
            })
            .cloned()
            .collect();
        let listed_ids: HashSet<&str> = listed.iter().map(|issue| issue.id.as_str()).collect();
        let context: Vec<Issue> = issues
            .values()
            .filter(|issue| !listed_ids.contains(issue.id.as_str()))
            .cloned()
            .collect();
        IssueGraph::new(listed, context)
    }
}

/// The `bd` CLI plus the `--db` selection, shared by every operation.
#[derive(Clone, Debug)]
pub struct Bd {
    program: OsString,
    db: Option<PathBuf>,
}

impl Bd {
    pub fn new(program: OsString, db: Option<PathBuf>) -> Self {
        Self { program, db }
    }

    fn name(&self) -> String {
        self.program.to_string_lossy().into_owned()
    }

    /// `--db` goes after `args` so subcommand positionals stay in front.
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(&self.program);
        command.args(args);
        if let Some(path) = &self.db {
            command.arg("--db").arg(path);
        }
        command
    }

    fn spawn_error(&self, error: io::Error) -> io::Error {
        io::Error::new(
            error.kind(),
            format!("could not run {}: {error}", self.name()),
        )
    }

    /// A non-zero exit as an error built from stderr, or stdout when stderr
    /// is empty.
    fn failure(&self, verb: &str, status: ExitStatus, stderr: &str, stdout: &str) -> io::Error {
        let message = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        io::Error::other(if message.is_empty() {
            format!("{} {verb} failed: {status}", self.name())
        } else {
            format!("{} {verb} failed: {message}", self.name())
        })
    }

    /// Runs to completion; a non-zero exit becomes an error.
    fn run(&self, verb: &str, command: &mut Command) -> io::Result<Output> {
        let output = command.output().map_err(|error| self.spawn_error(error))?;
        if output.status.success() {
            return Ok(output);
        }
        Err(self.failure(
            verb,
            output.status,
            &String::from_utf8_lossy(&output.stderr),
            &String::from_utf8_lossy(&output.stdout),
        ))
    }

    fn run_json(&self, args: &[&str]) -> io::Result<Value> {
        let verb = args.first().copied().unwrap_or("command");
        let output = self.run(verb, &mut self.command(args))?;
        serde_json::from_slice(&output.stdout).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} returned invalid JSON for {verb}: {error}", self.name()),
            )
        })
    }

    pub fn load(&self) -> io::Result<IssueGraph> {
        // One `bd list --all` call: per-ID `bd show` resolution costs ~250ms per
        // issue, so hydrating dependency targets from the same payload instead
        // keeps startup at a single fast query.
        let issues = parse_issue_collection(self.run_json(&["list", "--all", "--json"])?)?;
        let (listed, context): (Vec<Issue>, Vec<Issue>) = issues
            .into_iter()
            .partition(|issue| is_listed_status(&issue.status));
        if listed.is_empty() {
            return Ok(IssueGraph::default());
        }
        Ok(IssueGraph::new(listed, context))
    }

    pub fn load_issue(&self, issue_id: &str) -> io::Result<Issue> {
        parse_issue_collection(self.run_json(&["show", issue_id, "--json"])?)?
            .into_iter()
            .find(|issue| issue.id == issue_id)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("{} show returned no issue {issue_id}", self.name()),
                )
            })
    }

    /// `bd edit` opens `$EDITOR` itself, so the command inherits the terminal
    /// instead of having its output captured.
    pub fn edit(&self, field: EditField, issue_id: &str) -> io::Result<()> {
        let flag = match field {
            EditField::Title => "--title",
            EditField::Description => "--description",
        };
        let status = self
            .command(&["edit", issue_id, flag])
            .status()
            .map_err(|error| self.spawn_error(error))?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "{} edit failed: {status}",
                self.name()
            )))
        }
    }

    /// Reads the whole retained journal for its last `seq`, the position to
    /// follow from. A pruned journal refuses `--since 0` but reports its head.
    pub fn journal_head(&self) -> io::Result<JournalHead> {
        let mut child = self
            .command(&["events", "tail", "--since", "0", "--json"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| self.spawn_error(error))?;
        // A record is one line but a refusal is pretty-printed, so keep the
        // lines of the last JSON value only; the journal can hold 100k records.
        let mut last = String::new();
        if let Some(stdout) = child.stdout.take() {
            for line in BufReader::new(stdout).lines() {
                let line = line?;
                if line.starts_with('{') {
                    last.clear();
                }
                last.push_str(&line);
            }
        }
        let mut stderr = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            pipe.read_to_string(&mut stderr)?;
        }
        let status = child.wait()?;
        // `bd` serves an off journal's old records and says so only on stderr.
        if stderr.contains("events journal is disabled") {
            return Ok(JournalHead::Disabled);
        }

        #[derive(Deserialize)]
        struct Position {
            seq: Option<i64>,
            head: Option<i64>,
        }
        let position = serde_json::from_str::<Position>(&last).ok();
        let seq = if status.success() {
            position.and_then(|position| position.seq)
        } else {
            position.and_then(|position| position.head)
        };
        match seq {
            Some(seq) => Ok(JournalHead::At(seq)),
            None if status.success() && last.is_empty() => Ok(JournalHead::At(0)),
            None if status.success() => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} returned an invalid events record", self.name()),
            )),
            None => Err(self.failure("events", status, &stderr, &last)),
        }
    }

    /// Starts `bd events tail --follow` after `since`. The caller reads one
    /// JSON record per stdout line and kills the child when done.
    pub fn follow_journal(&self, since: i64) -> io::Result<Child> {
        self.command(&[
            "events",
            "tail",
            "--since",
            &since.to_string(),
            "--follow",
            "--json",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| self.spawn_error(error))
    }

    pub fn close_issue(&self, issue_id: &str) -> io::Result<()> {
        self.run("close", &mut self.command(&["close", issue_id]))
            .map(|_| ())
    }

    pub fn set_status(&self, issue_id: &str, status: &str) -> io::Result<()> {
        self.update(issue_id, "--status", status)
    }

    pub fn set_priority(&self, issue_id: &str, priority: i32) -> io::Result<()> {
        self.update(issue_id, "--priority", &format!("P{priority}"))
    }

    fn update(&self, issue_id: &str, flag: &str, value: &str) -> io::Result<()> {
        self.run(
            "update",
            &mut self.command(&["update", issue_id, flag, value]),
        )
        .map(|_| ())
    }

    pub fn create_issue(&self, draft: &AddIssueDraft) -> io::Result<String> {
        let priority = format!("P{}", draft.priority);
        let mut args = vec![
            "create",
            draft.title.as_str(),
            "--description",
            draft.description.as_str(),
            "--type",
            draft.issue_type.as_str(),
            "--priority",
            priority.as_str(),
        ];
        if let Some(parent_id) = &draft.parent_id {
            args.push("--parent");
            args.push(parent_id);
        }
        args.push("--silent");
        let output = self.run("create", &mut self.command(&args))?;
        let issue_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if issue_id.is_empty() {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} create returned no issue ID", self.name()),
            ))
        } else {
            Ok(issue_id)
        }
    }
}

fn parse_issue_collection(value: Value) -> io::Result<Vec<Issue>> {
    let issues = match value {
        Value::Array(values) => values,
        Value::Object(mut object) if object.contains_key("issues") => object
            .remove("issues")
            .and_then(|value| value.as_array().cloned())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "`issues` is not an array")
            })?,
        Value::Object(object) if object.contains_key("id") => vec![Value::Object(object)],
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unexpected issue JSON shape from bd",
            ))
        }
    };

    issues
        .into_iter()
        .map(|value| {
            serde_json::from_value(value).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("could not decode issue JSON: {error}"),
                )
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use crate::test_support::FakeBd;

    fn issue(id: &str, deps: &[&str]) -> Issue {
        Issue {
            id: id.to_string(),
            title: id.to_string(),
            dependencies: deps
                .iter()
                .map(|id| Dependency {
                    id: (*id).to_string(),
                    ..Dependency::default()
                })
                .collect(),
            ..Issue::default()
        }
    }

    #[test]
    fn roots_are_issues_not_targeted_by_another_issue() {
        let graph = IssueGraph::new(
            vec![issue("a", &["b"]), issue("b", &[]), issue("c", &[])],
            vec![],
        );
        assert_eq!(graph.roots(), &["a", "c"]);
    }

    #[test]
    fn parent_child_edges_are_reversed_for_the_tree() {
        let mut child = issue("hmb2", &["8gda"]);
        child.dependencies[0].dependency_type = "parent-child".to_string();
        let graph = IssueGraph::new(vec![child, issue("8gda", &[])], vec![]);

        assert_eq!(graph.roots(), &["8gda"]);
        assert_eq!(graph.tree_children("8gda"), &["hmb2"]);
        assert!(graph.tree_children("hmb2").is_empty());
    }

    #[test]
    fn cycles_remain_visible() {
        let graph = IssueGraph::new(vec![issue("a", &["b"]), issue("b", &["a"])], vec![]);
        assert_eq!(graph.roots(), &["a", "b"]);
    }

    #[test]
    fn context_issues_hydrate_dependencies_without_joining_the_tree() {
        let mut closed = issue("z", &["a"]);
        closed.status = "closed".to_string();
        let graph = IssueGraph::new(vec![issue("a", &["z"])], vec![closed]);

        assert_eq!(graph.issue("z").unwrap().title, "z");
        assert_eq!(graph.issue("z").unwrap().status, "closed");
        assert!(!graph.is_listed("z"));
        assert_eq!(graph.len(), 1);
        assert_eq!(graph.roots(), &["a"]);
    }

    #[test]
    fn with_issue_closed_moves_the_issue_into_context() {
        let graph = IssueGraph::new(vec![issue("a", &["b"]), issue("b", &[])], vec![]);

        let closed = graph.with_issue_closed("a");

        assert_eq!(closed.roots(), &["b"]);
        assert!(!closed.is_listed("a"));
        assert!(closed.is_listed("b"));
        assert_eq!(closed.issue("a").unwrap().status, "closed");
    }

    fn snapshot(id: &str, status: &str) -> Issue {
        Issue {
            id: id.to_string(),
            title: id.to_string(),
            status: status.to_string(),
            ..Issue::default()
        }
    }

    fn record(op: &str, issue_id: &str, issue: Option<Issue>) -> JournalRecord {
        JournalRecord {
            op: op.to_string(),
            issue_id: issue_id.to_string(),
            issue,
            dep: None,
        }
    }

    fn dep_record(op: &str, issue_id: &str, kind: &str, target: &str) -> JournalRecord {
        JournalRecord {
            dep: Some(JournalDep {
                kind: kind.to_string(),
                target: target.to_string(),
            }),
            ..record(op, issue_id, None)
        }
    }

    #[test]
    fn records_move_issues_in_and_out_of_the_tree_in_order() {
        let graph = IssueGraph::new(
            vec![snapshot("a", "open"), snapshot("b", "open")],
            vec![snapshot("z", "closed")],
        );

        let next = graph.with_records(&[
            record("create", "c", Some(snapshot("c", "open"))),
            record("close", "a", Some(snapshot("a", "closed"))),
            record("update", "z", Some(snapshot("z", "open"))),
            record("delete", "b", None),
        ]);

        assert_eq!(next.roots(), &["c", "z"]);
        assert!(!next.is_listed("a"));
        assert_eq!(next.issue("a").unwrap().status, "closed");
        assert!(next.issue("b").is_none());
    }

    #[test]
    fn records_keep_known_edges_and_apply_dependency_changes() {
        let mut child = snapshot("c", "open");
        child.dependencies.push(Dependency {
            id: "p".to_string(),
            dependency_type: "parent-child".to_string(),
            ..Dependency::default()
        });
        let graph = IssueGraph::new(vec![snapshot("p", "open"), child], vec![]);

        let renamed = Issue {
            title: "renamed".to_string(),
            ..snapshot("c", "open")
        };
        let updated = graph.with_records(&[record("update", "c", Some(renamed))]);
        assert_eq!(updated.issue("c").unwrap().title, "renamed");
        assert_eq!(
            updated.tree_children("p"),
            &["c"],
            "snapshots carry no edges"
        );

        let unparented =
            updated.with_records(&[dep_record("dep_remove", "c", "parent-child", "p")]);
        assert!(unparented.tree_children("p").is_empty());
        assert_eq!(unparented.roots(), &["p", "c"]);

        let reparented = unparented.with_records(&[
            dep_record("dep_add", "c", "parent-child", "p"),
            dep_record("dep_add", "c", "parent-child", "p"),
        ]);
        assert_eq!(reparented.tree_children("p"), &["c"]);
        assert_eq!(
            reparented.issue("c").unwrap().dependencies.len(),
            1,
            "dep_add is an upsert"
        );
    }

    #[test]
    fn parses_journal_lines_from_bd_events_tail() {
        let record: JournalRecord = serde_json::from_str(
            r#"{"seq":3,"ts":"2026-10-15T06:42:59Z","op":"dep_add","issue_id":"t-1","actor":"me","issue":{"id":"t-1","title":"Child","status":"open","priority":2,"labels":["x"],"metadata":{"k":"v"}},"dep":{"kind":"parent-child","target":"t-0","metadata":"{}"}}"#,
        )
        .unwrap();
        assert_eq!(record.issue.unwrap().title, "Child");
        assert_eq!(record.dep.unwrap().target, "t-0");

        let deleted: JournalRecord =
            serde_json::from_str(r#"{"seq":4,"op":"delete","issue_id":"t-1","issue":null}"#)
                .unwrap();
        assert!(deleted.issue.is_none());

        let refusal = r#"{"code":"events_journal_truncated","floor":41,"head":980,"since":12}"#;
        assert!(serde_json::from_str::<JournalRecord>(refusal).is_err());
    }

    #[test]
    fn replacing_issue_refreshes_its_description() {
        let mut graph = IssueGraph::new(vec![issue("a", &[])], vec![]);
        let mut refreshed = issue("a", &[]);
        refreshed.description = "Edited description".to_string();

        graph.replace_issue(refreshed);

        assert_eq!(graph.issue("a").unwrap().description, "Edited description");
        assert!(graph.is_listed("a"));
    }

    #[test]
    fn parses_list_and_show_shapes() {
        let wrapped = serde_json::json!({"issues": [{"id": "a", "title": "A"}]});
        let array = serde_json::json!([{"id": "b", "title": "B"}]);
        assert_eq!(parse_issue_collection(wrapped).unwrap()[0].id, "a");
        assert_eq!(parse_issue_collection(array).unwrap()[0].id, "b");
    }

    #[test]
    fn parses_hydrated_and_edge_dependency_shapes() {
        let value = serde_json::json!([{
            "id": "a",
            "dependencies": [
                {"id": "b", "title": "B", "status": "open", "priority": 1, "dependency_type": "blocks"},
                {"issue_id": "a", "depends_on_id": "c", "type": "parent-child", "metadata": "{}"},
            ],
        }]);
        let deps = parse_issue_collection(value).unwrap()[0]
            .dependencies
            .clone();
        assert_eq!(deps[0].id, "b");
        assert_eq!(deps[0].dependency_type, "blocks");
        assert_eq!(deps[1].id, "c");
        assert_eq!(deps[1].dependency_type, "parent-child");
    }

    #[cfg(unix)]
    #[test]
    fn load_lists_open_and_in_progress_issues_by_default() {
        let fake = FakeBd::new(
            "load",
            r#"[{"id":"open","status":"open"},{"id":"working","status":"in_progress"},{"id":"closed","status":"closed"}]"#,
        );

        let graph = fake.bd(None).load().unwrap();

        assert!(graph.is_listed("open"));
        assert!(graph.is_listed("working"));
        assert!(!graph.is_listed("closed"));
        assert_eq!(graph.len(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn create_issue_passes_parent_type_and_p1_to_bd() {
        let fake = FakeBd::new("create", "child-1");

        let issue_id = fake
            .bd(Some(PathBuf::from("/tmp/example.db")))
            .create_issue(&AddIssueDraft {
                parent_id: Some("parent-1".into()),
                title: "Child title".into(),
                description: "Body text".into(),
                issue_type: "feature".into(),
                priority: 1,
            })
            .unwrap();

        assert_eq!(issue_id, "child-1");
        assert_eq!(
            fake.arguments(),
            [
                "create",
                "Child title",
                "--description",
                "Body text",
                "--type",
                "feature",
                "--priority",
                "P1",
                "--parent",
                "parent-1",
                "--silent",
                "--db",
                "/tmp/example.db",
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn create_issue_without_parent_omits_the_parent_flag() {
        let fake = FakeBd::new("root", "root-1");

        let issue_id = fake
            .bd(None)
            .create_issue(&AddIssueDraft {
                parent_id: None,
                title: "Top level".into(),
                description: String::new(),
                issue_type: "task".into(),
                priority: 1,
            })
            .unwrap();

        assert_eq!(issue_id, "root-1");
        let recorded = fake.arguments();
        assert!(!recorded.iter().any(|argument| argument == "--parent"));
        assert!(recorded.iter().any(|argument| argument == "--silent"));
    }

    #[cfg(unix)]
    #[test]
    fn set_status_and_priority_run_bd_update() {
        let fake = FakeBd::new("update", "");

        let bd = fake.bd(None);
        bd.set_status("task-1", "in_progress").unwrap();
        bd.set_priority("task-1", 2).unwrap();

        assert_eq!(
            fake.arguments(),
            [
                "update",
                "task-1",
                "--status",
                "in_progress",
                "update",
                "task-1",
                "--priority",
                "P2",
            ]
        );
    }

    #[cfg(unix)]
    fn journal_head_from(body: &str) -> io::Result<JournalHead> {
        FakeBd::script("head", body).bd(None).journal_head()
    }

    #[cfg(unix)]
    #[test]
    fn journal_head_is_the_seq_of_the_last_record() {
        let head = journal_head_from(
            r#"printf '%s\n' '{"seq":6,"op":"create","issue_id":"a","issue":{"id":"a"}}' '{"seq":7,"op":"delete","issue_id":"a","issue":null}'"#,
        );
        assert_eq!(head.unwrap(), JournalHead::At(7));
        assert_eq!(journal_head_from(":").unwrap(), JournalHead::At(0));
    }

    #[cfg(unix)]
    #[test]
    fn journal_head_reports_a_disabled_journal() {
        let head = journal_head_from(
            r#"echo "note: the events journal is disabled for this workspace (enable with ...)" >&2"#,
        );
        assert_eq!(head.unwrap(), JournalHead::Disabled);
    }

    #[cfg(unix)]
    #[test]
    fn journal_head_reads_a_pruned_journal_from_its_refusal() {
        let head = journal_head_from(
            r#"printf '{\n  "code": "events_journal_truncated",\n  "floor": 41,\n  "head": 980,\n  "since": 0\n}\n'; exit 1"#,
        );
        assert_eq!(head.unwrap(), JournalHead::At(980));
    }

    #[cfg(unix)]
    #[test]
    fn journal_head_fails_on_a_bd_without_events() {
        let error =
            journal_head_from(r#"echo 'Error: unknown command "events" for "bd"' >&2; exit 1"#)
                .unwrap_err();
        assert!(error.to_string().contains("unknown command"), "{error}");
    }
}
