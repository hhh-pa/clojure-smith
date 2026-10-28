//! Structural invariants over generated programs: fast checks (no Clojure
//! runtime needed) that complement the end-to-end integration tests.
//!
//! The headline invariant: an atom must never be written inside its own
//! `swap!` retry context — not directly, and not transitively through a
//! called function. Violating it makes `swap!` retry forever, because every
//! attempt changes the atom and fails the compare-and-swap.

use clojure_smith::form::Form;
use clojure_smith::gen_expr::{MAX_CALL_LOOP_TRIPS, MAX_SELF_SITES};
use clojure_smith::gen_program::gen_program;
use clojure_smith::options::parse_generator_args;
use std::collections::{HashMap, HashSet};

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
