// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Reading and parsing `action.yml`.

use std::collections::BTreeMap;
use std::io::Read;
use std::os::fd::OwnedFd;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

use super::super::pack::open_dir;
use super::{CompositeStep, Input, MAX_META, Metadata, Output, Runs};

/// Reads `action.yml` or `action.yaml` from `dir`.
#[cfg(test)]
pub fn read_metadata(dir: &Path) -> Result<Metadata> {
    read_metadata_in(dir, Path::new(""))
}

/// Reads `action.yml` or `action.yaml` from `root/rel`. The directory
/// is reached one component at a time with `O_NOFOLLOW` from `root`,
/// and the file is opened the same way and must be a regular file, so
/// a step that swaps either for a link or a FIFO after the action was
/// resolved cannot make the agent read or block on something else.
pub fn read_metadata_in(root: &Path, rel: &Path) -> Result<Metadata> {
    let dir = open_dir(&nix::fcntl::AT_FDCWD, root)
        .with_context(|| format!("open {}", root.display()))?;
    read_metadata_at(dir, rel, &root.join(rel))
}

/// [`read_metadata_in`] from a directory that is already open; `shown`
/// names `root/rel` in messages.
pub(super) fn read_metadata_at(root: OwnedFd, rel: &Path, shown: &Path) -> Result<Metadata> {
    use nix::errno::Errno;
    use nix::fcntl::{OFlag, openat};
    use nix::sys::stat::Mode;

    let mut dir = root;
    for part in rel.components() {
        match part {
            std::path::Component::Normal(name) => {
                dir = openat(
                    &dir,
                    name,
                    OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                    Mode::empty(),
                )
                .with_context(|| format!("open {}", shown.display()))?;
            }
            std::path::Component::CurDir => {}
            _ => bail!("action path {} leaves the action", rel.display()),
        }
    }
    for name in ["action.yml", "action.yaml"] {
        let path = shown.join(name);
        let fd = match openat(
            &dir,
            name,
            OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(Errno::ENOENT) => continue,
            Err(Errno::ELOOP) => bail!("{} is a symbolic link", path.display()),
            Err(e) => return Err(e).with_context(|| format!("open {}", path.display())),
        };
        let file = std::fs::File::from(fd);
        if !file.metadata()?.is_file() {
            bail!("{} is not a regular file", path.display());
        }
        let mut text = String::new();
        file.take(MAX_META + 1)
            .read_to_string(&mut text)
            .with_context(|| format!("read {}", path.display()))?;
        if text.len() as u64 > MAX_META {
            bail!("{} is larger than 1 MiB", path.display());
        }
        return parse_metadata(&text).map_err(|e| anyhow!("parse {}: {e:#}", path.display()));
    }
    bail!("no action.yml or action.yaml in {}", shown.display())
}

fn parse_metadata(text: &str) -> Result<Metadata> {
    let raw: RawMeta = serde_saphyr::from_str_with_options(
        text,
        serde_saphyr::options! { strict_booleans: true, emit_comments: false },
    )?;
    let runs = match raw.runs.using.0.as_str() {
        using @ ("node12" | "node16" | "node20" | "node24") => Runs::Node {
            version: using.to_owned(),
            main: raw
                .runs
                .main
                .map(|v| v.0)
                .context("runs.main is required for a JavaScript action")?,
            pre: raw.runs.pre.map(|v| v.0),
            pre_if: raw.runs.pre_if.map(|v| v.0),
            post: raw.runs.post.map(|v| v.0),
            post_if: raw.runs.post_if.map(|v| v.0),
        },
        "composite" => Runs::Composite {
            steps: raw
                .runs
                .steps
                .context("runs.steps is required for a composite action")?
                .into_iter()
                .map(CompositeStep::from)
                .collect(),
        },
        "docker" => Runs::Docker,
        other => bail!("runs.using `{other}` is not supported"),
    };
    Ok(Metadata {
        name: raw.name.map(|v| v.0).unwrap_or_default(),
        inputs: raw
            .inputs
            .into_iter()
            .map(|(k, v)| {
                (
                    k,
                    Input {
                        description: v.description.map(|d| d.0).unwrap_or_default(),
                        required: v.required.is_some_and(|r| r.0 == "true"),
                        default: v.default.map(|d| d.0),
                    },
                )
            })
            .collect(),
        outputs: raw
            .outputs
            .into_iter()
            .map(|(k, v)| {
                (
                    k,
                    Output {
                        description: v.description.map(|d| d.0).unwrap_or_default(),
                        value: v.value.map(|d| d.0),
                    },
                )
            })
            .collect(),
        runs,
    })
}

