//! Structural invariants over generated programs: fast checks (no Clojure
//! runtime needed) that complement the end-to-end integration tests.
//!
//! The headline invariant: an atom must never be written inside its own
//! `swap!` retry context — not directly, and not transitively through a
//! called function. Violating it makes `swap!` retry forever, because every
//! attempt changes the atom and fails the compare-and-swap.

use clojure_smith::cost::{
    NESTED_LOOP_TRIPS, WORK_BUDGET_PER_FUNC, acc_growth_cap, linear_growth_cap,
};
use clojure_smith::form::{Form, parse_all, print_compact};
use clojure_smith::gen_expr::{MAX_CALL_LOOP_TRIPS, MAX_SELF_SITES};
use clojure_smith::gen_program::{emit_program, gen_program};
use clojure_smith::options::parse_generator_args;
use clojure_smith::types::MAP_KEYS;
use std::collections::{HashMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};

fn sym(form: &Form) -> Option<&str> {
    match form {
        Form::Symbol(s) => Some(s.as_str()),
        _ => None,
    }
}

fn is_write_head(head: &str) -> bool {
    matches!(head, "swap!" | "reset!" | "compare-and-set!")
}

/// Collect the global targeted by a `swap!`/`reset!`/`compare-and-set!`
/// form, if syntactically apparent.
fn write_target(form: &Form) -> Option<&str> {
    let Form::List(fs) = form else {
        return None;
    };
    let head = sym(fs.first()?)?;
    if !is_write_head(head) || fs.len() < 2 {
        return None;
    }
    sym(&fs[1])
}

/// Direct writes and called user functions within `forms`.
fn analyze_calls(
    forms: &[Form],
    user_funcs: &HashSet<String>,
) -> (HashSet<String>, HashSet<String>) {
    let mut writes = HashSet::new();
    let mut calls = HashSet::new();
    let mut stack: Vec<&Form> = forms.iter().collect();
    while let Some(f) = stack.pop() {
        if let Some(g) = write_target(f) {
            writes.insert(g.to_string());
        }
        if let Form::List(fs) = f
            && let Some(head) = fs.first().and_then(sym)
        {
            if user_funcs.contains(head) {
                calls.insert(head.to_string());
            }
            // Higher-order position: (mapv f coll), (reduce f init coll).
            if matches!(head, "mapv" | "filterv" | "reduce")
                && let Some(fname) = fs.get(1).and_then(sym)
                && user_funcs.contains(fname)
            {
                calls.insert(fname.to_string());
            }
        }
        stack.extend(f.children());
    }
    (writes, calls)
}

/// Transitive write sets per function (fixpoint over the call graph).
fn transitive_writes(bodies: &HashMap<String, Vec<Form>>) -> HashMap<String, HashSet<String>> {
    let user_funcs: HashSet<String> = bodies.keys().cloned().collect();
    let mut direct: HashMap<String, HashSet<String>> = HashMap::new();
    let mut calls: HashMap<String, HashSet<String>> = HashMap::new();
    for (name, body) in bodies {
        let (w, c) = analyze_calls(body, &user_funcs);
        direct.insert(name.clone(), w);
        calls.insert(name.clone(), c);
    }
    let mut trans = direct.clone();
    for _ in 0..10 {
        let mut changed = false;
        for (name, callee_set) in &calls {
            let mut add = HashSet::new();
            for c in callee_set {
                if c == name {
                    continue;
                }
                if let Some(w) = trans.get(c) {
                    add.extend(w.iter().cloned());
                }
            }
            let entry = trans.entry(name.clone()).or_default();
            let before = entry.len();
            entry.extend(add);
            changed |= entry.len() != before;
        }
        if !changed {
            break;
        }
    }
    trans
}

/// Walk `form`, tracking the stack of atoms whose `swap!` retry context we
/// are inside, and report any write (direct or via a called function) to a
/// forbidden atom.
fn check_form(
    form: &Form,
    forbidden: &mut Vec<String>,
    writes_of: &HashMap<String, HashSet<String>>,
    user_funcs: &HashSet<String>,
    errors: &mut Vec<String>,
) {
    if let Form::List(fs) = form
        && let Some(head) = fs.first().and_then(sym)
    {
        if is_write_head(head)
            && let Some(g) = fs.get(1).and_then(sym)
        {
            if forbidden.iter().any(|f| f == g) {
                errors.push(format!("{head} on {g} inside its own swap context"));
            }
            // Only swap!'s function body runs inside the retry
            // context; reset!/compare-and-set! arguments are
            // evaluated strictly before.
            if head == "swap!" {
                forbidden.push(g.to_string());
                for child in &fs[2..] {
                    check_form(child, forbidden, writes_of, user_funcs, errors);
                }
                forbidden.pop();
                return;
            }
        }
        if user_funcs.contains(head)
            && let Some(writes) = writes_of.get(head)
        {
            for f in forbidden.iter() {
                if writes.contains(f) {
                    errors.push(format!(
                        "call to {head} (writes {f}) inside {f}'s swap context"
                    ));
                }
            }
        }
        if matches!(head, "mapv" | "filterv" | "reduce")
            && let Some(fname) = fs.get(1).and_then(sym)
            && let Some(writes) = writes_of.get(fname)
        {
            for f in forbidden.iter() {
                if writes.contains(f) {
                    errors.push(format!(
                        "{head} over {fname} (writes {f}) inside {f}'s swap context"
                    ));
                }
            }
        }
    }
    for child in form.children() {
        check_form(child, forbidden, writes_of, user_funcs, errors);
    }
}

