// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

// Org-scale data for `npm run dev:mock`: the org "acme" has 1,000
// repositories, 5,000 runs over the last week, 900 secrets, 20
// environments per repository, one run with 104 jobs (a matrix of 72) and
// a 200,000 line log with errors. Everything is computed from indexes
// with a small hash, so it is the same on every start and nothing big is
// stored as a literal. Other organizations answer empty lists.
//
// Routes (all under /api/ci/): orgs/:org/{runs,needs-you,repos,
// repos/enabled,setup,environments}, repos/:o/:r/runs, runs/:id for the
// generated runs, and jobs/:id/{log,log/index,log/search} for their jobs.

import type { IncomingMessage, ServerResponse } from "node:http";

import { billing, environmentCounts, hasCard, seedEnvironments } from "./ci-settings.ts";

export const SCALE_ORG = "acme";
const NOW = Date.now();
const MINUTE = 60_000;
const ME = "octocat";

/** A 32 bit hash of two numbers: the only source of "randomness". */
function hash(a: number, b = 0): number {
  let h = (Math.imul(a + 0x9e3779b9, 0x85ebca6b) ^ Math.imul(b + 0x7f4a7c15, 0xc2b2ae35)) >>> 0;
  h ^= h >>> 15;
  h = Math.imul(h, 0x2c1b3c6d) >>> 0;
  h ^= h >>> 12;
  h = Math.imul(h, 0x297a2d39) >>> 0;
  return (h ^ (h >>> 15)) >>> 0;
}
const pick = <T>(list: readonly T[], a: number, b = 0): T => list[hash(a, b) % list.length] as T;

const ADJ = "fast quiet red blue calm bold dark pale warm cold tiny huge swift lazy brave eager grim keen lucid mellow nimble proud rapid sharp stout".split(" ");
const NOUN =
  "api web auth billing search gateway worker mailer cron ingest export import render cache queue router parser runtime sdk cli docs site mobile ios android edge proxy store events metrics logs deploy infra ledger notify media graph sync vault tools".split(
    " ",
  );
const NAMED = ["web", "api", "monorepo", "infra"];

const repoName = (i: number) => `${SCALE_ORG}/${NAMED[i] ?? `${ADJ[i % 25]}-${NOUN[Math.floor(i / 25) % 40]}`}`;
const REPO_COUNT = 1000;
const REPOS = Array.from({ length: REPO_COUNT }, (_, i) => repoName(i));
const defaultBranch = (i: number) => (i % 17 === 0 ? "master" : i % 11 === 0 ? "develop" : "main");

const WORKFLOWS = ["ci", "deploy", "release", "nightly", "lint", "e2e"];
const BRANCHES = [
  "main", "develop", "feat/nav", "fix/flaky-test", "chore/deps", "release/2026.11",
  "feature/very-long-branch-name-for-truncation-checks-with-ticket-ENG-12345-and-more",
  "dependabot/npm_and_yarn/typescript-eslint-8.72.0", "feat/billing-v2", "fix/oom-in-ingest",
];
const ACTORS = [ME, "gamunu", "alice", "bob", "carol", "dave", "erin", "frank", "grace", "heidi", "ivan", "judy", "mallory", "niaj", "olivia", "peggy", "rupert", "sybil", "trent", "victor", "wendy", "dependabot[bot]", "renovate[bot]"];
const EVENTS = ["push", "push", "push", "pull_request", "pull_request", "workflow_dispatch", "schedule"];

export interface Job {
  id: number;
  plan_id: string;
  name: string;
  status: string;
  conclusion: string | null;
  attempt: number;
  started_at: number | null;
  completed_at: number | null;
  steps: { index: number; name: string; conclusion?: string; duration_ms?: number; summary?: null }[];
  has_log: boolean;
  held: string | null;
  environment: string | null;
  approval: { decision: "approved" | "rejected"; by: string; at: number } | null;
}

interface RunRow {
  id: number;
  run_number: number;
  run_attempt: number;
  repository: string;
  workflow: string;
  event: string;
  ref: string;
  branch: string | null;
  sha: string;
  actor: string;
  status: string;
  conclusion: string | null;
  created_at: number;
  completed_at: number | null;
  duration_ms: number | null;
  jobs: { total: number; passed: number; failed: number; running: number; waiting: number; cancelled: number; skipped: number };
}

