// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The files a step writes to: `GITHUB_ENV`, `GITHUB_OUTPUT`,
//! `GITHUB_STATE`, `GITHUB_PATH` and `GITHUB_STEP_SUMMARY`.

use std::fmt;
use std::fs::{self, File};
use std::io::Read;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use nix::errno::Errno;
use nix::fcntl::{OFlag, open, openat};
use nix::sys::stat::Mode;
use nix::unistd::{UnlinkatFlags, fchown, unlinkat};

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

const DIR_NAME: &str = "_runner_file_commands";

/// Opens a directory without following a symlink at its last component.
fn open_dir(path: &Path) -> nix::Result<OwnedFd> {
    open(
        path,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
}

impl StepFiles {
    /// Removes whatever sits at `<temp>/_runner_file_commands` and makes
    /// a fresh root-owned directory there. Called once per job, before
    /// any step can plant something in a runner-owned temp directory.
    ///
    /// # Errors
    ///
    /// When the old entry cannot be removed or the directory not made.
    pub fn reset(temp: &Path) -> Result<()> {
        let dir = temp.join(DIR_NAME);
        match fs::symlink_metadata(&dir) {
            Ok(meta) if meta.is_dir() => fs::remove_dir_all(&dir),
            Ok(_) => fs::remove_file(&dir),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
        .with_context(|| format!("clearing {}", dir.display()))?;
        fs::create_dir(&dir).with_context(|| format!("creating {}", dir.display()))
    }

    /// Creates empty files for step number `step` under
    /// `<temp>/_runner_file_commands`, owned by `owner` when given. An
    /// entry already at a file's path is unlinked, never followed, and
    /// the file is opened with `O_EXCL | O_NOFOLLOW` and handed over
    /// with `fchown`, so a planted symlink cannot redirect the agent.
    ///
    /// # Errors
    ///
    /// When the directory is not a plain directory or a file cannot be
    /// made.
    pub fn create(
        temp: &Path,
        step: usize,
        owner: Option<(nix::unistd::Uid, nix::unistd::Gid)>,
    ) -> Result<Self> {
        let dir = temp.join(DIR_NAME);
        if let Err(e) = fs::create_dir(&dir)
            && e.kind() != std::io::ErrorKind::AlreadyExists
        {
            return Err(e).with_context(|| format!("creating {}", dir.display()));
        }
        let dirfd = open_dir(&dir).with_context(|| format!("opening {}", dir.display()))?;
        let make = |kind: &str| -> Result<PathBuf> {
            let name = format!("{kind}_{step}");
            let path = dir.join(&name);
            match unlinkat(&dirfd, name.as_str(), UnlinkatFlags::NoRemoveDir) {
                Ok(()) | Err(Errno::ENOENT) => {}
                Err(e) => {
                    return Err(e).with_context(|| format!("removing {}", path.display()));
                }
            }
            let fd = openat(
                &dirfd,
                name.as_str(),
                OFlag::O_WRONLY
                    | OFlag::O_CREAT
                    | OFlag::O_EXCL
                    | OFlag::O_NOFOLLOW
                    | OFlag::O_CLOEXEC,
                Mode::from_bits_truncate(0o600),
            )
            .with_context(|| format!("creating {}", path.display()))?;
            if let Some((uid, gid)) = owner {
                fchown(&fd, Some(uid), Some(gid))
                    .with_context(|| format!("chown {}", path.display()))?;
            }
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

impl StepFiles {
    /// Deletes the step's files. Called once they are read, so a step
    /// that left a process running cannot write to them for the agent
    /// to find later. An entry that is already gone is fine.
    pub fn remove(&self) {
        for path in [
            &self.env,
            &self.output,
            &self.state,
            &self.path,
            &self.summary,
        ] {
            let removed = path.parent().zip(path.file_name()).and_then(|(dir, name)| {
                let dirfd = open_dir(dir).ok()?;
                unlinkat(&dirfd, name, UnlinkatFlags::NoRemoveDir).ok()
            });
            if removed.is_none() {
                // Best effort: the directory is cleared at the next job.
                let _ = fs::remove_file(path);
            }
        }
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
    let name = file_name(&files.path);
    for (index, line) in path.lines().enumerate() {
        if line.contains('\0') {
            return Err(nul(&name, index + 1));
        }
        if !line.trim().is_empty() {
            written.path.push(line.to_owned());
        }
    }

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

fn nul(file: &str, line: usize) -> FileError {
    FileError {
        file: file.to_owned(),
        line: Some(line),
        message: "contains a NUL byte".to_owned(),
    }
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
    let opened = path
        .parent()
        .zip(path.file_name())
        .ok_or(Errno::EINVAL)
        .and_then(|(dir, name)| {
            let dirfd = open_dir(dir)?;
            openat(
                &dirfd,
                name,
                OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
                Mode::empty(),
            )
        });
    // A step may delete a file command file. That is the same as
    // writing nothing.
    let file = match opened {
        Ok(fd) => File::from(fd),
        Err(Errno::ENOENT) => return Ok((String::new(), false)),
        Err(e) => return Err(err(format!("cannot open: {e}"))),
    };
    match file.metadata() {
        Ok(meta) if meta.is_file() => {}
        _ => return Err(err("cannot open: not a regular file".to_owned())),
    }
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
            if line.contains('\0') {
                return Err(nul(file, number));
            }
            pairs.push((line[..eq].to_owned(), line[eq + 1..].to_owned()));
        } else if let Some(h) = heredoc {
            let name = &line[..h];
            let delimiter = &line[h + 2..];
            if name.is_empty() {
                return Err(fail(number, "empty name"));
            }
            if line.contains('\0') {
                return Err(nul(file, number));
            }
            if delimiter.is_empty() {
                return Err(fail(number, "empty delimiter"));
            }
            let mut value: Vec<&str> = Vec::new();
            let mut closed = false;
            for (at, body) in lines.by_ref() {
                if body == delimiter {
                    closed = true;
                    break;
                }
                if body.contains('\0') {
                    return Err(nul(file, at));
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
        let files = StepFiles::create(dir.path(), 1, None).unwrap();
        fs::write(&files.env, env).unwrap();
        (dir, files)
    }

    fn pair(k: &str, v: &str) -> (String, String) {
        (k.to_owned(), v.to_owned())
    }

    #[test]
    fn a_planted_symlink_is_replaced_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("victim");
        fs::write(&target, "keep").unwrap();
        StepFiles::reset(dir.path()).unwrap();
        let planted = dir.path().join(DIR_NAME).join("env_2");
        std::os::unix::fs::symlink(&target, &planted).unwrap();
        let files = StepFiles::create(dir.path(), 2, None).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep");
        assert!(!fs::symlink_metadata(&files.env).unwrap().is_symlink());
    }

    #[test]
    fn nul_bytes_are_refused_with_the_file_and_line() {
        for (text, line) in [
            ("A=1\nB=x\0y\n", 2),
            ("A\0=1\n", 1),
            ("A=1\nB<<EOF\nx\0\nEOF\n", 3),
            ("A<<E\0OF\nx\nE\0OF\n", 1),
        ] {
            let (_d, files) = files_with(text);
            let err = read(&files).unwrap_err();
            assert_eq!(err.line, Some(line), "{text:?}: {err}");
            assert!(err.message.contains("NUL"), "{err}");
            assert_eq!(err.file, "env_1");
        }
        let (_d, files) = files_with("");
        fs::write(&files.path, "/a\n/b\0c\n").unwrap();
        let err = read(&files).unwrap_err();
        assert_eq!((err.file.as_str(), err.line), ("path_1", Some(2)));
        let (_d, files) = files_with("");
        fs::write(&files.output, "o=\0\n").unwrap();
        assert!(read(&files).is_err());
    }

    #[test]
    fn remove_deletes_the_files() {
        let (_d, files) = files_with("A=1\n");
        files.remove();
        for path in [
            &files.env,
            &files.output,
            &files.state,
            &files.path,
            &files.summary,
        ] {
            assert!(fs::symlink_metadata(path).is_err(), "{}", path.display());
        }
        // Removing again is fine.
        files.remove();
    }

    #[test]
    fn github_path_skips_blank_and_whitespace_lines() {
        let (_dir, files) = files_with("");
        fs::write(&files.path, "/opt/a\n   \n\t\n\n/opt/b\n").unwrap();
        let written = read(&files).unwrap();
        assert_eq!(written.path, ["/opt/a", "/opt/b"]);
    }

    #[test]
    fn a_symlinked_directory_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), dir.path().join(DIR_NAME)).unwrap();
        assert!(StepFiles::create(dir.path(), 1, None).is_err());
        // The per-job reset replaces the link with a real directory.
        StepFiles::reset(dir.path()).unwrap();
        StepFiles::create(dir.path(), 1, None).unwrap();
        assert_eq!(fs::read_dir(elsewhere.path()).unwrap().count(), 0);
    }

    #[test]
    fn read_refuses_a_symlinked_file() {
        let (dir, files) = files_with("");
        let secret = dir.path().join("secret");
        fs::write(&secret, "TOKEN=hunter2\n").unwrap();
        fs::remove_file(&files.env).unwrap();
        std::os::unix::fs::symlink(&secret, &files.env).unwrap();
        let err = read(&files).unwrap_err();
        assert!(err.message.contains("cannot open"), "{err}");
    }

    #[test]
    fn files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, files) = files_with("");
        let mode = fs::metadata(&files.env).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
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
        let one = StepFiles::create(dir.path(), 1, None).unwrap();
        let two = StepFiles::create(dir.path(), 2, None).unwrap();
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
