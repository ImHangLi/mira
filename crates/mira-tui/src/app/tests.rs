use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use mira_protocol::clock::LocalClock;
use mira_protocol::ids::{ActionRef, Digest, RunId, ViewRef};
use mira_protocol::manifest::{ActionMode, JsonObject, ShowPolicy, ViewKind};
use mira_protocol::run::{Lifecycle, RunSummary};
use mira_protocol::time::Timestamp;

use super::close::close_label;
use super::*;

fn app() -> App {
    let (control, _) = tokio::sync::mpsc::unbounded_channel();
    let (read, _) = tokio::sync::mpsc::unbounded_channel();
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let root = mira_protocol::ids::AbsolutePath::parse("/tmp/mira-tui-test".into()).unwrap();
    let paths = mira_protocol::paths::WorkspacePaths::new(root);
    App::new(
        "/tmp/mira-tui-test".into(),
        LocalClock::UTC,
        Io {
            control,
            read,
            events,
            paths,
        },
    )
}

fn key(a: &mut App, code: KeyCode) {
    a.key(KeyEvent::new(code, KeyModifiers::NONE));
}

fn add_view(a: &mut App, r: &str, kind: ViewKind) {
    let view_ref: ViewRef = r.parse().unwrap();
    let plugin = view_ref.plugin.to_string();
    if !a.plugin_order.contains(&plugin) {
        a.plugin_order.push(plugin);
    }
    a.views.push(ViewItem {
        view_ref,
        title: r.into(),
        description: String::new(),
        tags: Vec::new(),
        kind,
    });
    a.refilter(None);
}

fn add_action(a: &mut App, r: &str, title: &str, tags: &[&str], description: &str) {
    let action_ref: ActionRef = r.parse().unwrap();
    let plugin = action_ref.plugin.to_string();
    if !a.plugin_order.contains(&plugin) {
        a.plugin_order.push(plugin);
    }
    a.items.push(Item {
        action_ref,
        title: title.into(),
        description: description.into(),
        tags: tags.iter().map(|t| (*t).to_owned()).collect(),
        mode: ActionMode::Task,
        show: ShowPolicy::OnRun,
        enabled: true,
        definition_hash: Digest::of_bytes(r.as_bytes()),
    });
    a.refilter(None);
}

fn search(a: &mut App, text: &str) -> Vec<String> {
    key(a, KeyCode::Char('/'));
    for c in text.chars() {
        key(a, KeyCode::Char(c));
    }
    key(a, KeyCode::Enter);
    a.visible
        .iter()
        .filter_map(|&e| a.entry_key(e).map(|k| k.to_string()))
        .collect()
}

#[test]
fn filter_keeps_matches_grouped_under_one_plugin_heading() {
    let mut a = app();
    add_action(&mut a, "dev.check", "Check", &[], "Run every check.");
    add_action(&mut a, "dev.env", "Environment check", &[], "Show env.");
    add_action(&mut a, "files.check", "Check files", &[], "Check files.");
    add_action(&mut a, "dev.fail", "Failing check", &[], "Fails.");
    let found = search(&mut a, "check");
    assert_eq!(found[0], "dev.check", "the best match stays first");
    let plugins: Vec<&str> = found.iter().map(|r| r.split('.').next().unwrap()).collect();
    let mut runs = plugins.clone();
    runs.dedup();
    let mut unique = runs.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(
        runs.len(),
        unique.len(),
        "each plugin appears in one block: {found:?}"
    );
}

#[test]
fn filter_ranks_the_best_match_first() {
    let mut a = app();
    add_action(
        &mut a,
        "dev.typecheck",
        "Typecheck",
        &[],
        "Run the type checker.",
    );
    add_action(&mut a, "dev.check", "Check", &[], "Run every check.");
    add_action(&mut a, "dev.lint", "Lint", &[], "Lint the code.");
    add_action(
        &mut a,
        "style.fix",
        "Fix style",
        &["lint"],
        "Formats files.",
    );
    add_action(&mut a, "dev.test", "Test", &[], "Run the tests.");
    assert_eq!(search(&mut a, "lint"), ["dev.lint", "style.fix"]);
    assert_eq!(a.selected_ref().unwrap().to_string(), "dev.lint");
    assert_eq!(search(&mut a, "check"), ["dev.check", "dev.typecheck"]);
    assert_eq!(a.selected_ref().unwrap().to_string(), "dev.check");
    // Any word matches; more matched words rank higher within a tier.
    assert_eq!(
        search(&mut a, "test tests"),
        ["dev.test"],
        "one entry matches both words"
    );
    assert_eq!(search(&mut a, "type every"), ["dev.typecheck", "dev.check"]);
    // Ties keep the list order.
    assert_eq!(
        search(&mut a, "run"),
        ["dev.typecheck", "dev.check", "dev.test"]
    );
}

