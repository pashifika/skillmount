//! Native integration coverage for the feature-gated process-supervision fixtures.

#![cfg(feature = "test-fixtures")]

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const FAKE_AGENT: &str = env!("CARGO_BIN_EXE_skillmount-fake-agent");
const SUPERVISOR_HARNESS: &str = env!("CARGO_BIN_EXE_skillmount-supervisor-harness");
const TIMEOUT: Duration = Duration::from_secs(10);

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        let nonce = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "skillmount-process-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create process-supervision test directory");
        Self(path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Debug, PartialEq, Eq)]
struct FakeRecord {
    pid: Option<u32>,
    arguments: Vec<Vec<u8>>,
    cwd: Vec<u8>,
    stdin: Option<Vec<u8>>,
    events: Vec<String>,
}

#[test]
fn direct_launch_preserves_executable_cwd_and_every_argument_layer() {
    let root = TestDir::new("argv");
    let fake = copy_fake_agent(&root.0);
    let cwd = root.0.join("launch cwd 日本語");
    fs::create_dir(&cwd).expect("create launch cwd");
    let record = root.0.join("record.txt");
    let cleanup = root.0.join("cleanup.txt");
    let outcome = root.0.join("outcome.txt");
    let shell_sentinel = root.0.join("shell-must-not-run");

    let injected = [
        OsString::from("--add-dir"),
        cwd.join("injected path").into_os_string(),
    ];
    let mut passthrough = vec![
        OsString::from("space value"),
        OsString::from("\"quoted value\""),
        OsString::from(r"C:\skills\path with spaces\"),
        OsString::from("日本語"),
        OsString::from(format!(
            "$(touch {}); echo should-not-run",
            shell_sentinel.display()
        )),
    ];
    passthrough.push(non_unicode_argument());
    let arguments: Vec<_> = injected.iter().chain(&passthrough).cloned().collect();

    let output = base_harness(&fake, &cwd, injected.len(), &arguments)
        .env("SKILLMOUNT_FAKE_RECORD", &record)
        .env("SKILLMOUNT_FAKE_BEHAVIOR", "exit")
        .env("SKILLMOUNT_FAKE_EXIT", "2")
        .env("SKILLMOUNT_HARNESS_CLEANUP_COUNTER", &cleanup)
        .env("SKILLMOUNT_HARNESS_OUTCOME", &outcome)
        .output()
        .expect("run supervisor harness");

    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert_eq!(cleanup_count(&cleanup), 1);
    assert!(
        !shell_sentinel.exists(),
        "an argument was interpreted by a shell"
    );
    let recorded = read_record(&record);
    assert_eq!(
        recorded.arguments,
        arguments
            .iter()
            .map(|value| os_bytes(value))
            .collect::<Vec<_>>()
    );
    let recorded_cwd = PathBuf::from(os_from_bytes(&recorded.cwd));
    assert_eq!(
        fs::canonicalize(recorded_cwd).expect("canonicalize recorded launch cwd"),
        fs::canonicalize(&cwd).expect("canonicalize expected launch cwd")
    );
    assert_eq!(recorded.events, ["ready"]);
    assert!(
        fs::read_to_string(outcome)
            .expect("read outcome")
            .contains("Exited(\n            2"),
        "the typed child status was not retained"
    );
}

#[test]
fn inherited_streams_reach_the_fake_child_without_supervisor_capture() {
    let root = TestDir::new("streams");
    let record = root.0.join("record.txt");
    let cleanup = root.0.join("cleanup.txt");
    let input = b"stdin\0payload\n";
    let mut child = base_harness(Path::new(FAKE_AGENT), &root.0, 0, &[])
        .env("SKILLMOUNT_FAKE_RECORD", &record)
        .env("SKILLMOUNT_FAKE_BEHAVIOR", "streams")
        .env("SKILLMOUNT_HARNESS_CLEANUP_COUNTER", &cleanup)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn stream harness");
    std::io::Write::write_all(child.stdin.as_mut().expect("piped harness stdin"), input)
        .expect("write harness stdin");
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("wait for stream harness");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, [b"fake-stdout:".as_slice(), input].concat());
    assert_eq!(output.stderr, [b"fake-stderr:".as_slice(), input].concat());
    assert_eq!(read_record(&record).stdin, Some(input.to_vec()));
    assert_eq!(cleanup_count(&cleanup), 1);
}

