// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

// The app's URLs in one place. The organization is always the
// repository's owner.

/** An organization's page: "" is Runs, else repositories, secrets, environments, settings, settings/billing, setup. */
export const orgUrl = (org: string, page = "") => `/o/${encodeURIComponent(org)}${page ? `/${page}` : ""}`;

const split = (repository: string | { org: string; repo: string }) =>
  typeof repository === "string" ? repository.split("/") : [repository.org, repository.repo];

export type RepoTab = "runs" | "workflows" | "branches" | "pulls";

/**
 * A repository's page. `repository` is `owner/name`. The Runs tab is
 * the bare URL; `filters` (branch, pull, workflow) narrow its runs.
 */
export const repoUrl = (repository: string | { org: string; repo: string }, tab: RepoTab = "runs", filters: Record<string, string | number> = {}) => {
  const [owner = "", name = ""] = split(repository);
  const query = new URLSearchParams(Object.entries(filters).map(([key, value]) => [key, String(value)])).toString();
  return `/o/${encodeURIComponent(owner)}/repositories/${encodeURIComponent(name)}${tab === "runs" ? "" : `/${tab}`}${query ? `?${query}` : ""}`;
};

/** A workflow's page in a repository: its runs. */
export const workflowUrl = (repository: string | { org: string; repo: string }, workflow: string) => `${repoUrl(repository, "workflows")}/${encodeURIComponent(workflow)}`;

/** The organization's runs filtered to one repository. */
export const orgRunsOfRepoUrl = (repository: string) => `${orgUrl(repository.split("/")[0] ?? "")}?repo=${encodeURIComponent(repository)}`;

/** One run, by id as /api/ci/runs/{id} takes it. */
export const runUrl = (repository: string | { org: string; repo: string }, run: number | string) => {
  const [owner = "", name = ""] = split(repository);
  return `/o/${encodeURIComponent(owner)}/repositories/${encodeURIComponent(name)}/runs/${run}`;
};

export const jobUrl = (repository: string | { org: string; repo: string }, run: number | string, job: number | string) =>
  `${runUrl(repository, run)}/jobs/${job}`;
