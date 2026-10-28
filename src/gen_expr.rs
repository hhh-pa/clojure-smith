//! Expression and statement generation.
//!
//! [`gen_expr`] produces a form of a statically known [`ValType`]. Types are
//! tracked for every local, global, and function signature, so programs are
//! well-typed by construction: every operand is generated at the type its
//! operator requires, risky operations (division, indexing, slicing) go
//! through the total helpers from the preamble, and all loops and recursion
//! carry explicit constant bounds.

use crate::form::{Form, print_compact};
use crate::options::GenerateOptions;
use crate::rng::Rng;
use crate::types::{
    MAP_KEYS, RecordDef, ValType, default_value, gen_literal, random_elem_type, random_scalar_type,
    random_type,
};
use std::collections::HashSet;
use std::time::Instant;

/// Marker panic payload when generation exceeds its wall-clock deadline
/// (see [`Ctx::deadline`]). The driver catches it and fails the seed
/// instead of stalling the whole drive.
#[derive(Debug)]
pub struct GenTimeout;

/// A generated function's signature.
#[derive(Clone, Debug)]
pub struct FuncSig {
    pub name: String,
    pub params: Vec<ValType>,
    pub ret: ValType,
    /// True when `params[0]` is the long recursion fuel.
    pub recursive: bool,
    /// Sorted names of globals this function may write, transitively
    /// through the functions it calls. Used to keep atoms out of their own
    /// `swap!` retry context (which would retry forever).
    pub writes: Vec<String>,
    /// Static work cost of one constrained evaluation of the body: every
    /// generated node plus charged calls and trip multipliers, but
    /// excluding self-recursion expansion (callers multiply by `2^fuel`).
    pub body_cost: u64,
}

/// Direct-recursion context. Installed only inside the `(pos? fuel)`
/// branch of a recursive function body, so self calls always terminate.
#[derive(Clone, Debug)]
pub struct SelfCall {
    pub name: String,
    pub params: Vec<ValType>,
    pub ret: ValType,
    pub fuel: String,
}

/// Everything in scope during generation.
#[derive(Clone, Debug, Default)]
pub struct Env {
    /// Locals (params, let bindings, loop counters) used as a stack.
    pub locals: Vec<(String, ValType)>,
    pub globals: Vec<(String, ValType)>,
    pub funcs: Vec<FuncSig>,
    pub records: Vec<RecordDef>,
}

/// Mutable generation context, threaded through every generator.
pub struct Ctx<'a> {
    pub opts: &'a GenerateOptions,
    pub rng: &'a mut Rng,
    pub env: Env,
    pub depth: u32,
    pub block_depth: u32,
    pub self_call: Option<SelfCall>,
    pub counter: u32,
    pub exprs: u64,
    /// Stack of atoms whose `swap!` retry context encloses the current
    /// position. Writing one of these here would make the outer `swap!`
    /// retry forever, so writes and calls-to-writers are excluded.
    pub swap_forbidden: Vec<String>,
    /// Globals written directly by the function currently generated.
    pub direct_writes: HashSet<String>,
    /// Functions called by the function currently generated.
    pub called: HashSet<String>,
    /// Loop-nesting depth of the current position. Self calls are only
    /// offered at depth 0: a self call under a loop would multiply the
    /// per-level branching factor and blow up exponentially in fuel depth.
    pub in_loop: u32,
    /// Static self-call sites generated in the current function (budget).
    pub self_sites: u32,
    /// Work units spent by the function currently generated, against
    /// `budget`. Every node, call, and trip multiplier is charged here as
    /// it is generated; the final total becomes the function's `body_cost`.
    pub spent: u64,
    /// Work budget for the function currently generated.
    pub budget: u64,
    /// Wall-clock deadline for generation, if any. [`Ctx::charge`] checks
    /// it and panics with [`GenTimeout`] past the deadline, so hostile
    /// option combinations stall one seed instead of the whole drive.
    /// `None` (interactive generation, tests) means unbounded.
    pub deadline: Option<Instant>,
}

impl<'a> Ctx<'a> {
    pub fn new(opts: &'a GenerateOptions, rng: &'a mut Rng) -> Self {
        // Clamp to the validated ceilings so library users who skip
        // validate() cannot recurse the generator off its stack.
        let depth = opts.max_expr_depth.min(crate::options::MAX_EXPR_DEPTH);
        let block_depth = opts.max_block_depth.min(crate::options::MAX_BLOCK_DEPTH);
        Self {
            opts,
            rng,
            env: Env::default(),
            depth,
            block_depth,
            self_call: None,
            counter: 0,
            exprs: 0,
            swap_forbidden: Vec::new(),
            direct_writes: HashSet::new(),
            called: HashSet::new(),
            in_loop: 0,
            self_sites: 0,
            spent: 0,
            budget: crate::cost::WORK_BUDGET_PER_FUNC,
            deadline: None,
        }
    }

    /// True when writing any of `writes` is currently forbidden.
    pub fn writes_forbidden(&self, writes: &[String]) -> bool {
        writes.iter().any(|w| self.swap_forbidden.contains(w))
    }

    /// Charge `n` work units against the current budget (saturating).
    /// Past [`Ctx::deadline`], panics with [`GenTimeout`] instead.
    pub fn charge(&mut self, n: u64) {
        if let Some(d) = self.deadline
            && Instant::now() >= d
        {
            std::panic::panic_any(GenTimeout);
        }
        self.spent = self.spent.saturating_add(n);
    }

    /// Work units still available.
    pub fn remaining(&self) -> u64 {
        self.budget.saturating_sub(self.spent)
    }

    /// True when a call to `sig` fits in the remaining budget at fuel
    /// zero (recursive calls shrink their drawn fuel to fit from there).
    pub fn call_affordable(&self, sig: &FuncSig) -> bool {
        sig.body_cost <= self.remaining()
    }

    /// A globally fresh name.
    pub fn fresh(&mut self, prefix: &str) -> String {
        let id = self.counter;
        self.counter += 1;
        format!("{prefix}{id}")
    }

    /// Names of visible locals with exactly type `t`. Template names
    /// (`i`, `acc`, `old`, `x0`, …) may shadow each other across nesting,
    /// so only the innermost binding of each name counts — returning a
    /// shadowed entry would resolve to the wrong variable at runtime.
    pub fn locals_of(&self, t: &ValType) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for (n, lt) in self.env.locals.iter().rev() {
            if seen.insert(n.as_str()) && lt == t {
                out.push(n.clone());
            }
        }
        out
    }

    pub fn globals_of(&self, t: &ValType) -> Vec<String> {
        self.env
            .globals
            .iter()
            .filter(|(_, gt)| gt == t)
            .map(|(n, _)| n.clone())
            .collect()
    }

    /// Signatures of all functions returning `t` that may be called here:
    /// functions writing a forbidden atom are excluded, and the recursive
    /// self entry is only offered outside swap contexts and loops and
    /// within the per-function site budget (the function's own write set is
    /// still being computed, so self calls are conservatively assumed to
    /// write everything; the loop/budget rules keep total work bounded).
    pub fn funcs_returning(&self, t: &ValType, allow_self: bool) -> Vec<FuncSig> {
        let mut out: Vec<FuncSig> = self
            .env
            .funcs
            .iter()
            .filter(|f| &f.ret == t && !self.writes_forbidden(&f.writes) && self.call_affordable(f))
            .cloned()
            .collect();
        if allow_self
            && self.swap_forbidden.is_empty()
            && self.in_loop == 0
            && self.self_sites < MAX_SELF_SITES
            && let Some(s) = &self.self_call
            && &s.ret == t
        {
            out.push(FuncSig {
                name: s.name.clone(),
                params: s.params.clone(),
                ret: s.ret.clone(),
                recursive: true,
                // Never consulted: self is excluded wherever write
                // sets matter (see above).
                writes: Vec::new(),
                // Never charged: self sites cost a unit at emission and
                // their expansion is paid by callers via fuel.
                body_cost: 0,
            });
        }
        out
    }
}

/// Run `f` with reduced depth budgets, restoring them afterwards.
fn nested<T>(ctx: &mut Ctx, dd: u32, db: u32, f: impl FnOnce(&mut Ctx) -> T) -> T {
    let (od, ob) = (ctx.depth, ctx.block_depth);
    ctx.depth = od.saturating_sub(dd);
    ctx.block_depth = ob.saturating_sub(db);
    let r = f(ctx);
    ctx.depth = od;
    ctx.block_depth = ob;
    r
}

/// Push locals for the duration of `f`.
fn with_locals<T>(
    ctx: &mut Ctx,
    bindings: Vec<(String, ValType)>,
    f: impl FnOnce(&mut Ctx) -> T,
) -> T {
    let mark = ctx.env.locals.len();
    ctx.env.locals.extend(bindings);
    let r = f(ctx);
    ctx.env.locals.truncate(mark);
    r
}

/// Forbid writes to `g` for the duration of `f` (we are inside `g`'s
/// `swap!` retry context).
fn with_forbidden<T>(ctx: &mut Ctx, g: &str, f: impl FnOnce(&mut Ctx) -> T) -> T {
    ctx.swap_forbidden.push(g.to_string());
    let r = f(ctx);
    ctx.swap_forbidden.pop();
    r
}

/// Maximum static self-call sites per recursive function. With fuel depth
/// `d`, worst-case calls are `MAX_SELF_SITES^d` (256 at the default fuel
/// of 8).
pub const MAX_SELF_SITES: u32 = 2;

/// Maximum trip count of a loop whose body calls user functions. Loops
/// without calls may use the full `--max-loop-iters`; loops with calls are
/// scaled down so that loop/call nesting cannot blow up exponentially along
/// the call DAG.
pub const MAX_CALL_LOOP_TRIPS: i64 = 3;

