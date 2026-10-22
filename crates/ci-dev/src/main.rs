// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! `hatchrun-ci-dev`: what `hatchrun-ci` needs around it to run on a
//! developer's machine, used by `crates/ci/scripts/e2e.sh`.
//!
//! - `fake-github` answers the GitHub and Polar API calls the Worker
//!   makes (see [`github`]).
//! - `launcher` starts `hatchrun-agent ci` for each instance the
//!   Worker's `Local` backend asks for (see [`launcher`]).
//! - `session` prints a new app session, for a script to store in the
//!   `sessions` table and send as the cookie (see [`session`]).

mod github;
mod launcher;
mod session;

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(version, about)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve a fake GitHub API, which also answers Polar's calls.
    FakeGithub {
        /// The port on 127.0.0.1.
        #[arg(long, default_value_t = 9101)]
        port: u16,
        /// The repository checkout whose `.hatchrun/config.yml` every
        /// repository serves.
        #[arg(long)]
        repo_dir: PathBuf,
    },
    /// Serve the local launcher protocol.
    Launcher {
        /// The port on 127.0.0.1.
        #[arg(long, default_value_t = 9102)]
        port: u16,
        /// The `hatchrun-agent` binary.
        #[arg(long)]
        agent: PathBuf,
        /// Where each instance gets its directory.
        #[arg(long)]
        work_root: PathBuf,
    },
    /// Print a new session as JSON: `cookie` and `csrf` for requests,
    /// `key` and `sealed` for its `sessions` row.
    Session {
        /// The user's GitHub token, sealed into the row.
        #[arg(long)]
        github_token: String,
    },
}

async fn serve(port: u16, app: axum::Router) -> Result<()> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("listen on {addr}"))?;
    eprintln!("listening on http://{addr}");
    axum::serve(listener, app).await.context("serve")
}

#[tokio::main]
async fn main() -> Result<()> {
    match Args::parse().command {
        Command::FakeGithub { port, repo_dir } => {
            serve(port, github::router(github::Fake::new(repo_dir))).await
        }
        Command::Launcher {
            port,
            agent,
            work_root,
        } => {
            let agent = std::fs::canonicalize(&agent)
                .with_context(|| format!("find {}", agent.display()))?;
            std::fs::create_dir_all(&work_root)
                .with_context(|| format!("create {}", work_root.display()))?;
            let work_root = std::fs::canonicalize(&work_root)?;
            serve(
                port,
                launcher::router(launcher::Launcher::new(agent, work_root)),
            )
            .await
        }
        Command::Session { github_token } => {
            println!("{}", session::new(&github_token)?);
            Ok(())
        }
    }
}
