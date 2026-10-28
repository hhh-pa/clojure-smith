//! Runtime support emitted into every generated program.
//!
//! The safe-math wrappers live in `preamble.clj`, which is embedded at
//! compile time and parsed with the same [`crate::form`] parser the reducer
//! uses — so the template is guaranteed to be syntactically valid. The entry
//! point is built programmatically because it depends on the generated
//! globals and functions.

use crate::form::{Form, parse_all};

const PREAMBLE_SRC: &str = include_str!("preamble.clj");

/// The safe-math and checksum helper forms.
pub fn preamble_forms() -> Vec<Form> {
    parse_all(PREAMBLE_SRC).expect("preamble template must parse")
}

/// `(ns <name>)`, requiring `clojure.string` when string support is on.
pub fn ns_form(name: &str, with_string_lib: bool) -> Form {
    if with_string_lib {
        Form::list(vec![
            Form::sym("ns"),
            Form::sym(name),
            Form::list(vec![
                Form::kw("require"),
                Form::vector(vec![
                    Form::sym("clojure.string"),
                    Form::kw("as"),
                    Form::sym("str"),
                ]),
            ]),
        ])
    } else {
        Form::list(vec![Form::sym("ns"), Form::sym(name)])
    }
}

/// The entry point: calls every generated function, hashes the results plus
/// all global state, and prints exactly one line. Any unexpected exception
/// is reported deterministically by class name instead of a stack trace.
pub fn entry_forms(global_names: &[String], calls: Vec<Form>, checksum: bool) -> Vec<Form> {
    let mut bindings = Vec::with_capacity(calls.len() * 2);
    let mut results = Vec::with_capacity(calls.len());
    for (i, call) in calls.into_iter().enumerate() {
        let name = format!("r{i}");
        bindings.push(Form::sym(name.clone()));
        bindings.push(call);
        results.push(Form::sym(name));
    }

    let body = if checksum {
        let mut hashed: Vec<Form> = global_names
            .iter()
            .map(|g| Form::Deref(Box::new(Form::sym(g.clone()))))
            .collect();
        hashed.extend(results);
        if hashed.is_empty() {
            hashed.push(Form::int(0));
        }
        Form::call(
            "println",
            vec![
                Form::string("checksum:"),
                Form::call("smith-hash", vec![Form::vector(hashed)]),
            ],
        )
    } else {
        Form::call("println", vec![Form::string("done")])
    };

    let guarded_body = if bindings.is_empty() {
        body
    } else {
        Form::list(vec![Form::sym("let"), Form::vector(bindings), body])
    };
    let guarded = Form::list(vec![
        Form::sym("try"),
        guarded_body,
        Form::list(vec![
            Form::sym("catch"),
            Form::sym("Throwable"),
            Form::sym("t"),
            Form::call(
                "println",
                vec![
                    Form::string("threw:"),
                    Form::call("pr-str", vec![Form::call("type", vec![Form::sym("t")])]),
                ],
            ),
        ]),
    ]);

    vec![
        Form::list(vec![
            Form::sym("defn"),
            Form::sym("smith-main"),
            Form::vector(vec![]),
            guarded,
        ]),
        Form::list(vec![Form::sym("smith-main")]),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::form::print_compact;

    #[test]
    fn preamble_parses_and_mentions_helpers() {
        let forms = preamble_forms();
        assert!(forms.len() >= 15, "expected helpers, got {}", forms.len());
        let names: Vec<String> = forms
            .iter()
            .filter_map(|f| match f {
                Form::List(fs) => match (&fs[0], &fs[1]) {
                    (Form::Symbol(h), Form::Symbol(n)) if h == "defn" => Some(n.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        for want in [
            "s+",
            "s-quot",
            "s-div",
            "s-nth",
            "s-subs",
            "s-get",
            "s-long",
            "s-take",
            "s-drop",
            "smith-hash",
        ] {
            assert!(
                names.iter().any(|n| n == want),
                "missing {want} in {names:?}"
            );
        }
    }

    #[test]
    fn entry_shape() {
        let entry = entry_forms(
            &["g0".to_string()],
            vec![Form::call("f0", vec![Form::int(1)])],
            true,
        );
        assert_eq!(entry.len(), 2);
        let text = print_compact(&entry[0]);
        assert!(text.contains("smith-main"), "{text}");
        assert!(text.contains("checksum:"), "{text}");
        assert!(text.contains("threw:"), "{text}");
        assert!(text.contains("@g0"), "{text}");
    }

    #[test]
    fn entry_without_checksum_or_calls() {
        let entry = entry_forms(&[], vec![], false);
        let text = print_compact(&entry[0]);
        assert!(text.contains("\"done\""), "{text}");
    }

    #[test]
    fn ns_shapes() {
        assert_eq!(print_compact(&ns_form("fuzz-1", false)), "(ns fuzz-1)");
        let with = print_compact(&ns_form("fuzz-1", true));
        assert!(with.contains("clojure.string"), "{with}");
    }
}
