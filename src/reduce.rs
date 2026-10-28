//! Test-case reduction: minimize a program while a test command still
//! reports it interesting (exit status 0).
//!
//! This is a simple structural reducer in the spirit of C-Reduce: it
//! alternatingly applies delta minimization over top-level forms and
//! replaces subforms with small constants, until reaching a fixpoint or
//! exhausting `--tries`. It understands the plain-Clojure syntax covered
//! by [`crate::form`]; exotic reader features are rejected at parse time.

use clap::Args;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Duration;

use crate::driver::run_command;
use crate::form::{Form, parse_all, print_compact, print_pretty};

#[derive(Args, Debug, Clone)]
pub struct ReduceOptions {
    /// Test command; `{}` is replaced with the candidate file (appended
    /// when absent). Exit status 0 means "still interesting".
    #[arg(long, value_name = "CMD")]
    pub test: String,

    /// Input program.
    pub input: PathBuf,

    /// Write the reduced program here instead of stdout.
    #[arg(short, long, value_name = "FILE")]
    pub output: Option<PathBuf>,

    /// Maximum predicate evaluations (0 = unlimited).
    #[arg(long, default_value_t = 10000)]
    pub tries: u64,

    /// Per-test timeout in seconds.
    #[arg(long, default_value_t = 60)]
    pub timeout: u64,

    /// Suppress the summary report.
    #[arg(long)]
    pub quiet: bool,

    /// Treat a test-command timeout as "still interesting", for reducing
    /// hangs. Pair with a short `--timeout`: every evaluation that still
    /// hangs costs the full timeout.
    #[arg(long)]
    pub timeout_is_interesting: bool,
}

struct Reducer {
    test: String,
    timeout: Duration,
    timeout_is_interesting: bool,
    /// 0 = unlimited.
    limit: u64,
    used: u64,
    tmp: PathBuf,
    last: Option<crate::driver::RunOutcome>,
    /// Environment failure (candidate staging or test spawn), distinct
    /// from an uninteresting verdict.
    env_error: Option<String>,
    /// Consecutive environment failures; abort past [`MAX_CONSEC_ENV_FAIL`].
    /// Timeouts are verdicts, not environment failures.
    consec_env_fail: u32,
}

/// Abort reduction after this many consecutive environment failures
/// (broken staging or an unspawnable test): every further evaluation
/// would misjudge.
const MAX_CONSEC_ENV_FAIL: u32 = 3;

impl Reducer {
    fn is_interesting(&mut self, forms: &[Form]) -> bool {
        if self.limit > 0 && self.used >= self.limit {
            return false;
        }
        self.used += 1;
        let mut text = String::new();
        for f in forms {
            text.push_str(&print_compact(f));
            text.push('\n');
        }
        if let Err(e) = std::fs::write(&self.tmp, &text) {
            self.note_env_fail(format!("cannot stage candidate: {e}"));
            return false;
        }
        let o = run_command(&self.test, &self.tmp, self.timeout);
        if let Some(e) = &o.spawn_error {
            self.note_env_fail(format!("test command could not start: {e}"));
            self.last = Some(o);
            return false;
        }
        self.consec_env_fail = 0;
        let interesting = o.exit == Some(0) || (self.timeout_is_interesting && o.timed_out);
        self.last = Some(o);
        interesting
    }

    fn note_env_fail(&mut self, msg: String) {
        self.env_error = Some(msg);
        self.consec_env_fail = self.consec_env_fail.saturating_add(1);
    }

    fn exhausted(&self) -> bool {
        self.limit > 0 && self.used >= self.limit
    }

    fn aborted(&self) -> bool {
        self.consec_env_fail >= MAX_CONSEC_ENV_FAIL
    }
}

// ---------------------------------------------------------------------------
// Paths into forms
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
enum Step {
    /// Nth child of a list/vector/set/quote/deref/meta node.
    Child(usize),
    /// Key of the nth map pair.
    Key(usize),
    /// Value of the nth map pair.
    Val(usize),
}

