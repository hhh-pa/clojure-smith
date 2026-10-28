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
}

struct Reducer {
    test: String,
    timeout: Duration,
    /// 0 = unlimited.
    limit: u64,
    used: u64,
    tmp: PathBuf,
}

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
        if std::fs::write(&self.tmp, &text).is_err() {
            return false;
        }
        let o = run_command(&self.test, &self.tmp, self.timeout);
        !o.timed_out && o.spawn_error.is_none() && o.exit == Some(0)
    }

    fn exhausted(&self) -> bool {
        self.limit > 0 && self.used >= self.limit
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
        Form::float(0.0),
        Form::boolean(false),
        Form::string(""),
        Form::nil(),
        Form::vector(vec![]),
        Form::map(vec![]),
    ]
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
            if r.exhausted() {
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
            if r.exhausted() {
                return forms;
            }
            let paths = collect_paths(&forms[idx]);
            let cur_size = forms[idx].size();
            let mut changed = false;
            'paths: for path in &paths {
                for c in &consts {
                    if r.exhausted() {
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

fn reduce_forms(r: &mut Reducer, mut forms: Vec<Form>) -> Vec<Form> {
    loop {
        let before = total_size(&forms);
        forms = ddmin_pass(r, forms);
        if r.exhausted() {
            return forms;
        }
        forms = scalar_pass(r, forms);
        if r.exhausted() {
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
    let tmp = std::env::temp_dir().join(format!(
        "clojure-smith-reduce-{}-{}",
        std::process::id(),
        crate::driver::unique_tag()
    ));
    let mut r = Reducer {
        test: opts.test.clone(),
        timeout: Duration::from_secs(opts.timeout),
        limit: opts.tries,
        used: 0,
        tmp: tmp.clone(),
    };
    if !r.is_interesting(&forms) {
        eprintln!("clojure-smith reduce: input is not interesting (test exited nonzero)");
        let _ = std::fs::remove_file(&tmp);
        return 1;
    }
    let before_forms = forms.len();
    let before_bytes = text.len();
    let reduced = reduce_forms(&mut r, forms);
    let _ = std::fs::remove_file(&tmp);

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
    use crate::form::parse_one;

    #[test]
    fn paths_and_replace() {
        let f = parse_one("(a [1 {:k v}])").unwrap();
        let paths = collect_paths(&f);
        assert!(paths.contains(&vec![]));
        assert!(paths.iter().all(|p| !p.is_empty() || true));
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
        let dir = std::env::temp_dir().join(format!("cs-reduce-{}", std::process::id()));
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
        let dir = std::env::temp_dir().join(format!("cs-red2-{}", std::process::id()));
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
        };
        assert_eq!(run_reduce(&opts), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
