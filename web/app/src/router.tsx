// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

// A small router over the History API: the app has a handful of
// screens and needs nothing more.

import { useSyncExternalStore, type AnchorHTMLAttributes, type MouseEvent } from "react";

const listeners = new Set<() => void>();

function subscribe(listener: () => void) {
  listeners.add(listener);
  window.addEventListener("popstate", listener);
  return () => {
    listeners.delete(listener);
    window.removeEventListener("popstate", listener);
  };
}

const location = () => window.location.pathname + window.location.search;

/** The current path and query. */
export function useLocation(): string {
  return useSyncExternalStore(subscribe, location);
}

export function navigate(to: string, replace = false) {
  if (replace) {
    window.history.replaceState(null, "", to);
  } else {
    window.history.pushState(null, "", to);
  }
  window.scrollTo(0, 0);
  for (const listener of listeners) {
    listener();
  }
}

/**
 * Sets or, for null and "", drops query parameters of the current
 * location in place: no new history entry and no scroll, for a view's
 * filters as they change.
 */
export function setQuery(params: Record<string, string | null>) {
  const url = new URL(window.location.href);
  for (const [name, value] of Object.entries(params)) {
    if (value) {
      url.searchParams.set(name, value);
    } else {
      url.searchParams.delete(name);
    }
  }
  window.history.replaceState(null, "", url.pathname + url.search + url.hash);
  for (const listener of listeners) {
    listener();
  }
}

/** A query parameter of the current location. */
export function useQuery(name: string): string | null {
  const current = useLocation();
  return new URL(current, window.location.origin).searchParams.get(name);
}

export function Link({ href, onClick, ...rest }: AnchorHTMLAttributes<HTMLAnchorElement> & { href: string }) {
  const follow = (event: MouseEvent<HTMLAnchorElement>) => {
    onClick?.(event);
    if (event.defaultPrevented || event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey) {
      return;
    }
    event.preventDefault();
    navigate(href);
  };
  return <a href={href} onClick={follow} {...rest} />;
}

/** A query parameter that holds a comma separated list, as an array. */
export function useQueryList(name: string): string[] {
  const value = useQuery(name);
  return value ? value.split(",").filter(Boolean) : [];
}

/**
 * The app's routes, all under an organization:
 *
 *   /o/:org                                        Runs
 *   /o/:org/repositories                           Repositories
 *   /o/:org/repositories/:repo[/runs]              one repository, Runs tab
 *   /o/:org/repositories/:repo/(workflows|branches|pulls)   its other tabs
 *   /o/:org/repositories/:repo/workflows/:workflow one workflow's runs
 *   /o/:org/repositories/:repo/runs/:n[/jobs/:j]   one run (n is the run id) and a job of it
 *   /o/:org/secrets                                Secrets and variables
 *   /o/:org/environments                           Environments
 *   /o/:org/settings/billing                       Settings, Billing (/settings redirects here)
 *   /o/:org/settings/setup                         First run checklist
 *   /o/:org/settings/audit                         Audit log (org admin)
 */
export type OrgPage = "kit" | "runs" | "repositories" | "repository" | "secrets" | "environments" | "billing" | "audit" | "setup" | "run";

export type RepoTabId = "runs" | "workflows" | "branches" | "pulls";

export interface OrgRoute {
  org: string;
  page: OrgPage;
  /** The repository and run pages. `repo` is the name without the owner, which is `org`. */
  repo: string | null;
  run: string | null;
  job: string | null;
  /** The repository page's tab. */
  tab: RepoTabId | null;
  /** The workflow of a workflow page. */
  workflow: string | null;
}

const NAME = "[A-Za-z0-9._-]+";

const PAGES: Record<string, OrgPage> = {
  repositories: "repositories",
  secrets: "secrets",
  environments: "environments",
  "settings/billing": "billing",
  "settings/setup": "setup",
  "settings/audit": "audit",
  // The component gallery, under `npm run dev:mock` only.
  ...(import.meta.env.MODE === "mock" ? { kit: "kit" as const } : {}),
};

const NONE = { repo: null, run: null, job: null, tab: null, workflow: null } as const;

/** The route of a `/o/...` path, null for any other or an unknown page. */
export function matchOrg(path: string): OrgRoute | null {
  const run = new RegExp(`^/o/(${NAME})/repositories/(${NAME})/runs/(\\d+)(?:/jobs/(\\d+))?/?$`).exec(path);
  if (run?.[1] && run[2] && run[3]) {
    return { ...NONE, org: run[1], page: "run", repo: run[2], run: run[3], job: run[4] ?? null };
  }
  const repository = new RegExp(`^/o/(${NAME})/repositories/(${NAME})(?:/(runs|workflows|branches|pulls)(?:/([^/]+))?)?/?$`).exec(path);
  if (repository?.[1] && repository[2]) {
    const tab = (repository[3] ?? "runs") as RepoTabId;
    const workflow = repository[4] ? decodeURIComponent(repository[4]) : null;
    if (workflow !== null && tab !== "workflows") {
      return null;
    }
    return { ...NONE, org: repository[1], page: "repository", repo: repository[2], tab, workflow };
  }
  const page = new RegExp(`^/o/(${NAME})(?:/([a-z/]+?))?/?$`).exec(path);
  if (!page?.[1]) {
    return null;
  }
  if (page[2] === undefined) {
    return { ...NONE, org: page[1], page: "runs" };
  }
  const known = PAGES[page[2]];
  return known ? { ...NONE, org: page[1], page: known } : null;
}

/** A run page's parts; the top bar of a public page reduces to its repository. */
export interface CiRoute {
  org: string;
  repo: string;
  run: string;
  job: string | null;
}

export function matchCi(path: string): CiRoute | null {
  const found = matchOrg(path);
  return found?.page === "run" && found.repo && found.run
    ? { org: found.org, repo: found.repo, run: found.run, job: found.job }
    : null;
}

/** Where an old URL lives now, null for a path that moved nowhere. */
export function legacyCiRedirect(path: string): string | null {
  const orgRun = new RegExp(`^/o/(${NAME})/ci/(${NAME})/runs/(\\d+)(?:/jobs/(\\d+))?/?$`).exec(path);
  if (orgRun?.[1] && orgRun[2] && orgRun[3]) {
    return `/o/${orgRun[1]}/repositories/${orgRun[2]}/runs/${orgRun[3]}${orgRun[4] ? `/jobs/${orgRun[4]}` : ""}`;
  }
  const moved = new RegExp(`^/o/(${NAME})/(?:(setup|audit)|settings)/?$`).exec(path);
  if (moved?.[1]) {
    return `/o/${moved[1]}/settings/${moved[2] ?? "billing"}`;
  }
  const repos = new RegExp(`^/o/(${NAME})/repos/?$`).exec(path);
  if (repos?.[1]) {
    return `/o/${repos[1]}/repositories`;
  }
  const found = new RegExp(`^/ci/(${NAME})/(${NAME})(?:/runs/(\\d+)(?:/jobs/(\\d+))?|(/settings))?/?$`).exec(path);
  if (!found?.[1] || !found[2]) {
    return null;
  }
  const [, owner, repo, run, job, settings] = found;
  if (run) {
    return `/o/${owner}/repositories/${repo}/runs/${run}${job ? `/jobs/${job}` : ""}`;
  }
  const full = encodeURIComponent(`${owner}/${repo}`);
  return settings ? `/o/${owner}/secrets?scope=repo&repo=${full}` : `/o/${owner}/repositories/${repo}`;
}
