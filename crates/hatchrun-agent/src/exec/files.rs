// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The files a step writes to: `GITHUB_ENV`, `GITHUB_OUTPUT`,
//! `GITHUB_STATE`, `GITHUB_PATH` and `GITHUB_STEP_SUMMARY`.

use std::fmt;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The most a step may write to one file command file.
const MAX_FILE: usize = 16 << 20;

/// The most a step summary may hold. A larger summary is dropped.
pub const MAX_SUMMARY: usize = 1 << 20;

/// The file command files of one step.
#[derive(Debug, Clone)]
pub struct StepFiles {
    /// `GITHUB_ENV`.
    pub env: PathBuf,
    /// `GITHUB_OUTPUT`.
    pub output: PathBuf,
    /// `GITHUB_STATE`.
    pub state: PathBuf,
    /// `GITHUB_PATH`.
    pub path: PathBuf,
    /// `GITHUB_STEP_SUMMARY`.
    pub summary: PathBuf,
}

impl StepFiles {
    /// Creates empty files for step number `step` under
    /// `<temp>/_runner_file_commands`.
    pub fn create(temp: &Path, step: usize) -> Result<Self> {
        let dir = temp.join("_runner_file_commands");
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let make = |kind: &str| -> Result<PathBuf> {
            let path = dir.join(format!("{kind}_{step}"));
            File::create(&path).with_context(|| format!("creating {}", path.display()))?;
            Ok(path)
        };
        Ok(Self {
            env: make("env")?,
            output: make("output")?,
            state: make("state")?,
            path: make("path")?,
            summary: make("summary")?,
        })
    }
}

/// What a step wrote.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Written {
    /// `GITHUB_ENV` pairs, in file order.
    pub env: Vec<(String, String)>,
    /// `GITHUB_OUTPUT` pairs, in file order.
    pub outputs: Vec<(String, String)>,
    /// `GITHUB_STATE` pairs, in file order.
    pub state: Vec<(String, String)>,
    /// `GITHUB_PATH` directories, in file order. The executor puts the
    /// last one first.
    pub path: Vec<String>,
    /// `GITHUB_STEP_SUMMARY`, empty when it was over [`MAX_SUMMARY`].
    pub summary: String,
    /// Things the step wrote that were dropped.
    pub warnings: Vec<String>,
}

/// A file command file that cannot be read or parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileError {
    /// The file's name, such as `env_3`.
    pub file: String,
    /// The 1-based line, when the problem is on one.
    pub line: Option<usize>,
    /// What is wrong.
    pub message: String,
}

impl fmt::Display for FileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line {
            Some(line) => write!(f, "{} line {line}: {}", self.file, self.message),
            None => write!(f, "{}: {}", self.file, self.message),
        }
    }
}

impl std::error::Error for FileError {}