const RUNS = 5000;
/** The run with 100+ jobs, third newest. */
export const BIG_RUN = 9001;
const BIG_AT = 3;
const runId = (i: number) => (i === BIG_AT ? BIG_RUN : 50_000 - i);

// ---- the run with 104 jobs --------------------------------------------

const OS = ["linux", "macos", "windows"];
const NODE = [18, 20, 22];

interface Plan {
  name: string;
  /** Position in the run's order, 0-based. */
  k: number;
  environment?: string;
}

const bigPlans: Plan[] = (() => {
  const names = ["lint", "typecheck", "build (linux)", "build (macos)", "build (windows)"];
  for (const os of OS) for (const node of NODE) for (let shard = 1; shard <= 8; shard++) names.push(`test (${os}, node ${node}, shard ${shard}/8)`);
  for (let shard = 1; shard <= 24; shard++) names.push(`e2e (shard ${shard}/24)`);
  names.push("package", "deploy-staging", "deploy-production");
  return names.map((name, k) => ({ name, k, environment: name === "deploy-production" ? "production" : name === "deploy-staging" ? "staging" : undefined }));
})();

/** The matrix job whose log is 200,000 lines with errors. */
export const BIG_JOB_K = 5 + 8 + 2; // test (linux... node 20, shard 3/8)
const FAILED_K = new Set([BIG_JOB_K, BIG_JOB_K + 9, 5 + 3 * 8 + 4, 40]);

const jobId = (run: number, k: number) => 1_000_000 + run * 200 + k;
const decodeJob = (id: number) => ({ run: Math.floor((id - 1_000_000) / 200), k: (id - 1_000_000) % 200 });
export const BIG_JOB = jobId(BIG_RUN, BIG_JOB_K);

/** What an admin decided about a held job, by job id. */
const decisions = new Map<number, { decision: "approved" | "rejected"; at: number }>();
/** The runs with a decided job: their job counts are read from the jobs. */
const decidedRuns = new Set<number>();

function bigStatus(k: number): { status: string; conclusion: string | null; held: string | null } {
  if (k === bigPlans.length - 1) {
    const decision = decisions.get(jobId(BIG_RUN, k))?.decision;
    if (decision === "approved") return { status: "in_progress", conclusion: null, held: null };
    if (decision === "rejected") return { status: "completed", conclusion: "failure", held: null };
    return { status: "queued", conclusion: null, held: "approval" };
  }
  if (FAILED_K.has(k)) return { status: "completed", conclusion: "failure", held: null };
  if (k < 70) return { status: "completed", conclusion: "success", held: null };
  if (k < 84) return { status: "in_progress", conclusion: null, held: null };
  return { status: "queued", conclusion: null, held: null };
}

// ---- runs --------------------------------------------------------------

const outcome = (i: number): { status: string; conclusion: string | null; wait: boolean } => {
  if (i === 0) return { status: "in_progress", conclusion: null, wait: false };
  if (i === 1) return { status: "queued", conclusion: null, wait: false };
  if (i === 2 || i === 25 || i === 60) return { status: "queued", conclusion: null, wait: true };
  if (i === BIG_AT) return { status: "in_progress", conclusion: null, wait: true };
  if (i < 40 && i % 9 === 0) return { status: "in_progress", conclusion: null, wait: false };
  const r = hash(i, 5) % 100;
  return { status: "completed", conclusion: r < 85 ? "success" : r < 95 ? "failure" : "cancelled", wait: false };
};