/// True when `form` contains a call to a generated function (direct, or via
/// mapv/filterv/reduce/`->`). Conservative: quoted positions count too.
fn form_has_call(form: &Form, funcs: &[FuncSig]) -> bool {
    let is_user = |s: &str| funcs.iter().any(|f| f.name == s);
    let mut stack = vec![form];
    while let Some(f) = stack.pop() {
        if let Form::List(fs) = f
            && let Some(Form::Symbol(h)) = fs.first()
        {
            if is_user(h) {
                return true;
            }
            if matches!(h.as_str(), "mapv" | "filterv" | "reduce")
                && let Some(Form::Symbol(g)) = fs.get(1)
                && is_user(g)
            {
                return true;
            }
        }
        stack.extend(f.children());
    }
    false
}

fn pick_kind(ctx: &mut Ctx, kinds: &[(u64, u8)]) -> u8 {
    let weights: Vec<u64> = kinds.iter().map(|(w, _)| *w).collect();
    kinds[ctx.rng.pick_index_weighted(&weights)].1
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Generate a form of type `t`.
pub fn gen_expr(ctx: &mut Ctx, t: &ValType) -> Form {
    ctx.exprs += 1;
    ctx.charge(1);
    // The budget is a hard stop, not just an accounting device: with no
    // units left, only a (budget-clamped) leaf is generated. This bounds
    // straight-line code that shrinking never touches.
    if ctx.depth == 0 || ctx.remaining() == 0 {
        return gen_leaf(ctx, t);
    }
    let no_loops = ctx.opts.no_loops;
    let mut kinds: Vec<(u64, u8)> = vec![(4, 0)];
    if ctx.block_depth > 0 {
        kinds.push((3, 1)); // if
        kinds.push((3, 2)); // let
        if !no_loops {
            kinds.push((2, 3)); // loop
        }
        kinds.push((1, 4)); // case
        kinds.push((1, 5)); // cond
    }
    let allow_self = ctx.self_call.is_some();
    let calls = ctx.funcs_returning(t, allow_self);
    if !calls.is_empty() {
        kinds.push((3, 6));
    }
    if has_threadable(ctx, t) {
        kinds.push((1, 7));
    }
    kinds.push((6, 8)); // type-specific operator
    match pick_kind(ctx, &kinds) {
        1 => gen_if(ctx, t),
        2 => gen_let(ctx, t),
        3 => gen_loop(ctx, t),
        4 => gen_case(ctx, t),
        5 => gen_cond(ctx, t),
        6 => {
            let idx = ctx.rng.below(calls.len());
            gen_call_sig(ctx, &calls[idx].clone())
        }
        7 => gen_thread(ctx, t),
        8 => gen_op(ctx, t),
        _ => gen_leaf(ctx, t),
    }
}

/// Generate a body: up to `max_stmts` statements plus a final form of
/// type `t`.
pub fn gen_body(ctx: &mut Ctx, t: &ValType, max_stmts: u32) -> Vec<Form> {
    let n = if max_stmts == 0 {
        0
    } else {
        ctx.rng
            .range_usize(0, max_stmts.min(crate::options::MAX_BLOCK_SIZE) as usize)
    };
    let mut body = Vec::with_capacity(n + 1);
    for _ in 0..n {
        body.push(gen_stmt(ctx));
    }
    body.push(gen_expr(ctx, t));
    body
}

/// Generate a statement (its value is discarded).
pub fn gen_stmt(ctx: &mut Ctx) -> Form {
    // Tags: 0 expr, 1 swap!, 2 reset!, 3 compare-and-set!, 4 call,
    // 5 dotimes, 6 doseq.
    ctx.charge(1);
    let no_loops = ctx.opts.no_loops;
    let has_writable = !writable_globals(ctx).is_empty();
    let has_funcs = !stmt_call_cands(ctx).is_empty();
    let can_nest = ctx.block_depth > 0;
    let mut kinds: Vec<(u64, u8)> = vec![(2, 0)];
    if has_writable {
        kinds.push((3, 1));
        kinds.push((2, 2));
        kinds.push((1, 3));
    }
    if has_funcs {
        kinds.push((2, 4));
    }
    if !no_loops && can_nest {
        kinds.push((1, 5));
        // doseq iterates a vector-typed collection, so it is also gated
        // on vectors (its coll used to panic the generator outright).
        if !ctx.opts.no_vectors {
            kinds.push((1, 6));
        }
    }
    match pick_kind(ctx, &kinds) {
        1 => gen_swap(ctx),
        2 => gen_reset(ctx),
        3 => gen_cas(ctx),
        4 => gen_call_stmt(ctx),
        5 => gen_dotimes(ctx),
        6 => gen_doseq(ctx),
        _ => {
            let t = random_type(ctx.rng, ctx.opts, &ctx.env.records);
            gen_expr(ctx, &t)
        }
    }
}

// ---------------------------------------------------------------------------
// Leaves
// ---------------------------------------------------------------------------

/// A literal or a variable of type `t`.
fn gen_leaf(ctx: &mut Ctx, t: &ValType) -> Form {
    let mut vars: Vec<Form> = ctx.locals_of(t).into_iter().map(Form::sym).collect();
    vars.extend(
        ctx.globals_of(t)
            .into_iter()
            .map(|g| Form::Deref(Box::new(Form::sym(g)))),
    );
    if !vars.is_empty() && ctx.rng.chance(0.5) {
        let i = ctx.rng.below(vars.len());
        vars.swap_remove(i)
    } else {
        let rem = ctx.remaining();
        let (lit, cost) = gen_literal(ctx.rng, ctx.opts, &ctx.env.records, t, rem);
        ctx.charge(cost);
        lit
    }
}

// ---------------------------------------------------------------------------
// Control forms
// ---------------------------------------------------------------------------

fn gen_if(ctx: &mut Ctx, t: &ValType) -> Form {
    nested(ctx, 1, 1, |ctx| {
        let c = gen_expr(ctx, &ValType::Bool);
        let a = gen_expr(ctx, t);
        let b = gen_expr(ctx, t);
        Form::list(vec![Form::sym("if"), c, a, b])
    })
}

fn gen_let(ctx: &mut Ctx, t: &ValType) -> Form {
    nested(ctx, 1, 1, |ctx| {
        let max_block_size = ctx.opts.max_block_size;
        let n = ctx.rng.range_usize(1, 3);
        let mut flat = Vec::with_capacity(n * 2);
        let mark = ctx.env.locals.len();
        for _ in 0..n {
            let name = ctx.fresh("l");
            let bt = random_type(ctx.rng, ctx.opts, &ctx.env.records);
            let val = gen_expr(ctx, &bt);
            flat.push(Form::sym(name.clone()));
            flat.push(val);
            ctx.env.locals.push((name, bt));
        }
        let mut body = gen_body(ctx, t, max_block_size);
        ctx.env.locals.truncate(mark);
        let mut form = vec![Form::sym("let"), Form::vector(flat)];
        form.append(&mut body);
        Form::list(form)
    })
}

/// True when `form` contains a nested loop of any kind. Inside a
/// loop update, nested loops multiply per-trip work and value
/// production beyond what static charges can see.
fn contains_nested_loop(form: &Form) -> bool {
    let mut stack = vec![form];
    while let Some(f) = stack.pop() {
        if matches!(f.head_symbol(), Some("loop" | "for" | "dotimes" | "doseq")) {
            return true;
        }
        stack.extend(f.children());
    }
    false
}

fn gen_loop(ctx: &mut Ctx, t: &ValType) -> Form {
    nested(ctx, 1, 1, |ctx| {
        ctx.in_loop += 1;
        let max_iters = ctx.opts.max_loop_iters as i64;
        let no_safe_math = ctx.opts.no_safe_math;
        let before = ctx.spent;
        let init = gen_expr(ctx, t);
        let mark = ctx.env.locals.len();
        ctx.env.locals.push(("i".to_string(), ValType::Long));
        ctx.env.locals.push(("acc".to_string(), t.clone()));
        let update = gen_expr(ctx, t);
        ctx.env.locals.truncate(mark);
        ctx.in_loop -= 1;
        // Scale trips down when the update calls user functions, so
        // loop/call nesting cannot blow up along the call DAG.
        let has_calls = form_has_call(&update, &ctx.env.funcs);
        let cap = if has_calls {
            max_iters.min(MAX_CALL_LOOP_TRIPS)
        } else {
            max_iters
        };
        // Shrink trips to fit the remaining budget: the body was charged
        // once during generation, and each further trip pays again.
        let per_trip = ctx.spent.saturating_sub(before).max(1);
        let drawn = ctx.rng.range(0, cap);
        let mut iters = crate::cost::shrink_trips(drawn, per_trip, ctx.remaining());
        // A growable accumulator mentioned twice or more amplifies
        // geometrically (`(vec (interleave acc acc))` doubles per trip),
        // which the linear work budget cannot see: cap trips so the
        // result stays small. Scalars cannot grow, records have fixed
        // fields, so those keep the full trip range.
        if matches!(t, ValType::Vec(_) | ValType::Map(_) | ValType::Str) {
            let refs = update.count_sym("acc");
            iters = iters.min(crate::cost::acc_growth_cap(refs));
            if refs == 1 {
                // Linear growth: each trip adds at most the update's
                // own static bytes.
                let bytes = print_compact(&update).len() as u64;
                iters = iters.min(crate::cost::linear_growth_cap(bytes, ctx.remaining()));
            }
            if contains_nested_loop(&update) {
                // Nested loops multiply production opaquely (per-trip
                // charges see static size, not runtime output size).
                iters = iters.min(crate::cost::NESTED_LOOP_TRIPS);
            }
        }
        ctx.charge(
            (iters.max(0) as u64)
                .saturating_sub(1)
                .saturating_mul(per_trip),
        );
        let inc = if no_safe_math { "inc" } else { "s-inc" };
        // The `identity` wrappers keep both bindings boxed (Object-typed):
        // a bare boolean (or long) literal init would make the loop binding
        // a primitive local, and `recur` with a boxed argument then fails to
        // compile ("recur arg for primitive local").
        Form::list(vec![
            Form::sym("loop"),
            Form::vector(vec![
                Form::sym("i"),
                Form::call("identity", vec![Form::int(0)]),
                Form::sym("acc"),
                Form::call("identity", vec![init]),
            ]),
            Form::list(vec![
                Form::sym("if"),
                Form::call("<", vec![Form::sym("i"), Form::int(iters)]),
                Form::call("recur", vec![Form::call(inc, vec![Form::sym("i")]), update]),
                Form::sym("acc"),
            ]),
        ])
    })
}

fn gen_case(ctx: &mut Ctx, t: &ValType) -> Form {
    nested(ctx, 1, 1, |ctx| {
        let s = gen_expr(ctx, &ValType::Long);
        let k = ctx.rng.range_usize(1, 3);
        let mut pool: Vec<i64> = (-2..=10).collect();
        ctx.rng.shuffle(&mut pool);
        let mut form = vec![Form::sym("case"), s];
        for c in pool.into_iter().take(k) {
            form.push(Form::int(c));
            form.push(gen_expr(ctx, t));
        }
        form.push(gen_expr(ctx, t));
        Form::list(form)
    })
}

fn gen_cond(ctx: &mut Ctx, t: &ValType) -> Form {
    nested(ctx, 1, 1, |ctx| {
        let k = ctx.rng.range_usize(1, 3);
        let mut form = vec![Form::sym("cond")];
        for _ in 0..k {
            form.push(gen_expr(ctx, &ValType::Bool));
            form.push(gen_expr(ctx, t));
        }
        form.push(Form::kw("else"));
        form.push(gen_expr(ctx, t));
        Form::list(form)
    })
}

// ---------------------------------------------------------------------------
// Calls
// ---------------------------------------------------------------------------

fn has_threadable(ctx: &Ctx, t: &ValType) -> bool {
    let self_name = ctx.self_call.as_ref().map(|s| s.name.as_str());
    ctx.env.funcs.iter().any(|f| {
        &f.ret == t
            && !f.recursive
            && !f.params.is_empty()
            && !ctx.writes_forbidden(&f.writes)
            && ctx.call_affordable(f)
            && self_name != Some(f.name.as_str())
    })
}

fn gen_thread(ctx: &mut Ctx, t: &ValType) -> Form {
    let steps = ctx.rng.range_usize(1, 2);
    thread_expr(ctx, t, steps)
}

fn thread_expr(ctx: &mut Ctx, t: &ValType, steps: usize) -> Form {
    let self_name = ctx.self_call.as_ref().map(|s| s.name.clone());
    let cands: Vec<FuncSig> = ctx
        .env
        .funcs
        .iter()
        .filter(|f| {
            &f.ret == t
                && !f.recursive
                && !f.params.is_empty()
                && !ctx.writes_forbidden(&f.writes)
                && ctx.call_affordable(f)
                && self_name.as_ref() != Some(&f.name)
        })
        .cloned()
        .collect();
    if steps == 0 || cands.is_empty() {
        return gen_expr(ctx, t);
    }
    let f = cands[ctx.rng.below(cands.len())].clone();
    ctx.called.insert(f.name.clone());
    // Reserve the step price before generating the subexpression and the
    // remaining arguments (threaded functions are never recursive).
    ctx.charge(f.body_cost);
    nested(ctx, 1, 0, |ctx| {
        let sub = if steps > 1 && ctx.rng.chance(0.5) {
            thread_expr(ctx, &f.params[0], steps - 1)
        } else {
            gen_expr(ctx, &f.params[0])
        };
        let mut step = vec![Form::sym(f.name.clone())];
        for pt in f.params.iter().skip(1) {
            step.push(gen_expr(ctx, pt));
        }
        Form::list(vec![Form::sym("->"), sub, Form::list(step)])
    })
}

/// Generate a call to `sig`, handling recursion fuel: outer calls pass a
/// small constant, self calls pass the decremented fuel variable. The
/// call price (body times `2^fuel` for recursive callees) is reserved up
/// front from what remains; recursive fuel shrinks to fit.
fn gen_call_sig(ctx: &mut Ctx, sig: &FuncSig) -> Form {
    let is_self = ctx
        .self_call
        .as_ref()
        .map(|s| s.name == sig.name)
        .unwrap_or(false);
    if !is_self {
        // Self calls contribute nothing beyond the function's own direct
        // writes (least fixpoint), and self has no signature yet anyway.
        ctx.called.insert(sig.name.clone());
    } else {
        ctx.self_sites += 1;
    }
    let no_safe_math = ctx.opts.no_safe_math;
    let max_fuel = ctx.opts.max_recursion_depth as i64;
    let fuel = if is_self || !sig.recursive {
        None
    } else {
        let drawn = ctx.rng.range(0, max_fuel).max(0) as u64;
        // Candidates were filtered affordable, so fuel zero always fits;
        // shrink the draw from there (fail closed on a logic slip).
        let k_max = crate::cost::max_fuel_for(sig.body_cost, ctx.remaining()).unwrap_or(0);
        Some(drawn.min(k_max) as i64)
    };
    let price = match fuel {
        Some(k) => crate::cost::call_charge(sig.body_cost, true, k.max(0) as u64),
        None if is_self => 1,
        None => sig.body_cost,
    };
    ctx.charge(price);
    nested(ctx, 1, 0, |ctx| {
        let mut args = Vec::with_capacity(sig.params.len());
        for (i, pt) in sig.params.iter().enumerate() {
            if i == 0 && sig.recursive {
                if is_self {
                    let fuel = ctx.self_call.as_ref().unwrap().fuel.clone();
                    let dec = if no_safe_math { "dec" } else { "s-dec" };
                    args.push(Form::call(dec, vec![Form::sym(fuel)]));
                } else {
                    args.push(Form::int(fuel.unwrap_or(0)));
                }
            } else {
                args.push(gen_expr(ctx, pt));
            }
        }
        Form::call(&sig.name, args)
    })
}

// ---------------------------------------------------------------------------
// Statements
// ---------------------------------------------------------------------------

/// Globals that may be written in the current context: atoms whose
/// `swap!` retry context encloses this position are excluded (writing one
/// would retry forever).
fn writable_globals(ctx: &Ctx) -> Vec<(String, ValType)> {
    ctx.env
        .globals
        .iter()
        .filter(|(n, _)| !ctx.swap_forbidden.contains(n))
        .cloned()
        .collect()
}

fn pick_writable_global(ctx: &mut Ctx) -> (String, ValType) {
    let cands = writable_globals(ctx);
    debug_assert!(
        !cands.is_empty(),
        "write kind offered with no writable global"
    );
    cands[ctx.rng.below(cands.len())].clone()
}

/// Functions that may be called in statement position here.
fn stmt_call_cands(ctx: &Ctx) -> Vec<FuncSig> {
    let mut cands: Vec<FuncSig> = ctx
        .env
        .funcs
        .iter()
        .filter(|f| !ctx.writes_forbidden(&f.writes) && ctx.call_affordable(f))
        .cloned()
        .collect();
    if ctx.swap_forbidden.is_empty()
        && ctx.in_loop == 0
        && ctx.self_sites < MAX_SELF_SITES
        && let Some(s) = &ctx.self_call
    {
        cands.push(FuncSig {
            name: s.name.clone(),
            params: s.params.clone(),
            ret: s.ret.clone(),
            recursive: true,
            writes: Vec::new(),
            // Never charged: see the sibling self entry above.
            body_cost: 0,
        });
    }
    cands
}

fn gen_swap(ctx: &mut Ctx) -> Form {
    let (name, gt) = pick_writable_global(ctx);
    ctx.direct_writes.insert(name.clone());
    nested(ctx, 1, 0, |ctx| {
        let body = with_forbidden(ctx, &name, |ctx| {
            with_locals(ctx, vec![("old".to_string(), gt.clone())], |ctx| {
                gen_expr(ctx, &gt)
            })
        });
        Form::call(
            "swap!",
            vec![
                Form::sym(name),
                Form::list(vec![
                    Form::sym("fn"),
                    Form::vector(vec![Form::sym("old")]),
                    body,
                ]),
            ],
        )
    })
}

fn gen_reset(ctx: &mut Ctx) -> Form {
    let (name, gt) = pick_writable_global(ctx);
    ctx.direct_writes.insert(name.clone());
    nested(ctx, 1, 0, |ctx| {
        let v = gen_expr(ctx, &gt);
        Form::call("reset!", vec![Form::sym(name), v])
    })
}

fn gen_cas(ctx: &mut Ctx) -> Form {
    let (name, gt) = pick_writable_global(ctx);
    ctx.direct_writes.insert(name.clone());
    nested(ctx, 1, 0, |ctx| {
        let old = gen_expr(ctx, &gt);
        let new = gen_expr(ctx, &gt);
        Form::call("compare-and-set!", vec![Form::sym(name), old, new])
    })
}

fn gen_call_stmt(ctx: &mut Ctx) -> Form {
    let cands = stmt_call_cands(ctx);
    if cands.is_empty() {
        // Unreachable: the call kind is only offered when candidates exist.
        let t = random_scalar_type(ctx.rng, ctx.opts);
        return gen_expr(ctx, &t);
    }
    let idx = ctx.rng.below(cands.len());
    gen_call_sig(ctx, &cands[idx].clone())
}

fn gen_dotimes(ctx: &mut Ctx) -> Form {
    nested(ctx, 1, 1, |ctx| {
        let max_iters = ctx.opts.max_loop_iters as i64;
        let k = ctx.rng.range_usize(1, 3);
        ctx.in_loop += 1;
        let before = ctx.spent;
        let body = with_locals(ctx, vec![("i".to_string(), ValType::Long)], |ctx| {
            let mut body = Vec::with_capacity(k);
            for _ in 0..k {
                body.push(gen_stmt(ctx));
            }
            body
        });
        ctx.in_loop -= 1;
        let has_calls = body.iter().any(|f| form_has_call(f, &ctx.env.funcs));
        let cap = if has_calls {
            max_iters.min(MAX_CALL_LOOP_TRIPS)
        } else {
            max_iters
        };
        // Shrink trips to fit the remaining budget (see gen_loop).
        let per_trip = ctx.spent.saturating_sub(before).max(1);
        let drawn = ctx.rng.range(0, cap);
        let n = crate::cost::shrink_trips(drawn, per_trip, ctx.remaining());
        ctx.charge((n.max(0) as u64).saturating_sub(1).saturating_mul(per_trip));
        let mut form = vec![
            Form::sym("dotimes"),
            Form::vector(vec![Form::sym("i"), Form::int(n)]),
        ];
        form.extend(body);
        Form::list(form)
    })
}

fn gen_doseq(ctx: &mut Ctx) -> Form {
    nested(ctx, 1, 1, |ctx| {
        ctx.in_loop += 1;
        let n = ctx.rng.range_usize(1, 2);
        let mut flat = Vec::with_capacity(n * 2);
        let mut bound = Vec::with_capacity(n);
        for i in 0..n {
            let et = random_elem_type(ctx.rng, ctx.opts, &ctx.env.records);
            let v = gen_expr(ctx, &ValType::Vec(Box::new(et.clone())));
            let name = format!("x{i}");
            flat.push(Form::sym(name.clone()));
            flat.push(v);
            bound.push((name, et));
        }
        let k = ctx.rng.range_usize(1, 3);
        let before = ctx.spent;
        let body = with_locals(ctx, bound, |ctx| {
            let mut body = Vec::with_capacity(k);
            for _ in 0..k {
                body.push(gen_stmt(ctx));
            }
            body
        });
        ctx.in_loop -= 1;
        // Bound cartesian trips by the remaining budget (raw `take` with a
        // small constant is total), keeping the tighter trip cap when the
        // body calls user functions.
        let has_calls = body.iter().any(|f| form_has_call(f, &ctx.env.funcs));
        let per_trip = ctx.spent.saturating_sub(before).max(1);
        let mut k_take = crate::cost::take_k(n, per_trip, ctx.remaining());
        if has_calls {
            k_take = k_take.min(MAX_CALL_LOOP_TRIPS);
        }
        for i in (1..flat.len()).step_by(2) {
            let coll = std::mem::replace(&mut flat[i], Form::nil());
            flat[i] = Form::call("take", vec![Form::int(k_take), coll]);
        }
        let trips = (k_take.max(0) as u64).saturating_pow(n as u32);
        ctx.charge(trips.saturating_sub(1).saturating_mul(per_trip));
        let mut form = vec![Form::sym("doseq"), Form::vector(flat)];
        form.extend(body);
        Form::list(form)
    })
}

// ---------------------------------------------------------------------------
// Type-specific operators
// ---------------------------------------------------------------------------

fn gen_op(ctx: &mut Ctx, t: &ValType) -> Form {
    match t {
        ValType::Long => gen_long_op(ctx),
        ValType::Double => gen_double_op(ctx),
        ValType::Bool => gen_bool_op(ctx),
        ValType::Str => gen_str_op(ctx),
        ValType::Vec(e) => gen_vec_op(ctx, e),
        ValType::Map(v) => gen_map_op(ctx, v),
        ValType::Record(i) => gen_record_op(ctx, *i),
    }
}

/// `(s-nth <vec> <idx> <default>)`.
fn gen_nth(ctx: &mut Ctx, t: &ValType) -> Form {
    nested(ctx, 1, 0, |ctx| {
        let v = gen_expr(ctx, &ValType::Vec(Box::new(t.clone())));
        let i = gen_expr(ctx, &ValType::Long);
        let d = default_value(&ctx.env.records, t);
        Form::call("s-nth", vec![v, i, d])
    })
}

/// `(s-peek <vec> <default>)`.
fn gen_peek(ctx: &mut Ctx, t: &ValType) -> Form {
    nested(ctx, 1, 0, |ctx| {
        let v = gen_expr(ctx, &ValType::Vec(Box::new(t.clone())));
        let d = default_value(&ctx.env.records, t);
        Form::call("s-peek", vec![v, d])
    })
}

/// `(s-get <map> <key> <default>)`.
fn gen_get(ctx: &mut Ctx, t: &ValType) -> Form {
    nested(ctx, 1, 0, |ctx| {
        let m = gen_expr(ctx, &ValType::Map(Box::new(t.clone())));
        let k = if ctx.rng.chance(0.7) {
            Form::kw(*ctx.rng.pick(&MAP_KEYS))
        } else {
            Form::kw("missing")
        };
        let d = default_value(&ctx.env.records, t);
        Form::call("s-get", vec![m, k, d])
    })
}

/// `(:field <record>)` for a record with a field of type `t`.
fn record_fields_of(records: &[RecordDef], t: &ValType) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (i, r) in records.iter().enumerate() {
        for (fname, ft) in &r.fields {
            if ft == t {
                out.push((i, fname.clone()));
            }
        }
    }
    out
}

