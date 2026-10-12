//! Parses `/etc/environment` style files.

use std::io::ErrorKind;
use std::path::Path;

use anyhow::{Context, Result};

/// Reads `KEY=VALUE` lines, skipping blanks and comments and stripping one
/// pair of matching quotes around the value. A missing file is empty.
pub fn read(path: &Path) -> Result<Vec<(String, String)>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(parse(&text)),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(err).with_context(|| format!("read {}", path.display())),
    }
}

fn parse(text: &str) -> Vec<(String, String)> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| line.split_once('='))
        .filter(|(key, _)| !key.is_empty())
        .map(|(key, value)| (key.to_owned(), unquote(value).to_owned()))
        .collect()
}

fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|v| v.strip_suffix(quote))
        {
            return inner;
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_environment_file() {
        let text = "# comment\n\nPATH=/usr/bin:/bin\nImageOS=\"ubuntu26\"\nQUOTED='single'\nnot a pair\n=novalue\nEMPTY=\n";
        assert_eq!(
            parse(text),
            [
                ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
                ("ImageOS".to_owned(), "ubuntu26".to_owned()),
                ("QUOTED".to_owned(), "single".to_owned()),
                ("EMPTY".to_owned(), String::new()),
            ]
        );
    }

    #[test]
    fn missing_file_is_empty() {
        assert_eq!(
            read(Path::new("/nonexistent/hatchrun/environment")).unwrap(),
            Vec::new()
        );
    }
}