function countsFor(i: number, o: ReturnType<typeof outcome>): RunRow["jobs"] {
  if (i === BIG_AT) {
    const all = bigPlans.map((plan) => bigStatus(plan.k));
    return {
      total: all.length,
      passed: all.filter((s) => s.conclusion === "success").length,
      failed: all.filter((s) => s.conclusion === "failure").length,
      running: all.filter((s) => s.status === "in_progress").length,
      waiting: all.filter((s) => s.held).length,
      cancelled: all.filter((s) => s.conclusion === "cancelled").length,
      skipped: all.filter((s) => s.conclusion === "skipped").length,
    };
  }
  const total = 1 + (hash(i, 6) % 8);
  if (o.status === "completed") {
    const failed = o.conclusion === "failure" ? 1 + (hash(i, 7) % Math.max(1, Math.min(2, total))) : 0;
    const cancelled = o.conclusion === "cancelled" ? 1 : 0;
    return { total, passed: o.conclusion === "success" ? total : total - failed - cancelled, failed, running: 0, waiting: 0, cancelled, skipped: 0 };
  }
  const waiting = o.wait ? 1 : 0;
  const running = o.status === "in_progress" ? 1 : 0;
  return { total, passed: Math.max(0, total - running - waiting - (o.status === "queued" ? 1 : 0)), failed: 0, running, waiting, cancelled: 0, skipped: 0 };
}

const runCache = new Map<number, RunRow>();

function runAt(i: number): RunRow {
  const cached = runCache.get(i);
  if (cached) return cached;
  const o = outcome(i);
  const topRepo = hash(i, 1) % 100 < 40;
  const repoIx = i === BIG_AT ? 2 : topRepo ? hash(i, 2) % 20 : hash(i, 3) % REPO_COUNT;
  const branch = pick(BRANCHES, i, 4);
  const created = NOW - i * 115_000 - (hash(i, 8) % 60_000);
  const duration = 40_000 + (hash(i, 9) % 25) * MINUTE;
  const finished = o.status === "completed";
  const sha = Array.from({ length: 5 }, (_, n) => hash(i, 100 + n).toString(16).padStart(8, "0")).join("");
  const row: RunRow = {
    id: runId(i),
    run_number: 3000 - Math.floor(i / 7) + (repoIx % 5),
    run_attempt: 1,
    repository: REPOS[repoIx] as string,
    workflow: i === BIG_AT ? "ci" : pick(WORKFLOWS, i, 10),
    event: pick(EVENTS, i, 11),
    ref: `refs/heads/${i === BIG_AT ? "main" : branch}`,
    branch: i === BIG_AT ? "main" : branch,
    sha: sha.slice(0, 40),
    actor: i === BIG_AT ? ME : pick(ACTORS, i, 12),
    status: o.status,
    conclusion: o.conclusion,
    created_at: created,
    completed_at: finished ? created + duration : null,
    duration_ms: finished ? duration : null,
    jobs: countsFor(i, o),
  };
  runCache.set(i, row);
  return row;
}

/** The run as answered: once one of its jobs is decided its counts are read from the jobs. */
function viewOf(run: RunRow): RunRow {
  if (!decidedRuns.has(run.id)) return run;
  const jobs = jobsOf(run);
  return {
    ...run,
    jobs: {
      total: jobs.length,
      passed: jobs.filter((job) => job.conclusion === "success").length,
      failed: jobs.filter((job) => job.conclusion === "failure").length,
      running: jobs.filter((job) => job.status === "in_progress").length,
      waiting: jobs.filter((job) => job.held).length,
      cancelled: jobs.filter((job) => job.conclusion === "cancelled").length,
      skipped: jobs.filter((job) => job.conclusion === "skipped").length,
    },
  };
}

const indexOfRun = (id: number) => (id === BIG_RUN ? BIG_AT : 50_000 - id);
/** The generated id the big run displaced is not a run. */
const validRun = (id: number) => id === BIG_RUN || (id > 50_000 - RUNS && id <= 50_000 && indexOfRun(id) !== BIG_AT);

// ---- paging, filters ----------------------------------------------------

const encode = (offset: number) => Buffer.from(`o:${offset}`).toString("base64url");
const decode = (cursor: string | null) => {
  const found = /^o:(\d+)$/.exec(cursor ? Buffer.from(cursor, "base64url").toString() : "");
  return found ? Number(found[1]) : 0;
};

/** The `Page<T>` of a list for the request's `cursor` and `limit`. */
export function paginate<T>(items: T[], url: URL) {
  const limit = Math.min(100, Math.max(1, Number(url.searchParams.get("limit")) || 50));
  const offset = decode(url.searchParams.get("cursor"));
  const next = offset + limit < items.length ? encode(offset + limit) : null;
  return { items: items.slice(offset, offset + limit), total: items.length, next };
}

const list = (url: URL, name: string) => (url.searchParams.get(name) ?? "").split(",").filter(Boolean);

