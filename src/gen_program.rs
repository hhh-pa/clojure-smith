//! Whole-program assembly and the generate mode.
//!
//! A program is a namespace form, the preamble helpers, zero or more
//! records, global atoms, and functions, plus an entry point that calls
//! every function, hashes all results and global state, and prints exactly
//! one line.

use crate::form::{Form, print_pretty};
use crate::gen_expr::Ctx;
use crate::gen_func::gen_func;
use crate::options::{GenerateOptions, MAX_FUNCS, MAX_GLOBALS, MAX_RECORD_FIELDS, MAX_RECORDS};

/// Cap on total printed bytes of global initializers. Literals are
/// budget-clamped per global against the remainder of this cap.
const GLOBAL_INIT_BYTES: u64 = 1_000_000;
use crate::preamble::{entry_forms, ns_form, preamble_forms};
use crate::rng::Rng;
use crate::types::{RecordDef, gen_literal, random_scalar_type, random_type};

pub struct Program {
    pub forms: Vec<Form>,
    pub seed: i64,
    pub funcs: usize,
    pub globals: usize,
    pub records: usize,
    pub exprs: u64,
}

/// Generate a program for `(opts, seed)`. Deterministic: the same inputs
/// always produce the same program.
pub fn gen_program(opts: &GenerateOptions, seed: i64) -> Program {
    let mut rng = Rng::new(seed as u64);
    let mut ctx = Ctx::new(opts, &mut rng);
    ctx.deadline = opts.gen_timeout.map(|t| std::time::Instant::now() + t);

    // Records first: globals and functions may reference them.
    // Draws clamp to the validated ceilings so library users who skip
    // validate() cannot stall generation with absurd maxima.
    let n_records = if opts.no_records {
        0
    } else {
        ctx.rng
            .range_usize(0, opts.max_records.min(MAX_RECORDS) as usize)
    };
    let mut record_forms = Vec::with_capacity(n_records);
    for i in 0..n_records {
        let nf = ctx.rng.range_usize(
            1,
            opts.max_record_fields.clamp(1, MAX_RECORD_FIELDS) as usize,
        );
        let mut fields = Vec::with_capacity(nf);
        for j in 0..nf {
            fields.push((format!("f{j}"), random_scalar_type(ctx.rng, ctx.opts)));
        }
        let name = format!("rec{i}");
        let field_syms: Vec<Form> = fields.iter().map(|(n, _)| Form::sym(n.clone())).collect();
        record_forms.push(Form::list(vec![
            Form::sym("defrecord"),
            Form::sym(name.clone()),
            Form::vector(field_syms),
        ]));
        ctx.env.records.push(RecordDef { name, fields });
    }

    // Globals: top-level atoms holding literals.
    let n_globals = if opts.no_globals {
        0
    } else {
        ctx.rng
            .range_usize(0, opts.max_globals.min(MAX_GLOBALS) as usize)
    };
    let mut global_names = Vec::with_capacity(n_globals);
    let mut global_forms = Vec::with_capacity(n_globals);
    // Global initializers run once at load, so they are not charged to a
    // function budget; instead their total printed size is capped here.
    let mut gbytes = 0u64;
    for i in 0..n_globals {
        let gt = random_type(ctx.rng, ctx.opts, &ctx.env.records);
        let budget = GLOBAL_INIT_BYTES.saturating_sub(gbytes);
        let (init, icost) = gen_literal(ctx.rng, ctx.opts, &ctx.env.records, &gt, budget);
        gbytes = gbytes.saturating_add(icost);
        let name = format!("g{i}");
        global_forms.push(Form::call(
            "def",
            vec![Form::sym(name.clone()), Form::call("atom", vec![init])],
        ));
        ctx.env.globals.push((name.clone(), gt));
        global_names.push(name);
    }

    // Functions, sequentially so later ones can call earlier ones.
    let n_funcs = if opts.max_funcs == 0 {
        0
    } else {
        ctx.rng
            .range_usize(1, opts.max_funcs.min(MAX_FUNCS) as usize)
    };
    let mut func_forms = Vec::with_capacity(n_funcs);
    for i in 0..n_funcs {
        let gf = gen_func(&mut ctx, i);
        func_forms.push(gf.form);
        ctx.env.funcs.push(gf.sig);
    }

    // Entry calls: literals, or derefs of type-matching globals. Every
    // function is called exactly once. The entry budget is the sum of
    // body costs (fuel zero for all) plus a fixed slack; recursive fuel
    // is drawn in a second pass and shrunk to fit the slack, so no call
    // is ever skipped yet total work stays bounded.
    let sigs = ctx.env.funcs.clone();
    ctx.spent = 0;
    ctx.budget = sigs
        .iter()
        .map(|s| s.body_cost)
        .fold(0u64, |a, b| a.saturating_add(b))
        .saturating_add(crate::cost::WORK_SLACK_ENTRY);
    let mut calls = Vec::with_capacity(sigs.len());
    for sig in &sigs {
        let mut args = Vec::with_capacity(sig.params.len());
        for (i, pt) in sig.params.iter().enumerate() {
            if i == 0 && sig.recursive {
                args.push(Form::int(0));
            } else {
                let gd = ctx.globals_of(pt);
                if !gd.is_empty() && ctx.rng.chance(0.5) {
                    let g = gd[ctx.rng.below(gd.len())].clone();
                    args.push(Form::Deref(Box::new(Form::sym(g))));
                } else {
                    let rem = ctx.remaining();
                    let (lit, cost) = gen_literal(ctx.rng, ctx.opts, &ctx.env.records, pt, rem);
                    ctx.charge(cost);
                    args.push(lit);
                }
            }
        }
        ctx.charge(sig.body_cost);
        calls.push(Form::call(&sig.name, args));
    }
    for (call, sig) in calls.iter_mut().zip(sigs.iter()) {
        if !sig.recursive {
            continue;
        }
        let drawn = ctx.rng.range(0, opts.max_recursion_depth as i64).max(0) as u64;
        // Extra over the fuel-zero price already paid: body * (2^k - 1).
        let mut best = 0u64;
        let mut k = 0u64;
        while k < drawn.min(62) {
            let extra = sig
                .body_cost
                .saturating_mul(crate::cost::fuel_factor(k + 1).saturating_sub(1));
            if extra > ctx.remaining() {
                break;
            }
            k += 1;
            best = k;
        }
        let extra = sig
            .body_cost
            .saturating_mul(crate::cost::fuel_factor(best).saturating_sub(1));
        ctx.charge(extra);
        if let Form::List(fs) = call
            && fs.len() > 1
        {
            fs[1] = Form::int(best as i64);
        }
    }

    let mut forms = vec![ns_form(&format!("fuzz-{}", seed as u64), !opts.no_strings)];
    forms.extend(preamble_forms());
    forms.extend(record_forms);
    forms.extend(global_forms);
    forms.extend(func_forms);
    forms.extend(entry_forms(&global_names, calls, !opts.no_checksum));

    Program {
        forms,
        seed,
        funcs: n_funcs,
        globals: n_globals,
        records: n_records,
        exprs: ctx.exprs,
    }
}