#[cfg(target_os = "macos")]
#[test]
fn interactive_child_keeps_foreground_tty_read_access() {
    let root = TestDir::new("foreground-tty");
    let record = root.0.join("record.txt");
    let cleanup = root.0.join("cleanup.txt");
    let input = b"interactive input\n";
    let mut child = Command::new("/usr/bin/script")
        .args(["-q", "/dev/null", SUPERVISOR_HARNESS])
        .env("SKILLMOUNT_HARNESS_EXECUTABLE", FAKE_AGENT)
        .env("SKILLMOUNT_HARNESS_CWD", &root.0)
        .env("SKILLMOUNT_HARNESS_INJECTED_COUNT", "0")
        .env("SKILLMOUNT_HARNESS_CLEANUP_COUNTER", &cleanup)
        .env("SKILLMOUNT_FAKE_RECORD", &record)
        .env("SKILLMOUNT_FAKE_BEHAVIOR", "streams")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn harness under a pseudo-terminal");
    wait_for_event_count(&record, 1);
    std::io::Write::write_all(child.stdin.as_mut().expect("piped script stdin"), input)
        .expect("write pseudo-terminal input");

    let status = wait_for_exit(&mut child);

    assert!(status.success(), "{status:?}");
    assert_eq!(read_record(&record).stdin, Some(input.to_vec()));
    assert_eq!(cleanup_count(&cleanup), 1);
}

#[cfg(target_os = "macos")]
#[test]
fn one_terminal_ctrl_c_reaches_an_interactive_child_once() {
    let root = TestDir::new("foreground-interrupt");
    let record = root.0.join("record.txt");
    let cleanup = root.0.join("cleanup.txt");
    let outcome = root.0.join("outcome.txt");
    let mut child = Command::new("/usr/bin/script")
        .args(["-q", "/dev/null", SUPERVISOR_HARNESS])
        .env("SKILLMOUNT_HARNESS_EXECUTABLE", FAKE_AGENT)
        .env("SKILLMOUNT_HARNESS_CWD", &root.0)
        .env("SKILLMOUNT_HARNESS_INJECTED_COUNT", "0")
        .env("SKILLMOUNT_HARNESS_CLEANUP_COUNTER", &cleanup)
        .env("SKILLMOUNT_HARNESS_OUTCOME", &outcome)
        .env("SKILLMOUNT_FAKE_RECORD", &record)
        .env("SKILLMOUNT_FAKE_BEHAVIOR", "ignore-first")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn waiting harness under a pseudo-terminal");
    wait_for_event_count(&record, 1);

    std::io::Write::write_all(child.stdin.as_mut().expect("piped script stdin"), b"\x03")
        .expect("write one terminal interrupt");
    wait_for_event_count(&record, 2);
    thread::sleep(Duration::from_millis(200));

    assert_eq!(signal_event_count(&read_record(&record)), 1);
    assert!(
        child
            .try_wait()
            .expect("poll interactive harness")
            .is_none(),
        "one terminal interrupt must not become the child's force-like second event"
    );

    std::io::Write::write_all(child.stdin.as_mut().expect("piped script stdin"), b"\x03")
        .expect("write teardown interrupt");
    let _status = wait_for_exit(&mut child);

    assert_eq!(cleanup_count(&cleanup), 1);
    let outcome = fs::read_to_string(outcome).expect("read interactive interrupt outcome");
    assert!(outcome.contains("DeliveredByPlatform"), "{outcome}");
}