function matchesStatus(run: RunRow, wanted: string[]): boolean {
  const active = run.status !== "completed";
  return wanted.some((state) => {
    switch (state) {
      case "queued":
        return run.status === "queued" && run.jobs.waiting === 0;
      case "running":
        return run.status === "in_progress";
      case "waiting":
        return active && run.jobs.waiting > 0;
      case "success":
      case "failure":
      case "cancelled":
        return run.conclusion === state;
      default:
        return false;
    }
  });
}

function filterRuns(url: URL, repoFixed?: string): RunRow[] {
  const status = list(url, "status");
  const repos = repoFixed ? [repoFixed] : list(url, "repo");
  const branch = url.searchParams.get("branch");
  const actorParam = url.searchParams.get("actor");
  const actor = actorParam === "me" ? ME : actorParam;
  const q = (url.searchParams.get("q") ?? "").toLowerCase();
  const since = url.searchParams.has("since") ? Number(url.searchParams.get("since")) : NOW - 7 * 24 * 60 * MINUTE;
  const until = url.searchParams.has("until") ? Number(url.searchParams.get("until")) : Infinity;
  const out: RunRow[] = [];
  for (let i = 0; i < RUNS; i++) {
    const run = runAt(i);
    if (run.created_at < since) break;
    if (run.created_at > until) continue;
    if (repos.length && !repos.includes(run.repository)) continue;
    if (branch && run.branch !== branch) continue;
    if (actor && run.actor !== actor) continue;
    if (status.length && !matchesStatus(run, status)) continue;
    if (q && !`${run.repository} ${run.workflow} ${run.branch} ${run.sha} ${run.actor} #${run.run_number}`.toLowerCase().includes(q)) continue;
    out.push(run);
  }
  return out;
}

// ---- repositories, environments, secrets ------------------------------------

let enabledAll: boolean | null = null;
const enabledSet = new Set<string>();
const hashEnabled = (i: number) => hash(i, 50) % 10 !== 0;
const isEnabled = (name: string, i: number) => (enabledAll === null ? hashEnabled(i) : enabledAll || enabledSet.has(name));

const lastRunOf = new Map<string, RunRow>();
function lastRuns() {
  if (lastRunOf.size === 0) {
    for (let i = 0; i < RUNS; i++) {
      const run = runAt(i);
      if (!lastRunOf.has(run.repository)) lastRunOf.set(run.repository, run);
    }
  }
  return lastRunOf;
}

function repoRows() {
  const last = lastRuns();
  return REPOS.map((name, i) => {
    const run = last.get(name);
    return {
      name,
      default_branch: defaultBranch(i),
      enabled: isEnabled(name, i),
      last_run: run ? { id: run.id, run_number: run.run_number, status: run.status, conclusion: run.conclusion, created_at: run.created_at } : null,
    };
  }).sort((a, b) => (b.last_run?.created_at ?? 0) - (a.last_run?.created_at ?? 0) || a.name.localeCompare(b.name));
}

const ENVS = ["production", "staging", "qa", "dev", "preview", "canary", "sandbox", "demo", "eu-west", "eu-central", "us-east", "us-west", "ap-south", "ap-northeast", "sa-east", "dr", "load-test", "security", "uat", "internal"];

/** The environments a repository of the org starts with, for ci-settings.ts to seed its store from. */
export function scaleEnvironments(repository: string): [string, { branches: string[]; approval: boolean; updatedAt: number }][] {
  const r = REPOS.indexOf(repository);
  return ENVS.map((name, e) => [
    name,
    {
      branches: e === 0 ? [defaultBranch(r)] : e === 1 ? ["main", "release/*"] : [],
      approval: e === 0 || e === 14,
      updatedAt: NOW - (1 + (hash(r, e) % 90)) * 24 * 60 * MINUTE,
    },
  ]);
}

/** The org's environments as the repository store holds them now, so edits and deletes show here. */
function environmentRows() {
  return REPOS.flatMap((repository) => [...seedEnvironments(repository).entries()].map(([name, row]) => ({ repository, name, ...row })));
}

