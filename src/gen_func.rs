//! Function generation.
//!
//! Functions are generated sequentially: each new function may call any
//! previously defined one (plus itself, when recursive). Recursion is
//! direct only and always guarded by a fuel parameter: the body has the
//! shape `(if (pos? fuel) <may-call-self> <base>)`, outer calls pass a
//! small constant fuel, and self calls pass the decremented fuel — so
//! recursion always terminates.

use crate::form::Form;
use crate::gen_expr::{Ctx, FuncSig, SelfCall, gen_body, gen_expr, gen_stmt};
use crate::types::{ValType, random_type};
use std::collections::HashSet;

pub struct GeneratedFunc {
    pub form: Form,
    pub sig: FuncSig,
}

pub fn gen_func(ctx: &mut Ctx, idx: usize) -> GeneratedFunc {
    ctx.direct_writes.clear();
    ctx.called.clear();
    ctx.self_sites = 0;
    let name = format!("f{idx}");
    let no_recursion = ctx.opts.no_recursion;
    let max_params = ctx.opts.max_params as usize;
    let max_block_size = ctx.opts.max_block_size;
    let recursive = !no_recursion && ctx.rng.chance(0.25);
    let ret = random_type(ctx.rng, ctx.opts, &ctx.env.records);

    let mut params = Vec::new();
    let mut param_names = Vec::new();
    if recursive {
        params.push(ValType::Long);
        param_names.push("fuel".to_string());
    }
    let n_params = ctx.rng.range_usize(0, max_params);
    for _ in 0..n_params {
        params.push(random_type(ctx.rng, ctx.opts, &ctx.env.records));
        param_names.push(ctx.fresh("p"));
    }

    let mark = ctx.env.locals.len();
    for (n, t) in param_names.iter().zip(params.iter()) {
        ctx.env.locals.push((n.clone(), t.clone()));
    }
    let body = if recursive {
        gen_recursive_body(ctx, &name, &params, &ret, max_block_size)
    } else {
        gen_body(ctx, &ret, max_block_size)
    };
    ctx.env.locals.truncate(mark);

    let mut form = vec![
        Form::sym("defn"),
        Form::sym(name.clone()),
        Form::vector(param_names.into_iter().map(Form::sym).collect()),
    ];
    form.extend(body);

    // Transitive write set: direct writes plus everything callees may
    // write. Self calls add nothing (least fixpoint over finite,
    // fuel-bounded recursion depth) and were not recorded anyway.
    let mut writes: HashSet<String> = ctx.direct_writes.clone();
    for c in ctx.called.iter() {
        if c == &name {
            continue;
        }
        if let Some(f) = ctx.env.funcs.iter().find(|f| &f.name == c) {
            writes.extend(f.writes.iter().cloned());
        }
    }
    let mut writes: Vec<String> = writes.into_iter().collect();
    writes.sort();

    GeneratedFunc {
        form: Form::list(form),
        sig: FuncSig {
            name,
            params,
            ret,
            recursive,
            writes,
        },
    }
}

fn gen_recursive_body(
    ctx: &mut Ctx,
    name: &str,
    params: &[ValType],
    ret: &ValType,
    max_block_size: u32,
) -> Vec<Form> {
    // Shared statements run unconditionally and cannot call self.
    let n_shared = if max_block_size == 0 {
        0
    } else {
        ctx.rng.range_usize(0, max_block_size as usize)
    };
    let mut body = Vec::with_capacity(n_shared + 1);
    for _ in 0..n_shared {
        body.push(gen_stmt(ctx));
    }
    // The recursive branch installs the self context; the base branch
    // cannot see it.
    let rec = {
        ctx.self_call = Some(SelfCall {
            name: name.to_string(),
            params: params.to_vec(),
            ret: ret.clone(),
            fuel: "fuel".to_string(),
        });
        let r = gen_expr(ctx, ret);
        ctx.self_call = None;
        r
    };
    let base = gen_expr(ctx, ret);
    body.push(Form::list(vec![
        Form::sym("if"),
        Form::call("pos?", vec![Form::sym("fuel")]),
        rec,
        base,
    ]));
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::form::print_compact;
    use crate::options::parse_generator_args;
    use crate::rng::Rng;

    #[test]
    fn func_shapes() {
        let opts = parse_generator_args(Vec::<String>::new()).unwrap();
        let mut rng = Rng::new(5);
        let mut ctx = Ctx::new(&opts, &mut rng);
        let mut saw_recursive = false;
        for i in 0..10 {
            let gf = gen_func(&mut ctx, i);
            let text = print_compact(&gf.form);
            assert!(text.starts_with(&format!("(defn f{i} [")), "{text}");
            assert!(gf.form.size() > 4, "suspiciously small: {text}");
            if gf.sig.recursive {
                saw_recursive = true;
                assert_eq!(gf.sig.params[0], ValType::Long);
                assert!(text.contains("pos? fuel"), "{text}");
            }
            ctx.env.funcs.push(gf.sig);
        }
        assert!(
            saw_recursive,
            "expected some recursive functions over 10 draws"
        );
    }
}