fn gen_field(ctx: &mut Ctx, t: &ValType) -> Form {
    let cands = record_fields_of(&ctx.env.records, t);
    debug_assert!(!cands.is_empty());
    if cands.is_empty() {
        return gen_leaf(ctx, t);
    }
    let (i, fname) = cands[ctx.rng.below(cands.len())].clone();
    nested(ctx, 1, 0, |ctx| {
        let r = gen_expr(ctx, &ValType::Record(i));
        Form::list(vec![Form::kw(fname), r])
    })
}

/// `(reduce f init coll)` over a matching two-argument function.
fn reduce_cands(ctx: &Ctx, t: &ValType) -> Vec<FuncSig> {
    let self_name = ctx.self_call.as_ref().map(|s| s.name.as_str());
    ctx.env
        .funcs
        .iter()
        .filter(|f| {
            !f.recursive
                && self_name != Some(f.name.as_str())
                && !ctx.writes_forbidden(&f.writes)
                && ctx.call_affordable(f)
                && f.params.len() == 2
                && &f.params[0] == t
                && &f.ret == t
        })
        .cloned()
        .collect()
}

fn gen_reduce(ctx: &mut Ctx, t: &ValType) -> Form {
    // The collection is always vector-typed.
    if ctx.opts.no_vectors {
        return gen_leaf(ctx, t);
    }
    let cands = reduce_cands(ctx, t);
    if cands.is_empty() {
        return gen_leaf(ctx, t);
    }
    let f = cands[ctx.rng.below(cands.len())].clone();
    ctx.called.insert(f.name.clone());
    nested(ctx, 1, 0, |ctx| {
        let init = gen_expr(ctx, t);
        let coll = gen_expr(ctx, &ValType::Vec(Box::new(f.params[1].clone())));
        // Cap applications by the remaining budget (see the mapv arm).
        let k = crate::cost::take_k(1, f.body_cost.max(1), ctx.remaining());
        ctx.charge((k.max(0) as u64).saturating_mul(f.body_cost));
        let coll = Form::call("take", vec![Form::int(k), coll]);
        Form::call("reduce", vec![Form::sym(f.name.clone()), init, coll])
    })
}