/// A YAML scalar kept as its text: strings, booleans and numbers all
/// read as the string a workflow would see.
#[derive(Debug)]
struct Scalar(String);

impl<'de> Deserialize<'de> for Scalar {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl serde::de::Visitor<'_> for V {
            type Value = Scalar;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a string, number or boolean")
            }

            fn visit_str<E>(self, v: &str) -> Result<Scalar, E> {
                Ok(Scalar(v.to_owned()))
            }

            fn visit_bool<E>(self, v: bool) -> Result<Scalar, E> {
                Ok(Scalar(v.to_string()))
            }

            fn visit_i64<E>(self, v: i64) -> Result<Scalar, E> {
                Ok(Scalar(v.to_string()))
            }

            fn visit_u64<E>(self, v: u64) -> Result<Scalar, E> {
                Ok(Scalar(v.to_string()))
            }

            fn visit_f64<E>(self, v: f64) -> Result<Scalar, E> {
                Ok(Scalar(v.to_string()))
            }
        }
        d.deserialize_any(V)
    }
}

#[derive(Deserialize)]
struct RawMeta {
    name: Option<Scalar>,
    #[serde(default)]
    inputs: BTreeMap<String, RawInput>,
    #[serde(default)]
    outputs: BTreeMap<String, RawOutput>,
    runs: RawRuns,
}

#[derive(Deserialize)]
struct RawInput {
    description: Option<Scalar>,
    required: Option<Scalar>,
    default: Option<Scalar>,
}

#[derive(Deserialize)]
struct RawOutput {
    description: Option<Scalar>,
    value: Option<Scalar>,
}

#[derive(Deserialize)]
struct RawRuns {
    using: Scalar,
    main: Option<Scalar>,
    pre: Option<Scalar>,
    #[serde(rename = "pre-if")]
    pre_if: Option<Scalar>,
    post: Option<Scalar>,
    #[serde(rename = "post-if")]
    post_if: Option<Scalar>,
    steps: Option<Vec<RawStep>>,
}

#[derive(Deserialize)]
struct RawStep {
    id: Option<Scalar>,
    name: Option<Scalar>,
    #[serde(rename = "if")]
    if_: Option<Scalar>,
    run: Option<Scalar>,
    shell: Option<Scalar>,
    uses: Option<Scalar>,
    #[serde(default)]
    with: BTreeMap<String, Scalar>,
    #[serde(default)]
    env: BTreeMap<String, Scalar>,
    #[serde(rename = "working-directory")]
    working_directory: Option<Scalar>,
    #[serde(rename = "continue-on-error")]
    continue_on_error: Option<Scalar>,
}

impl From<RawStep> for CompositeStep {
    fn from(s: RawStep) -> Self {
        let map = |m: BTreeMap<String, Scalar>| m.into_iter().map(|(k, v)| (k, v.0)).collect();
        Self {
            id: s.id.map(|v| v.0),
            name: s.name.map(|v| v.0),
            if_: s.if_.map(|v| v.0),
            run: s.run.map(|v| v.0),
            shell: s.shell.map(|v| v.0),
            uses: s.uses.map(|v| v.0),
            with: map(s.with),
            env: map(s.env),
            working_directory: s.working_directory.map(|v| v.0),
            continue_on_error: s.continue_on_error.map(|v| v.0),
        }
    }
}