/// Check a whole program. Returns human-readable violations.
fn check_program(forms: &[Form]) -> Vec<String> {
    // Collect function bodies: (defn name [params] body...).
    let mut bodies: HashMap<String, Vec<Form>> = HashMap::new();
    for f in forms {
        if let Form::List(fs) = f
            && fs.len() >= 4
            && sym(&fs[0]) == Some("defn")
            && let Some(name) = sym(&fs[1])
        {
            // Only user functions (f0, f1, ...) participate in the
            // call graph; helpers are pure.
            if name.starts_with('f') && name[1..].chars().all(|c| c.is_ascii_digit()) {
                bodies.insert(name.to_string(), fs[3..].to_vec());
            }
        }
    }
    let user_funcs: HashSet<String> = bodies.keys().cloned().collect();
    let writes_of = transitive_writes(&bodies);
    let mut errors = Vec::new();
    for (name, body) in &bodies {
        for f in body {
            let mut forbidden = Vec::new();
            let before = errors.len();
            check_form(f, &mut forbidden, &writes_of, &user_funcs, &mut errors);
            for e in errors.iter_mut().skip(before) {
                *e = format!("{name}: {e}");
            }
        }
    }
    // Entry point calls run outside any swap context; still walk them for
    // direct violations (there should be no writes at all).
    for f in forms {
        if let Form::List(fs) = f {
            let is_entry =
                fs.len() >= 2 && sym(&fs[0]) == Some("defn") && sym(&fs[1]) == Some("smith-main");
            if is_entry {
                let mut forbidden = Vec::new();
                check_form(f, &mut forbidden, &writes_of, &user_funcs, &mut errors);
            }
        }
    }
    errors.sort();
    errors.dedup();
    errors
}

#[test]
fn no_nested_same_atom_writes() {
    let opts = parse_generator_args(Vec::<String>::new()).unwrap();
    for seed in 0..60 {
        let prog = gen_program(&opts, seed);
        let errors = check_program(&prog.forms);
        assert!(
            errors.is_empty(),
            "seed {seed} violates the swap invariant: {errors:?}"
        );
    }
}

#[test]
fn no_nested_same_atom_writes_under_options() {
    for args in [
        vec!["--max-funcs", "8", "--max-block-size", "6"],
        vec!["--max-globals", "1"],
        vec!["--no-loops", "--no-records"],
        vec!["--no-safe-math"],
    ] {
        let opts = parse_generator_args(args.clone()).unwrap();
        for seed in [0, 1, 5, 22, 42] {
            let prog = gen_program(&opts, seed);
            let errors = check_program(&prog.forms);
            assert!(errors.is_empty(), "args {args:?} seed {seed}: {errors:?}");
        }
    }
}

// ---------------------------------------------------------------------------
// Recursion and work bounds: with fuel depth `d`, a recursive function must
// make at most MAX_SELF_SITES^d self calls, and loops whose bodies call user
// functions must be trip-scaled — otherwise runtime work blows up
// exponentially and programs time out despite terminating in theory.
// ---------------------------------------------------------------------------

fn is_loop_head(head: &str) -> bool {
    matches!(head, "loop" | "dotimes" | "doseq" | "for")
}

/// Scan a function body for self calls: returns (plain sites, under-loop
/// sites, higher-order uses).
fn scan_self(body: &[Form], own: &str) -> (usize, usize, usize) {
    let (mut sites, mut looped, mut hof) = (0, 0, 0);
    let mut stack: Vec<(&Form, u32)> = body.iter().map(|f| (f, 0)).collect();
    while let Some((f, depth)) = stack.pop() {
        if let Form::List(fs) = f
            && let Some(head) = fs.first().and_then(sym)
        {
            if head == own {
                if depth > 0 {
                    looped += 1;
                } else {
                    sites += 1;
                }
            }
            if matches!(head, "mapv" | "filterv" | "reduce") && fs.get(1).and_then(sym) == Some(own)
            {
                hof += 1;
            }
            if is_loop_head(head) {
                for c in f.children() {
                    stack.push((c, depth + 1));
                }
                continue;
            }
        }
        for c in f.children() {
            stack.push((c, depth));
        }
    }
    (sites, looped, hof)
}

fn user_bodies(forms: &[Form]) -> Vec<(String, Vec<Form>)> {
    let mut out = Vec::new();
    for f in forms {
        if let Form::List(fs) = f
            && fs.len() >= 4
            && sym(&fs[0]) == Some("defn")
            && let Some(name) = sym(&fs[1])
            && name.starts_with('f')
            && name[1..].chars().all(|c| c.is_ascii_digit())
        {
            out.push((name.to_string(), fs[3..].to_vec()));
        }
    }
    out
}

#[test]
fn recursion_is_bounded() {
    let opt_sets: Vec<Vec<&str>> = vec![
        vec![],
        vec![
            "--max-funcs",
            "8",
            "--max-block-size",
            "8",
            "--max-recursion-depth",
            "8",
        ],
        vec!["--max-loop-iters", "32"],
    ];
    for args in &opt_sets {
        let opts = parse_generator_args(args.clone()).unwrap();
        for seed in 0..60 {
            let prog = gen_program(&opts, seed);
            for (name, body) in user_bodies(&prog.forms) {
                let (sites, looped, hof) = scan_self(&body, &name);
                assert_eq!(
                    looped, 0,
                    "{args:?} seed {seed} {name}: self call under loop"
                );
                assert_eq!(hof, 0, "{args:?} seed {seed} {name}: self in HOF position");
                assert!(
                    sites <= MAX_SELF_SITES as usize,
                    "{args:?} seed {seed} {name}: {sites} self sites"
                );
            }
        }
    }
}

/// True when any of `forms` calls a user function.
fn body_has_call(forms: &[Form], user_funcs: &HashSet<String>) -> bool {
    let mut stack: Vec<&Form> = forms.iter().collect();
    while let Some(f) = stack.pop() {
        if let Form::List(fs) = f
            && let Some(head) = fs.first().and_then(sym)
        {
            if user_funcs.contains(head) {
                return true;
            }
            if matches!(head, "mapv" | "filterv" | "reduce")
                && let Some(g) = fs.get(1).and_then(sym)
                && user_funcs.contains(g)
            {
                return true;
            }
        }
        stack.extend(f.children());
    }
    false
}