// --- Long ---

fn gen_long_op(ctx: &mut Ctx) -> Form {
    // Tags: 0 arith, 1 unary, 2 div, 3 bit, 4 count, 5 compare, 6 double->long,
    // 7 nth, 8 peek, 9 get, 10 field, 11 reduce, 12 name-count.
    let o = ctx.opts;
    let mut kinds: Vec<(u64, u8)> = vec![(6, 0), (2, 1)];
    if !o.no_divs {
        kinds.push((3, 2));
    }
    if !o.no_bitops {
        kinds.push((4, 3));
    }
    if !o.no_vectors || !o.no_maps || !o.no_strings {
        kinds.push((2, 4));
    }
    kinds.push((1, 5));
    if !o.no_doubles {
        kinds.push((2, 6));
    }
    if !o.no_vectors {
        kinds.push((3, 7));
        kinds.push((1, 8));
    }
    if !o.no_maps {
        kinds.push((2, 9));
    }
    if !record_fields_of(&ctx.env.records, &ValType::Long).is_empty() {
        kinds.push((2, 10));
    }
    if !o.no_higher_order && !reduce_cands(ctx, &ValType::Long).is_empty() {
        kinds.push((1, 11));
    }
    // count-of-name needs strings.
    if !o.no_strings {
        kinds.push((1, 12));
    }
    match pick_kind(ctx, &kinds) {
        1 => gen_long_unary(ctx),
        2 => gen_long_div(ctx),
        3 => gen_long_bit(ctx),
        4 => gen_count(ctx),
        5 => gen_compare(ctx),
        6 => nested(ctx, 1, 0, |ctx| {
            // `long` throws on infinities and out-of-range finite doubles,
            // so double-to-long conversion goes through the total wrapper.
            let d = gen_expr(ctx, &ValType::Double);
            Form::call("s-long", vec![d])
        }),
        7 => gen_nth(ctx, &ValType::Long),
        8 => gen_peek(ctx, &ValType::Long),
        9 => gen_get(ctx, &ValType::Long),
        10 => gen_field(ctx, &ValType::Long),
        11 => gen_reduce(ctx, &ValType::Long),
        12 => nested(ctx, 1, 0, |ctx| {
            let k = Form::kw(*ctx.rng.pick(&MAP_KEYS));
            Form::call(
                "long",
                vec![Form::call("count", vec![Form::call("name", vec![k])])],
            )
        }),
        _ => nested(ctx, 1, 0, |ctx| {
            let no_safe_math = ctx.opts.no_safe_math;
            let op = match ctx.rng.below(3) {
                0 => ["s+", "+"],
                1 => ["s-", "-"],
                _ => ["s*", "*"],
            };
            let name = if no_safe_math { op[1] } else { op[0] };
            let a = gen_expr(ctx, &ValType::Long);
            let b = gen_expr(ctx, &ValType::Long);
            Form::call(name, vec![a, b])
        }),
    }
}