fn run(a: &str, id: &RunId, lifecycle: Lifecycle) -> RunSummary {
    RunSummary {
        requester: None,
        run_id: id.clone(),
        action_ref: Some(a.parse().unwrap()),
        lifecycle,
        reported_health: mira_protocol::run::ReportedHealth::unknown(),
        definition_hash: Digest::of_bytes(a.as_bytes()),
        started_at: Timestamp::now(),
    }
}

#[test]
fn run_notices_go_when_the_run_moves_on() {
    let mut a = app();
    add_action(&mut a, "dev.web", "Web", &[], "");
    add_action(&mut a, "dev.lint", "Lint", &[], "");
    let web: ActionRef = "dev.web".parse().unwrap();
    let id: RunId = "r_0000000000004000800000000000000a".parse().unwrap();
    a.apply_runs(None, vec![run("dev.web", &id, Lifecycle::Starting)]);
    a.info_run(&web, &id, "started dev.web");
    // Starting to running keeps the notice.
    a.apply_runs(None, vec![run("dev.web", &id, Lifecycle::Running)]);
    assert!(a.notice.is_some());
    // Another selected item drops it.
    key(&mut a, KeyCode::Char('j'));
    assert!(a.notice.is_none());
    key(&mut a, KeyCode::Char('k'));
    let stopping = Lifecycle::Stopping {
        reason: mira_protocol::run::StopReason::User,
    };
    a.apply_runs(None, vec![run("dev.web", &id, stopping)]);
    a.info_run(&web, &id, "stopping dev.web");
    assert!(a.notice.is_some());
    // The run ended.
    a.apply_runs(None, vec![]);
    assert!(a.notice.is_none());
    // Errors stay.
    a.error("dev.web did not stop");
    a.apply_runs(None, vec![]);
    key(&mut a, KeyCode::Char('j'));
    assert!(a.notice.is_some());
}

#[test]
fn a_task_form_runs_again_and_a_process_form_restarts() {
    let mut a = app();
    add_action(&mut a, "dev.test", "Test", &[], "");
    let r: ActionRef = "dev.test".parse().unwrap();
    let schema: JsonObject = serde_json::from_value(serde_json::json!({
        "type": "object",
        "properties": {"only": {"type": "string"}}
    }))
    .unwrap();
    let hash = a.items[0].definition_hash.clone();
    a.inputs.insert(r.clone(), (hash, Inputs::Form(schema)));
    a.last.insert(
        r,
        LastRun {
            run_id: "r_0000000000004000800000000000000a".parse().unwrap(),
            lifecycle: Lifecycle::Finished {
                outcome: mira_protocol::run::Outcome::Succeeded,
            },
            exit: None,
            started_at: None,
            ended_at: None,
            result: None,
            cleanup: None,
        },
    );
    key(&mut a, KeyCode::Char('r'));
    let enter = |a: &App| {
        a.bindings()
            .into_iter()
            .find(|b| b.keys == "Enter")
            .map(|b| b.label)
    };
    assert_eq!(enter(&a).as_deref(), Some("run"));
    a.items[0].mode = ActionMode::Process;
    assert_eq!(enter(&a).as_deref(), Some("restart"));
}

#[test]
fn closing_messages_name_the_runs_in_plain_words() {
    assert_eq!(close_message(&Close::Last, 2), "Stopping 2 runs.");
    assert_eq!(close_label(&Close::Last, 2), "quit (stops 2 runs)");
    assert_eq!(close_label(&Close::Last, 0), "quit");
    assert_eq!(
        close_message(&Close::Kept(None), 2),
        "2 runs keep running. `mira down` stops them."
    );
    assert_eq!(
        close_label(&Close::Kept(Some("16:07".into())), 2),
        "quit (keeps running until 16:07)"
    );
    assert_eq!(
        close_message(&Close::Kept(Some("16:07".into())), 1),
        "1 run keeps running until 16:07. `mira down` stops it."
    );
    assert_eq!(
        close_message(&Close::OtherWindows(1), 2),
        "Another Mira window is open. 2 runs keep running."
    );
    assert_eq!(close_message(&Close::NoSession, 0), "Nothing was running.");
}