/// Check trip scaling of every loop in `forms`, appending violations.
fn check_trips(forms: &[Form], user_funcs: &HashSet<String>, errors: &mut Vec<String>) {
    let mut stack: Vec<&Form> = forms.iter().collect();
    while let Some(f) = stack.pop() {
        let Form::List(fs) = f else {
            stack.extend(f.children());
            continue;
        };
        let Some(head) = fs.first().and_then(sym) else {
            stack.extend(f.children());
            continue;
        };
        match head {
            // (loop [i INIT acc INIT2] (if (< i N) (recur … UPDATE) …))
            "loop" if fs.len() == 3 => {
                if let Form::List(cond) = &fs[2]
                    && cond.len() == 4
                    && sym(&cond[0]) == Some("if")
                    && let Form::List(test) = &cond[1]
                    && test.len() == 3
                    && let Form::Int(n) = &test[2]
                {
                    let update = &cond[2];
                    if body_has_call(std::slice::from_ref(update), user_funcs)
                        && *n > MAX_CALL_LOOP_TRIPS
                    {
                        errors.push(format!("loop with calls trips {n}"));
                    }
                }
                stack.extend(f.children());
            }
            // (dotimes [i N] body…)
            "dotimes" if fs.len() >= 3 => {
                if let Form::Vector(bind) = &fs[1]
                    && bind.len() == 2
                    && let Form::Int(n) = &bind[1]
                    && body_has_call(&fs[2..], user_funcs)
                    && *n > MAX_CALL_LOOP_TRIPS
                {
                    errors.push(format!("dotimes with calls trips {n}"));
                }
                stack.extend(f.children());
            }
            // (doseq [x C …] body…) / (vec (for [x C … (:when …)] body))
            "doseq" | "for" if fs.len() >= 3 => {
                if let Form::Vector(flat) = &fs[1] {
                    let mut colls = Vec::new();
                    let mut i = 0;
                    while i + 1 < flat.len() {
                        if matches!(&flat[i], Form::Keyword(k) if k == "when") {
                            break;
                        }
                        colls.push(&flat[i + 1]);
                        i += 2;
                    }
                    let mut rest: Vec<&Form> = fs[2..].iter().collect();
                    if i + 1 < flat.len() {
                        rest.push(&flat[i + 1]); // :when condition runs per trip
                    }
                    let rest: Vec<Form> = rest.into_iter().cloned().collect();
                    if body_has_call(&rest, user_funcs) {
                        for c in colls {
                            // The work budget may shrink the cap below 3;
                            // what matters is that trips stay bounded.
                            let capped = matches!(c,
                                Form::List(fs)
                                if fs.len() == 3 && sym(&fs[0]) == Some("take")
                                && matches!(&fs[1], Form::Int(n) if (0..=MAX_CALL_LOOP_TRIPS).contains(n)));
                            if !capped {
                                errors.push(format!("{head} with calls and uncapped coll"));
                            }
                        }
                    }
                }
                stack.extend(f.children());
            }
            _ => stack.extend(f.children()),
        }
    }
}