fn gen_long_unary(ctx: &mut Ctx) -> Form {
    nested(ctx, 1, 0, |ctx| {
        let no_safe_math = ctx.opts.no_safe_math;
        let op = match ctx.rng.below(3) {
            0 => ["s-neg", "-"],
            1 => ["s-inc", "inc"],
            _ => ["s-dec", "dec"],
        };
        let name = if no_safe_math { op[1] } else { op[0] };
        let a = gen_expr(ctx, &ValType::Long);
        Form::call(name, vec![a])
    })
}

fn gen_long_div(ctx: &mut Ctx) -> Form {
    nested(ctx, 1, 0, |ctx| {
        let no_safe_math = ctx.opts.no_safe_math;
        let op = if ctx.rng.next_bool() {
            ["s-quot", "quot"]
        } else {
            ["s-rem", "rem"]
        };
        let name = if no_safe_math { op[1] } else { op[0] };
        let a = gen_expr(ctx, &ValType::Long);
        let b = gen_expr(ctx, &ValType::Long);
        Form::call(name, vec![a, b])
    })
}

fn gen_long_bit(ctx: &mut Ctx) -> Form {
    nested(ctx, 1, 0, |ctx| match ctx.rng.below(7) {
        0 => {
            let (a, b) = (gen_expr(ctx, &ValType::Long), gen_expr(ctx, &ValType::Long));
            Form::call("bit-and", vec![a, b])
        }
        1 => {
            let (a, b) = (gen_expr(ctx, &ValType::Long), gen_expr(ctx, &ValType::Long));
            Form::call("bit-or", vec![a, b])
        }
        2 => {
            let (a, b) = (gen_expr(ctx, &ValType::Long), gen_expr(ctx, &ValType::Long));
            Form::call("bit-xor", vec![a, b])
        }
        3 => {
            let a = gen_expr(ctx, &ValType::Long);
            Form::call("bit-not", vec![a])
        }
        4 => {
            let (a, b) = (gen_expr(ctx, &ValType::Long), gen_expr(ctx, &ValType::Long));
            Form::call("bit-shift-left", vec![a, b])
        }
        5 => {
            let (a, b) = (gen_expr(ctx, &ValType::Long), gen_expr(ctx, &ValType::Long));
            Form::call("bit-shift-right", vec![a, b])
        }
        _ => {
            let (a, b) = (gen_expr(ctx, &ValType::Long), gen_expr(ctx, &ValType::Long));
            Form::call("unsigned-bit-shift-right", vec![a, b])
        }
    })
}

/// `(long (count <coll>))`. The coercion keeps the static type exact:
/// `count` returns an Integer, not a Long.
fn gen_count(ctx: &mut Ctx) -> Form {
    nested(ctx, 1, 0, |ctx| {
        let mut kinds: Vec<(u64, u8)> = Vec::new();
        if !ctx.opts.no_vectors {
            kinds.push((2, 0));
        }
        if !ctx.opts.no_maps {
            kinds.push((2, 1));
        }
        if !ctx.opts.no_strings {
            kinds.push((1, 2));
        }
        let coll = match pick_kind(ctx, &kinds) {
            1 => {
                let vt = random_elem_type(ctx.rng, ctx.opts, &ctx.env.records);
                gen_expr(ctx, &ValType::Map(Box::new(vt)))
            }
            2 => gen_expr(ctx, &ValType::Str),
            _ => {
                let et = random_elem_type(ctx.rng, ctx.opts, &ctx.env.records);
                gen_expr(ctx, &ValType::Vec(Box::new(et)))
            }
        };
        Form::call("long", vec![Form::call("count", vec![coll])])
    })
}

/// `(long (compare <a> <b>))`, again coerced (compare returns an int).
fn gen_compare(ctx: &mut Ctx) -> Form {
    nested(ctx, 1, 0, |ctx| {
        let use_strings = !ctx.opts.no_strings && ctx.rng.chance(0.3);
        let t = if use_strings {
            ValType::Str
        } else {
            ValType::Long
        };
        let a = gen_expr(ctx, &t);
        let b = gen_expr(ctx, &t);
        Form::call("long", vec![Form::call("compare", vec![a, b])])
    })
}

// --- Double ---

fn gen_double_op(ctx: &mut Ctx) -> Form {
    // Tags: 0 arith, 1 unary, 2 from-long, 3 nth, 4 peek, 5 get, 6 field,
    // 7 reduce.
    let o = ctx.opts;
    let mut kinds: Vec<(u64, u8)> = vec![(8, 0), (1, 1), (3, 2)];
    if !o.no_vectors {
        kinds.push((3, 3));
        kinds.push((1, 4));
    }
    if !o.no_maps {
        kinds.push((2, 5));
    }
    if !record_fields_of(&ctx.env.records, &ValType::Double).is_empty() {
        kinds.push((2, 6));
    }
    if !o.no_higher_order && !reduce_cands(ctx, &ValType::Double).is_empty() {
        kinds.push((1, 7));
    }
    match pick_kind(ctx, &kinds) {
        1 => nested(ctx, 1, 0, |ctx| {
            let a = gen_expr(ctx, &ValType::Double);
            Form::call("-", vec![a])
        }),
        2 => nested(ctx, 1, 0, |ctx| {
            let a = gen_expr(ctx, &ValType::Long);
            Form::call("double", vec![a])
        }),
        3 => gen_nth(ctx, &ValType::Double),
        4 => gen_peek(ctx, &ValType::Double),
        5 => gen_get(ctx, &ValType::Double),
        6 => gen_field(ctx, &ValType::Double),
        7 => gen_reduce(ctx, &ValType::Double),
        _ => nested(ctx, 1, 0, |ctx| {
            // `+ - *` are total on doubles, but raw `/` is not: it
            // resolves `Numbers/divide` by static operand type, and the
            // (Object, Object) overload throws "Divide by zero" on a zero
            // divisor — only the primitive (double, double) path yields
            // Inf/NaN. Object-typed operands (params, derefs, calls)
            // therefore need `s-div`, which retypes both sides.
            let div = if ctx.opts.no_safe_math { "/" } else { "s-div" };
            let op = if ctx.opts.no_divs {
                ["+", "-", "*"][ctx.rng.below(3)]
            } else {
                ["+", "-", "*", div][ctx.rng.below(4)]
            };
            let a = gen_expr(ctx, &ValType::Double);
            let b = gen_expr(ctx, &ValType::Double);
            Form::call(op, vec![a, b])
        }),
    }
}

// --- Bool ---

fn gen_bool_op(ctx: &mut Ctx) -> Form {
    // Tags: 0 comparison, 1 equality, 2 logic, 3 long-pred, 4 type-pred,
    // 5 empty?, 6 contains?, 7 distinct?, 8 str-pred, 9 infinite?,
    // 10 field, 11 nth, 12 peek, 13 get, 14 reduce, 15 cas.
    let o = ctx.opts;
    let mut kinds: Vec<(u64, u8)> = vec![(6, 0), (4, 1), (3, 2), (3, 3), (3, 4)];
    if !o.no_vectors || !o.no_maps || !o.no_strings {
        kinds.push((1, 5));
    }
    if !o.no_maps {
        kinds.push((1, 6));
    }
    kinds.push((1, 7));
    if !o.no_strings {
        kinds.push((2, 8));
    }
    if !o.no_doubles {
        kinds.push((1, 9));
    }
    if !record_fields_of(&ctx.env.records, &ValType::Bool).is_empty() {
        kinds.push((2, 10));
    }
    if !o.no_vectors {
        kinds.push((3, 11));
        kinds.push((1, 12));
    }
    if !o.no_maps {
        kinds.push((2, 13));
    }
    if !o.no_higher_order && !reduce_cands(ctx, &ValType::Bool).is_empty() {
        kinds.push((1, 14));
    }
    if !writable_globals(ctx).is_empty() {
        kinds.push((1, 15));
    }
    match pick_kind(ctx, &kinds) {
        1 => gen_equality(ctx),
        2 => gen_logic(ctx),
        3 => gen_long_pred(ctx),
        4 => gen_type_pred(ctx),
        5 => gen_empty(ctx),
        6 => gen_contains(ctx),
        7 => gen_distinct(ctx),
        8 => gen_str_pred(ctx),
        9 => nested(ctx, 1, 0, |ctx| {
            let a = gen_expr(ctx, &ValType::Double);
            Form::call("infinite?", vec![a])
        }),
        10 => gen_field(ctx, &ValType::Bool),
        11 => gen_nth(ctx, &ValType::Bool),
        12 => gen_peek(ctx, &ValType::Bool),
        13 => gen_get(ctx, &ValType::Bool),
        14 => gen_reduce(ctx, &ValType::Bool),
        15 => gen_cas(ctx),
        _ => gen_comparison(ctx),
    }
}

