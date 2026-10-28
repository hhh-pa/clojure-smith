use clap::{Parser, Subcommand};
use clojure_smith::driver::DriveOptions;
use clojure_smith::gen_program::run_generate;
use clojure_smith::options::GenerateOptions;
use clojure_smith::reduce::ReduceOptions;

#[derive(Parser, Debug)]
#[command(
    name = "clojure-smith",
    version,
    about = "A random generator of Clojure programs, for differential testing of Clojure compilers",
    long_about = "clojure-smith generates random, deterministic Clojure programs and runs them \
                  under multiple configurations to find compiler bugs by differential testing.\n\n\
                  With no subcommand it generates one program (like Csmith's `csmith > out.c`):\n    \
                  clojure-smith --seed 42 -o prog.clj\n\n\
                  The `drive` subcommand loops that process over many seeds and compares \
                  outputs; `reduce` minimizes a failing program."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    #[command(flatten)]
    generate: GenerateOptions,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Generate programs for many seeds, run each under several runners,
    /// and report output divergences.
    Drive(DriveOptions),
    /// Minimize a program while a test command still reports it interesting.
    Reduce(ReduceOptions),
}

fn main() {
    let cli = Cli::parse();
    let code = match cli.command {
        None => run_generate(&cli.generate),
        Some(Commands::Drive(o)) => clojure_smith::driver::run_drive(&o),
        Some(Commands::Reduce(o)) => clojure_smith::reduce::run_reduce(&o),
    };
    std::process::exit(code);
}
