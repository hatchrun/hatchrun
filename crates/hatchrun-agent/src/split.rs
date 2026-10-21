// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! `hatchrun-agent split`: picks this parallel node's share of a list
//! of tests. It runs as the runner user inside steps, so the environment,
//! the timings file and stdin are all untrusted input and are bounded.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};

/// The most bytes read from stdin or the timings file.
const MAX_BYTES: u64 = 64 * 1024 * 1024;

/// The most items one split takes.
const MAX_ITEMS: usize = 1_000_000;

/// Prints this node's items, one per line. Exit codes: 0 done, 2 the
/// input cannot be used.
pub fn run(items: Vec<String>) -> ExitCode {
    let mut out = std::io::stdout().lock();
    match select(items).and_then(|chosen| {
        let mut text = String::new();
        for item in &chosen {
            text.push_str(item);
            text.push('\n');
        }
        out.write_all(text.as_bytes())
            .and_then(|()| out.flush())
            .context("write stdout")
    }) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("split: {err:#}");
            ExitCode::from(2)
        }
    }
}

fn select(args: Vec<String>) -> Result<Vec<String>> {
    let node = node()?;
    let items = if args.is_empty() {
        read_items(std::io::stdin().lock())?
    } else {
        args
    };
    if items.len() > MAX_ITEMS {
        bail!("more than {MAX_ITEMS} items");
    }
    let Some((index, total)) = node else {
        return Ok(items);
    };
    let timings = timings()?;
    let refs: Vec<&str> = items.iter().map(String::as_str).collect();
    Ok(hatchrun_workflow::split(&refs, &timings, index, total)
        .into_iter()
        .map(str::to_owned)
        .collect())
}

/// `(index, total)` from `HATCHRUN_NODE_INDEX` and `HATCHRUN_NODE_TOTAL`,
/// or `None` when neither is set.
fn node() -> Result<Option<(u32, u32)>> {
    let index = number("HATCHRUN_NODE_INDEX")?;
    let total = number("HATCHRUN_NODE_TOTAL")?;
    match (index, total) {
        (None, None) => Ok(None),
        (Some(index), Some(total)) if total > 0 && index < total => Ok(Some((index, total))),
        (Some(index), Some(total)) => {
            bail!("HATCHRUN_NODE_INDEX {index} is not below HATCHRUN_NODE_TOTAL {total}")
        }
        _ => bail!("HATCHRUN_NODE_INDEX and HATCHRUN_NODE_TOTAL must be set together"),
    }
}

fn number(name: &str) -> Result<Option<u32>> {
    match std::env::var(name) {
        Ok(value) => value
            .parse()
            .map(Some)
            .with_context(|| format!("{name} is not a number")),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => bail!("{name} is not a number"),
    }
}

/// The timings in the file `HATCHRUN_TIMINGS` names. No variable or no
/// file gives none; a file that cannot be read or parsed is an error.
fn timings() -> Result<BTreeMap<String, f64>> {
    let path = match std::env::var_os("HATCHRUN_TIMINGS") {
        Some(path) if !path.is_empty() => path,
        _ => return Ok(BTreeMap::new()),
    };
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(err) => return Err(err).with_context(|| format!("open {}", path.to_string_lossy())),
    };
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read {}", path.to_string_lossy()))?;
    if bytes.len() as u64 > MAX_BYTES {
        bail!(
            "{} is larger than {MAX_BYTES} bytes",
            path.to_string_lossy()
        );
    }
    serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.to_string_lossy()))
}

/// The non-blank lines of `input`, which holds at most `MAX_BYTES`.
fn read_items(input: impl Read) -> Result<Vec<String>> {
    let mut bytes = Vec::new();
    input
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("read stdin")?;
    if bytes.len() as u64 > MAX_BYTES {
        bail!("stdin is larger than {MAX_BYTES} bytes");
    }
    let text = String::from_utf8(bytes).context("stdin is not UTF-8")?;
    let mut items = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        if items.len() == MAX_ITEMS {
            bail!("more than {MAX_ITEMS} items");
        }
        items.push(line.to_owned());
    }
    Ok(items)
}
