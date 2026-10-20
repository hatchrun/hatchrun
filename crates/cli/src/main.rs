// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The `hatchrun` command line. Customers run `validate` and `convert`
//! locally; jobs run `tests split` on the runner.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use hatchrun_workflow::{Diagnostic, convert, parse, split, to_yaml, validate};

#[derive(Parser)]
#[command(name = "hatchrun", version = env!("HATCHRUN_VERSION"), about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Check a workflow file and print every problem found
    Validate {
        /// The workflow file
        #[arg(default_value = ".hatchrun/config.yml")]
        path: PathBuf,
    },
    /// Convert GitHub Actions workflow files into a hatchrun config
    ///
    /// Jobs and files that cannot convert are left out and reported; the
    /// config holds the rest, and the exit code is 1 when any were left out.
    Convert {
        /// The files to convert (default: .github/workflows/*.yml and *.yaml)
        files: Vec<PathBuf>,
        /// Write the config here instead of stdout
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Commands for test runs
    Tests {
        #[command(subcommand)]
        command: TestsCommand,
    },
}

#[derive(Subcommand)]
enum TestsCommand {
    /// Print the items this node should run
    Split(SplitArgs),
}

#[derive(Clone, Copy, ValueEnum)]
enum By {
    /// Balance by recorded timings
    Timings,
    /// Balance by item count, ignoring timings
    Name,
}

#[derive(Args)]
struct SplitArgs {
    /// How to balance the nodes
    #[arg(long, value_enum, default_value = "timings")]
    by: By,
    /// A JSON object of item to seconds
    #[arg(long, env = "HATCHRUN_TIMINGS_FILE")]
    timings: Option<PathBuf>,
    /// This node, counting from 0
    #[arg(long, env = "HATCHRUN_NODE_INDEX", default_value_t = 0)]
    index: u32,
    /// How many nodes there are
    #[arg(long, env = "HATCHRUN_NODE_TOTAL", default_value_t = 1)]
    total: u32,
    /// The items to split (default: one per line from stdin)
    items: Vec<String>,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("hatchrun: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    match cli.command {
        Command::Validate { path } => run_validate(&path),
        Command::Convert { files, output } => run_convert(files, output.as_deref()),
        Command::Tests {
            command: TestsCommand::Split(args),
        } => run_split(args),
    }
}

fn run_validate(path: &Path) -> Result<ExitCode> {
    let source =
        std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let problems = match parse(&source) {
        Ok(config) => validate(&config),
        Err(problems) => problems,
    };
    for problem in &problems {
        println!("{}", located(path, problem));
    }
    Ok(exit(problems.is_empty()))
}

/// `PATH:line:column: message`, or `PATH: message` with no position.
fn located(path: &Path, problem: &Diagnostic) -> String {
    let path = path.display();
    match (problem.line, problem.column) {
        (Some(line), Some(column)) => format!("{path}:{line}:{column}: {}", problem.message),
        (Some(line), None) => format!("{path}:{line}: {}", problem.message),
        _ => format!("{path}: {}", problem.message),
    }
}

fn run_convert(mut files: Vec<PathBuf>, output: Option<&Path>) -> Result<ExitCode> {
    if files.is_empty() {
        files = default_workflows()?;
    }
    let mut inputs = Vec::new();
    // Problems name the file by its name; print them with its path.
    let mut paths: BTreeMap<String, &Path> = BTreeMap::new();
    for path in &files {
        let source =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let name = path.file_name().map_or_else(
            || path.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        paths.insert(name.clone(), path);
        inputs.push((name, source));
    }
    let shown = |problem: &Diagnostic| match problem.file.as_deref().and_then(|f| paths.get(f)) {
        Some(path) => located(path, problem),
        None => problem.to_string(),
    };
    let conversion = match convert(&inputs) {
        Ok(conversion) => conversion,
        Err(problems) => {
            for problem in &problems {
                eprintln!("{}", shown(problem));
            }
            return Ok(ExitCode::FAILURE);
        }
    };
    for warning in &conversion.warnings {
        eprintln!("{}", shown(warning));
    }
    for error in &conversion.errors {
        eprintln!("{}", shown(error));
    }
    let yaml = to_yaml(&conversion.config);
    match output {
        Some(dest) => {
            std::fs::write(dest, yaml).with_context(|| format!("write {}", dest.display()))?;
        }
        None => print!("{yaml}"),
    }
    Ok(exit(conversion.errors.is_empty()))
}

/// `.github/workflows/*.yml` and `*.yaml`, sorted by name.
fn default_workflows() -> Result<Vec<PathBuf>> {
    let dir = Path::new(".github/workflows");
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let path = entry?.path();
        if path.is_file() && path.extension().is_some_and(|e| e == "yml" || e == "yaml") {
            found.push(path);
        }
    }
    found.sort();
    Ok(found)
}

fn run_split(args: SplitArgs) -> Result<ExitCode> {
    let items = if args.items.is_empty() {
        let mut input = String::new();
        std::io::stdin()
            .read_to_string(&mut input)
            .context("read stdin")?;
        input
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(str::to_owned)
            .collect()
    } else {
        args.items
    };
    let timings: BTreeMap<String, f64> = match (args.by, &args.timings) {
        (By::Timings, Some(path)) => {
            let json = std::fs::read_to_string(path)
                .with_context(|| format!("read timings {}", path.display()))?;
            serde_json::from_str(&json)
                .with_context(|| format!("parse timings {}", path.display()))?
        }
        _ => BTreeMap::new(),
    };
    let refs: Vec<&str> = items.iter().map(String::as_str).collect();
    let mine = split(&refs, &timings, args.index, args.total);
    println!("{}", mine.join(" "));
    Ok(ExitCode::SUCCESS)
}

fn exit(ok: bool) -> ExitCode {
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