#[test]
fn spawn_failure_still_runs_cleanup_once_and_maps_to_missing_input() {
    let root = TestDir::new("spawn-failure");
    let cleanup = root.0.join("cleanup.txt");
    let outcome = root.0.join("outcome.txt");
    let missing = root.0.join("missing agent executable");

    let output = base_harness(&missing, &root.0, 0, &[])
        .env("SKILLMOUNT_HARNESS_CLEANUP_COUNTER", &cleanup)
        .env("SKILLMOUNT_HARNESS_OUTCOME", &outcome)
        .output()
        .expect("run missing-executable harness");

    assert_eq!(output.status.code(), Some(66), "{output:?}");
    assert_eq!(cleanup_count(&cleanup), 1);
    let outcome = fs::read_to_string(outcome).expect("read spawn outcome");
    assert!(outcome.contains("stage: Spawn"), "{outcome}");
    assert!(outcome.contains("kind: NotFound"), "{outcome}");
}

#[test]
fn spawn_failure_retains_the_requested_launch_cwd() {
    let root = TestDir::new("missing-cwd");
    let cleanup = root.0.join("cleanup.txt");
    let outcome = root.0.join("outcome.txt");
    let missing_cwd = root.0.join("missing launch cwd");

    let output = base_harness(Path::new(FAKE_AGENT), &missing_cwd, 0, &[])
        .env("SKILLMOUNT_HARNESS_CLEANUP_COUNTER", &cleanup)
        .env("SKILLMOUNT_HARNESS_OUTCOME", &outcome)
        .output()
        .expect("run missing-CWD harness");

    let outcome = fs::read_to_string(&outcome).expect("read missing-CWD outcome");
    assert_eq!(output.status.code(), Some(66), "{output:?}\n{outcome}");
    assert_eq!(cleanup_count(&cleanup), 1);
    assert!(outcome.contains("stage: Spawn"), "{outcome}");
    let expected_cwd = format!("{missing_cwd:?}");
    assert!(outcome.contains(&expected_cwd), "{outcome}");
}

#[test]
fn sequential_supervision_sessions_reuse_the_process_dispatcher() {
    let root = TestDir::new("sequential-sessions");
    let record = root.0.join("record.txt");
    let cleanup = root.0.join("cleanup.txt");

    let output = base_harness(Path::new(FAKE_AGENT), &root.0, 0, &[])
        .env("SKILLMOUNT_FAKE_RECORD", &record)
        .env("SKILLMOUNT_FAKE_BEHAVIOR", "exit")
        .env("SKILLMOUNT_HARNESS_CLEANUP_COUNTER", &cleanup)
        .env("SKILLMOUNT_HARNESS_RUNS", "2")
        .output()
        .expect("run sequential supervision harness");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(cleanup_count(&cleanup), 2);
}

#[test]
fn cleanup_failure_never_overwrites_a_failed_child() {
    let root = TestDir::new("cleanup-precedence");
    for (child_code, expected_code) in [(0, 73), (9, 9)] {
        let record = root.0.join(format!("record-{child_code}.txt"));
        let cleanup = root.0.join(format!("cleanup-{child_code}.txt"));
        let outcome = root.0.join(format!("outcome-{child_code}.txt"));
        let output = base_harness(Path::new(FAKE_AGENT), &root.0, 0, &[])
            .env("SKILLMOUNT_FAKE_RECORD", &record)
            .env("SKILLMOUNT_FAKE_BEHAVIOR", "exit")
            .env("SKILLMOUNT_FAKE_EXIT", child_code.to_string())
            .env("SKILLMOUNT_HARNESS_CLEANUP_COUNTER", &cleanup)
            .env("SKILLMOUNT_HARNESS_CLEANUP_FAIL", "1")
            .env("SKILLMOUNT_HARNESS_OUTCOME", &outcome)
            .output()
            .expect("run cleanup-precedence harness");

        assert_eq!(output.status.code(), Some(expected_code), "{output:?}");
        assert_eq!(cleanup_count(&cleanup), 1);
        let outcome = fs::read_to_string(outcome).expect("read cleanup outcome");
        assert!(outcome.contains("retained-mount"), "{outcome}");
        assert!(outcome.contains("retained-journal"), "{outcome}");
        if child_code == 9 {
            assert!(outcome.contains("secondary: ["), "{outcome}");
        }
    }
}