fn gen_comparison(ctx: &mut Ctx) -> Form {
    nested(ctx, 1, 0, |ctx| {
        let no_doubles = ctx.opts.no_doubles;
        let op = ["<", ">", "<=", ">="][ctx.rng.below(4)];
        // Occasionally compare across the two numeric types (total).
        let (ta, tb) = if !no_doubles && ctx.rng.chance(0.3) {
            if ctx.rng.next_bool() {
                (ValType::Long, ValType::Double)
            } else {
                (ValType::Double, ValType::Long)
            }
        } else if !no_doubles && ctx.rng.chance(0.3) {
            (ValType::Double, ValType::Double)
        } else {
            (ValType::Long, ValType::Long)
        };
        let a = gen_expr(ctx, &ta);
        let b = gen_expr(ctx, &tb);
        Form::call(op, vec![a, b])
    })
}

fn gen_equality(ctx: &mut Ctx) -> Form {
    nested(ctx, 1, 0, |ctx| {
        let op = if ctx.rng.next_bool() { "=" } else { "not=" };
        // Usually same-type, occasionally cross-type (total).
        if ctx.rng.chance(0.7) {
            let t = random_type(ctx.rng, ctx.opts, &ctx.env.records);
            let a = gen_expr(ctx, &t);
            let b = gen_expr(ctx, &t);
            Form::call(op, vec![a, b])
        } else {
            let mut scalars = vec![ValType::Long, ValType::Bool];
            if !ctx.opts.no_doubles {
                scalars.push(ValType::Double);
            }
            if !ctx.opts.no_strings {
                scalars.push(ValType::Str);
            }
            let ai = ctx.rng.below(scalars.len());
            let mut bi = ctx.rng.below(scalars.len());
            while bi == ai {
                bi = ctx.rng.below(scalars.len());
            }
            let a = gen_expr(ctx, &scalars[ai]);
            let b = gen_expr(ctx, &scalars[bi]);
            Form::call(op, vec![a, b])
        }
    })
}

fn gen_logic(ctx: &mut Ctx) -> Form {
    nested(ctx, 1, 0, |ctx| match ctx.rng.below(3) {
        0 => {
            let a = gen_expr(ctx, &ValType::Bool);
            Form::call("not", vec![a])
        }
        1 => {
            let (a, b) = (gen_expr(ctx, &ValType::Bool), gen_expr(ctx, &ValType::Bool));
            Form::call("and", vec![a, b])
        }
        _ => {
            let (a, b) = (gen_expr(ctx, &ValType::Bool), gen_expr(ctx, &ValType::Bool));
            Form::call("or", vec![a, b])
        }
    })
}

fn gen_long_pred(ctx: &mut Ctx) -> Form {
    nested(ctx, 1, 0, |ctx| {
        let op = ["zero?", "pos?", "neg?", "even?", "odd?"][ctx.rng.below(5)];
        let a = gen_expr(ctx, &ValType::Long);
        Form::call(op, vec![a])
    })
}

fn gen_type_pred(ctx: &mut Ctx) -> Form {
    nested(ctx, 1, 0, |ctx| {
        // Only predicates for enabled types: a `vector?` that can never
        // be true is dead weight (and violates --no-vectors).
        let mut ops = vec!["number?", "boolean?", "keyword?"];
        if !ctx.opts.no_strings {
            ops.push("string?");
        }
        if !ctx.opts.no_vectors {
            ops.push("vector?");
        }
        if !ctx.opts.no_maps {
            ops.push("map?");
        }
        let op = ops[ctx.rng.below(ops.len())];
        let t = random_type(ctx.rng, ctx.opts, &ctx.env.records);
        let a = gen_expr(ctx, &t);
        Form::call(op, vec![a])
    })
}

fn gen_empty(ctx: &mut Ctx) -> Form {
    nested(ctx, 1, 0, |ctx| {
        let mut kinds: Vec<(u64, u8)> = Vec::new();
        if !ctx.opts.no_vectors {
            kinds.push((2, 0));
        }
        if !ctx.opts.no_maps {
            kinds.push((2, 1));
        }
        if !ctx.opts.no_strings {
            kinds.push((1, 2));
        }
        let coll = match pick_kind(ctx, &kinds) {
            1 => {
                let vt = random_elem_type(ctx.rng, ctx.opts, &ctx.env.records);
                gen_expr(ctx, &ValType::Map(Box::new(vt)))
            }
            2 => gen_expr(ctx, &ValType::Str),
            _ => {
                let et = random_elem_type(ctx.rng, ctx.opts, &ctx.env.records);
                gen_expr(ctx, &ValType::Vec(Box::new(et)))
            }
        };
        Form::call("empty?", vec![coll])
    })
}

fn gen_contains(ctx: &mut Ctx) -> Form {
    nested(ctx, 1, 0, |ctx| {
        let vt = random_elem_type(ctx.rng, ctx.opts, &ctx.env.records);
        let m = gen_expr(ctx, &ValType::Map(Box::new(vt)));
        let k = if ctx.rng.chance(0.7) {
            Form::kw(*ctx.rng.pick(&MAP_KEYS))
        } else {
            Form::kw("missing")
        };
        Form::call("contains?", vec![m, k])
    })
}

fn gen_distinct(ctx: &mut Ctx) -> Form {
    nested(ctx, 1, 0, |ctx| {
        let t = random_scalar_type(ctx.rng, ctx.opts);
        let a = gen_expr(ctx, &t);
        let b = gen_expr(ctx, &t);
        Form::call("distinct?", vec![a, b])
    })
}

fn gen_str_pred(ctx: &mut Ctx) -> Form {
    nested(ctx, 1, 0, |ctx| {
        match ctx.rng.below(4) {
            0 => {
                let a = gen_expr(ctx, &ValType::Str);
                Form::call("str/blank?", vec![a])
            }
            n => {
                // Locale-free predicates only: upper-case/lower-case depend
                // on the default locale and would be nondeterministic.
                let op = ["str/includes?", "str/starts-with?", "str/ends-with?"][n - 1];
                let a = gen_expr(ctx, &ValType::Str);
                let rem = ctx.remaining();
                let (b, bcost) =
                    gen_literal(ctx.rng, ctx.opts, &ctx.env.records, &ValType::Str, rem);
                ctx.charge(bcost);
                Form::call(op, vec![a, b])
            }
        }
    })
}

// --- Str ---

/// Regex sources that are always valid and cheap to match.
const SAFE_PATTERNS: &[&str] = &[",", " ", "a", "\\d", "[ab]+", ";", ":"];

fn gen_str_op(ctx: &mut Ctx) -> Form {
    // Tags: 0 str, 1 subs, 2 join, 3 trim, 4 reverse, 5 name,
    // 6 nth, 7 peek, 8 get, 9 field, 10 reduce.
    // (str/split returns a vector, so it lives in the vec generator.)
    debug_assert!(!ctx.opts.no_strings);
    let o = ctx.opts;
    let mut kinds: Vec<(u64, u8)> = vec![(6, 0), (3, 1)];
    if !o.no_vectors {
        kinds.push((2, 2));
        kinds.push((3, 6));
        kinds.push((1, 7));
    }
    kinds.push((1, 3));
    kinds.push((1, 4));
    kinds.push((1, 5));
    if !o.no_maps {
        kinds.push((2, 8));
    }
    if !record_fields_of(&ctx.env.records, &ValType::Str).is_empty() {
        kinds.push((2, 9));
    }
    if !o.no_higher_order && !reduce_cands(ctx, &ValType::Str).is_empty() {
        kinds.push((1, 10));
    }
    match pick_kind(ctx, &kinds) {
        1 => nested(ctx, 1, 0, |ctx| {
            let s = gen_expr(ctx, &ValType::Str);
            let a = gen_expr(ctx, &ValType::Long);
            let b = gen_expr(ctx, &ValType::Long);
            Form::call("s-subs", vec![s, a, b])
        }),
        2 => nested(ctx, 1, 0, |ctx| {
            let rem = ctx.remaining();
            let (sep, scost) = gen_literal(ctx.rng, ctx.opts, &ctx.env.records, &ValType::Str, rem);
            ctx.charge(scost);
            ctx.charge(crate::cost::DATA_OP_PREMIUM);
            let coll = gen_expr(ctx, &ValType::Vec(Box::new(ValType::Str)));
            Form::call("str/join", vec![sep, coll])
        }),
        3 => nested(ctx, 1, 0, |ctx| {
            let s = gen_expr(ctx, &ValType::Str);
            Form::call("str/trim", vec![s])
        }),
        4 => nested(ctx, 1, 0, |ctx| {
            let s = gen_expr(ctx, &ValType::Str);
            Form::call("str/reverse", vec![s])
        }),
        5 => Form::call("name", vec![Form::kw(*ctx.rng.pick(&MAP_KEYS))]),
        6 => gen_nth(ctx, &ValType::Str),
        7 => gen_peek(ctx, &ValType::Str),
        8 => gen_get(ctx, &ValType::Str),
        9 => gen_field(ctx, &ValType::Str),
        10 => gen_reduce(ctx, &ValType::Str),
        _ => nested(ctx, 1, 0, |ctx| {
            let n = ctx.rng.range_usize(1, 3);
            let mut args = Vec::with_capacity(n);
            for _ in 0..n {
                let t = random_type(ctx.rng, ctx.opts, &ctx.env.records);
                args.push(gen_expr(ctx, &t));
            }
            Form::call("str", args)
        }),
    }
}

// --- Vec ---