fn children(form: &Form) -> Vec<(Step, &Form)> {
    match form {
        Form::List(fs) | Form::Vector(fs) | Form::Set(fs) => fs
            .iter()
            .enumerate()
            .map(|(i, f)| (Step::Child(i), f))
            .collect(),
        Form::Map(ps) => {
            let mut out = Vec::with_capacity(ps.len() * 2);
            for (i, (k, v)) in ps.iter().enumerate() {
                out.push((Step::Key(i), k));
                out.push((Step::Val(i), v));
            }
            out
        }
        Form::Quote(x) | Form::Deref(x) => vec![(Step::Child(0), x)],
        Form::Meta(m, x) => vec![(Step::Child(0), m), (Step::Child(1), x)],
        _ => Vec::new(),
    }
}

/// All paths in `form`, outermost first, including the empty (root) path.
fn collect_paths(form: &Form) -> Vec<Vec<Step>> {
    let mut out = vec![Vec::new()];
    let mut queue: VecDeque<(Vec<Step>, &Form)> = VecDeque::from([(Vec::new(), form)]);
    while let Some((path, node)) = queue.pop_front() {
        for (step, child) in children(node) {
            let mut p = path.clone();
            p.push(step);
            out.push(p.clone());
            queue.push_back((p, child));
        }
    }
    out
}

fn replace_at(form: &Form, path: &[Step], new: &Form) -> Form {
    if path.is_empty() {
        return new.clone();
    }
    match (form, &path[0]) {
        (Form::List(fs), Step::Child(i)) => {
            let mut fs = fs.clone();
            fs[*i] = replace_at(&fs[*i], &path[1..], new);
            Form::List(fs)
        }
        (Form::Vector(fs), Step::Child(i)) => {
            let mut fs = fs.clone();
            fs[*i] = replace_at(&fs[*i], &path[1..], new);
            Form::Vector(fs)
        }
        (Form::Set(fs), Step::Child(i)) => {
            let mut fs = fs.clone();
            fs[*i] = replace_at(&fs[*i], &path[1..], new);
            Form::Set(fs)
        }
        (Form::Map(ps), Step::Key(i)) => {
            let mut ps = ps.clone();
            ps[*i].0 = replace_at(&ps[*i].0, &path[1..], new);
            Form::Map(ps)
        }
        (Form::Map(ps), Step::Val(i)) => {
            let mut ps = ps.clone();
            ps[*i].1 = replace_at(&ps[*i].1, &path[1..], new);
            Form::Map(ps)
        }
        (Form::Quote(x), Step::Child(0)) => Form::Quote(Box::new(replace_at(x, &path[1..], new))),
        (Form::Deref(x), Step::Child(0)) => Form::Deref(Box::new(replace_at(x, &path[1..], new))),
        (Form::Meta(m, x), Step::Child(0)) => {
            Form::Meta(Box::new(replace_at(m, &path[1..], new)), x.clone())
        }
        (Form::Meta(m, x), Step::Child(1)) => {
            Form::Meta(m.clone(), Box::new(replace_at(x, &path[1..], new)))
        }
        _ => panic!("stale reducer path: {path:?} in {form:?}"),
    }
}

fn consts() -> Vec<Form> {
    vec![
        Form::int(0),
        Form::int(1),
        Form::float(0.0),
        Form::boolean(false),
        Form::boolean(true),
        Form::string(""),
        Form::nil(),
        Form::vector(vec![]),
        Form::map(vec![]),
    ]
}