/** Names and repos of the org's 900 secrets and 300 variables, for ci-settings.ts to seed from. */
export function orgItems(kind: "secrets" | "vars") {
  const count = kind === "secrets" ? 900 : 300;
  const stems = "NPM TOKEN DEPLOY KEY AWS GCP SENTRY DATADOG SLACK DOCKER REGISTRY SIGNING STRIPE SMTP DB CACHE S3 CDN TWILIO PAGER".split(" ");
  return Array.from({ length: count }, (_, n) => ({
    name: `${stems[n % stems.length]}_${stems[Math.floor(n / stems.length) % stems.length]}_${n}`,
    value: kind === "vars" ? `value-${n}` : "x",
    updatedAt: NOW - (1 + (hash(n, 400) % 200)) * 24 * 60 * MINUTE,
    repositories: kind === "secrets" && n % 3 === 0 ? Array.from({ length: 1 + (n % 14) }, (_, j) => REPOS[hash(n, j) % REPO_COUNT] as string).sort() : null,
  }));
}
export const scaleRepoNames = () => REPOS;

// ---- jobs and logs --------------------------------------------------------------

const GENERIC = ["build", "test", "lint", "deploy", "e2e", "package", "docs", "release"];

function jobsOf(run: RunRow): Job[] {
  const i = indexOfRun(run.id);
  const plans: Plan[] = i === BIG_AT ? bigPlans : Array.from({ length: run.jobs.total }, (_, k) => ({ name: GENERIC[(hash(i, 20) + k) % GENERIC.length] as string, k }));
  type State = { status: string; conclusion: string | null; held: string | null };
  // Generic runs list their jobs passed first, then failed, running, held, and the rest.
  const order: State[] = [
    ...Array<State>(run.jobs.passed).fill({ status: "completed", conclusion: "success", held: null }),
    ...Array<State>(run.jobs.failed).fill({ status: "completed", conclusion: "failure", held: null }),
    ...Array<State>(run.jobs.running).fill({ status: "in_progress", conclusion: null, held: null }),
    ...Array<State>(run.jobs.waiting).fill({ status: "queued", conclusion: null, held: "approval" }),
  ];
  const rest: State =
    run.status === "completed"
      ? { status: "completed", conclusion: run.conclusion === "cancelled" ? "cancelled" : "skipped", held: null }
      : { status: "queued", conclusion: null, held: null };
  return plans.map((plan) => {
    const id = jobId(run.id, plan.k);
    let state: State = i === BIG_AT ? bigStatus(plan.k) : (order[plan.k] ?? rest);
    const decision = decisions.get(id);
    if (state.held && decision) {
      state = decision.decision === "approved" ? { status: "in_progress", conclusion: null, held: null } : { status: "completed", conclusion: "failure", held: null };
    }
    const started = state.status === "queued" ? null : run.created_at + 5000 + (hash(id, 1) % 60_000);
    const done = state.status === "completed" && started !== null ? started + 20_000 + (hash(id, 2) % 400_000) : null;
    const held = state.held;
    return {
      id,
      plan_id: plan.name.split(" ")[0] as string,
      name: plan.name,
      status: held ? "queued" : state.status,
      conclusion: state.conclusion,
      attempt: 1,
      started_at: started,
      completed_at: done,
      steps: STEP_NAMES.map((name, index) => ({
        index,
        name,
        ...(state.status === "completed"
          ? { conclusion: state.conclusion === "failure" && index === 3 ? "failure" : "success", duration_ms: 1000 + (hash(id, index) % 30_000) }
          : { summary: null }),
      })),
      has_log: state.status !== "queued",
      held,
      environment: plan.environment ?? null,
      approval: decision ? { decision: decision.decision, by: ME, at: decision.at } : null,
    };
  });
}

const STEP_NAMES = ["Set up job", "Check out", "Install", "Test", "Post"];

interface Log {
  text: string;
  offsets: Float64Array;
  lines: string[];
  steps: { index: number; name: string; offset: number; line: number; lines: number }[];
  errors: { offset: number; line: number; step: number; text: string }[];
  /** All error lines, not capped; the index caps at 200. */
  stepOfLine: (line: number) => number;
}

const logCache = new Map<number, Log>();
const FILES = ["src/runner.test.ts", "packages/core/src/parse.test.ts", "packages/ui/src/Table.test.tsx", "services/ingest/src/queue.test.ts", "lib/cache/store.test.ts", "apps/web/src/router.test.ts"];

