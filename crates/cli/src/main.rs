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
use hatchrun_workflow::{
    Config, Diagnostic, MAX_SOURCE, convert, parse, to_yaml, validate, workflow_needs,
};

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
        /// Also print the secrets and variables each workflow reads
        #[arg(long)]
        needs: bool,
    },
    /// Convert GitHub Actions or Forgejo Actions workflow files into a
    /// hatchrun config
    ///
    /// Jobs and files that cannot convert are left out and reported; the
    /// config holds the rest, and the exit code is 1 when any were left out.
    /// The last lines name the secrets and variables each workflow reads.
    Convert {
        /// The files to convert (default: *.yml and *.yaml in .github/workflows,
        /// .forgejo/workflows and .gitea/workflows)
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
        Command::Validate { path, needs } => run_validate(&path, needs),
        Command::Convert { files, output } => run_convert(files, output.as_deref()),
    }
}

/// Reads a workflow file, stopping one byte past [`MAX_SOURCE`] so a huge
/// file is refused without being loaded into memory. The inner `Err` is
/// the refusal, for the caller to report its own way.
fn read_source(path: &Path) -> Result<Result<String, String>> {
    let read = || format!("read {}", path.display());
    let file = std::fs::File::open(path).with_context(read)?;
    let mut bytes = Vec::new();
    file.take(MAX_SOURCE as u64 + 1)
        .read_to_end(&mut bytes)
        .with_context(read)?;
    if bytes.len() > MAX_SOURCE {
        let size = std::fs::metadata(path).map_or(bytes.len() as u64, |m| m.len());
        return Ok(Err(format!(
            "{} is {size} bytes; the limit is {MAX_SOURCE}",
            path.display()
        )));
    }
    Ok(Ok(String::from_utf8(bytes).with_context(read)?))
}

fn run_validate(path: &Path, needs: bool) -> Result<ExitCode> {
    let source = read_source(path)?.map_err(anyhow::Error::msg)?;
    let (problems, config) = match parse(&source) {
        Ok(config) => (validate(&config), Some(config)),
        Err(problems) => (problems, None),
    };
    for problem in &problems {
        println!("{}", located(path, problem));
    }
    if needs && let Some(config) = &config {
        for line in needs_lines(config) {
            println!("{line}");
        }
    }
    Ok(exit(problems.is_empty()))
}

/// `<workflow>: secrets A, B; vars C` for each workflow that reads a
/// secret or a variable. `GITHUB_TOKEN` is left out: it is minted for
/// each job, not stored.
fn needs_lines(config: &Config) -> Vec<String> {
    config
        .workflows
        .iter()
        .filter_map(|(name, wf)| {
            let needs = workflow_needs(config, wf);
            (!needs.is_empty()).then(|| format!("{name}: {needs}"))
        })
        .collect()
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
    // Files too large to read are reported and left out; the rest still
    // convert, as files that fail to convert do.
    let mut refused = Vec::new();
    for path in &files {
        let source = match read_source(path)? {
            Ok(source) => source,
            Err(refusal) => {
                refused.push(refusal);
                continue;
            }
        };
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
    for refusal in &refused {
        eprintln!("{refusal}");
    }
    if inputs.is_empty() {
        return Ok(ExitCode::FAILURE);
    }
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
    for line in needs_lines(&conversion.config) {
        eprintln!("{line}");
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
    Ok(exit(conversion.errors.is_empty() && refused.is_empty()))
}

/// The directories `convert` reads when no files are named: GitHub's,
/// then Forgejo's and Gitea's.
const WORKFLOW_DIRS: &[&str] = &[
    ".github/workflows",
    ".forgejo/workflows",
    ".gitea/workflows",
];

/// `*.yml` and `*.yaml` in each of [`WORKFLOW_DIRS`] that exists, sorted
/// by name within each.
fn default_workflows() -> Result<Vec<PathBuf>> {
    let mut all = Vec::new();
    let mut any = false;
    for dir in WORKFLOW_DIRS.iter().map(Path::new) {
        if !dir.is_dir() {
            continue;
        }
        any = true;
        let mut found = Vec::new();
        for entry in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
            let path = entry?.path();
            if path.is_file() && path.extension().is_some_and(|e| e == "yml" || e == "yaml") {
                found.push(path);
            }
        }
        found.sort();
        all.extend(found);
    }
    if !any {
        anyhow::bail!(
            "found none of {}; name the files to convert",
            WORKFLOW_DIRS.join(", ")
        );
    }
    Ok(all)
}

fn exit(ok: bool) -> ExitCode {
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