#[test]
fn interrupt_during_cleanup_returns_to_platform_default_handling() {
    let root = TestDir::new("cleanup-interrupt");
    let record = root.0.join("record.txt");
    let cleanup = root.0.join("cleanup.txt");
    let mut command = base_harness(Path::new(FAKE_AGENT), &root.0, 0, &[]);
    command
        .env("SKILLMOUNT_FAKE_RECORD", &record)
        .env("SKILLMOUNT_FAKE_BEHAVIOR", "exit")
        .env("SKILLMOUNT_HARNESS_CLEANUP_COUNTER", &cleanup)
        .env("SKILLMOUNT_HARNESS_CLEANUP_DELAY_MS", "10000")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    configure_controller_target(&mut command);
    let mut child = command.spawn().expect("spawn cleanup-interrupt harness");
    wait_for_cleanup_count(&cleanup, 1);

    send_interrupt(child.id());
    if wait_for_exit_within(&mut child, Duration::from_secs(2)).is_none() {
        let _ = child.kill();
        let _ = child.wait();
        panic!("the active dispatcher swallowed an interrupt during cleanup");
    }
}

#[test]
fn first_interrupt_reaches_waiting_child_then_cleanup_runs_once() {
    let root = TestDir::new("first-interrupt");
    let record = root.0.join("record.txt");
    let cleanup = root.0.join("cleanup.txt");
    let outcome = root.0.join("outcome.txt");
    let mut command = waiting_harness(&root.0, &record, &cleanup, &outcome, "wait");
    configure_controller_target(&mut command);
    let mut child = command.spawn().expect("spawn waiting harness");
    wait_for_event_count(&record, 1);

    send_interrupt(child.id());
    let status = wait_for_exit(&mut child);

    assert!(status.success(), "{status:?}");
    assert_eq!(cleanup_count(&cleanup), 1);
    assert_eq!(signal_event_count(&read_record(&record)), 1);
    let outcome = fs::read_to_string(outcome).expect("read interrupt outcome");
    assert!(outcome.contains("Graceful"), "{outcome}");
    #[cfg(unix)]
    assert!(outcome.contains("Forwarded"), "{outcome}");
    #[cfg(windows)]
    {
        assert!(outcome.contains("Break"), "{outcome}");
        assert!(outcome.contains("DeliveredByPlatform"), "{outcome}");
    }
}

#[cfg(unix)]
#[test]
fn first_sigterm_reaches_waiting_child_then_cleanup_runs_once() {
    let root = TestDir::new("termination");
    let record = root.0.join("record.txt");
    let cleanup = root.0.join("cleanup.txt");
    let outcome = root.0.join("outcome.txt");
    let mut command = waiting_harness(&root.0, &record, &cleanup, &outcome, "wait");
    configure_controller_target(&mut command);
    let mut child = command.spawn().expect("spawn termination harness");
    wait_for_event_count(&record, 1);

    send_termination(child.id());
    let status = wait_for_exit(&mut child);

    assert!(status.success(), "{status:?}");
    assert_eq!(cleanup_count(&cleanup), 1);
    assert!(
        read_record(&record)
            .events
            .iter()
            .any(|event| event == "signal:15")
    );
    let outcome = fs::read_to_string(outcome).expect("read termination outcome");
    assert!(outcome.contains("Terminate"), "{outcome}");
}

