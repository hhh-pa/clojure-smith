//! Differential testing driver.
//!
//! For each seed, `drive` generates a program, runs it under every
//! `--runner` command, and compares the outputs byte for byte. Any
//! divergence — or any runner failure — is saved under `--fail-dir` for
//! later minimization with `reduce`.
//!
//! Generator options are forwarded after `--`:
//! `clojure-smith drive --seeds 0-99 -- --max-funcs 8 --no-loops`.

use clap::Args;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::gen_program::{emit_program, gen_program};
use crate::options::{GenerateOptions, parse_generator_args};

#[derive(Args, Debug, Clone)]
pub struct DriveOptions {
    /// Seeds to test: "N", "A-B", "A,B,C", or combinations like "1-3,7".
    #[arg(long, default_value = "0-9")]
    pub seeds: String,

    /// Runner command; `{}` is replaced with the program file (appended
    /// when absent). Repeat for multiple runners. Defaults to a plain and
    /// a direct-linking Clojure run.
    #[arg(long, value_name = "CMD")]
    pub runner: Vec<String>,

    /// Per-run timeout in seconds.
    #[arg(long, default_value_t = 120)]
    pub timeout: u64,

    /// Directory for diverging/failing cases.
    #[arg(long, default_value = "failures")]
    pub fail_dir: PathBuf,

    /// Parallel jobs.
    #[arg(long, default_value_t = 1)]
    pub jobs: usize,

    /// Only print failures and the summary.
    #[arg(long)]
    pub quiet: bool,

    /// Generator options, after `--`.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub gen_opts: Vec<String>,
}

impl DriveOptions {
    fn runners(&self) -> Vec<String> {
        if self.runner.is_empty() {
            vec![
                "clojure -M {}".to_string(),
                "clojure -J-Dclojure.compiler.direct-linking=true -M {}".to_string(),
            ]
        } else {
            self.runner.clone()
        }
    }
}

static UNIQUE: AtomicUsize = AtomicUsize::new(0);

/// A process-wide unique tag for temp paths, so parallel seeds and tests
/// never share a staging file.
pub fn unique_tag() -> usize {
    UNIQUE.fetch_add(1, Ordering::SeqCst)
}

/// Maximum seeds in one `--seeds` spec. Ranges are span-capped
/// individually below, but without a total cap a long comma list (or many
/// ranges) could exhaust memory before dedup.
pub const MAX_SEEDS: usize = 1_000_000;

/// Parse a seed spec into a sorted, deduplicated seed list.
pub fn parse_seeds(spec: &str) -> Result<Vec<i64>, String> {
    let mut out = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Ok(n) = part.parse::<i64>() {
            if out.len() >= MAX_SEEDS {
                return Err(format!("too many seeds (max {MAX_SEEDS})"));
            }
            out.push(n);
            continue;
        }
        // Range A-B, where either side may be negative: try every interior
        // '-' as the separator.
        let bytes = part.as_bytes();
        let mut parsed = false;
        for i in 1..bytes.len() {
            if bytes[i] == b'-'
                && let (Ok(a), Ok(b)) = (part[..i].parse::<i64>(), part[i + 1..].parse::<i64>())
            {
                if a > b {
                    return Err(format!("invalid reversed range: {part}"));
                }
                if (b as i128 - a as i128) >= MAX_SEEDS as i128 {
                    return Err(format!("range too large: {part}"));
                }
                let span = (b as i128 - a as i128 + 1) as usize;
                if out.len().saturating_add(span) > MAX_SEEDS {
                    return Err(format!("too many seeds (max {MAX_SEEDS})"));
                }
                out.extend(a..=b);
                parsed = true;
                break;
            }
        }
        if !parsed {
            return Err(format!("invalid seed spec: {part}"));
        }
    }
    if out.is_empty() {
        return Err("no seeds selected".to_string());
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

// ---------------------------------------------------------------------------
// Command execution (shared with the reducer)
// ---------------------------------------------------------------------------

/// The outcome of one runner/test invocation.
#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub timed_out: bool,
    pub spawn_error: Option<String>,
    pub exit: Option<i32>,
    /// Terminating signal, when the child died by signal (Unix only;
    /// `exit` is `None` then, which used to print as `exited with None`).
    pub signal: Option<i32>,
    /// Captured stdout, capped at 1 MiB.
    pub out: Vec<u8>,
    /// Captured stderr, capped at 1 MiB.
    pub err: Vec<u8>,
}

