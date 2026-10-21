// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The `hatchrun` command line. Customers run `validate` and `convert`
//! locally.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use hatchrun_workflow::{Diagnostic, MAX_SOURCE, convert, parse, to_yaml, validate};

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
    }
}

/// Reads a workflow file, stopping one byte past [`MAX_SOURCE`] so a huge
/// file is refused without being loaded into memory.
fn read_source(path: &Path) -> Result<String> {
    let read = || format!("read {}", path.display());
    let file = std::fs::File::open(path).with_context(read)?;
    let mut bytes = Vec::new();
    file.take(MAX_SOURCE as u64 + 1)
        .read_to_end(&mut bytes)
        .with_context(read)?;
    if bytes.len() > MAX_SOURCE {
        let size = std::fs::metadata(path).map_or(bytes.len() as u64, |m| m.len());
        anyhow::bail!(
            "{} is {size} bytes; the limit is {MAX_SOURCE}",
            path.display()
        );
    }
    String::from_utf8(bytes).with_context(read)
}

fn run_validate(path: &Path) -> Result<ExitCode> {
    let source = read_source(path)?;
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
        let source = read_source(path)?;
        let name = path.file_name().map_or_else(
            || path.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        if let Some(other) = paths.insert(name.clone(), path) {
            anyhow::bail!(
                "{} and {} have the same file name; convert them separately",
                other.display(),
                path.display()
            );
        }
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
            // `-o .hatchrun/config.yml` in a repository that has no
            // `.hatchrun/` yet.
            if let Some(dir) = dest.parent().filter(|d| !d.as_os_str().is_empty()) {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("create {}", dir.display()))?;
            }
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

fn exit(ok: bool) -> ExitCode {
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