function buildLog(id: number): Log | null {
  const hit = logCache.get(id);
  if (hit) return hit;
  const { run: runIdValue } = decodeJob(id);
  if (!validRun(runIdValue)) return null;
  const run = runAt(indexOfRun(runIdValue));
  const job = jobsOf(run).find((candidate) => candidate.id === id);
  if (!job) return null;
  const big = id === BIG_JOB;
  const total = big ? 200_000 : 120 + (hash(id, 3) % 500);
  const failed = job.conclusion === "failure";
  const lines: string[] = [];
  const steps: Log["steps"] = [];
  const errors: Log["errors"] = [];
  const sizes = [6, 12, 40, total - 62, 4];
  sizes.forEach((count, index) => {
    steps.push({ index, name: STEP_NAMES[index] as string, offset: 0, line: lines.length, lines: count + 1 });
    lines.push(`== ${STEP_NAMES[index]}`);
    for (let n = 0; n < count; n++) {
      const seed = hash(id, index * 1_000_000 + n);
      let text: string;
      if (index === 3) {
        const file = FILES[seed % FILES.length];
        const everyError = big ? 1300 : 90;
        if (failed && n % everyError === everyError - 1) text = `##[error]FAIL ${file} > case ${n}: expected 200 received 503 (${seed % 900} ms)`;
        else if (failed && n === count - 1) text = "##[error]Process completed with exit code 1";
        else if (seed % 97 === 0) text = `\u001b[33mwarn\u001b[0m deprecated call in ${file}:${seed % 400}`;
        else text = `PASS ${file} case ${n} (${seed % 900} ms) ${"." .repeat(seed % 24)}`;
      } else if (index === 2) text = `npm http fetch GET 200 https://registry.npmjs.org/pkg-${seed % 5000} ${seed % 300}ms`;
      else if (index === 1) text = n === 0 ? `Cloning ${run.repository} into /work` : `remote: Counting objects: ${n * 7}%`;
      else text = `${STEP_NAMES[index]?.toLowerCase()} line ${n}`;
      lines.push(text);
    }
  });
  if (failed && !lines.some((line) => line.startsWith("##[error]"))) lines[lines.length - 2] = "##[error]Process completed with exit code 1";
  const offsets = new Float64Array(lines.length);
  let at = 0;
  lines.forEach((line, n) => {
    offsets[n] = at;
    at += line.length + 1;
  });
  for (const step of steps) step.offset = offsets[step.line] as number;
  const stepOfLine = (line: number) => {
    for (let s = steps.length - 1; s >= 0; s--) if (line >= (steps[s] as Log["steps"][number]).line) return s;
    return 0;
  };
  lines.forEach((line, n) => {
    if (line.startsWith("##[error]")) errors.push({ offset: offsets[n] as number, line: n, step: stepOfLine(n), text: line.slice(0, 300) });
  });
  const log = { text: lines.join("\n") + "\n", offsets, lines, steps, errors, stepOfLine };
  if (!big && logCache.size > 40) logCache.clear();
  logCache.set(id, log);
  return log;
}

// ---- the handler --------------------------------------------------------------------

const json = (res: ServerResponse, status: number, body?: unknown, headers: Record<string, string> = {}) => {
  res.statusCode = status;
  for (const [name, value] of Object.entries(headers)) res.setHeader(name, value);
  if (status === 204) return void res.end();
  res.setHeader("Content-Type", "application/json");
  res.end(JSON.stringify(body));
};

const empty = (url: URL) => paginate([], url);

/**
 * Answers a request of /api/ci that the scale data owns; false when
 * `path` (after `/api/ci/`) is not one, so the caller goes on.
 */