fn exec_run(at_ms: i64, lifecycle: Lifecycle) -> RunSummary {
    RunSummary {
        requester: None,
        run_id: RunId::random(),
        action_ref: None,
        lifecycle,
        reported_health: mira_protocol::run::ReportedHealth::unknown(),
        definition_hash: Digest::of_bytes(b"exec"),
        started_at: Timestamp::from_unix_ms(at_ms),
    }
}

#[test]
fn one_off_runs_keep_the_newest_five_and_every_running_one() {
    let done = Lifecycle::Finished {
        outcome: mira_protocol::run::Outcome::Succeeded,
    };
    let mut list: Vec<OneOff> = (0..12)
        .map(|n| {
            let r = exec_run(1_000 + n, done);
            OneOff::new(r.run_id, r.lifecycle, r.started_at)
        })
        .collect();
    let old = exec_run(10, Lifecycle::Running);
    list.push(OneOff::new(
        old.run_id.clone(),
        old.lifecycle,
        old.started_at,
    ));
    let hidden = keep_oneoffs(&mut list);
    assert_eq!(hidden.get("earlier"), Some(&7));
    let starts: Vec<i64> = list.iter().map(|o| o.started_at.unix_ms()).collect();
    let mut want: Vec<i64> = (7..12).rev().map(|n| 1_000 + n).collect();
    want.push(10);
    assert_eq!(starts, want, "newest first; the old running run stays");
    assert_eq!(list.last().map(|o| &o.run_id), Some(&old.run_id));
}

#[test]
fn one_off_runs_list_first_and_keep_the_selection() {
    let mut a = app();
    add_action(&mut a, "dev.check", "Check", &[], "");
    let first = exec_run(1_000, Lifecycle::Running);
    a.apply_runs(None, vec![first.clone()]);
    assert_eq!(a.visible, [Entry::OneOff(0), Entry::Action(0)]);
    // Select the action; a newer run lists above the older one and keeps the choice.
    key(&mut a, KeyCode::Char('j'));
    assert_eq!(a.selected_ref().unwrap().to_string(), "dev.check");
    let second = exec_run(2_000, Lifecycle::Running);
    a.apply_runs(None, vec![first.clone(), second.clone()]);
    assert_eq!(
        a.visible,
        [Entry::OneOff(0), Entry::OneOff(1), Entry::Action(0)]
    );
    assert_eq!(a.oneoffs[0].run_id, second.run_id);
    assert_eq!(a.selected_ref().unwrap().to_string(), "dev.check");
    assert_eq!(a.adhoc_runs, 2);
    // A one-off run shows its keys; `s` stops it while it runs.
    key(&mut a, KeyCode::Char('k'));
    assert_eq!(a.selected_oneoff().map(|o| &o.run_id), Some(&first.run_id));
    let has = |a: &App, k: &str, l: &str| {
        a.bindings()
            .iter()
            .any(|b| b.footer && b.keys == k && b.label == l)
    };
    assert!(has(&a, "s", "stop"));
    assert!(!a.bindings().iter().any(|b| b.cmd == Cmd::Restart));
    key(&mut a, KeyCode::Char('s'));
    assert!(a.oneoffs[1].stopping);
    assert!(!has(&a, "s", "stop"), "one stop at a time");
    // A filter lists matching tools before one-off runs.
    a.oneoffs[0].label = Some("check again".into());
    assert_eq!(
        search(&mut a, "check"),
        ["dev.check".to_owned(), second.run_id.to_string()]
    );
}

#[test]
fn esc_leaves_the_view_focus() {
    let mut a = app();
    add_view(&mut a, "dev.grid", ViewKind::Table);
    key(&mut a, KeyCode::Enter);
    assert!(a.focus == Focus::Logs);
    // Esc still goes back; the footer shows the one key that works everywhere instead.
    assert!(
        a.bindings()
            .iter()
            .any(|b| b.keys == "Esc" && b.label == "back")
    );
    assert!(
        a.bindings()
            .iter()
            .any(|b| b.footer && b.keys == "Ctrl-T" && b.label == "back to tools")
    );
    key(&mut a, KeyCode::Esc);
    assert!(a.focus == Focus::List);
}
