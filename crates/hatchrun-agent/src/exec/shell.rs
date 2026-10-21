// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Which program runs a `run:` script and how.

/// A shell command line. `{0}` in an argument stands for the script file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shell {
    /// The program to start.
    pub program: String,
    /// Its arguments, with `{0}` where the script's path goes.
    pub args: Vec<String>,
    /// The script file's extension, without the dot.
    pub extension: &'static str,
}

impl Shell {
    /// The arguments with the script's path in place of `{0}`.
    pub fn args_for(&self, script: &str) -> Vec<String> {
        self.args
            .iter()
            .map(|arg| arg.replace("{0}", script))
            .collect()
    }
}

fn shell(program: &str, args: &[&str], extension: &'static str) -> Shell {
    Shell {
        program: program.to_owned(),
        args: args.iter().map(|&a| a.to_owned()).collect(),
        extension,
    }
}

/// Picks the shell for a step's `shell:` value, as GitHub's runner does.
/// `has_bash` says whether `bash` is on the step's `PATH`; it decides
/// the default.
///
/// # Errors
///
/// When a custom shell has no `{0}`.
pub fn resolve(shell_name: Option<&str>, has_bash: bool) -> Result<Shell, String> {
    let Some(name) = shell_name.map(str::trim) else {
        return Ok(if has_bash {
            shell("bash", &["-e", "{0}"], "sh")
        } else {
            shell("sh", &["-e", "{0}"], "sh")
        });
    };
    match name {
        "bash" => Ok(shell(
            "bash",
            &["--noprofile", "--norc", "-eo", "pipefail", "{0}"],
            "sh",
        )),
        "sh" => Ok(shell("sh", &["-e", "{0}"], "sh")),
        "python" => Ok(shell("python", &["{0}"], "py")),
        "pwsh" => Ok(shell("pwsh", &["-command", ". '{0}'"], "ps1")),
        custom => {
            if !custom.contains("{0}") {
                return Err(format!(
                    "the shell `{custom}` has no `{{0}}`, which stands for the script file"
                ));
            }
            let mut words = custom.split_whitespace();
            let program = words.next().unwrap_or_default();
            let extension = if program.starts_with("python") {
                "py"
            } else if program.starts_with("pwsh") || program.starts_with("powershell") {
                "ps1"
            } else {
                "sh"
            };
            Ok(Shell {
                program: program.to_owned(),
                args: words.map(str::to_owned).collect(),
                extension,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_prefers_bash() {
        let s = resolve(None, true).unwrap();
        assert_eq!(
            (s.program.as_str(), s.args.as_slice()),
            ("bash", &["-e".to_owned(), "{0}".to_owned()][..])
        );
        let s = resolve(None, false).unwrap();
        assert_eq!(s.program, "sh");
    }

    #[test]
    fn named_shells() {
        let bash = resolve(Some("bash"), false).unwrap();
        assert_eq!(
            bash.args_for("/t/s.sh"),
            ["--noprofile", "--norc", "-eo", "pipefail", "/t/s.sh"]
        );
        assert_eq!(
            resolve(Some("sh"), true).unwrap().args_for("x"),
            ["-e", "x"]
        );
        let py = resolve(Some("python"), true).unwrap();
        assert_eq!((py.program.as_str(), py.extension), ("python", "py"));
        let pwsh = resolve(Some("pwsh"), true).unwrap();
        assert_eq!(pwsh.args_for("/a b/s.ps1"), ["-command", ". '/a b/s.ps1'"]);
    }

    #[test]
    fn custom_shell_needs_the_placeholder() {
        let s = resolve(Some("python3 -u {0}"), true).unwrap();
        assert_eq!(s.program, "python3");
        assert_eq!(s.args_for("f.py"), ["-u", "f.py"]);
        assert_eq!(s.extension, "py");
        let err = resolve(Some("python3"), true).unwrap_err();
        assert!(err.contains("{0}"), "{err}");
    }
}