#[test]
fn second_interrupt_forces_the_waiting_child_then_cleanup_runs_once() {
    let root = TestDir::new("second-interrupt");
    let record = root.0.join("record.txt");
    let cleanup = root.0.join("cleanup.txt");
    let outcome = root.0.join("outcome.txt");
    let mut command = waiting_harness(&root.0, &record, &cleanup, &outcome, "ignore-all");
    configure_controller_target(&mut command);
    let mut child = command.spawn().expect("spawn force-path harness");
    wait_for_event_count(&record, 1);

    send_interrupt(child.id());
    wait_for_event_count(&record, 2);
    send_interrupt(child.id());
    let status = wait_for_exit(&mut child);

    let outcome = fs::read_to_string(outcome).expect("read force outcome");
    assert!(
        !status.success(),
        "the force path should not report child success: {status:?}\n{outcome}"
    );
    assert_eq!(cleanup_count(&cleanup), 1);
    assert!(outcome.contains("Forced"), "{outcome}");
    assert!(outcome.contains("Terminated"), "{outcome}");
}

#[cfg(unix)]
#[test]
fn two_handler_occurrences_reach_the_force_path_without_an_intermediate_drain() {
    let root = TestDir::new("queued-interrupts");
    let record = root.0.join("record.txt");
    let cleanup = root.0.join("cleanup.txt");
    let outcome = root.0.join("outcome.txt");
    let mut child = waiting_harness(&root.0, &record, &cleanup, &outcome, "ignore-first")
        .env("SKILLMOUNT_HARNESS_SELF_INTERRUPTS", "2")
        .spawn()
        .expect("spawn queued-interrupt harness");

    if wait_for_exit_within(&mut child, Duration::from_secs(2)).is_none() {
        send_interrupt(child.id());
        let _ = wait_for_exit(&mut child);
        panic!("two completed signal-handler invocations did not reach the force path");
    }

    assert_eq!(cleanup_count(&cleanup), 1);
    let outcome = fs::read_to_string(outcome).expect("read queued-interrupt outcome");
    assert!(outcome.contains("Forced"), "{outcome}");
}

#[test]
fn first_interrupt_reaches_a_child_process_group_descendant() {
    let root = TestDir::new("descendant");
    let record = root.0.join("parent-record.txt");
    let descendant = root.0.join("descendant-record.txt");
    let cleanup = root.0.join("cleanup.txt");
    let outcome = root.0.join("outcome.txt");
    let mut command = waiting_harness(&root.0, &record, &cleanup, &outcome, "descendant-wait");
    command.env("SKILLMOUNT_FAKE_DESCENDANT_RECORD", &descendant);
    configure_controller_target(&mut command);
    let mut child = command.spawn().expect("spawn descendant harness");
    wait_for_event_count(&record, 1);
    wait_for_event_count(&descendant, 1);

    send_interrupt(child.id());
    let status = wait_for_exit(&mut child);
    wait_for_event_count(&descendant, 2);

    assert!(status.success(), "{status:?}");
    assert_eq!(signal_event_count(&read_record(&record)), 1);
    assert_eq!(signal_event_count(&read_record(&descendant)), 1);
    assert_eq!(cleanup_count(&cleanup), 1);
}

#[cfg(unix)]
#[test]
fn reaped_group_leader_with_live_descendant_defers_cleanup_within_bound() {
    let root = TestDir::new("orphan-descendant");
    let record = root.0.join("parent-record.txt");
    let descendant = root.0.join("descendant-record.txt");
    let cleanup = root.0.join("cleanup.txt");
    let outcome = root.0.join("outcome.txt");
    let mut command = waiting_harness(
        &root.0,
        &record,
        &cleanup,
        &outcome,
        "orphan-descendant-ignore-all",
    );
    command.env("SKILLMOUNT_FAKE_DESCENDANT_RECORD", &descendant);
    let mut child = command.spawn().expect("spawn orphan-descendant harness");
    wait_for_event_count(&descendant, 1);
    let descendant_pid = read_record(&descendant)
        .pid
        .expect("fake descendant records its PID");
    let descendant = UnixProcessGuard(descendant_pid);

    let started = Instant::now();
    let status = wait_for_exit(&mut child);

    assert_eq!(status.code(), Some(70), "{status:?}");
    assert!(started.elapsed() < TIMEOUT);
    assert!(!cleanup.exists(), "cleanup ran before process-domain proof");
    assert!(unix_process_is_running(descendant.0));
    let outcome = fs::read_to_string(outcome).expect("read orphan-descendant outcome");
    assert!(outcome.contains("Uncertain"), "{outcome}");
    assert!(outcome.contains("Deferred"), "{outcome}");
    assert!(outcome.contains("ContainmentProbe"), "{outcome}");
    assert!(outcome.contains("TimedOut"), "{outcome}");
}