/// Reads everything the step wrote.
pub fn read(files: &StepFiles) -> Result<Written, FileError> {
    let mut written = Written::default();

    let env = read_text(&files.env, MAX_FILE)?;
    for (key, value) in parse_pairs(&env, &file_name(&files.env))? {
        if key == "NODE_OPTIONS" {
            written
                .warnings
                .push("NODE_OPTIONS cannot be set with GITHUB_ENV and was ignored".to_owned());
        } else {
            written.env.push((key, value));
        }
    }

    let output = read_text(&files.output, MAX_FILE)?;
    written.outputs = parse_pairs(&output, &file_name(&files.output))?;

    let state = read_text(&files.state, MAX_FILE)?;
    written.state = parse_pairs(&state, &file_name(&files.state))?;

    let path = read_text(&files.path, MAX_FILE)?;
    written.path = path
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();

    let (summary, over) = read_capped(&files.summary, MAX_SUMMARY)?;
    if over {
        written.warnings.push(format!(
            "the step summary is over {MAX_SUMMARY} bytes and was dropped"
        ));
    } else {
        written.summary = summary;
    }
    Ok(written)
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// Reads at most `cap` bytes and says whether the file held more.
fn read_capped(path: &Path, cap: usize) -> Result<(String, bool), FileError> {
    let err = |message: String| FileError {
        file: file_name(path),
        line: None,
        message,
    };
    let file = match File::open(path) {
        Ok(file) => file,
        // A step may delete a file command file. That is the same as
        // writing nothing.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((String::new(), false)),
        Err(e) => return Err(err(format!("cannot open: {e}"))),
    };
    let mut bytes = Vec::new();
    // One byte past the cap is enough to know the file is too big, and
    // keeps a runaway step from filling memory.
    file.take(cap as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| err(format!("cannot read: {e}")))?;
    let over = bytes.len() > cap;
    bytes.truncate(cap);
    Ok((String::from_utf8_lossy(&bytes).into_owned(), over))
}

/// Reads a file command file. More than `cap` bytes fails the step.
fn read_text(path: &Path, cap: usize) -> Result<String, FileError> {
    let (text, over) = read_capped(path, cap)?;
    if over {
        return Err(FileError {
            file: file_name(path),
            line: None,
            message: format!("larger than {cap} bytes"),
        });
    }
    Ok(text)
}

/// Parses `NAME=VALUE` and `NAME<<DELIM` blocks, the way GitHub's
/// runner does. Blank lines between entries are skipped.
fn parse_pairs(text: &str, file: &str) -> Result<Vec<(String, String)>, FileError> {
    let fail = |line: usize, message: &str| FileError {
        file: file.to_owned(),
        line: Some(line),
        message: message.to_owned(),
    };
    let mut pairs = Vec::new();
    let mut lines = text.lines().enumerate().map(|(i, l)| (i + 1, l));
    while let Some((number, line)) = lines.next() {
        if line.is_empty() {
            continue;
        }
        let equals = line.find('=');
        let heredoc = line.find("<<");
        // Whichever marker comes first decides the form.
        if let Some(eq) = equals
            && heredoc.is_none_or(|h| eq < h)
        {
            if eq == 0 {
                return Err(fail(number, "empty name"));
            }
            pairs.push((line[..eq].to_owned(), line[eq + 1..].to_owned()));
        } else if let Some(h) = heredoc {
            let name = &line[..h];
            let delimiter = &line[h + 2..];
            if name.is_empty() {
                return Err(fail(number, "empty name"));
            }
            if delimiter.is_empty() {
                return Err(fail(number, "empty delimiter"));
            }
            let mut value: Vec<&str> = Vec::new();
            let mut closed = false;
            for (_, body) in lines.by_ref() {
                if body == delimiter {
                    closed = true;
                    break;
                }
                value.push(body);
            }
            if !closed {
                return Err(fail(
                    number,
                    &format!("missing closing delimiter `{delimiter}`"),
                ));
            }
            pairs.push((name.to_owned(), value.join("\n")));
        } else {
            return Err(fail(number, "expected NAME=VALUE or NAME<<DELIMITER"));
        }
    }
    Ok(pairs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files_with(env: &str) -> (tempfile::TempDir, StepFiles) {
        let dir = tempfile::tempdir().unwrap();
        let files = StepFiles::create(dir.path(), 1).unwrap();
        fs::write(&files.env, env).unwrap();
        (dir, files)
    }

    fn pair(k: &str, v: &str) -> (String, String) {
        (k.to_owned(), v.to_owned())
    }

    #[test]
    fn a_deleted_file_is_empty() {
        let (_dir, files) = files_with("A=1\n");
        fs::remove_file(&files.output).unwrap();
        fs::remove_file(&files.summary).unwrap();
        let written = read(&files).unwrap();
        assert_eq!(written.env, [pair("A", "1")]);
        assert_eq!(written.outputs, Vec::new());
    }

    #[test]
    fn create_makes_empty_files_per_step() {
        let dir = tempfile::tempdir().unwrap();
        let one = StepFiles::create(dir.path(), 1).unwrap();
        let two = StepFiles::create(dir.path(), 2).unwrap();
        assert_ne!(one.env, two.env);
        assert!(
            one.env
                .starts_with(dir.path().join("_runner_file_commands"))
        );
        assert_eq!(read(&one).unwrap(), Written::default());
    }

    #[test]
    fn reads_name_value_and_heredoc() {
        let (_d, files) = files_with("A=1\nB<<EOF\nx\ny\nEOF\n");
        let w = read(&files).unwrap();
        assert_eq!(w.env, vec![pair("A", "1"), pair("B", "x\ny")]);
    }

    #[test]
    fn reads_outputs_and_state() {
        let (_d, files) = files_with("");
        fs::write(&files.output, "o=1\r\nm<<X\r\na\r\nX\r\n").unwrap();
        fs::write(&files.state, "s=a=b\n").unwrap();
        let w = read(&files).unwrap();
        assert_eq!(w.outputs, vec![pair("o", "1"), pair("m", "a")]);
        assert_eq!(w.state, vec![pair("s", "a=b")]);
    }

    #[test]
    fn heredoc_without_end_fails() {
        let (_d, files) = files_with("B<<EOF\nx\n");
        let e = read(&files).unwrap_err();
        assert_eq!(e.file, "env_1");
        assert_eq!(e.line, Some(1));
    }

    #[test]
    fn value_holding_the_delimiter_ends_early() {
        assert_eq!(
            parse_pairs("B<<EOF\nEOF\n", "env_1").unwrap(),
            vec![pair("B", "")]
        );
        let (_d, files) = files_with("B<<EOF\nEOF\nEOF\n");
        let e = read(&files).unwrap_err();
        assert_eq!((e.file.as_str(), e.line), ("env_1", Some(3)));
    }

    #[test]
    fn bad_lines_name_the_file_and_line() {
        let (_d, files) = files_with("A=1\n\nnonsense\n");
        let e = read(&files).unwrap_err();
        assert_eq!((e.file.as_str(), e.line), ("env_1", Some(3)));
        let (_d, files) = files_with("=x\n");
        let e = read(&files).unwrap_err();
        assert_eq!(e.line, Some(1));
        assert!(e.to_string().contains("env_1 line 1"));
    }

    #[test]
    fn names_are_case_sensitive() {
        let (_d, files) = files_with("a=1\nA=2\n");
        assert_eq!(
            read(&files).unwrap().env,
            vec![pair("a", "1"), pair("A", "2")]
        );
    }

    #[test]
    fn node_options_is_refused() {
        let (_d, files) = files_with("NODE_OPTIONS=--require x\nOK=1\n");
        let w = read(&files).unwrap();
        assert_eq!(w.env, vec![pair("OK", "1")]);
        assert_eq!(w.warnings.len(), 1);
        assert!(w.warnings[0].contains("NODE_OPTIONS"));
    }

    #[test]
    fn path_lines_prepend_in_order() {
        let (_d, files) = files_with("");
        fs::write(&files.path, "/a\n\n/b\r\n").unwrap();
        assert_eq!(read(&files).unwrap().path, vec!["/a", "/b"]);
    }

    #[test]
    fn summary_over_the_cap_is_dropped() {
        let (_d, files) = files_with("");
        fs::write(&files.summary, "x".repeat(MAX_SUMMARY + 1)).unwrap();
        let w = read(&files).unwrap();
        assert_eq!(w.summary, "");
        assert_eq!(w.warnings.len(), 1);
        fs::write(&files.summary, "x".repeat(MAX_SUMMARY)).unwrap();
        assert_eq!(read(&files).unwrap().summary.len(), MAX_SUMMARY);
    }

    #[test]
    fn a_file_over_the_cap_fails() {
        let (_d, files) = files_with("");
        fs::write(&files.env, vec![b'a'; 4096]).unwrap();
        let e = read_text(&files.env, 1024).unwrap_err();
        assert_eq!(e.file, "env_1");
        assert!(read_text(&files.env, 4096).is_ok());
        let (text, over) = read_capped(&files.env, 1024).unwrap();
        assert_eq!((text.len(), over), (1024, true));
    }
}
