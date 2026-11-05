// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

// The app's URLs in one place. The organization is always the
// repository's owner.

/** An organization's page: "" is Runs, else repos, secrets, environments, settings, settings/billing, setup. */
export const orgUrl = (org: string, page = "") => `/o/${encodeURIComponent(org)}${page ? `/${page}` : ""}`;

/** Runs of one repository (the Runs page filtered to it). `repository` is `owner/name`. */
export const repoRunsUrl = (repository: string | { org: string; repo: string }) => {
  const full = typeof repository === "string" ? repository : `${repository.org}/${repository.repo}`;
  const owner = full.split("/")[0] ?? "";
  return `${orgUrl(owner)}?repo=${encodeURIComponent(full)}`;
};

/** One run, by id as /api/ci/runs/{id} takes it. */
export const runUrl = (repository: string | { org: string; repo: string }, run: number | string) => {
  const [owner = "", name = ""] = typeof repository === "string" ? repository.split("/") : [repository.org, repository.repo];
  return `/o/${encodeURIComponent(owner)}/ci/${encodeURIComponent(name)}/runs/${run}`;
};

export const jobUrl = (repository: string | { org: string; repo: string }, run: number | string, job: number | string) =>
  `${runUrl(repository, run)}/jobs/${job}`;