#[cfg(windows)]
#[test]
fn second_interrupt_terminates_the_windows_job_descendant_before_cleanup() {
    let root = TestDir::new("windows-job-descendant");
    let record = root.0.join("parent-record.txt");
    let descendant = root.0.join("descendant-record.txt");
    let cleanup = root.0.join("cleanup.txt");
    let outcome = root.0.join("outcome.txt");
    let mut command = waiting_harness(
        &root.0,
        &record,
        &cleanup,
        &outcome,
        "descendant-ignore-all",
    );
    command.env("SKILLMOUNT_FAKE_DESCENDANT_RECORD", &descendant);
    configure_controller_target(&mut command);
    let mut child = command.spawn().expect("spawn Windows Job Object harness");
    wait_for_event_count(&record, 1);
    wait_for_event_count(&descendant, 1);
    let descendant_pid = read_record(&descendant)
        .pid
        .expect("fake descendant records its PID");

    send_interrupt(child.id());
    wait_for_event_count(&record, 2);
    wait_for_event_count(&descendant, 2);
    send_interrupt(child.id());
    let status = wait_for_exit(&mut child);

    assert!(!status.success(), "{status:?}");
    assert_eq!(cleanup_count(&cleanup), 1);
    assert!(
        !skillmount::process::test_support::process_is_running(descendant_pid)
            .expect("query descendant liveness"),
        "cleanup ran while descendant {descendant_pid} remained alive"
    );
    let outcome = fs::read_to_string(outcome).expect("read Windows Job Object outcome");
    assert!(outcome.contains("Forced"), "{outcome}");
    assert!(outcome.contains("Terminated"), "{outcome}");
}

#[cfg(windows)]
#[test]
fn windows_batch_launch_is_rejected_before_implicit_shell_execution() {
    let root = TestDir::new("windows-batch");
    let batch = root.0.join("agent.CMD");
    let sentinel = root.0.join("batch-ran");
    let cleanup = root.0.join("cleanup.txt");
    let outcome = root.0.join("outcome.txt");
    fs::write(&batch, "@echo launched>batch-ran\r\n").expect("write batch fixture");

    let output = base_harness(&batch, &root.0, 0, &[])
        .env("SKILLMOUNT_HARNESS_CLEANUP_COUNTER", &cleanup)
        .env("SKILLMOUNT_HARNESS_OUTCOME", &outcome)
        .output()
        .expect("run batch validation harness");

    assert_eq!(output.status.code(), Some(70), "{output:?}");
    assert_eq!(cleanup_count(&cleanup), 1);
    assert!(!sentinel.exists(), "the batch file reached cmd.exe");
    let outcome = fs::read_to_string(outcome).expect("read batch validation outcome");
    assert!(outcome.contains("stage: LaunchValidation"), "{outcome}");
}

#[cfg(windows)]
#[test]
fn exceptional_windows_status_is_retained_and_normalized() {
    let root = TestDir::new("windows-status");
    let record = root.0.join("record.txt");
    let outcome = root.0.join("outcome.txt");
    let cleanup = root.0.join("cleanup.txt");
    let raw_status = 0xc000_013au32;
    let output = base_harness(Path::new(FAKE_AGENT), &root.0, 0, &[])
        .env("SKILLMOUNT_FAKE_RECORD", &record)
        .env("SKILLMOUNT_FAKE_RAW_EXIT", raw_status.to_string())
        .env("SKILLMOUNT_HARNESS_CLEANUP_COUNTER", &cleanup)
        .env("SKILLMOUNT_HARNESS_OUTCOME", &outcome)
        .output()
        .expect("run exceptional-status harness");

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert_eq!(cleanup_count(&cleanup), 1);
    let outcome = fs::read_to_string(outcome).expect("read Windows outcome");
    assert!(outcome.contains(&raw_status.to_string()), "{outcome}");
}