/// The subform at `path`, mirroring [`replace_at`]. Panics on stale paths.
fn get_at<'a>(form: &'a Form, path: &[Step]) -> &'a Form {
    let mut node = form;
    for step in path {
        node = match (node, step) {
            (Form::List(fs) | Form::Vector(fs) | Form::Set(fs), Step::Child(i)) => &fs[*i],
            (Form::Map(ps), Step::Key(i)) => &ps[*i].0,
            (Form::Map(ps), Step::Val(i)) => &ps[*i].1,
            (Form::Quote(x) | Form::Deref(x), Step::Child(0)) => x,
            (Form::Meta(m, _), Step::Child(0)) => m,
            (Form::Meta(_, x), Step::Child(1)) => x,
            _ => panic!("stale reducer path: {path:?}"),
        };
    }
    node
}

/// Drop candidates for a node: a list can try each element, a quote or
/// deref its child (`(println X)` -> `X`). Anything else has none.
fn drop_kids(node: &Form) -> Vec<Form> {
    match node {
        Form::List(fs) => fs.clone(),
        Form::Quote(x) | Form::Deref(x) => vec![(**x).clone()],
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Reduction passes
// ---------------------------------------------------------------------------

fn total_size(forms: &[Form]) -> usize {
    forms.iter().map(Form::size).sum()
}

/// Delta minimization over top-level forms: try removing contiguous chunks,
/// halving the chunk size down to 1.
fn ddmin_pass(r: &mut Reducer, mut forms: Vec<Form>) -> Vec<Form> {
    if forms.len() < 2 {
        return forms;
    }
    let mut size = forms.len() / 2;
    loop {
        let mut i = 0;
        while i < forms.len() {
            if r.exhausted() || r.aborted() {
                return forms;
            }
            let end = (i + size).min(forms.len());
            let mut cand = Vec::with_capacity(forms.len() - (end - i));
            cand.extend_from_slice(&forms[..i]);
            cand.extend_from_slice(&forms[end..]);
            if r.is_interesting(&cand) {
                forms = cand;
            } else {
                i += size;
            }
        }
        if size <= 1 {
            return forms;
        }
        size = (size / 2).max(1);
    }
}

/// Try replacing every subform of every top-level form with small constants.
fn scalar_pass(r: &mut Reducer, mut forms: Vec<Form>) -> Vec<Form> {
    let consts = consts();
    for idx in 0..forms.len() {
        loop {
            if r.exhausted() || r.aborted() {
                return forms;
            }
            let paths = collect_paths(&forms[idx]);
            let cur_size = forms[idx].size();
            let mut changed = false;
            'paths: for path in &paths {
                for c in &consts {
                    if r.exhausted() || r.aborted() {
                        return forms;
                    }
                    let replaced = replace_at(&forms[idx], path, c);
                    if replaced.size() >= cur_size {
                        // Only strictly smaller candidates are worth
                        // testing: this both skips no-op replacements and
                        // guarantees termination (no oscillation between
                        // same-size constants).
                        continue;
                    }
                    let mut cand = forms.clone();
                    cand[idx] = replaced;
                    if r.is_interesting(&cand) {
                        forms = cand;
                        changed = true;
                        break 'paths;
                    }
                }
            }
            if !changed {
                break;
            }
        }
    }
    forms
}

/// Explain an uninteresting input: a broken test command (exit 2)
/// must not masquerade as an uninteresting case (exit 1). The command
/// runs under `sh -c`, so a missing command surfaces as exit 127 with a
/// "not found" diagnostic rather than a spawn error. Returns the process
/// exit code.
fn report_uninteresting(r: &Reducer, timeout_is_interesting: bool) -> i32 {
    let (msg, code) = uninteresting_message(r, timeout_is_interesting);
    eprintln!("{msg}");
    code
}

/// Explain an uninteresting input as `(message, exit code)`: a broken
/// environment (exit 2) must not masquerade as an uninteresting case
/// (exit 1).
fn uninteresting_message(r: &Reducer, timeout_is_interesting: bool) -> (String, i32) {
    if let Some(e) = &r.env_error {
        return (format!("clojure-smith reduce: {e}"), 2);
    }
    if let Some(o) = &r.last {
        if o.timed_out && !timeout_is_interesting {
            return (
                "clojure-smith reduce: input is not interesting (test timed out; pass --timeout-is-interesting to reduce hangs)"
                    .to_string(),
                1,
            );
        }
        if o.exit == Some(127) && test_command_missing(&r.test, o) {
            let first = String::from_utf8_lossy(&o.err)
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            let msg = if first.is_empty() {
                "clojure-smith reduce: test command not found; check --test spelling".to_string()
            } else {
                format!(
                    "clojure-smith reduce: test command not found ({first}); check --test spelling"
                )
            };
            return (msg, 2);
        }
    }
    if timeout_is_interesting {
        (
            "clojure-smith reduce: input is not interesting (test exited nonzero without timing out)"
                .to_string(),
            1,
        )
    } else {
        (
            "clojure-smith reduce: input is not interesting (test exited nonzero)".to_string(),
            1,
        )
    }
}

/// Decide whether an exit-127 means the test command itself is missing:
/// directly when its first word is absent from PATH, or when the shell's
/// own diagnostic says so (covers scripts whose inner commands are
/// missing).
fn test_command_missing(test: &str, o: &crate::driver::RunOutcome) -> bool {
    if first_word_missing(test) == Some(true) {
        return true;
    }
    String::from_utf8_lossy(&o.err).contains("not found")
}

/// Whether the template's first word fails `command -v`; `None` when the
/// probe itself cannot run, the template is empty, or the word looks like
/// a flag (callers fall back to the stderr sniff then).
fn first_word_missing(test: &str) -> Option<bool> {
    let mut words = test.split_whitespace().peekable();
    // Skip VAR=value environment prefixes.
    while let Some(w) = words.peek() {
        let mut chars = w.chars();
        let is_assign =
            matches!(chars.next(), Some(c) if c.is_alphabetic() || c == '_') && w.contains('=');
        if !is_assign {
            break;
        }
        words.next();
    }
    let word = words.next()?;
    if word.is_empty() || word.starts_with('-') {
        return None;
    }
    let quoted = format!("'{}'", word.replace('\'', "'\\''"));
    let status = std::process::Command::new("sh")
        .args(["-c", &format!("command -v {quoted} >/dev/null 2>&1")])
        .status()
        .ok()?;
    Some(!status.success())
}

/// Try replacing every list-typed subform with each of its children.
/// This collapses nesting the scalar pass cannot (`(println X)` -> `X`
/// keeps `X`'s content, while no constant does).
fn drop_pass(r: &mut Reducer, mut forms: Vec<Form>) -> Vec<Form> {
    for idx in 0..forms.len() {
        loop {
            if r.exhausted() || r.aborted() {
                return forms;
            }
            let paths = collect_paths(&forms[idx]);
            let cur_size = forms[idx].size();
            let mut changed = false;
            'paths: for path in &paths {
                for kid in drop_kids(get_at(&forms[idx], path)) {
                    if r.exhausted() || r.aborted() {
                        return forms;
                    }
                    let replaced = replace_at(&forms[idx], path, &kid);
                    if replaced.size() >= cur_size {
                        continue;
                    }
                    let mut cand = forms.clone();
                    cand[idx] = replaced;
                    if r.is_interesting(&cand) {
                        forms = cand;
                        changed = true;
                        break 'paths;
                    }
                }
            }
            if !changed {
                break;
            }
        }
    }
    forms
}