impl RunOutcome {
    /// One-line status: `exit 0`, `signal 9`, `timeout`, `spawn error: …`.
    pub fn status_text(&self) -> String {
        if self.timed_out {
            "timeout".to_string()
        } else if let Some(e) = &self.spawn_error {
            format!("spawn error: {e}")
        } else if let Some(c) = self.exit {
            format!("exit {c}")
        } else if let Some(s) = self.signal {
            format!("signal {s}")
        } else {
            "unknown status".to_string()
        }
    }
}

#[cfg(unix)]
fn exit_signal(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(not(unix))]
fn exit_signal(_status: &std::process::ExitStatus) -> Option<i32> {
    None
}

/// Quote `path` for `sh -c`: single-quote it, escaping embedded
/// quotes. Temp paths rarely need this, but a space in `TMPDIR` must not
/// break runners or, worse, inject shell syntax.
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

/// Run `cmd_template` via `sh -c`, substituting `{}` with `file` (or
/// appending the path when the template has no placeholder). The path is
/// shell-quoted in both cases.
pub fn run_command(cmd_template: &str, file: &Path, timeout: Duration) -> RunOutcome {
    let quoted = shell_quote(file);
    let script = if cmd_template.contains("{}") {
        cmd_template.replace("{}", &quoted)
    } else {
        format!("{cmd_template} {quoted}")
    };
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(&script)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Own process group, so a timeout can kill the runner's whole tree
    // (see kill_child).
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return RunOutcome {
                timed_out: false,
                spawn_error: Some(e.to_string()),
                exit: None,
                signal: None,
                out: Vec::new(),
                err: Vec::new(),
            };
        }
    };
    // Drain both pipes on threads so a chatty child can never block on a
    // full pipe buffer while we wait for it. Chunks land in shared
    // buffers as they arrive, so output drained before an orphaned
    // grandchild wedges the pipes is still kept; completion is only a
    // grace-limited wait.
    fn drain<R: std::io::Read>(h: Option<R>, buf: std::sync::Arc<std::sync::Mutex<Vec<u8>>>) {
        let Some(mut h) = h else { return };
        let mut tmp = [0u8; 8192];
        loop {
            match h.read(&mut tmp) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut b = buf.lock().unwrap();
                    let room = (1usize << 20).saturating_sub(b.len());
                    if room == 0 {
                        break;
                    }
                    b.extend_from_slice(&tmp[..n.min(room)]);
                }
            }
        }
    }
    let (out_tx, out_rx) = std::sync::mpsc::channel();
    let out_buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    std::thread::spawn({
        let h = child.stdout.take();
        let buf = out_buf.clone();
        move || {
            drain(h, buf);
            let _ = out_tx.send(());
        }
    });
    let (err_tx, err_rx) = std::sync::mpsc::channel();
    let err_buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    std::thread::spawn({
        let h = child.stderr.take();
        let buf = err_buf.clone();
        move || {
            drain(h, buf);
            let _ = err_tx.send(());
        }
    });
    // The error arm below returns early; gather output through this.
    // Whatever arrived before the grace expires is kept.
    let gather = |out_rx: std::sync::mpsc::Receiver<()>, err_rx: std::sync::mpsc::Receiver<()>| {
        let grace = Duration::from_secs(2);
        let _ = out_rx.recv_timeout(grace);
        let _ = err_rx.recv_timeout(grace);
        (
            out_buf.lock().unwrap().clone(),
            err_buf.lock().unwrap().clone(),
        )
    };
    // The standard library has no wait-with-timeout, so poll.
    let start = Instant::now();
    let mut timed_out = false;
    let mut exit = None;
    let mut signal = None;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                exit = status.code();
                signal = exit_signal(&status);
                break;
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    kill_child(&mut child);
                    timed_out = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => {
                let (out, err) = gather(out_rx, err_rx);
                return RunOutcome {
                    timed_out: false,
                    spawn_error: Some(e.to_string()),
                    exit: None,
                    signal: None,
                    out,
                    err,
                };
            }
        }
    }
    let (out, err) = gather(out_rx, err_rx);
    RunOutcome {
        timed_out,
        spawn_error: None,
        exit,
        signal,
        out,
        err,
    }
}