fn base_harness(
    executable: &Path,
    cwd: &Path,
    injected_count: usize,
    arguments: &[OsString],
) -> Command {
    let mut command = Command::new(SUPERVISOR_HARNESS);
    command
        .env("SKILLMOUNT_HARNESS_EXECUTABLE", executable)
        .env("SKILLMOUNT_HARNESS_CWD", cwd)
        .env(
            "SKILLMOUNT_HARNESS_INJECTED_COUNT",
            injected_count.to_string(),
        )
        .args(arguments);
    command
}

fn waiting_harness(
    root: &Path,
    record: &Path,
    cleanup: &Path,
    outcome: &Path,
    behavior: &str,
) -> Command {
    let mut command = base_harness(Path::new(FAKE_AGENT), root, 0, &[]);
    command
        .env("SKILLMOUNT_FAKE_RECORD", record)
        .env("SKILLMOUNT_FAKE_BEHAVIOR", behavior)
        .env("SKILLMOUNT_HARNESS_CLEANUP_COUNTER", cleanup)
        .env("SKILLMOUNT_HARNESS_OUTCOME", outcome)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn copy_fake_agent(root: &Path) -> PathBuf {
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    let destination = root.join(format!("fake agent 日本語{suffix}"));
    fs::copy(FAKE_AGENT, &destination).expect("copy fake agent to edge-case path");
    destination
}

fn read_record(path: &Path) -> FakeRecord {
    let text = fs::read_to_string(path).expect("read fake-agent record");
    let mut lines = text.lines();
    assert_eq!(lines.next(), Some("SMFAKE1"));
    let mut record = FakeRecord {
        pid: None,
        arguments: Vec::new(),
        cwd: Vec::new(),
        stdin: None,
        events: Vec::new(),
    };
    for line in lines {
        if let Some(value) = line.strip_prefix("pid=") {
            record.pid = Some(value.parse::<u32>().expect("valid fake-agent PID"));
        } else if let Some(value) = line.strip_prefix("arg=") {
            record.arguments.push(decode_hex(value));
        } else if let Some(value) = line.strip_prefix("cwd=") {
            record.cwd = decode_hex(value);
        } else if let Some(value) = line.strip_prefix("stdin=") {
            record.stdin = Some(decode_hex(value));
        } else if let Some(value) = line.strip_prefix("event=") {
            record.events.push(value.to_owned());
        } else {
            panic!("unknown fake-agent record line: {line:?}");
        }
    }
    record
}

fn decode_hex(value: &str) -> Vec<u8> {
    assert_eq!(value.len() % 2, 0, "odd hexadecimal record");
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let pair = std::str::from_utf8(pair).expect("ASCII hexadecimal pair");
            u8::from_str_radix(pair, 16).expect("valid hexadecimal pair")
        })
        .collect()
}

fn cleanup_count(path: &Path) -> usize {
    fs::read_to_string(path)
        .expect("read cleanup counter")
        .lines()
        .count()
}