export function handleScale(
  req: IncomingMessage,
  res: ServerResponse,
  url: URL,
  path: string,
  session: { signedIn: boolean; csrf: string },
  body: unknown,
): boolean {
  let found: RegExpExecArray | null;
  const method = req.method ?? "GET";

  if ((found = /^orgs\/([^/]+)\/runs$/.exec(path))) {
    const org = decodeURIComponent(found[1] as string);
    const counts = { all: 0, running: 0, waiting: 0, failed: 0, passed: 0 };
    if (org !== SCALE_ORG) {
      json(res, 200, { ...empty(url), counts });
      return true;
    }
    const unfiltered = new URL(url);
    unfiltered.searchParams.delete("status");
    for (const run of filterRuns(unfiltered)) {
      counts.all++;
      if (matchesStatus(run, ["running"])) counts.running++;
      if (matchesStatus(run, ["waiting"])) counts.waiting++;
      if (matchesStatus(run, ["failure"])) counts.failed++;
      if (matchesStatus(run, ["success"])) counts.passed++;
    }
    const page = paginate(filterRuns(url), url);
    json(res, 200, { ...page, items: page.items.map(viewOf), counts });
    return true;
  }
  if ((found = /^repos\/([^/]+)\/([^/]+)\/runs$/.exec(path)) && found[1] === SCALE_ORG) {
    const page = paginate(filterRuns(url, `${found[1]}/${found[2]}`), url);
    json(res, 200, { ...page, items: page.items.map(viewOf) });
    return true;
  }
  if ((found = /^orgs\/([^/]+)\/needs-you$/.exec(path))) {
    if (found[1] !== SCALE_ORG) return json(res, 200, { approvals: [], failing: [] }), true;
    const approvals = [2, 25, 60, BIG_AT].flatMap((i) => {
      const run = runAt(i);
      const held = jobsOf(run).find((job) => job.held);
      return held ? [{ run: viewOf(run), job: { id: held.id, name: held.name, environment: held.environment ?? "production" }, since: run.created_at + 5 * MINUTE }] : [];
    });
    const seen = new Set<string>();
    const failing = [];
    for (let i = 0; i < RUNS && failing.length < 4; i++) {
      const run = runAt(i);
      if (run.conclusion === "failure" && run.branch === "main" && !seen.has(run.repository)) {
        seen.add(run.repository);
        failing.push({ repository: run.repository, branch: "main", run: viewOf(run) });
      }
    }
    json(res, 200, { approvals, failing });
    return true;
  }
  if ((found = /^orgs\/([^/]+)\/repos$/.exec(path))) {
    if (method !== "GET") return json(res, 405, { error: "method not allowed" }), true;
    if (found[1] !== SCALE_ORG) return json(res, 200, empty(url)), true;
    const q = (url.searchParams.get("q") ?? "").toLowerCase();
    const enabled = url.searchParams.get("enabled");
    json(
      res,
      200,
      paginate(
        repoRows().filter((repo) => (!q || repo.name.toLowerCase().includes(q)) && (enabled === null || String(repo.enabled) === enabled)),
        url,
      ),
    );
    return true;
  }
  if ((found = /^orgs\/([^/]+)\/repos\/enabled$/.exec(path))) {
    if (method !== "PUT") return json(res, 405, { error: "method not allowed" }), true;
    if (!session.signedIn) return json(res, 401, { error: "sign in to change repositories" }), true;
    if (decodeURIComponent(found[1] as string) !== SCALE_ORG) return json(res, 404, { error: "not found" }), true;
    if (req.headers["x-csrf-token"] !== session.csrf) return json(res, 403, { error: "missing or wrong CSRF token" }), true;
    const asked = (body ?? {}) as { all?: unknown; repos?: unknown };
    if (typeof asked.all !== "boolean" || !Array.isArray(asked.repos)) return json(res, 400, { error: 'expected {"all": true, "repos": []}' }), true;
    enabledAll = asked.all;
    enabledSet.clear();
    for (const name of asked.repos as string[]) enabledSet.add(name);
    json(res, 204);
    return true;
  }
  if ((found = /^orgs\/([^/]+)\/setup$/.exec(path))) {
    const org = decodeURIComponent(found[1] as string);
    const scale = org === SCALE_ORG;
    const count = scale ? REPOS.filter((name, i) => isEnabled(name, i)).length : 0;
    const first = scale ? runAt(RUNS - 1) : null;
    json(res, 200, {
      app: { installed: true, repos: scale ? REPO_COUNT : 0, manage_url: `https://github.com/organizations/${org}/settings/installations/1` },
      enabled: { all: scale && enabledAll === true, count },
      first_run: first ? { id: first.id, repository: first.repository } : null,
      billing: { card: hasCard(org), cap_dollars: billing.capDollars },
    });
    return true;
  }
  if ((found = /^orgs\/([^/]+)\/environments$/.exec(path))) {
    if (found[1] !== SCALE_ORG) return json(res, 200, empty(url)), true;
    const q = (url.searchParams.get("q") ?? "").toLowerCase();
    const repo = url.searchParams.get("repo");
    const page = paginate(environmentRows().filter((row) => (!repo || row.repository === repo) && (!q || `${row.repository} ${row.name}`.toLowerCase().includes(q))), url);
    json(res, 200, { ...page, items: page.items.map((row) => ({ ...row, ...environmentCounts(row.repository, row.name) })) });
    return true;
  }

  if ((found = /^runs\/(\d+)$/.exec(path)) && validRun(Number(found[1]))) {
    const run = runAt(indexOfRun(Number(found[1])));
    json(res, 200, { access: "admin", run: viewOf(run), jobs: jobsOf(run) });
    return true;
  }
  if (/^runs\/\d+\/cancel$/.test(path) && validRun(Number(path.split("/")[1]))) {
    const run = runAt(indexOfRun(Number(path.split("/")[1])));
    if (run.status !== "completed") {
      // The jobs that had not finished are listed as cancelled, as for a cancelled run of the data.
      run.status = "completed";
      run.conclusion = "cancelled";
      run.completed_at = Date.now();
      run.duration_ms = run.completed_at - run.created_at;
      run.jobs.running = 0;
      run.jobs.waiting = 0;
    }
    json(res, 204);
    return true;
  }
  if ((found = /^jobs\/(\d+)\/(approve|reject)$/.exec(path)) && Number(found[1]) >= 1_000_000) {
    if (!session.signedIn) return json(res, 401, { error: "sign in to approve or reject jobs" }), true;
    if (req.headers["x-csrf-token"] !== session.csrf) return json(res, 403, { error: "missing or wrong CSRF token" }), true;
    const { run: owner } = decodeJob(Number(found[1]));
    const job = validRun(owner) ? jobsOf(runAt(indexOfRun(owner))).find((candidate) => candidate.id === Number(found?.[1])) : undefined;
    if (!job) return json(res, 404, { error: "not found" }), true;
    if (job.held !== "approval") return json(res, 409, { error: "This job is not waiting for approval" }), true;
    decisions.set(job.id, { decision: found[2] === "approve" ? "approved" : "rejected", at: Date.now() });
    decidedRuns.add(owner);
    json(res, 204);
    return true;
  }
  if ((found = /^jobs\/(\d+)\/(log|log\/index|log\/search)$/.exec(path)) && Number(found[1]) >= 1_000_000) {
    const log = buildLog(Number(found[1]));
    if (!log) return json(res, 404, { error: "not found" }), true;
    if (found[2] === "log/index") {
      json(res, 200, {
        bytes: log.text.length,
        lines: log.lines.length,
        steps: log.steps,
        dropped_lines: 0,
        errors: log.errors.slice(0, 200),
        first_error: log.errors[0] ?? null,
      });
    } else if (found[2] === "log/search") {
      const q = (url.searchParams.get("q") ?? "").toLowerCase();
      const start = decode(url.searchParams.get("cursor"));
      const matches: { offset: number; line: number; step: number }[] = [];
      let total = 0;
      for (let n = start; q && n < log.lines.length; n++) {
        if ((log.lines[n] as string).toLowerCase().includes(q)) {
          total++;
          if (matches.length < 1000) matches.push({ offset: log.offsets[n] as number, line: n, step: log.stepOfLine(n) });
        }
      }
      json(res, 200, { matches, total, truncated: total > matches.length });
    } else {
      const offset = Number(url.searchParams.get("offset") ?? 0);
      const length = Math.min(1024 * 1024, Number(url.searchParams.get("length") ?? log.text.length));
      const ranged = url.searchParams.has("length") || url.searchParams.has("offset");
      res.statusCode = 200;
      res.setHeader("Content-Type", "text/plain; charset=utf-8");
      res.setHeader("x-log-bytes", String(log.text.length));
      if (ranged) res.setHeader("x-log-offset", String(offset));
      res.end(ranged ? log.text.slice(offset, offset + length) : log.text);
    }
    return true;
  }
  return false;
}