/// Render a program to source text, with a header comment unless quiet.
pub fn emit_program(prog: &Program, opts: &GenerateOptions) -> String {
    let mut out = String::new();
    if !opts.quiet {
        out.push_str(&format!(
            ";; Generated by clojure-smith {} -- https://github.com/hhh-pa/clojure-smith\n",
            env!("CARGO_PKG_VERSION")
        ));
        out.push_str(&format!(";; seed: {}\n", prog.seed));
        out.push_str(&format!(";; options: {}\n", opts.summary()));
        out.push_str(&format!(
            ";; stats: funcs={} globals={} records={} exprs={}\n",
            prog.funcs, prog.globals, prog.records, prog.exprs
        ));
        out.push('\n');
    }
    for (i, f) in prog.forms.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&print_pretty(f));
        out.push('\n');
    }
    out
}

/// Run the generate mode. Returns the process exit code.
pub fn run_generate(opts: &GenerateOptions) -> i32 {
    if let Err(e) = opts.validate() {
        eprintln!("clojure-smith: {e}");
        return 2;
    }
    let seed = opts.seed.unwrap_or_else(random_seed);
    let prog = gen_program(opts, seed);
    let text = emit_program(&prog, opts);
    let write_result = match &opts.output {
        Some(path) => std::fs::write(path, &text).map_err(|e| e.to_string()),
        None => {
            use std::io::Write;
            std::io::stdout()
                .write_all(text.as_bytes())
                .map_err(|e| e.to_string())
        }
    };
    if let Err(e) = write_result {
        eprintln!("clojure-smith: failed to write output: {e}");
        return 1;
    }
    if !opts.quiet {
        eprintln!(
            "clojure-smith: seed {} funcs={} globals={} records={} exprs={}",
            seed, prog.funcs, prog.globals, prog.records, prog.exprs
        );
    }
    0
}

fn random_seed() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9e3779b97f4a7c15);
    let pid = std::process::id() as u64;
    nanos.wrapping_add(pid.wrapping_mul(0x9e3779b97f4a7c15)) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::form::parse_all;
    use crate::options::parse_generator_args;

    fn opts(args: &[&str]) -> GenerateOptions {
        parse_generator_args(args.iter().map(|s| s.to_string())).unwrap()
    }

    #[test]
    fn deterministic() {
        let o = opts(&[]);
        let a = emit_program(&gen_program(&o, 123), &o);
        let b = emit_program(&gen_program(&o, 123), &o);
        assert_eq!(a, b);
    }

    #[test]
    fn seeds_and_options_diverge() {
        let o = opts(&[]);
        let a = emit_program(&gen_program(&o, 1), &o);
        let b = emit_program(&gen_program(&o, 2), &o);
        assert_ne!(a, b);
        let o2 = opts(&["--no-loops"]);
        let c = emit_program(&gen_program(&o2, 1), &o2);
        assert_ne!(a, c);
        // The preamble's smith-hash has its own loop; nothing else may.
        assert_eq!(c.matches("(loop ").count(), 1);
        assert!(!c.contains("(dotimes ") && !c.contains("(doseq "));
    }

    #[test]
    fn header_and_shape() {
        let o = opts(&[]);
        let prog = gen_program(&o, 42);
        let text = emit_program(&prog, &o);
        assert!(text.contains(";; seed: 42"), "{text}");
        assert!(text.contains("(ns fuzz-42"), "{text}");
        assert!(text.contains("smith-main"), "{text}");
        // The emitted text must re-parse to the same forms.
        let reparsed = parse_all(&text).expect("emitted program must parse");
        assert_eq!(reparsed, prog.forms);
    }

    #[test]
    fn degenerate_options() {
        // Everything disabled: still a valid (tiny) program.
        let o = opts(&[
            "--max-funcs",
            "0",
            "--no-globals",
            "--no-records",
            "--no-vectors",
            "--no-maps",
            "--no-strings",
            "--no-doubles",
            "--no-loops",
            "--no-checksum",
        ]);
        o.validate().unwrap();
        let prog = gen_program(&o, 9);
        let text = emit_program(&prog, &o);
        let reparsed = parse_all(&text).expect("degenerate program must parse");
        assert_eq!(reparsed, prog.forms);
        assert!(text.contains("\"done\""), "{text}");
    }
}