/// Kill a timed-out child and its whole process tree. The child is
/// `sh -c` in its own process group, so `child.kill()` alone would
/// orphan the real runner (the JVM) behind it.
#[cfg(unix)]
fn kill_child(child: &mut std::process::Child) {
    // Group leader == child pid after `process_group(0)`.
    let pgid = child.id() as libc::pid_t;
    // SAFETY: killpg on a valid pgid has no memory effects; if it fails
    // (already reaped, no permission) fall back to killing the child.
    if unsafe { libc::killpg(pgid, libc::SIGKILL) } != 0 {
        let _ = child.kill();
    }
    let _ = child.wait();
}

#[cfg(not(unix))]
fn kill_child(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

// ---------------------------------------------------------------------------
// The drive loop
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
enum Status {
    Ok,
    Mismatch,
    Error,
}

struct SeedResult {
    seed: i64,
    status: Status,
    detail: String,
    threw_on_all: bool,
    program: String,
    outcomes: Vec<RunOutcome>,
}

fn run_seed(
    gopts: &GenerateOptions,
    runners: &[String],
    timeout: Duration,
    seed: i64,
) -> SeedResult {
    // A generator panic must fail this seed, not the whole drive (in a
    // worker thread it would otherwise abort every other seed too).
    let prog =
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| gen_program(gopts, seed))) {
            Ok(p) => p,
            Err(_) => {
                return SeedResult {
                    seed,
                    status: Status::Error,
                    detail: "generator panicked".to_string(),
                    threw_on_all: false,
                    program: String::new(),
                    outcomes: Vec::new(),
                };
            }
        };
    let program = emit_program(&prog, gopts);

    let dir = std::env::temp_dir().join(format!(
        "clojure-smith-{}-{seed}-{}",
        std::process::id(),
        unique_tag()
    ));
    let file = dir.join("prog.clj");
    if let Err(e) = (|| -> std::io::Result<()> {
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(&file, &program)
    })() {
        return SeedResult {
            seed,
            status: Status::Error,
            detail: format!("cannot stage program file: {e}"),
            threw_on_all: false,
            program,
            outcomes: Vec::new(),
        };
    }

    let outcomes: Vec<RunOutcome> = runners
        .iter()
        .map(|r| run_command(r, &file, timeout))
        .collect();
    let _ = std::fs::remove_dir_all(&dir);

    // Any runner failure poisons the seed: identical failures prove nothing.
    for (i, o) in outcomes.iter().enumerate() {
        if let Some(e) = &o.spawn_error {
            return SeedResult {
                seed,
                status: Status::Error,
                detail: format!("runner {i} failed to spawn: {e}"),
                threw_on_all: false,
                program,
                outcomes,
            };
        }
        if o.timed_out {
            return SeedResult {
                seed,
                status: Status::Error,
                detail: format!("runner {i} timed out"),
                threw_on_all: false,
                program,
                outcomes,
            };
        }
        if o.exit != Some(0) {
            return SeedResult {
                seed,
                status: Status::Error,
                detail: format!("runner {i}: {}", o.status_text()),
                threw_on_all: false,
                program,
                outcomes,
            };
        }
    }
    let threw_on_all =
        !outcomes.is_empty() && outcomes.iter().all(|o| o.out.starts_with(b"threw:"));
    if outcomes.windows(2).all(|w| w[0].out == w[1].out) {
        SeedResult {
            seed,
            status: Status::Ok,
            detail: String::new(),
            threw_on_all,
            program,
            outcomes,
        }
    } else {
        SeedResult {
            seed,
            status: Status::Mismatch,
            detail: "outputs differ".to_string(),
            threw_on_all: false,
            program,
            outcomes,
        }
    }
}

fn save_failure(fail_dir: &Path, result: &SeedResult) -> std::io::Result<PathBuf> {
    let dir = fail_dir.join(format!("seed-{}", result.seed));
    // Start fresh: a previous drive may have saved a different-shaped
    // failure (fewer runners, timeout vs mismatch) under the same name,
    // and stale files would mislead the reducer.
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("prog.clj"), &result.program)?;
    for (i, o) in result.outcomes.iter().enumerate() {
        std::fs::write(dir.join(format!("runner-{i}.stdout")), &o.out)?;
        std::fs::write(dir.join(format!("runner-{i}.stderr")), &o.err)?;
        std::fs::write(dir.join(format!("runner-{i}.status")), o.status_text())?;
    }
    Ok(dir)
}