fn reduce_forms(r: &mut Reducer, mut forms: Vec<Form>) -> Vec<Form> {
    loop {
        let before = total_size(&forms);
        forms = ddmin_pass(r, forms);
        if r.exhausted() || r.aborted() {
            return forms;
        }
        forms = scalar_pass(r, forms);
        if r.exhausted() || r.aborted() {
            return forms;
        }
        forms = drop_pass(r, forms);
        if r.exhausted() || r.aborted() {
            return forms;
        }
        if total_size(&forms) >= before {
            return forms;
        }
    }
}

/// Run the reduce mode. Returns the process exit code.
pub fn run_reduce(opts: &ReduceOptions) -> i32 {
    if opts.timeout == 0 {
        eprintln!("clojure-smith reduce: --timeout must be at least 1 second");
        return 2;
    }
    let text = match std::fs::read_to_string(&opts.input) {
        Ok(t) => t,
        Err(e) => {
            eprintln!(
                "clojure-smith reduce: cannot read {}: {e}",
                opts.input.display()
            );
            return 2;
        }
    };
    let forms = match parse_all(&text) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("clojure-smith reduce: {}: {e}", opts.input.display());
            return 2;
        }
    };
    if forms.is_empty() {
        eprintln!("clojure-smith reduce: input contains no forms");
        return 2;
    }
    let tmp = crate::driver::secure_tmp("clojure-smith-reduce");
    let mut r = Reducer {
        test: opts.test.clone(),
        timeout: Duration::from_secs(opts.timeout),
        timeout_is_interesting: opts.timeout_is_interesting,
        limit: opts.tries,
        used: 0,
        tmp: tmp.clone(),
        last: None,
        env_error: None,
        consec_env_fail: 0,
    };
    if !r.is_interesting(&forms) {
        let code = report_uninteresting(&r, opts.timeout_is_interesting);
        let _ = std::fs::remove_file(&tmp);
        return code;
    }
    let before_forms = forms.len();
    let before_bytes = text.len();
    let reduced = reduce_forms(&mut r, forms);
    let _ = std::fs::remove_file(&tmp);
    if r.aborted() {
        eprintln!(
            "clojure-smith reduce: aborting: test environment failing repeatedly ({})",
            r.env_error.as_deref().unwrap_or("unknown error")
        );
        return 2;
    }

    let mut out_text = String::new();
    for (i, f) in reduced.iter().enumerate() {
        if i > 0 {
            out_text.push('\n');
        }
        out_text.push_str(&print_pretty(f));
        out_text.push('\n');
    }
    let write_result = match &opts.output {
        Some(path) => std::fs::write(path, &out_text).map_err(|e| e.to_string()),
        None => {
            use std::io::Write;
            std::io::stdout()
                .write_all(out_text.as_bytes())
                .map_err(|e| e.to_string())
        }
    };
    if let Err(e) = write_result {
        eprintln!("clojure-smith reduce: failed to write output: {e}");
        return 1;
    }
    if !opts.quiet {
        eprintln!(
            "clojure-smith reduce: {} -> {} forms, {} -> {} bytes, {} tries",
            before_forms,
            reduced.len(),
            before_bytes,
            out_text.len(),
            r.used
        );
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::unique_tag;
    use crate::form::parse_one;

    #[test]
    fn paths_and_replace() {
        let f = parse_one("(a [1 {:k v}])").unwrap();
        let paths = collect_paths(&f);
        // The root (empty) path appears exactly once.
        assert_eq!(paths.iter().filter(|p| p.is_empty()).count(), 1);
        // Root-first ordering: parents precede children.
        let pos = |p: Vec<Step>| paths.iter().position(|x| *x == p).unwrap();
        assert!(pos(vec![]) < pos(vec![Step::Child(1)]));
        assert!(
            pos(vec![Step::Child(1), Step::Child(1), Step::Val(0)]) > pos(vec![Step::Child(1)])
        );
        let r = replace_at(
            &f,
            &[Step::Child(1), Step::Child(1), Step::Val(0)],
            &Form::int(0),
        );
        assert_eq!(print_compact(&r), "(a [1 {:k 0}])");
        let r = replace_at(&f, &[], &Form::nil());
        assert_eq!(r, Form::Nil);
    }

    #[cfg(unix)]
    #[test]
    fn reduces_with_grep_predicate() {
        let dir =
            std::env::temp_dir().join(format!("cs-reduce-{}-{}", std::process::id(), unique_tag()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("prog.clj");
        let output = dir.join("small.clj");
        std::fs::write(
            &input,
            "(ns tiny)\n(defn f0 [x] (+ x 1))\n(defn f1 [y] (* y 2))\n(println \"MAGIC\" (f1 21))\n",
        )
        .unwrap();
        let opts = ReduceOptions {
            test: "grep -q MAGIC {} && grep -q f1 {}".to_string(),
            input,
            output: Some(output.clone()),
            tries: 10000,
            timeout: 30,
            quiet: true,
            timeout_is_interesting: false,
        };
        assert_eq!(run_reduce(&opts), 0);
        let reduced = std::fs::read_to_string(&output).unwrap();
        assert!(reduced.contains("MAGIC"), "{reduced}");
        assert!(reduced.contains("f1"), "{reduced}");
        assert!(!reduced.contains("f0"), "{reduced}");
        assert!(reduced.len() < 90, "not reduced enough: {reduced}");
        // Reduced output must still parse.
        parse_all(&reduced).expect("reduced output must parse");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_uninteresting_input() {
        let dir =
            std::env::temp_dir().join(format!("cs-red2-{}-{}", std::process::id(), unique_tag()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("prog.clj");
        std::fs::write(&input, "(println :hello)\n").unwrap();
        let opts = ReduceOptions {
            test: "grep -q MAGIC {}".to_string(),
            input,
            output: None,
            tries: 100,
            timeout: 30,
            quiet: true,
            timeout_is_interesting: false,
        };
        assert_eq!(run_reduce(&opts), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn get_at_mirrors_replace() {
        let f = parse_one("(a [1 {:k v}])").unwrap();
        for path in collect_paths(&f) {
            let node = get_at(&f, &path);
            let back = replace_at(&f, &path, node);
            assert_eq!(back, f, "roundtrip failed at {path:?}");
        }
        assert_eq!(
            get_at(&f, &[Step::Child(1), Step::Child(1), Step::Val(0)]),
            &Form::sym("v")
        );
    }

    #[cfg(unix)]
    #[test]
    fn drop_pass_collapses_nesting() {
        // Only dropping (not constants) can reduce this to `:MAGIC`.
        let dir =
            std::env::temp_dir().join(format!("cs-red4-{}-{}", std::process::id(), unique_tag()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("prog.clj");
        let output = dir.join("small.clj");
        std::fs::write(&input, "(println (str :a :MAGIC))\n").unwrap();
        let opts = ReduceOptions {
            test: "grep -q MAGIC {}".to_string(),
            input,
            output: Some(output.clone()),
            tries: 10000,
            timeout: 30,
            quiet: true,
            timeout_is_interesting: false,
        };
        assert_eq!(run_reduce(&opts), 0);
        let reduced = std::fs::read_to_string(&output).unwrap();
        assert_eq!(reduced.trim(), ":MAGIC", "{reduced:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn reducer_for(test: &str) -> Reducer {
        Reducer {
            test: test.to_string(),
            timeout: Duration::from_secs(30),
            timeout_is_interesting: false,
            limit: 0,
            used: 0,
            tmp: std::path::PathBuf::from("unused"),
            last: None,
            env_error: None,
            consec_env_fail: 0,
        }
    }

    fn outcome() -> crate::driver::RunOutcome {
        crate::driver::RunOutcome {
            timed_out: false,
            spawn_error: None,
            exit: None,
            signal: None,
            truncated: false,
            out: Vec::new(),
            err: Vec::new(),
        }
    }

    #[test]
    fn uninteresting_messages() {
        // Environment failure: usage error, not a verdict.
        let mut r = reducer_for("true {}");
        r.env_error = Some("cannot stage candidate: nope".to_string());
        let (msg, code) = uninteresting_message(&r, false);
        assert_eq!(code, 2);
        assert!(msg.contains("cannot stage candidate"), "{msg}");
        // Initial timeout without the flag: hint at the flag.
        let mut r = reducer_for("sleep 5 {}");
        let mut o = outcome();
        o.timed_out = true;
        r.last = Some(o);
        let (msg, code) = uninteresting_message(&r, false);
        assert_eq!(code, 1);
        assert!(msg.contains("--timeout-is-interesting"), "{msg}");
        // Plain nonzero exit stays a quiet verdict.
        let mut r = reducer_for("false {}");
        let mut o = outcome();
        o.exit = Some(1);
        r.last = Some(o);
        let (msg, code) = uninteresting_message(&r, false);
        assert_eq!(code, 1);
        assert!(msg.contains("exited nonzero"), "{msg}");
    }

    #[test]
    fn env_failures_abort() {
        let mut r = reducer_for("true {}");
        r.tmp = std::env::temp_dir().join(format!(
            "cs-nope-{}-{}/candidate",
            std::process::id(),
            crate::driver::unique_tag()
        ));
        let forms = vec![parse_one("(def a 1)").unwrap()];
        for _ in 0..3 {
            assert!(!r.is_interesting(&forms));
        }
        assert!(r.aborted());
        assert!(r.env_error.as_deref().unwrap().contains("cannot stage"));
    }

    #[cfg(unix)]
    #[test]
    fn exit_127_existing_command_is_verdict() {
        // `sh` exists: a 127 from the test itself is a verdict (1),
        // not a usage error (2).
        let dir =
            std::env::temp_dir().join(format!("cs-127-{}-{}", std::process::id(), unique_tag()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("prog.clj");
        std::fs::write(&input, "(println :hello)\n").unwrap();
        let opts = ReduceOptions {
            test: "sh -c 'exit 127' {}".to_string(),
            input,
            output: None,
            tries: 100,
            timeout: 30,
            quiet: true,
            timeout_is_interesting: false,
        };
        assert_eq!(run_reduce(&opts), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn timeout_is_interesting_reduces_hang() {
        // Hangs iff the candidate mentions MAGIC: minimization must
        // proceed to the smallest hanging form instead of refusing input.
        let dir =
            std::env::temp_dir().join(format!("cs-hang-{}-{}", std::process::id(), unique_tag()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("prog.clj");
        let output = dir.join("small.clj");
        std::fs::write(&input, "(def MAGIC 1)\n(def b 2)\n").unwrap();
        let opts = ReduceOptions {
            test: "grep -q MAGIC {} && sleep 5".to_string(),
            input,
            output: Some(output.clone()),
            tries: 100,
            timeout: 1,
            quiet: true,
            timeout_is_interesting: true,
        };
        assert_eq!(run_reduce(&opts), 0);
        let reduced = std::fs::read_to_string(&output).unwrap();
        assert_eq!(reduced.trim(), "MAGIC", "{reduced:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn broken_test_command_is_usage_error() {
        let dir =
            std::env::temp_dir().join(format!("cs-red3-{}-{}", std::process::id(), unique_tag()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("prog.clj");
        std::fs::write(&input, "(println :hello)\n").unwrap();
        // A mistyped command must report exit 2, not "not interesting".
        let opts = ReduceOptions {
            test: "csmith-no-such-command-xyz {}".to_string(),
            input: input.clone(),
            output: None,
            tries: 100,
            timeout: 30,
            quiet: true,
            timeout_is_interesting: false,
        };
        assert_eq!(run_reduce(&opts), 2);
        // A working command with a nonzero exit stays "not interesting".
        let opts = ReduceOptions {
            test: "false {}".to_string(),
            input,
            output: None,
            tries: 100,
            timeout: 30,
            quiet: true,
            timeout_is_interesting: false,
        };
        assert_eq!(run_reduce(&opts), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