fn mapv_cands(ctx: &Ctx, e: &ValType) -> Vec<FuncSig> {
    let self_name = ctx.self_call.as_ref().map(|s| s.name.as_str());
    ctx.env
        .funcs
        .iter()
        .filter(|f| {
            !f.recursive
                && self_name != Some(f.name.as_str())
                && !ctx.writes_forbidden(&f.writes)
                && ctx.call_affordable(f)
                && f.params.len() == 1
                && &f.ret == e
        })
        .cloned()
        .collect()
}

fn filterv_cands(ctx: &Ctx, e: &ValType) -> Vec<FuncSig> {
    let self_name = ctx.self_call.as_ref().map(|s| s.name.as_str());
    ctx.env
        .funcs
        .iter()
        .filter(|f| {
            !f.recursive
                && self_name != Some(f.name.as_str())
                && !ctx.writes_forbidden(&f.writes)
                && ctx.call_affordable(f)
                && f.params.len() == 1
                && &f.params[0] == e
                && f.ret == ValType::Bool
        })
        .cloned()
        .collect()
}

fn gen_vec_op(ctx: &mut Ctx, e: &ValType) -> Form {
    // Tags: 0 conj, 1 assoc, 2 pop, 3 subvec, 4 take/drop, 5 sort,
    // 6 concat/interleave, 7 range, 8 for, 9 vals, 10 distinct,
    // 11 mapv, 12 filterv, 13 str/split.
    debug_assert!(!ctx.opts.no_vectors);
    let o = ctx.opts;
    let mut kinds: Vec<(u64, u8)> = vec![(4, 0), (2, 1), (1, 2), (2, 3), (2, 4)];
    // No `sort` over doubles: `(compare ##NaN x)` is 0 for every x, so
    // the comparator is inconsistent and TimSort's behavior (garbage
    // order or IllegalArgumentException) is implementation-defined.
    if matches!(e, ValType::Long | ValType::Bool | ValType::Str) {
        kinds.push((1, 5));
    }
    kinds.push((1, 6));
    if *e == ValType::Long {
        kinds.push((1, 7));
    }
    // `for` is a loop (unlike `range` alone, which is inert data).
    if !o.no_loops {
        kinds.push((1, 8));
    }
    if !o.no_maps {
        kinds.push((1, 9));
    }
    kinds.push((1, 10));
    if !o.no_higher_order {
        if !mapv_cands(ctx, e).is_empty() {
            kinds.push((2, 11));
        }
        if !filterv_cands(ctx, e).is_empty() {
            kinds.push((2, 12));
        }
    }
    if *e == ValType::Str && !o.no_strings {
        kinds.push((1, 13));
    }
    match pick_kind(ctx, &kinds) {
        1 => nested(ctx, 1, 0, |ctx| {
            let v = gen_expr(ctx, &ValType::Vec(Box::new(e.clone())));
            let i = gen_expr(ctx, &ValType::Long);
            let x = gen_expr(ctx, e);
            Form::call("s-assoc-v", vec![v, i, x])
        }),
        2 => nested(ctx, 1, 0, |ctx| {
            let v = gen_expr(ctx, &ValType::Vec(Box::new(e.clone())));
            Form::call("s-pop", vec![v])
        }),
        3 => nested(ctx, 1, 0, |ctx| {
            let v = gen_expr(ctx, &ValType::Vec(Box::new(e.clone())));
            let a = gen_expr(ctx, &ValType::Long);
            let b = gen_expr(ctx, &ValType::Long);
            Form::call("s-subvec", vec![v, a, b])
        }),
        4 => nested(ctx, 1, 0, |ctx| {
            // The total wrappers: raw take/drop cast the count to int and
            // throw on huge longs.
            let op = if ctx.rng.next_bool() {
                "s-take"
            } else {
                "s-drop"
            };
            let n = gen_expr(ctx, &ValType::Long);
            let v = gen_expr(ctx, &ValType::Vec(Box::new(e.clone())));
            Form::call(op, vec![n, v])
        }),
        5 => nested(ctx, 1, 0, |ctx| {
            ctx.charge(crate::cost::DATA_OP_PREMIUM);
            let v = gen_expr(ctx, &ValType::Vec(Box::new(e.clone())));
            Form::call("vec", vec![Form::call("sort", vec![v])])
        }),
        6 => nested(ctx, 1, 0, |ctx| {
            ctx.charge(crate::cost::DATA_OP_PREMIUM);
            let op = if ctx.rng.next_bool() {
                "concat"
            } else {
                "interleave"
            };
            let t = ValType::Vec(Box::new(e.clone()));
            let a = gen_expr(ctx, &t);
            let b = gen_expr(ctx, &t);
            Form::call("vec", vec![Form::call(op, vec![a, b])])
        }),
        7 => {
            let max_iters = ctx.opts.max_loop_iters as i64;
            let drawn = ctx.rng.range(0, max_iters);
            // Realizing `(range n)` costs per element: shrink to fit the
            // remaining budget and charge it (an 800k range used to be
            // free, blowing time and memory outside the budget).
            let n = crate::cost::shrink_trips(drawn, 1, ctx.remaining());
            ctx.charge(n.max(0) as u64);
            Form::call("vec", vec![Form::call("range", vec![Form::int(n)])])
        }
        8 => gen_for(ctx, e),
        9 => nested(ctx, 1, 0, |ctx| {
            let m = gen_expr(ctx, &ValType::Map(Box::new(e.clone())));
            Form::call("vec", vec![Form::call("vals", vec![m])])
        }),
        10 => nested(ctx, 1, 0, |ctx| {
            ctx.charge(crate::cost::DATA_OP_PREMIUM);
            let v = gen_expr(ctx, &ValType::Vec(Box::new(e.clone())));
            Form::call("vec", vec![Form::call("distinct", vec![v])])
        }),
        11 => {
            let cands = mapv_cands(ctx, e);
            let f = cands[ctx.rng.below(cands.len())].clone();
            ctx.called.insert(f.name.clone());
            nested(ctx, 1, 0, |ctx| {
                let coll = gen_expr(ctx, &ValType::Vec(Box::new(f.params[0].clone())));
                // Cap applications by the remaining budget (take zero is
                // valid: an empty result).
                let k = crate::cost::take_k(1, f.body_cost.max(1), ctx.remaining());
                ctx.charge((k.max(0) as u64).saturating_mul(f.body_cost));
                let coll = Form::call("take", vec![Form::int(k), coll]);
                Form::call("mapv", vec![Form::sym(f.name.clone()), coll])
            })
        }
        12 => {
            let cands = filterv_cands(ctx, e);
            let f = cands[ctx.rng.below(cands.len())].clone();
            ctx.called.insert(f.name.clone());
            nested(ctx, 1, 0, |ctx| {
                let t = ValType::Vec(Box::new(e.clone()));
                let coll = gen_expr(ctx, &t);
                let k = crate::cost::take_k(1, f.body_cost.max(1), ctx.remaining());
                ctx.charge((k.max(0) as u64).saturating_mul(f.body_cost));
                let coll = Form::call("take", vec![Form::int(k), coll]);
                Form::call("filterv", vec![Form::sym(f.name.clone()), coll])
            })
        }
        13 => nested(ctx, 1, 0, |ctx| {
            let s = gen_expr(ctx, &ValType::Str);
            let pat = Form::Regex((*ctx.rng.pick(SAFE_PATTERNS)).to_string());
            Form::call("str/split", vec![s, pat])
        }),
        _ => nested(ctx, 1, 0, |ctx| {
            let v = gen_expr(ctx, &ValType::Vec(Box::new(e.clone())));
            let n = ctx.rng.range_usize(1, 2);
            let mut args = vec![v];
            for _ in 0..n {
                args.push(gen_expr(ctx, e));
            }
            Form::call("conj", args)
        }),
    }
}

fn gen_for(ctx: &mut Ctx, e: &ValType) -> Form {
    nested(ctx, 1, 1, |ctx| {
        ctx.in_loop += 1;
        let n = ctx.rng.range_usize(1, 2);
        let mut flat = Vec::with_capacity(n * 2);
        let mut bound = Vec::with_capacity(n);
        for i in 0..n {
            let et = random_elem_type(ctx.rng, ctx.opts, &ctx.env.records);
            let v = gen_expr(ctx, &ValType::Vec(Box::new(et.clone())));
            let name = format!("x{i}");
            flat.push(Form::sym(name.clone()));
            flat.push(v);
            bound.push((name, et));
        }
        let with_when = ctx.rng.chance(0.4);
        let before = ctx.spent;
        let (body, cond) = with_locals(ctx, bound, |ctx| {
            let c = with_when.then(|| gen_expr(ctx, &ValType::Bool));
            let b = gen_expr(ctx, e);
            (b, c)
        });
        ctx.in_loop -= 1;
        let has_calls = form_has_call(&body, &ctx.env.funcs)
            || cond
                .as_ref()
                .is_some_and(|c| form_has_call(c, &ctx.env.funcs));
        // Bound cartesian trips by the remaining budget (see gen_doseq).
        let per_trip = ctx.spent.saturating_sub(before).max(1);
        let mut k_take = crate::cost::take_k(n, per_trip, ctx.remaining());
        if has_calls {
            k_take = k_take.min(MAX_CALL_LOOP_TRIPS);
        }
        for i in (1..flat.len()).step_by(2) {
            let coll = std::mem::replace(&mut flat[i], Form::nil());
            flat[i] = Form::call("take", vec![Form::int(k_take), coll]);
        }
        let trips = (k_take.max(0) as u64).saturating_pow(n as u32);
        ctx.charge(trips.saturating_sub(1).saturating_mul(per_trip));
        if let Some(c) = cond {
            flat.push(Form::kw("when"));
            flat.push(c);
        }
        Form::call(
            "vec",
            vec![Form::list(vec![Form::sym("for"), Form::vector(flat), body])],
        )
    })
}