fn wait_for_cleanup_count(path: &Path, expected: usize) {
    let started = Instant::now();
    while started.elapsed() < TIMEOUT {
        if fs::read_to_string(path).map_or(0, |text| text.lines().count()) >= expected {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!(
        "timed out waiting for {expected} cleanup invocation(s) at {}",
        path.display()
    );
}

fn signal_event_count(record: &FakeRecord) -> usize {
    record
        .events
        .iter()
        .filter(|event| event.starts_with("signal:") || event.as_str() == "console")
        .count()
}

fn wait_for_event_count(path: &Path, expected: usize) {
    let started = Instant::now();
    while started.elapsed() < TIMEOUT {
        if let Ok(text) = fs::read_to_string(path) {
            let complete_events = text
                .split_inclusive('\n')
                .filter(|line| line.ends_with('\n') && line.starts_with("event="))
                .count();
            if complete_events >= expected {
                return;
            }
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!(
        "timed out waiting for {expected} fake-agent event(s) at {}",
        path.display()
    );
}

fn wait_for_exit(child: &mut Child) -> ExitStatus {
    if let Some(status) = wait_for_exit_within(child, TIMEOUT) {
        return status;
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("timed out waiting for supervisor harness {}", child.id());
}

fn wait_for_exit_within(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if let Some(status) = child.try_wait().expect("poll harness child") {
            return Some(status);
        }
        thread::sleep(Duration::from_millis(10));
    }
    None
}

#[cfg(unix)]
fn send_interrupt(process_group_leader: u32) {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;

    let pid = i32::try_from(process_group_leader).expect("Unix PID fits i32");
    kill(Pid::from_raw(pid), Signal::SIGINT).expect("send SIGINT to harness");
}

#[cfg(unix)]
fn send_termination(process_group_leader: u32) {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;

    let pid = i32::try_from(process_group_leader).expect("Unix PID fits i32");
    kill(Pid::from_raw(pid), Signal::SIGTERM).expect("send SIGTERM to harness");
}

#[cfg(unix)]
struct UnixProcessGuard(u32);

#[cfg(unix)]
impl Drop for UnixProcessGuard {
    fn drop(&mut self) {
        use nix::sys::signal::{Signal, kill};
        use nix::unistd::Pid;

        let pid = i32::try_from(self.0).expect("Unix PID fits i32");
        let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
    }
}

#[cfg(unix)]
fn unix_process_is_running(process_id: u32) -> bool {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;

    let pid = i32::try_from(process_id).expect("Unix PID fits i32");
    match kill(Pid::from_raw(pid), None) {
        Ok(()) | Err(Errno::EPERM) => true,
        Err(Errno::ESRCH) => false,
        Err(error) => panic!("query descendant {process_id} liveness: {error}"),
    }
}

#[cfg(windows)]
fn send_interrupt(process_group_leader: u32) {
    skillmount::process::test_support::send_console_break(process_group_leader)
        .expect("send console break to harness group");
}

#[cfg(unix)]
fn configure_controller_target(_command: &mut Command) {}

#[cfg(windows)]
fn configure_controller_target(command: &mut Command) {
    use std::os::windows::process::CommandExt;

    use windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP;

    command.creation_flags(CREATE_NEW_PROCESS_GROUP);
}

#[cfg(unix)]
fn os_bytes(value: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;

    value.as_bytes().to_vec()
}

#[cfg(windows)]
fn os_bytes(value: &OsStr) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;

    value.encode_wide().flat_map(u16::to_le_bytes).collect()
}

#[cfg(unix)]
fn non_unicode_argument() -> OsString {
    use std::os::unix::ffi::OsStringExt;

    OsString::from_vec(vec![b'n', b'o', b'n', b'-', 0xff])
}

#[cfg(unix)]
fn os_from_bytes(value: &[u8]) -> OsString {
    use std::os::unix::ffi::OsStringExt;

    OsString::from_vec(value.to_vec())
}

#[cfg(windows)]
fn os_from_bytes(value: &[u8]) -> OsString {
    use std::os::windows::ffi::OsStringExt;

    assert_eq!(value.len() % 2, 0, "odd Windows wide-string record");
    let wide: Vec<_> = value
        .chunks_exact(2)
        .map(|unit| u16::from_le_bytes([unit[0], unit[1]]))
        .collect();
    OsString::from_wide(&wide)
}

#[cfg(windows)]
fn non_unicode_argument() -> OsString {
    use std::os::windows::ffi::OsStringExt;

    OsString::from_wide(&[u16::from(b'n'), u16::from(b'o'), u16::from(b'n'), 0xd800])
}
