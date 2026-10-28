//! End-to-end tests: generate programs with the real binary and execute
//! them with the real Clojure CLI when available.

use std::path::{Path, PathBuf};
use std::process::Command;

use clojure_smith::driver::unique_tag;

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_clojure-smith"))
}

fn clojure_available() -> bool {
    Command::new("clojure")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "cs-it-{}-{name}-{}",
        std::process::id(),
        unique_tag()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run_clojure(file: &Path) -> (bool, String) {
    let out = Command::new("clojure")
        .arg("-M")
        .arg(file)
        .output()
        .expect("spawn clojure");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

#[test]
fn cli_help_version_and_errors() {
    let bin = binary();
    let help = Command::new(&bin).arg("--help").output().unwrap();
    assert!(help.status.success());
    let text = String::from_utf8_lossy(&help.stdout);
    assert!(text.contains("--seed"), "{text}");
    assert!(text.contains("drive"), "{text}");
    assert!(text.contains("reduce"), "{text}");

    let ver = Command::new(&bin).arg("--version").output().unwrap();
    assert!(ver.status.success());
    assert!(String::from_utf8_lossy(&ver.stdout).contains("0.1.0"));

    assert!(
        !Command::new(&bin)
            .arg("--bogus-flag")
            .output()
            .unwrap()
            .status
            .success()
    );
    // Option validation is enforced.
    assert!(
        !Command::new(&bin)
            .args(["--max-params", "21"])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(
        !Command::new(&bin)
            .args(["--max-map-size", "9"])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(
        !Command::new(&bin)
            .args(["--max-string-len", "5000"])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(
        !Command::new(&bin)
            .args(["--max-vector-len", "2000"])
            .output()
            .unwrap()
            .status
            .success()
    );
}

#[test]
fn generate_is_byte_identical() {
    let dir = tmpdir("det");
    let a = dir.join("a.clj");
    let b = dir.join("b.clj");
    for f in [&a, &b] {
        assert!(
            Command::new(binary())
                .args(["--seed", "99", "--quiet"])
                .arg("-o")
                .arg(f)
                .status()
                .unwrap()
                .success()
        );
    }
    assert_eq!(std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());
    let _ = std::fs::remove_dir_all(&dir);
}

/// The core property: generated programs run, print exactly one checksum
/// line, and are deterministic across runs.
#[test]
fn generated_programs_run_and_are_deterministic() {
    if !clojure_available() {
        eprintln!("skipping: clojure not on PATH");
        return;
    }
    let dir = tmpdir("gen");
    // (extra args, seeds)
    let cases: &[(&[&str], &[i64])] = &[
        (&[], &[1, 2, 3, 7, 42]),
        // Regression seeds: each once failed to run cleanly under default
        // options (primitive loop locals, nested same-atom swaps retrying
        // forever, `(long ##Inf)` throwing, shadowed loop variables,
        // take/drop int overflow, or exponential recursion blowup).
        (
            &[],
            &[15, 17, 19, 22, 25, 26, 27, 29, 32, 33, 35, 38, 39, 40],
        ),
        (&["--no-loops"], &[11]),
        (&["--no-vectors", "--no-maps"], &[12]),
        (&["--no-strings", "--no-doubles", "--no-records"], &[13]),
        (&["--no-bitops", "--no-divs", "--no-recursion"], &[14]),
        (&["--no-safe-math"], &[15, 16]),
        (
            &[
                "--max-funcs",
                "8",
                "--max-expr-depth",
                "6",
                "--max-block-size",
                "6",
            ],
            &[17],
        ),
    ];
    for (extra, seeds) in cases {
        for seed in *seeds {
            let file = dir.join(format!("prog-{seed}.clj"));
            let mut cmd = Command::new(binary());
            cmd.args(["--seed", &seed.to_string(), "--quiet"])
                .arg("-o")
                .arg(&file);
            for a in *extra {
                cmd.arg(a);
            }
            assert!(cmd.status().unwrap().success(), "generate {seed} {extra:?}");
            let (ok1, out1) = run_clojure(&file);
            assert!(ok1, "seed {seed} {extra:?} exited nonzero");
            let (ok2, out2) = run_clojure(&file);
            assert!(ok2, "seed {seed} {extra:?} second run failed");
            assert_eq!(out1, out2, "nondeterministic seed {seed} {extra:?}");
            let lines: Vec<&str> = out1.lines().collect();
            assert_eq!(lines.len(), 1, "expected one line: {out1:?}");
            let line = lines[0];
            if extra.contains(&"--no-safe-math") {
                assert!(
                    line.starts_with("checksum: ") || line.starts_with("threw: "),
                    "{line:?}"
                );
            } else {
                assert!(
                    line.starts_with("checksum: "),
                    "unexpected threw with safe math (seed {seed} {extra:?}): {line:?}"
                );
                line["checksum: ".len()..]
                    .parse::<i64>()
                    .expect("checksum must be an integer");
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The reducer must handle real generated code (exercising the parser on
/// every emitted syntax feature) and make it smaller.
#[test]
#[cfg(unix)]
fn reduce_shrinks_generated_program() {
    let dir = tmpdir("red");
    let prog = dir.join("prog.clj");
    let small = dir.join("small.clj");
    assert!(
        Command::new(binary())
            .args(["--seed", "3", "--quiet"])
            .arg("-o")
            .arg(&prog)
            .status()
            .unwrap()
            .success()
    );
    let before = std::fs::read_to_string(&prog).unwrap();
    let status = Command::new(binary())
        .arg("reduce")
        .arg("--test")
        .arg("grep -q smith-hash {} && grep -q checksum {}")
        .arg(&prog)
        .arg("-o")
        .arg(&small)
        .args(["--tries", "500", "--quiet"])
        .status()
        .unwrap();
    assert!(status.success());
    let after = std::fs::read_to_string(&small).unwrap();
    assert!(after.len() < before.len(), "not reduced");
    assert!(after.contains("smith-hash") && after.contains("checksum"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Suppressed-feature programs must still run: generation under each
/// `--no-*` combination produces a program that exits 0 with a single
/// checksum line (the structural suppression matrix only checks syntax).
#[test]
fn suppressed_programs_run() {
    if !clojure_available() {
        eprintln!("skipping: clojure not on PATH");
        return;
    }
    let dir = tmpdir("sup");
    let cases: &[&[&str]] = &[
        &["--no-vectors", "--no-maps", "--no-records"],
        &["--no-loops", "--no-recursion", "--no-higher-order"],
        &["--no-strings", "--no-doubles", "--no-bitops", "--no-divs"],
        &[
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
        ],
    ];
    for (i, extra) in cases.iter().enumerate() {
        let file = dir.join(format!("sup-{i}.clj"));
        let mut cmd = Command::new(binary());
        cmd.args(["--seed", "7", "--quiet"]).arg("-o").arg(&file);
        for a in *extra {
            cmd.arg(a);
        }
        assert!(cmd.status().unwrap().success(), "generate {extra:?}");
        let (ok, out) = run_clojure(&file);
        assert!(ok, "{extra:?} exited nonzero");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 1, "{extra:?}: expected one line: {out:?}");
        assert!(lines[0].starts_with("checksum: "), "{extra:?}: {out:?}");
        lines[0]["checksum: ".len()..]
            .parse::<i64>()
            .expect("checksum must be an integer");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn drive_no_checksum_runs_and_warns() {
    if !clojure_available() {
        eprintln!("skipping: clojure not on PATH");
        return;
    }
    let dir = tmpdir("drive-nc");
    let out = Command::new(binary())
        .args([
            "drive",
            "--seeds",
            "1-2",
            "--jobs",
            "1",
            "--quiet",
            "--fail-dir",
        ])
        .arg(dir.join("failures"))
        .args(["--", "--no-checksum"])
        .output()
        .expect("spawn drive");
    assert!(out.status.success(), "drive failed: {out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("disables the differential oracle"),
        "missing warning: {err:?}"
    );
    assert!(
        err.contains("2 ok, 0 mismatch, 0 error"),
        "missing summary: {err:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