#[test]
fn loops_with_calls_are_scaled() {
    let opt_sets: Vec<Vec<&str>> = vec![
        vec![],
        vec!["--max-funcs", "8", "--max-block-size", "8"],
        vec!["--max-loop-iters", "64"],
    ];
    for args in &opt_sets {
        let opts = parse_generator_args(args.clone()).unwrap();
        for seed in 0..60 {
            let prog = gen_program(&opts, seed);
            let bodies = user_bodies(&prog.forms);
            let user_funcs: HashSet<String> = bodies.iter().map(|(n, _)| n.clone()).collect();
            for (name, body) in &bodies {
                let mut errors = Vec::new();
                check_trips(body, &user_funcs, &mut errors);
                assert!(errors.is_empty(), "{args:?} seed {seed} {name}: {errors:?}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// String/vector shape discipline: `str/split` returns a vector, so it must
// never appear where a string is required. Seed 309 (large options) once
// generated `(str/trim (str/split ...))`, which throws ClassCastException.
// ---------------------------------------------------------------------------

/// Call heads that require a string argument, with the string positions.
const STRICT_STR_ARGS: &[(&str, &[usize])] = &[
    ("s-subs", &[0]),
    ("str/blank?", &[0]),
    ("str/ends-with?", &[0]),
    ("str/includes?", &[0]),
    ("str/join", &[0]),
    ("str/reverse", &[0]),
    ("str/starts-with?", &[0]),
    ("str/trim", &[0]),
];

fn is_split(form: &Form) -> bool {
    matches!(form, Form::List(fs) if fs.first().and_then(sym) == Some("str/split"))
}

/// Every `str/split` occurrence must sit in vector position: none may be a
/// direct string-typed argument of a strict consumer. Returns violations and
/// the number of splits seen.
fn check_split_positions(forms: &[Form]) -> (Vec<String>, usize) {
    let mut errors = Vec::new();
    let mut splits = 0;
    let mut stack: Vec<&Form> = forms.iter().collect();
    while let Some(f) = stack.pop() {
        if is_split(f) {
            splits += 1;
        }
        if let Form::List(fs) = f
            && let Some(head) = fs.first().and_then(sym)
            && let Some((_, idxs)) = STRICT_STR_ARGS.iter().find(|(name, _)| *name == head)
        {
            for &i in *idxs {
                if matches!(fs.get(i + 1), Some(arg) if is_split(arg)) {
                    errors.push(format!("{head} arg {i} is str/split"));
                }
            }
        }
        stack.extend(f.children());
    }
    (errors, splits)
}

#[test]
fn split_never_in_string_position() {
    let opt_sets: Vec<Vec<&str>> = vec![
        vec![],
        vec!["--max-funcs", "8", "--max-block-size", "8"],
        vec![
            "--max-funcs",
            "8",
            "--max-expr-depth",
            "6",
            "--max-block-size",
            "6",
            "--max-loop-iters",
            "32",
        ],
    ];
    let mut total_splits = 0;
    for args in &opt_sets {
        let opts = parse_generator_args(args.clone()).unwrap();
        for seed in 0..200 {
            let prog = gen_program(&opts, seed);
            let (errors, splits) = check_split_positions(&prog.forms);
            assert!(errors.is_empty(), "{args:?} seed {seed}: {errors:?}");
            total_splits += splits;
        }
    }
    // Smoke check: split is still generated (in vector positions), so the
    // invariant above is not vacuous.
    assert!(total_splits > 0, "no str/split generated in sweep");
}

// ---------------------------------------------------------------------------
// Double division: raw `/` is not total — the `/` inline resolves
// `Numbers/divide` by static operand type, and the (Object, Object)
// overload throws "Divide by zero" on a zero divisor (only the primitive
// (double, double) path yields Inf/NaN). Generated code must therefore use
// `s-div` whenever safe math is on (seed 307 threw exactly this way).
// ---------------------------------------------------------------------------

/// True for preamble helper definitions, which are exempt from the scan
/// (`s-div`'s own body legitimately contains a primitive-typed `/`).
fn is_helper_defn(form: &Form) -> bool {
    if let Form::List(fs) = form
        && fs.len() >= 2
        && sym(&fs[0]) == Some("defn")
        && let Some(name) = sym(&fs[1])
    {
        return name.starts_with("s-") || name.starts_with("smith-");
    }
    false
}

/// Count raw `/` calls and `s-div` calls in generated (non-preamble) code.
fn count_division(forms: &[Form]) -> (usize, usize) {
    let (mut raw, mut safe) = (0, 0);
    let mut stack: Vec<&Form> = forms.iter().filter(|f| !is_helper_defn(f)).collect();
    while let Some(f) = stack.pop() {
        if let Form::List(fs) = f
            && let Some(head) = fs.first().and_then(sym)
        {
            match head {
                "/" => raw += 1,
                "s-div" => safe += 1,
                _ => {}
            }
        }
        stack.extend(f.children());
    }
    (raw, safe)
}

#[test]
fn no_raw_division_under_safe_math() {
    let opt_sets: Vec<Vec<&str>> = vec![
        vec![],
        vec!["--max-funcs", "8", "--max-block-size", "8"],
        vec![
            "--max-funcs",
            "8",
            "--max-expr-depth",
            "6",
            "--max-block-size",
            "6",
            "--max-loop-iters",
            "32",
        ],
    ];
    let mut total_safe = 0;
    for args in &opt_sets {
        let opts = parse_generator_args(args.clone()).unwrap();
        for seed in 0..200 {
            let prog = gen_program(&opts, seed);
            let (raw, safe) = count_division(&prog.forms);
            assert_eq!(raw, 0, "{args:?} seed {seed}: raw `/` in generated code");
            total_safe += safe;
        }
    }
    // Smoke check: s-div is actually emitted, so the ban is not vacuous.
    assert!(total_safe > 0, "no s-div generated in sweep");
}

// ---------------------------------------------------------------------------
// Work budget structure: every trip multiplier and fuel value in generated
// code is a shrunk literal, so the static cost model holds by construction:
// doseq/for/mapv/filterv/reduce collections are always `(take K ...)` with
// a literal K, loop trip counts are literals within max, fuel arguments are
// literals within range, and self calls always pass decremented fuel.
// ---------------------------------------------------------------------------

/// True when `form` is `(take K ...)` with a literal non-negative `K`.
fn is_take_cap(form: &Form) -> bool {
    matches!(form,
        Form::List(fs)
        if fs.len() == 3 && sym(&fs[0]) == Some("take") && matches!(&fs[1], Form::Int(n) if *n >= 0))
}

/// (name, is_recursive) for user functions.
fn user_sigs(forms: &[Form]) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    for f in forms {
        if let Form::List(fs) = f
            && fs.len() >= 4
            && sym(&fs[0]) == Some("defn")
            && let Some(name) = sym(&fs[1])
            && name.starts_with('f')
            && name[1..].chars().all(|c| c.is_ascii_digit())
            && let Form::Vector(params) = &fs[2]
        {
            let rec = params.first().and_then(sym) == Some("fuel");
            out.push((name.to_string(), rec));
        }
    }
    out
}

fn check_body_budget(
    name: &str,
    body: &[Form],
    sigs: &[(String, bool)],
    max_iters: i64,
    max_fuel: i64,
    errors: &mut Vec<String>,
) {
    let is_user = |h: &str| sigs.iter().any(|(m, _)| m == h);
    let is_rec = |h: &str| sigs.iter().any(|(m, r)| m == h && *r);
    let mut stack: Vec<&Form> = body.iter().collect();
    while let Some(f) = stack.pop() {
        if let Form::List(fs) = f
            && let Some(head) = fs.first().and_then(sym)
        {
            match head {
                "doseq" | "for" if fs.len() >= 3 => {
                    if let Form::Vector(flat) = &fs[1] {
                        let mut i = 0;
                        while i + 1 < flat.len() {
                            if matches!(&flat[i], Form::Keyword(k) if k == "when") {
                                break;
                            }
                            if !is_take_cap(&flat[i + 1]) {
                                errors.push(format!("{name}: {head} coll is not (take K>=0 ...)"));
                            }
                            i += 2;
                        }
                    }
                }
                "mapv" | "filterv" if fs.len() == 3 => {
                    if !is_take_cap(&fs[2]) {
                        errors.push(format!("{name}: {head} coll is not (take K>=0 ...)"));
                    }
                }
                "reduce" if fs.len() == 4 => {
                    if !is_take_cap(&fs[3]) {
                        errors.push(format!("{name}: reduce coll is not (take K>=0 ...)"));
                    }
                }
                // (loop [i (identity 0) acc ..] (if (< i N) ..)): N in range.
                "loop" if fs.len() == 3 => {
                    let mut ok = false;
                    if let Form::List(cond) = &fs[2]
                        && cond.len() == 4
                        && let Form::List(test) = &cond[1]
                        && test.len() == 3
                        && let Form::Int(n) = &test[2]
                        && (0..=max_iters).contains(n)
                    {
                        ok = true;
                    }
                    if !ok {
                        errors.push(format!("{name}: loop trips not a literal in range"));
                    }
                }
                // (dotimes [i N] ..): N in range.
                "dotimes" if fs.len() >= 3 => {
                    let ok = matches!(&fs[1],
                        Form::Vector(bind) if bind.len() == 2
                        && matches!(&bind[1], Form::Int(n) if (0..=max_iters).contains(n)));
                    if !ok {
                        errors.push(format!("{name}: dotimes count not a literal in range"));
                    }
                }
                h if is_user(h) && is_rec(h) => {
                    let self_fuel = h == name
                        && matches!(fs.get(1),
                            Some(Form::List(d))
                            if d.len() == 2
                            && matches!(sym(&d[0]), Some("s-dec" | "dec"))
                            && sym(&d[1]) == Some("fuel"));
                    // Fuel draws are inclusive on both ends.
                    let lit_fuel = matches!(fs.get(1),
                        Some(Form::Int(k)) if (0..=max_fuel).contains(k));
                    if !(self_fuel || lit_fuel) {
                        errors.push(format!("{name}: call to {h} without fuel literal"));
                    }
                }
                _ => {}
            }
        }
        stack.extend(f.children());
    }
}

#[test]
fn work_budget_shapes_hold() {
    let opt_sets: Vec<Vec<&str>> = vec![
        vec![],
        vec!["--max-funcs", "8", "--max-block-size", "8"],
        vec![
            "--max-funcs",
            "8",
            "--max-expr-depth",
            "6",
            "--max-block-size",
            "6",
            "--max-loop-iters",
            "32",
        ],
        vec!["--no-safe-math"],
        vec!["--max-recursion-depth", "3", "--max-loop-iters", "4"],
    ];
    for args in &opt_sets {
        let opts = parse_generator_args(args.clone()).unwrap();
        let max_iters = opts.max_loop_iters as i64;
        let max_fuel = opts.max_recursion_depth as i64;
        for seed in 0..200 {
            let prog = gen_program(&opts, seed);
            let sigs = user_sigs(&prog.forms);
            let mut errors = Vec::new();
            for (fname, body) in user_bodies(&prog.forms) {
                check_body_budget(&fname, &body, &sigs, max_iters, max_fuel, &mut errors);
            }
            // Entry calls carry shrunk fuel literals too.
            for f in &prog.forms {
                if let Form::List(fs) = f
                    && fs.len() >= 4
                    && sym(&fs[0]) == Some("defn")
                    && sym(&fs[1]) == Some("smith-main")
                {
                    check_body_budget("<entry>", &fs[3..], &sigs, max_iters, max_fuel, &mut errors);
                }
            }
            assert!(errors.is_empty(), "{args:?} seed {seed}: {errors:?}");
        }
    }
}

// ---------------------------------------------------------------------------
// Flag suppression matrix: every `--no-*` flag must actually suppress its
// constructs — and must never panic the generator. Forced-type sites that
// bypass kind gating (doseq's vector coll under `--no-vectors` panicked
// with exit 101) are exactly what this catches.
// ---------------------------------------------------------------------------

/// Banned call heads per suppression flag (checked in user function bodies).
fn banned_heads(flag: &str) -> &'static [&'static str] {
    match flag {
        "--no-vectors" => &[
            "vec",
            "vector?",
            "mapv",
            "filterv",
            "doseq",
            "for",
            "range",
            "s-nth",
            "s-peek",
            "s-pop",
            "s-subvec",
            "s-assoc-v",
            "s-take",
            "s-drop",
            "conj",
            "interleave",
            "concat",
            "sort",
            "distinct",
            "vals",
            "str/split",
        ],
        "--no-maps" => &[
            "dissoc",
            "select-keys",
            "merge",
            "zipmap",
            "s-get",
            "vals",
            "map?",
        ],
        "--no-strings" => &[
            "str",
            "name",
            "s-subs",
            "str/split",
            "str/join",
            "str/trim",
            "str/reverse",
            "str/blank?",
            "str/includes?",
            "str/starts-with?",
            "str/ends-with?",
            "string?",
        ],
        "--no-doubles" => &["double", "s-div"],
        "--no-loops" => &["loop", "dotimes", "doseq", "for"],
        "--no-higher-order" => &["mapv", "filterv", "reduce"],
        "--no-bitops" => &[
            "bit-and",
            "bit-or",
            "bit-xor",
            "bit-not",
            "bit-shift-left",
            "bit-shift-right",
            "unsigned-bit-shift-right",
        ],
        "--no-divs" => &["quot", "rem", "/", "s-quot", "s-rem", "s-div"],
        "--no-globals" => &["atom", "swap!", "reset!", "compare-and-set!"],
        _ => &[],
    }
}

/// Walk `form`, flagging vector *values* while exempting binding-vector
/// syntax (`let`/`loop`/`doseq`/`for`/`dotimes`/`fn` position 1, `defn`
/// position 2). Binding contents are still scanned: a collection expression
/// there is a value, so a vector inside one is a violation.
fn check_no_vector_values(form: &Form, fname: &str, errors: &mut Vec<String>) {
    match form {
        Form::Vector(_) => errors.push(format!("{fname}: vector value under --no-vectors")),
        Form::List(fs) => {
            let head = fs.first().and_then(sym);
            for (i, c) in fs.iter().enumerate().skip(1) {
                let is_binding = matches!(
                    head,
                    Some("let" | "loop" | "doseq" | "for" | "dotimes" | "fn")
                ) && i == 1
                    || matches!(head, Some("defn")) && i == 2;
                if is_binding && let Form::Vector(items) = c {
                    for b in items {
                        check_no_vector_values(b, fname, errors);
                    }
                } else {
                    check_no_vector_values(c, fname, errors);
                }
            }
        }
        Form::Map(ps) => {
            for (k, v) in ps {
                check_no_vector_values(k, fname, errors);
                check_no_vector_values(v, fname, errors);
            }
        }
        _ => {
            for c in form.children() {
                check_no_vector_values(c, fname, errors);
            }
        }
    }
}

fn check_flag_suppression(
    flag: &str,
    forms: &[Form],
    bodies: &[(String, Vec<Form>)],
    errors: &mut Vec<String>,
) {
    let banned = banned_heads(flag);
    for (fname, body) in bodies {
        let mut stack: Vec<&Form> = body.iter().collect();
        while let Some(f) = stack.pop() {
            match f {
                Form::List(fs)
                    if fs
                        .first()
                        .and_then(sym)
                        .is_some_and(|h| banned.contains(&h)) =>
                {
                    let h = fs.first().and_then(sym).unwrap();
                    errors.push(format!("{fname}: banned head `{h}` under {flag}"));
                }
                Form::List(fs)
                    if flag == "--no-maps" && fs.first().and_then(sym) == Some("assoc") =>
                {
                    // Map assoc uses pooled :k* keys; record assoc uses
                    // field names, and anything else is fail-closed.
                    let map_key = matches!(fs.get(2),
                        Some(Form::Keyword(k)) if MAP_KEYS.contains(&k.as_str()));
                    let non_key = !matches!(fs.get(2), Some(Form::Keyword(_)));
                    if map_key || non_key {
                        errors.push(format!("{fname}: map assoc under --no-maps"));
                    }
                }
                Form::Map(_) if flag == "--no-maps" => {
                    errors.push(format!("{fname}: map literal under --no-maps"));
                }
                Form::Str(_) | Form::Char(_) | Form::Regex(_) if flag == "--no-strings" => {
                    errors.push(format!("{fname}: string literal under --no-strings"));
                }
                Form::Float(_) if flag == "--no-doubles" => {
                    errors.push(format!("{fname}: double literal under --no-doubles"));
                }
                Form::Deref(_) if flag == "--no-globals" => {
                    errors.push(format!("{fname}: deref under --no-globals"));
                }
                _ => {}
            }
            stack.extend(f.children());
        }
        if flag == "--no-vectors" {
            for f in body {
                check_no_vector_values(f, fname, errors);
            }
        }
    }
    // Top-level forms: record/global definitions live outside bodies.
    for f in forms {
        if let Form::List(fs) = f
            && let Some(head) = fs.first().and_then(sym)
        {
            match head {
                "defrecord" if flag == "--no-records" => {
                    errors.push("<top>: defrecord under --no-records".to_string())
                }
                "def" if flag == "--no-globals" => {
                    errors.push("<top>: global def under --no-globals".to_string())
                }
                "defn" if flag == "--no-records" => {
                    // Record constructors are `->recN`; plain `->` is the
                    // threading macro and is fine.
                    let mut stack = vec![f];
                    while let Some(g) = stack.pop() {
                        if let Form::List(gs) = g
                            && gs.first().and_then(sym).is_some_and(|h| {
                                h.starts_with("->rec") && h[4..].chars().all(|c| c.is_ascii_digit())
                            })
                        {
                            errors.push("<top>: record ctor under --no-records".to_string());
                            break;
                        }
                        stack.extend(g.children());
                    }
                }
                "defn" if flag == "--no-recursion" => {
                    if let Some(Form::Vector(params)) = fs.get(2)
                        && params.first().and_then(sym) == Some("fuel")
                    {
                        errors.push("<top>: fuel param under --no-recursion".to_string());
                    }
                }
                _ => {}
            }
        }
    }
}

#[test]
fn suppression_matrix() {
    let flags = [
        "--no-vectors",
        "--no-maps",
        "--no-strings",
        "--no-doubles",
        "--no-records",
        "--no-loops",
        "--no-recursion",
        "--no-globals",
        "--no-higher-order",
        "--no-bitops",
        "--no-divs",
        "--no-safe-math",
        "--no-checksum",
    ];
    let mut configs: Vec<Vec<&str>> = flags.iter().map(|f| vec![*f]).collect();
    // Kitchen sink: everything off at once must still generate cleanly.
    configs.push(flags.to_vec());
    let mut failures = Vec::new();
    for args in &configs {
        let opts = parse_generator_args(args.clone()).unwrap();
        for seed in 0..50 {
            let prog = match catch_unwind(AssertUnwindSafe(|| gen_program(&opts, seed))) {
                Ok(p) => p,
                Err(_) => {
                    failures.push(format!("{args:?} seed {seed}: generator panicked"));
                    continue;
                }
            };
            let bodies = user_bodies(&prog.forms);
            let mut errors = Vec::new();
            for flag in args.iter() {
                check_flag_suppression(flag, &prog.forms, &bodies, &mut errors);
            }
            for e in errors {
                failures.push(format!("{args:?} seed {seed}: {e}"));
            }
            if failures.len() > 40 {
                break;
            }
        }
        if failures.len() > 40 {
            break;
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Heads whose value is definitely a growable collection or string, so a
/// `loop` update headed by one is subject to the accumulator growth cap.
/// Type-directed generation guarantees the update's type matches its head;
/// `if`/`let`/calls and friends are skipped (missed coverage, never a
/// false positive). Bare `assoc` is handled separately: map assoc uses
/// pooled `:k*` keys while record assoc (fixed fields, uncapped) uses
/// field names.
const GROWABLE_UPDATE_HEADS: &[&str] = &[
    "vec",
    "vector",
    "conj",
    "interleave",
    "concat",
    "merge",
    "zipmap",
    "str",
    "subvec",
    "s-take",
    "s-drop",
    "sort",
    "distinct",
    "mapv",
    "filterv",
    "select-keys",
    "dissoc",
    "s-pop",
    "s-assoc-v",
    "vals",
    "str/split",
];

/// A `loop` update mentioning a growable accumulator twice or more must
/// have its trips capped: the memory growth is geometric while the work
/// budget is linear. Regression test for the seed-496 OOM
/// (`(vec (interleave acc acc))` with 42 trips under high
/// `--max-loop-iters`).
#[test]
fn loop_acc_growth_is_bounded() {
    let opt_sets: Vec<Vec<&str>> = vec![
        vec![],
        vec!["--max-funcs", "8", "--max-block-size", "8"],
        vec!["--max-loop-iters", "100000"],
    ];
    let mut checked = 0;
    let mut checked_linear = 0;
    let mut checked_nested = 0;
    for args in &opt_sets {
        let opts = parse_generator_args(args.clone()).unwrap();
        for seed in 0..150 {
            let prog = gen_program(&opts, seed);
            let bodies = user_bodies(&prog.forms);
            for (name, body) in &bodies {
                let mut stack: Vec<&Form> = body.iter().collect();
                while let Some(f) = stack.pop() {
                    let Form::List(fs) = f else {
                        stack.extend(f.children());
                        continue;
                    };
                    if sym(&fs[0]) != Some("loop") || fs.len() != 3 {
                        stack.extend(f.children());
                        continue;
                    }
                    // (loop [i _ acc (identity INIT)] (if (< i N) (recur _ UPDATE) _))
                    let mut growable = false;
                    if let Form::Vector(bind) = &fs[1]
                        && bind.len() == 4
                        && let Form::List(id) = &bind[3]
                        && id.len() == 2
                        && sym(&id[0]) == Some("identity")
                        && matches!(&id[1], Form::Vector(_) | Form::Map(_) | Form::Str(_))
                    {
                        growable = true;
                    }
                    if let Form::List(cond) = &fs[2]
                        && cond.len() == 4
                        && sym(&cond[0]) == Some("if")
                        && let Form::List(test) = &cond[1]
                        && test.len() == 3
                        && let Form::Int(n) = &test[2]
                        && let Form::List(recur) = &cond[2]
                        && recur.len() == 3
                        && sym(&recur[0]) == Some("recur")
                    {
                        let update = &recur[2];
                        if let Form::List(ufs) = update
                            && let Some(h) = ufs.first().and_then(sym)
                        {
                            if GROWABLE_UPDATE_HEADS.contains(&h) {
                                growable = true;
                            }
                            if h == "assoc"
                                && matches!(ufs.get(2),
                                    Some(Form::Keyword(k)) if MAP_KEYS.contains(&k.as_str()))
                            {
                                growable = true;
                            }
                        }
                        let refs = update.count_sym("acc");
                        if growable && refs > 1 {
                            checked += 1;
                            let cap = acc_growth_cap(refs);
                            assert!(
                                *n <= cap,
                                "{args:?} seed {seed} {name}: growable acc x{refs} with {n} trips (cap {cap})"
                            );
                        }
                        // Linear growth: trips * update bytes fits the
                        // budget. The generator caps against the live
                        // remainder; the full budget is the sound static
                        // over-approximation.
                        if growable && refs == 1 {
                            checked_linear += 1;
                            let bytes = print_compact(update).len() as u64;
                            let cap = linear_growth_cap(bytes, WORK_BUDGET_PER_FUNC);
                            assert!(
                                *n <= cap,
                                "{args:?} seed {seed} {name}: linear acc +{bytes}B x{refs} with {n} trips (cap {cap})"
                            );
                        }
                        // Nested loops multiply production opaquely.
                        if growable && has_nested_loop(update) {
                            checked_nested += 1;
                            assert!(
                                *n <= NESTED_LOOP_TRIPS,
                                "{args:?} seed {seed} {name}: nested loop with {n} trips (cap {NESTED_LOOP_TRIPS})"
                            );
                        }
                    }
                    stack.extend(f.children());
                }
            }
        }
    }
    assert!(checked > 0, "no multi-ref growable loops observed");
    assert!(checked_linear > 0, "no linear growable loops observed");
    assert!(checked_nested > 0, "no nested growable loops observed");
}

/// True when `form` contains a loop form of any kind (mirrors the
/// generator's nesting guard).
fn has_nested_loop(form: &Form) -> bool {
    let mut stack = vec![form];
    while let Some(f) = stack.pop() {
        if matches!(f.head_symbol(), Some("loop" | "for" | "dotimes" | "doseq")) {
            return true;
        }
        stack.extend(f.children());
    }
    false
}

/// Every `(range n)` is shrunk to fit the remaining budget and charged
/// per element, so `n` never exceeds the per-function budget no matter
/// how large `--max-loop-iters` draws. Regression test: an 800k range
/// used to be free, blowing time and memory outside the budget.
#[test]
fn range_sizes_fit_budget() {
    let opt_sets: Vec<Vec<&str>> = vec![vec![], vec!["--max-loop-iters", "1000000"]];
    let mut checked = 0;
    for args in &opt_sets {
        let opts = parse_generator_args(args.clone()).unwrap();
        for seed in 0..150 {
            let prog = gen_program(&opts, seed);
            let mut stack: Vec<&Form> = prog.forms.iter().collect();
            while let Some(f) = stack.pop() {
                if let Form::List(fs) = f
                    && fs.len() == 2
                    && sym(&fs[0]) == Some("range")
                    && let Form::Int(n) = &fs[1]
                {
                    checked += 1;
                    assert!(
                        *n >= 0 && *n <= WORK_BUDGET_PER_FUNC as i64,
                        "{args:?} seed {seed}: (range {n}) exceeds budget {WORK_BUDGET_PER_FUNC}"
                    );
                }
                stack.extend(f.children());
            }
        }
    }
    assert!(checked > 0, "no ranges observed");
}

/// `case` keys must be distinct integers (a duplicate key is a
/// compile error in Clojure), and every map literal must stay within the
/// pooled key set at 8 entries so it is an array-map with deterministic
/// order rather than a hash-map.
#[test]
fn case_keys_distinct_and_maps_small() {
    let opt_sets: Vec<Vec<&str>> = vec![vec![], vec!["--max-funcs", "8", "--max-block-size", "8"]];
    for args in &opt_sets {
        let opts = parse_generator_args(args.clone()).unwrap();
        for seed in 0..150 {
            let prog = gen_program(&opts, seed);
            let mut stack: Vec<&Form> = prog.forms.iter().collect();
            while let Some(f) = stack.pop() {
                match f {
                    Form::List(fs) if fs.first().and_then(sym) == Some("case") => {
                        // (case SCRUTINY K1 V1 ... Kn Vn DEFAULT)
                        assert!(
                            fs.len() >= 5 && fs.len() % 2 == 1,
                            "{args:?} seed {seed}: malformed case"
                        );
                        let mut keys = HashSet::new();
                        let mut i = 2;
                        while i + 1 < fs.len() {
                            match &fs[i] {
                                Form::Int(k) => {
                                    assert!(
                                        keys.insert(k),
                                        "{args:?} seed {seed}: duplicate case key {k}"
                                    );
                                }
                                other => {
                                    panic!("{args:?} seed {seed}: non-integer case key {other:?}")
                                }
                            }
                            i += 2;
                        }
                    }
                    Form::Map(pairs) => {
                        assert!(
                            pairs.len() <= MAP_KEYS.len(),
                            "{args:?} seed {seed}: map with {} entries",
                            pairs.len()
                        );
                        for (k, _) in pairs {
                            match k {
                                Form::Keyword(k) => assert!(
                                    MAP_KEYS.contains(&k.as_str()),
                                    "{args:?} seed {seed}: unpooled map key {k:?}"
                                ),
                                other => {
                                    panic!("{args:?} seed {seed}: non-keyword map key {other:?}")
                                }
                            }
                        }
                    }
                    _ => {}
                }
                stack.extend(f.children());
            }
        }
    }
}

/// Emitted programs must re-parse to the exact same forms: any
/// pretty-printer infidelity (bad float/string escaping, dropped nodes)
/// corrupts every downstream consumer, including `drive` and `reduce`.
#[test]
fn emitted_programs_reparse_exactly() {
    let opt_sets: Vec<Vec<&str>> = vec![
        vec![],
        vec!["--max-funcs", "8", "--max-block-size", "8"],
        vec!["--no-safe-math"],
        vec!["--no-vectors", "--no-maps", "--no-records"],
    ];
    for args in &opt_sets {
        let opts = parse_generator_args(args.clone()).unwrap();
        for seed in 0..50 {
            let prog = gen_program(&opts, seed);
            let text = emit_program(&prog, &opts);
            let back = parse_all(&text)
                .unwrap_or_else(|e| panic!("{args:?} seed {seed}: reparse failed: {e}"));
            assert_eq!(back, prog.forms, "{args:?} seed {seed}: reparse mismatch");
        }
    }
}

/// No function may exceed Clojure's 20-parameter limit, even at
/// `--max-params 20`: recursive functions take a fuel parameter in
/// addition to the drawn ones, so the draw must reserve that slot.
/// (JVM Clojure rejects 21-param `defn` with "Can't specify more than
/// 20 params".)
#[test]
fn arities_respect_param_limit() {
    let args = vec!["--max-params", "20", "--max-funcs", "8"];
    let opts = parse_generator_args(args).unwrap();
    for seed in 0..100 {
        let prog = gen_program(&opts, seed);
        for f in &prog.forms {
            if let Form::List(fs) = f
                && fs.len() >= 4
                && sym(&fs[0]) == Some("defn")
                && let Form::Vector(params) = &fs[2]
            {
                assert!(
                    params.len() <= 20,
                    "seed {seed}: {} has {} params",
                    sym(&fs[1]).unwrap_or("?"),
                    params.len()
                );
            }
        }
    }
}

/// Hostile maxima set directly (bypassing `validate()`, as library
/// users can) must still generate a bounded program quickly: the budget
/// hard-stops straight-line code, draws clamp to ceilings, and literals
/// fit the remaining budget. Pre-fix, block-size 500 took 43s for 61MB.
#[test]
fn hostile_library_maxima_stay_bounded() {
    let mut opts = parse_generator_args::<Vec<String>, String>(Vec::new()).unwrap();
    opts.max_block_size = 20000;
    opts.max_funcs = 2;
    opts.max_expr_depth = 1000000;
    opts.max_globals = 100000;
    let prog = gen_program(&opts, 1);
    let text = emit_program(&prog, &opts);
    assert!(
        text.len() < 2_000_000,
        "hostile maxima emitted {} bytes",
        text.len()
    );
    assert!(
        prog.exprs < 200_000,
        "hostile maxima generated {} exprs",
        prog.exprs
    );
}

/// Zero maxima must still generate a complete program: no panics on
/// empty draws (`below(0)`), and the entry point must exist and print
/// something even with no functions or globals to hash.
#[test]
fn zero_options_generate_cleanly() {
    let zeros = vec![
        "--max-funcs",
        "0",
        "--max-params",
        "0",
        "--max-globals",
        "0",
        "--max-records",
        "0",
        "--max-record-fields",
        "0",
        "--max-block-depth",
        "0",
        "--max-block-size",
        "0",
        "--max-expr-depth",
        "0",
        "--max-vector-len",
        "0",
        "--max-map-size",
        "0",
        "--max-string-len",
        "0",
        "--max-loop-iters",
        "0",
        "--max-recursion-depth",
        "0",
    ];
    let mut kitchen = zeros.clone();
    kitchen.extend([
        "--no-vectors",
        "--no-maps",
        "--no-strings",
        "--no-doubles",
        "--no-records",
        "--no-loops",
        "--no-recursion",
        "--no-globals",
        "--no-higher-order",
        "--no-bitops",
        "--no-divs",
    ]);
    for args in [&zeros[..], &kitchen[..]] {
        let opts = parse_generator_args(args.to_vec()).unwrap();
        opts.validate().expect("zero options validate");
        for seed in 0..30 {
            let prog = match catch_unwind(AssertUnwindSafe(|| gen_program(&opts, seed))) {
                Ok(p) => p,
                Err(_) => panic!("{args:?} seed {seed}: generator panicked"),
            };
            assert!(!prog.forms.is_empty(), "{args:?} seed {seed}: no forms");
            let text = emit_program(&prog, &opts);
            assert!(
                text.contains("smith-main"),
                "{args:?} seed {seed}: no entry point"
            );
            assert!(
                text.contains("checksum:") || text.contains("\"done\""),
                "{args:?} seed {seed}: entry prints nothing"
            );
        }
    }
}

/// Every call head and bare function-valued symbol in user forms.
fn collect_op_positions(forms: &[Form]) -> (HashSet<String>, HashSet<String>) {
    let mut heads = HashSet::new();
    let mut bare = HashSet::new();
    let mut stack: Vec<(&Form, bool)> = forms.iter().map(|f| (f, false)).collect();
    while let Some((f, _)) = stack.pop() {
        match f {
            Form::Symbol(s) => {
                bare.insert(s.clone());
            }
            Form::List(fs) => {
                if let Some(head) = fs.first().and_then(sym) {
                    heads.insert(head.to_string());
                }
                stack.extend(fs.iter().map(|g| (g, false)));
            }
            Form::Vector(v) | Form::Set(v) => stack.extend(v.iter().map(|g| (g, false))),
            Form::Map(pairs) => {
                stack.extend(pairs.iter().flat_map(|(k, v)| [k, v]).map(|g| (g, false)));
            }
            _ => {}
        }
    }
    (heads, bare)
}

#[test]
fn partial_ops_are_banned_under_safe_math() {
    // `/`, `quot`, `rem` throw on a zero divisor; under safe math user
    // code must only reach them through the total `s-` wrappers — as
    // call heads and as bare function values (which HOF could apply).
    // Under `--no-safe-math` the ban lifts and the wrappers vanish.
    let safe = parse_generator_args(Vec::<String>::new()).unwrap();
    let raw = parse_generator_args(vec!["--no-safe-math"]).unwrap();
    // prog.forms = [ns, ...preamble, ...user]; the preamble legitimately
    // uses raw ops inside the wrapper definitions, so skip that prefix.
    let skip = 1 + clojure_smith::preamble::preamble_forms().len();
    for seed in 0..40 {
        let prog = gen_program(&safe, seed);
        let (heads, bare) = collect_op_positions(&prog.forms[skip..]);
        for op in ["/", "quot", "rem"] {
            assert!(!heads.contains(op), "seed {seed}: raw `{op}` call head");
            assert!(!bare.contains(op), "seed {seed}: raw `{op}` as value");
        }
        let prog = gen_program(&raw, seed);
        let (heads, bare) = collect_op_positions(&prog.forms[skip..]);
        for op in [
            "s-div", "s-quot", "s-rem", "s+", "s-", "s-neg", "s-inc", "s-dec",
        ] {
            assert!(
                !heads.contains(op),
                "seed {seed}: wrapper `{op}` under --no-safe-math"
            );
            assert!(
                !bare.contains(op),
                "seed {seed}: wrapper `{op}` value under --no-safe-math"
            );
        }
    }
}