// --- Map ---

fn distinct_pool_keys(ctx: &mut Ctx, max: usize) -> Vec<Form> {
    let n = ctx.rng.range_usize(1, max.min(MAP_KEYS.len()));
    let mut keys = MAP_KEYS.to_vec();
    ctx.rng.shuffle(&mut keys);
    keys.into_iter().take(n).map(Form::kw).collect()
}

fn gen_map_op(ctx: &mut Ctx, v: &ValType) -> Form {
    // Tags: 0 assoc, 1 dissoc, 2 select-keys, 3 merge, 4 zipmap.
    debug_assert!(!ctx.opts.no_maps);
    let mut kinds = vec![(4, 0), (1, 1)];
    // select-keys key lists, the merge key list, and zipmap values are
    // all vectors.
    if !ctx.opts.no_vectors {
        kinds.push((1, 2));
        kinds.push((1, 3));
        kinds.push((1, 4));
    }
    match pick_kind(ctx, &kinds) {
        1 => nested(ctx, 1, 0, |ctx| {
            let m = gen_expr(ctx, &ValType::Map(Box::new(v.clone())));
            let k = Form::kw(*ctx.rng.pick(&MAP_KEYS));
            Form::call("dissoc", vec![m, k])
        }),
        2 => nested(ctx, 1, 0, |ctx| {
            let m = gen_expr(ctx, &ValType::Map(Box::new(v.clone())));
            let ks = distinct_pool_keys(ctx, 4);
            Form::call("select-keys", vec![m, Form::vector(ks)])
        }),
        3 => nested(ctx, 1, 0, |ctx| {
            // Merging two small maps could exceed 8 entries and flip the
            // result into a hash-map; selecting the pooled keys back out
            // preserves the array-map invariant.
            let t = ValType::Map(Box::new(v.clone()));
            let a = gen_expr(ctx, &t);
            let b = gen_expr(ctx, &t);
            let all: Vec<Form> = MAP_KEYS.iter().map(|k| Form::kw(*k)).collect();
            Form::call(
                "select-keys",
                vec![Form::call("merge", vec![a, b]), Form::vector(all)],
            )
        }),
        4 => nested(ctx, 1, 0, |ctx| {
            let ks = distinct_pool_keys(ctx, 4);
            let vals = gen_expr(ctx, &ValType::Vec(Box::new(v.clone())));
            Form::call("zipmap", vec![Form::vector(ks), vals])
        }),
        _ => nested(ctx, 1, 0, |ctx| {
            let m = gen_expr(ctx, &ValType::Map(Box::new(v.clone())));
            let k = Form::kw(*ctx.rng.pick(&MAP_KEYS));
            let x = gen_expr(ctx, v);
            Form::call("assoc", vec![m, k, x])
        }),
    }
}

// --- Record ---

fn gen_record_op(ctx: &mut Ctx, idx: usize) -> Form {
    // Tags: 0 ctor, 1 assoc, 2 nth, 3 peek, 4 get, 5 reduce.
    let t = ValType::Record(idx);
    let o = ctx.opts;
    let mut kinds: Vec<(u64, u8)> = vec![(4, 0), (3, 1)];
    if !o.no_vectors {
        kinds.push((3, 2));
        kinds.push((1, 3));
    }
    if !o.no_maps {
        kinds.push((2, 4));
    }
    if !o.no_higher_order && !reduce_cands(ctx, &t).is_empty() {
        kinds.push((1, 5));
    }
    match pick_kind(ctx, &kinds) {
        1 => {
            let fields = ctx.env.records[idx].fields.clone();
            let fi = ctx.rng.below(fields.len());
            let (fname, ft) = fields[fi].clone();
            nested(ctx, 1, 0, |ctx| {
                let r = gen_expr(ctx, &ValType::Record(idx));
                let x = gen_expr(ctx, &ft);
                Form::call("assoc", vec![r, Form::kw(fname), x])
            })
        }
        2 => gen_nth(ctx, &t),
        3 => gen_peek(ctx, &t),
        4 => gen_get(ctx, &t),
        5 => gen_reduce(ctx, &t),
        _ => {
            let fields = ctx.env.records[idx].fields.clone();
            nested(ctx, 1, 0, |ctx| {
                let mut args = Vec::with_capacity(fields.len());
                for (_, ft) in &fields {
                    args.push(gen_expr(ctx, ft));
                }
                let ctor = ctx.env.records[idx].ctor_name();
                Form::call(&ctor, args)
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::parse_generator_args;

    fn ctx_for<'a>(opts: &'a GenerateOptions, rng: &'a mut Rng) -> Ctx<'a> {
        Ctx::new(opts, rng)
    }

    #[test]
    fn expired_deadline_panics_with_gen_timeout() {
        let opts = parse_generator_args::<Vec<String>, String>(Vec::new()).unwrap();
        let mut rng = Rng::new(1);
        let mut ctx = ctx_for(&opts, &mut rng);
        ctx.deadline = Some(Instant::now() - std::time::Duration::from_secs(1));
        let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| ctx.charge(1)))
            .expect_err("charge past deadline must panic");
        assert!(err.is::<GenTimeout>(), "wrong panic payload");
        // No deadline: charging is total.
        let mut ctx = ctx_for(&opts, &mut rng);
        ctx.charge(u64::MAX);
        assert_eq!(ctx.remaining(), 0);
    }

    #[test]
    fn sort_never_over_doubles() {
        // NaN makes the sort comparator inconsistent (TimSort garbage or
        // throw), so the sort arm is not offered for Double elements.
        // Longs keep it (positive control against a dead arm).
        fn is_sort(form: &Form) -> bool {
            matches!(form, Form::List(fs)
                if fs.len() == 2 && fs.first().and_then(sym) == Some("vec")
                && matches!(&fs[1], Form::List(g) if g.first().and_then(sym) == Some("sort")))
        }
        fn sym(form: &Form) -> Option<&str> {
            match form {
                Form::Symbol(s) => Some(s.as_str()),
                _ => None,
            }
        }
        let opts = parse_generator_args::<Vec<String>, String>(Vec::new()).unwrap();
        for seed in 0..500 {
            let mut rng = Rng::new(seed);
            let mut ctx = ctx_for(&opts, &mut rng);
            let d = gen_vec_op(&mut ctx, &ValType::Double);
            assert!(!is_sort(&d), "sorted doubles at seed {seed}: {d:?}");
        }
        let mut sorts = 0;
        for seed in 0..500 {
            let mut rng = Rng::new(100000 + seed);
            let mut ctx = ctx_for(&opts, &mut rng);
            if is_sort(&gen_vec_op(&mut ctx, &ValType::Long)) {
                sorts += 1;
            }
        }
        assert!(sorts > 0, "sort arm never fires for longs");
    }

    #[test]
    fn generates_with_empty_env() {
        // The first function sees no earlier functions and possibly no
        // globals; generation must not panic and must produce something.
        let opts =
            parse_generator_args(["--max-expr-depth", "3", "--max-block-depth", "2"]).unwrap();
        let mut rng = Rng::new(42);
        let mut ctx = ctx_for(&opts, &mut rng);
        for t in [
            ValType::Long,
            ValType::Double,
            ValType::Bool,
            ValType::Str,
            ValType::Vec(Box::new(ValType::Long)),
            ValType::Map(Box::new(ValType::Bool)),
        ] {
            let f = gen_expr(&mut ctx, &t);
            assert!(f.size() >= 1, "empty form for {t:?}");
            let text = print_compact(&f);
            assert!(!text.is_empty());
        }
        let body = gen_body(&mut ctx, &ValType::Long, 3);
        assert!(!body.is_empty());
    }

    #[test]
    fn generation_is_deterministic() {
        let opts = parse_generator_args(Vec::<String>::new()).unwrap();
        let mut rng_a = Rng::new(7);
        let mut rng_b = Rng::new(7);
        let mut ctx_a = ctx_for(&opts, &mut rng_a);
        let mut ctx_b = ctx_for(&opts, &mut rng_b);
        for _ in 0..20 {
            let t = random_type(ctx_a.rng, ctx_a.opts, &ctx_a.env.records);
            // Advance both RNGs identically.
            let t2 = random_type(ctx_b.rng, ctx_b.opts, &ctx_b.env.records);
            assert_eq!(t, t2);
            assert_eq!(gen_expr(&mut ctx_a, &t), gen_expr(&mut ctx_b, &t));
        }
    }

    #[test]
    fn locals_shadow_innermost_wins() {
        // Template names (x0, i, old, …) shadow across nesting; lookup must
        // resolve to the innermost binding, or an expression could observe
        // a variable of the wrong type at runtime.
        let opts = parse_generator_args(Vec::<String>::new()).unwrap();
        let mut rng = Rng::new(1);
        let mut ctx = ctx_for(&opts, &mut rng);
        ctx.env.locals.push(("x".to_string(), ValType::Long));
        ctx.env.locals.push(("x".to_string(), ValType::Str));
        assert!(ctx.locals_of(&ValType::Long).is_empty());
        assert_eq!(ctx.locals_of(&ValType::Str), vec!["x".to_string()]);
    }

    #[test]
    fn depth_zero_yields_leaves() {
        let opts = parse_generator_args(Vec::<String>::new()).unwrap();
        let mut rng = Rng::new(1);
        let mut ctx = ctx_for(&opts, &mut rng);
        ctx.depth = 0;
        // A leaf is an atom, a variable, a deref, or a (shallow) literal.
        for _ in 0..50 {
            let t = random_scalar_type(ctx.rng, ctx.opts);
            let f = gen_expr(&mut ctx, &t);
            assert!(f.size() <= 3, "non-leaf at depth 0: {:?}", f);
        }
    }
}