fn save_failure_report(fail_dir: &Path, result: &SeedResult) {
    match save_failure(fail_dir, result) {
        Ok(dir) => eprintln!("  saved to {}", dir.display()),
        Err(e) => eprintln!(
            "  warning: could not save {}: {e}",
            fail_dir.join(format!("seed-{}", result.seed)).display()
        ),
    }
}

/// Run the drive mode. Returns the process exit code.
pub fn run_drive(opts: &DriveOptions) -> i32 {
    let seeds = match parse_seeds(&opts.seeds) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("clojure-smith drive: {e}");
            return 2;
        }
    };
    if opts.jobs == 0 {
        eprintln!("clojure-smith drive: --jobs must be at least 1");
        return 2;
    }
    if opts.timeout == 0 {
        eprintln!("clojure-smith drive: --timeout must be at least 1 second");
        return 2;
    }
    let mut gopts = match parse_generator_args(&opts.gen_opts) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("clojure-smith drive: bad generator option: {e}");
            return 2;
        }
    };
    if gopts.seed.is_some() {
        eprintln!("clojure-smith drive: --seed cannot be forwarded (the driver sets it)");
        return 2;
    }
    if gopts.output.is_some() {
        eprintln!("clojure-smith drive: --output cannot be forwarded (the driver manages files)");
        return 2;
    }
    if let Err(e) = gopts.validate() {
        eprintln!("clojure-smith drive: {e}");
        return 2;
    }
    gopts.quiet = true; // Failure files stay clean; progress goes to stderr.
    if gopts.no_checksum {
        eprintln!(
            "clojure-smith drive: warning: --no-checksum disables the differential oracle; \
             every runner prints \"done\" and only crashes can diverge"
        );
    }
    let runners = opts.runners();
    let timeout = Duration::from_secs(opts.timeout);

    if !opts.quiet {
        eprintln!(
            "clojure-smith drive: {} seed(s), {} runner(s), {} job(s)",
            seeds.len(),
            runners.len(),
            opts.jobs
        );
    }

    let results: Vec<SeedResult> = if opts.jobs == 1 || seeds.len() == 1 {
        seeds
            .iter()
            .map(|&s| run_seed(&gopts, &runners, timeout, s))
            .collect()
    } else {
        let next = AtomicUsize::new(0);
        let slots: Mutex<Vec<Option<SeedResult>>> =
            Mutex::new((0..seeds.len()).map(|_| None).collect());
        std::thread::scope(|s| {
            for _ in 0..opts.jobs.min(seeds.len()) {
                s.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, Ordering::SeqCst);
                        if i >= seeds.len() {
                            break;
                        }
                        let r = run_seed(&gopts, &runners, timeout, seeds[i]);
                        slots.lock().unwrap()[i] = Some(r);
                    }
                });
            }
        });
        slots
            .into_inner()
            .unwrap()
            .into_iter()
            .map(|r| r.expect("worker did not fill its slot"))
            .collect()
    };

    let mut ok = 0;
    let mut mismatches = 0;
    let mut errors = 0;
    for r in &results {
        match r.status {
            Status::Ok => {
                ok += 1;
                if !opts.quiet {
                    eprintln!("seed {}: OK", r.seed);
                }
                if r.threw_on_all {
                    eprintln!(
                        "seed {}: warning: all runners printed \"threw:\" (possible generator bug)",
                        r.seed
                    );
                }
            }
            Status::Mismatch => {
                mismatches += 1;
                eprintln!("seed {}: MISMATCH", r.seed);
                save_failure_report(&opts.fail_dir, r);
            }
            Status::Error => {
                errors += 1;
                eprintln!("seed {}: ERROR: {}", r.seed, r.detail);
                save_failure_report(&opts.fail_dir, r);
            }
        }
    }
    eprintln!("clojure-smith drive: {ok} ok, {mismatches} mismatch, {errors} error");
    // Exit codes: 0 all agree, 1 divergences found (the interesting case),
    // 2 bad usage (returned earlier), 3 failures without any clean
    // comparison (timeouts, panics, spawn errors).
    if mismatches > 0 {
        1
    } else if errors > 0 {
        3
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_specs() {
        assert_eq!(parse_seeds("5").unwrap(), vec![5]);
        assert_eq!(parse_seeds("0-3").unwrap(), vec![0, 1, 2, 3]);
        assert_eq!(parse_seeds("1-3,7").unwrap(), vec![1, 2, 3, 7]);
        assert_eq!(parse_seeds("3,1-2").unwrap(), vec![1, 2, 3]);
        assert_eq!(parse_seeds("-3--1").unwrap(), vec![-3, -2, -1]);
        assert_eq!(parse_seeds("-2,4").unwrap(), vec![-2, 4]);
        assert!(parse_seeds("").is_err());
        assert!(parse_seeds("3-1").is_err());
        assert!(parse_seeds("abc").is_err());
        assert!(parse_seeds("0-2000000").is_err());
        // Two individually fine ranges exceed the total cap.
        assert!(parse_seeds("0-600000,1000000-1600000").is_err());
        assert_eq!(parse_seeds("0-999999").unwrap().len(), MAX_SEEDS);
    }

    #[cfg(unix)]
    #[test]
    fn command_basics() {
        use std::io::Write;
        let dir =
            std::env::temp_dir().join(format!("cs-test-{}-{}", std::process::id(), unique_tag()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("f.txt");
        std::fs::write(&file, "hello\n").unwrap();

        let o = run_command("cat {}", &file, Duration::from_secs(10));
        assert!(!o.timed_out);
        assert_eq!(o.exit, Some(0));
        assert_eq!(o.out, b"hello\n");

        // No placeholder: path is appended.
        let o = run_command("cat", &file, Duration::from_secs(10));
        assert_eq!(o.out, b"hello\n");

        let o = run_command(
            "echo out; echo err >&2; exit 3 #{}",
            &file,
            Duration::from_secs(10),
        );
        assert_eq!(o.exit, Some(3));
        assert_eq!(o.out, b"out\n");
        assert_eq!(o.err, b"err\n");

        let start = Instant::now();
        let o = run_command("sleep 30 #{}", &file, Duration::from_secs(1));
        assert!(o.timed_out);
        assert!(start.elapsed() < Duration::from_secs(10));

        let _ = std::io::stderr().flush();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn drive_plumbing_with_cat() {
        // `cat` runners trivially agree; exercises staging/comparison.
        let dir =
            std::env::temp_dir().join(format!("cs-drive-{}-{}", std::process::id(), unique_tag()));
        let _ = std::fs::remove_dir_all(&dir);
        let opts = DriveOptions {
            seeds: "1-2".to_string(),
            runner: vec!["cat {}".to_string(), "cat {}".to_string()],
            timeout: 30,
            fail_dir: dir.join("failures"),
            jobs: 2,
            quiet: true,
            gen_opts: vec!["--max-funcs".to_string(), "2".to_string()],
        };
        assert_eq!(run_drive(&opts), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn drive_detects_mismatch() {
        let dir =
            std::env::temp_dir().join(format!("cs-mism-{}-{}", std::process::id(), unique_tag()));
        let _ = std::fs::remove_dir_all(&dir);
        let opts = DriveOptions {
            seeds: "5".to_string(),
            runner: vec!["echo one".to_string(), "echo two".to_string()],
            timeout: 30,
            fail_dir: dir.join("failures"),
            jobs: 1,
            quiet: true,
            gen_opts: vec![],
        };
        assert_eq!(run_drive(&opts), 1);
        assert!(dir.join("failures/seed-5/prog.clj").exists());
        assert!(dir.join("failures/seed-5/runner-0.stdout").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn drive_failure_dir_is_fresh_and_fallible() {
        let dir =
            std::env::temp_dir().join(format!("cs-save-{}-{}", std::process::id(), unique_tag()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mk = |fail_dir: std::path::PathBuf| DriveOptions {
            seeds: "5".to_string(),
            runner: vec!["echo one".to_string(), "echo two".to_string()],
            timeout: 30,
            fail_dir,
            jobs: 1,
            quiet: true,
            gen_opts: vec![],
        };
        // A stale file from a previous drive must not survive.
        let stale = dir.join("failures/seed-5/stale.txt");
        std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
        std::fs::write(&stale, "stale").unwrap();
        assert_eq!(run_drive(&mk(dir.join("failures"))), 1);
        assert!(!stale.exists());
        assert!(dir.join("failures/seed-5/prog.clj").exists());
        // An unwritable fail dir warns but still reports the mismatch.
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, "not a dir").unwrap();
        assert_eq!(run_drive(&mk(blocker)), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn drive_errors_without_mismatch_exit_3() {
        let dir =
            std::env::temp_dir().join(format!("cs-err-{}-{}", std::process::id(), unique_tag()));
        let _ = std::fs::remove_dir_all(&dir);
        let opts = DriveOptions {
            seeds: "5".to_string(),
            runner: vec!["false".to_string()],
            timeout: 30,
            fail_dir: dir.join("failures"),
            jobs: 1,
            quiet: true,
            gen_opts: vec![],
        };
        // No clean comparison was possible: distinct from "found a
        // divergence" (1) and "all agree" (0).
        assert_eq!(run_drive(&opts), 3);
        assert!(dir.join("failures/seed-5/prog.clj").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn status_texts() {
        let base = RunOutcome {
            timed_out: false,
            spawn_error: None,
            exit: None,
            signal: None,
            out: Vec::new(),
            err: Vec::new(),
        };
        assert_eq!(
            RunOutcome {
                exit: Some(3),
                ..base.clone()
            }
            .status_text(),
            "exit 3"
        );
        assert_eq!(
            RunOutcome {
                signal: Some(9),
                ..base.clone()
            }
            .status_text(),
            "signal 9"
        );
        assert_eq!(
            RunOutcome {
                timed_out: true,
                ..base.clone()
            }
            .status_text(),
            "timeout"
        );
        assert_eq!(
            RunOutcome {
                spawn_error: Some("nope".to_string()),
                ..base.clone()
            }
            .status_text(),
            "spawn error: nope"
        );
    }

    #[cfg(unix)]
    #[test]
    fn command_quotes_paths_with_spaces() {
        let dir =
            std::env::temp_dir().join(format!("cs-quote-{}-{}", std::process::id(), unique_tag()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a b'c.txt");
        std::fs::write(&file, "spaced\n").unwrap();
        // Unquoted, the space would split the path into two arguments.
        let o = run_command("cat {}", &file, Duration::from_secs(10));
        assert_eq!(o.exit, Some(0));
        assert_eq!(o.out, b"spaced\n");
        let o = run_command("cat", &file, Duration::from_secs(10));
        assert_eq!(o.out, b"spaced\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn command_reports_signals() {
        let dir =
            std::env::temp_dir().join(format!("cs-sig-{}-{}", std::process::id(), unique_tag()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("f.txt");
        std::fs::write(&file, "x").unwrap();
        let o = run_command("kill -9 $$ #{}", &file, Duration::from_secs(10));
        assert_eq!(o.exit, None);
        assert_eq!(o.signal, Some(9));
        assert_eq!(o.status_text(), "signal 9");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn timeout_kills_grandchildren() {
        // The background sleep would create the flag at 3s; the timeout
        // fires at 1s and must take the whole process group with it.
        let dir =
            std::env::temp_dir().join(format!("cs-pg-{}-{}", std::process::id(), unique_tag()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let flag = dir.join("survived");
        let o = run_command(
            "(echo dying; sleep 3; touch {}) & wait",
            &flag,
            Duration::from_secs(1),
        );
        assert!(o.timed_out);
        assert_eq!(o.out, b"dying\n");
        std::thread::sleep(Duration::from_secs(4));
        assert!(!flag.exists(), "grandchild survived the timeout kill");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn lingering_grandchild_keeps_partial_output() {
        // `setsid` detaches and holds the pipes past the grace period
        // (the shell itself exits right away); output drained before
        // that must survive instead of coming back empty.
        let dir =
            std::env::temp_dir().join(format!("cs-part-{}-{}", std::process::id(), unique_tag()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("f.txt");
        std::fs::write(&file, "x").unwrap();
        let o = run_command(
            "echo hello; setsid sleep 5 #{}",
            &file,
            Duration::from_secs(10),
        );
        assert!(!o.timed_out);
        assert_eq!(o.exit, Some(0));
        assert_eq!(o.out, b"hello\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
