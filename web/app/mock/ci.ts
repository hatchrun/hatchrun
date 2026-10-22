// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

// A stand-in for /api/ci under `npm run dev:mock`, in the shapes of
// crates/ci/src/api.rs. The repository e2e/demo is public. Run 1 has
// job 1, which finished with a stored log and an index, and job 2,
// which runs: its socket replays three `events` messages and `end`,
// after which the job is finished and answers with its stored log.
// Restart the dev server to replay it. e2e/secret is private: a viewer
// who is not signed in gets a 404, as the API answers.

import { createHash } from "node:crypto";
import type { IncomingMessage, Server, ServerResponse } from "node:http";
import type { Duplex } from "node:stream";

import type { LogIndex } from "../src/api.ts";
import { handleCiSettings } from "./ci-settings.ts";

interface Step {
  index: number;
  name: string;
  lines: string[];
  conclusion: "success" | "failure";
  ms: number;
}

type Event =
  | { type: "step_started"; index: number; name: string }
  | { type: "line"; text: string }
  | { type: "step_finished"; index: number; summary: { name: string; outcome: string; conclusion: string; duration_ms: number } };

const green = (text: string) => `\u001b[32m${text}\u001b[0m`;
const red = (text: string) => `\u001b[1;31m${text}\u001b[0m`;

const finishedSteps: Step[] = [
  { index: 0, name: "Set up job", lines: ["Runner: ubuntu-26.04", `Image ready ${green("✓")}`], conclusion: "success", ms: 2100 },
  {
    index: 1,
    name: "Check out",
    lines: ["Cloning e2e/demo into /work", "<img src=x onerror=alert(1)>", "HEAD is now at 3f2a9c1 mock commit"],
    conclusion: "success",
    ms: 4300,
  },
  {
    index: 2,
    name: "Test",
    lines: [
      "running 3 tests",
      `test parse ... ${green("ok")}`,
      `test render ... ${green("ok")}`,
      `test store ... ${red("FAILED")}`,
      "\u001b]0;title that is dropped\u0007progress 10%\rprogress 100%",
      "error: 1 test failed",
    ],
    conclusion: "failure",
    ms: 18_400,
  },
];

const setUp: Step = { index: 0, name: "Set up job", lines: ["Runner: ubuntu-26.04"], conclusion: "success", ms: 1900 };
const build: Step = { index: 1, name: "Build", lines: ["Compiling hatchrun v0.1.0", `Compiled ${green("✓")}`], conclusion: "success", ms: 9000 };
const test: Step = { index: 2, name: "Test", lines: ["running 2 tests", "test a ... ok", "test b ... ok", "all passed"], conclusion: "success", ms: 5000 };
const liveSteps: Step[] = [setUp, build, test];

const text = (steps: Step[]) => steps.map((step) => [`== ${step.name}`, ...step.lines].join("\n")).join("\n") + "\n";

function indexOf(steps: Step[]): LogIndex {
  let offset = 0;
  const marks = steps.map((step) => {
    const mark = { index: step.index, name: step.name, offset };
    offset += Buffer.byteLength(text([step]));
    return mark;
  });
  return { bytes: offset, steps: marks, dropped_lines: 0 };
}

const summary = (step: Step) => ({
  name: step.name,
  outcome: step.conclusion,
  conclusion: step.conclusion,
  duration_ms: step.ms,
});

/** What the running job's socket sends: the backlog (steps 0 and 1 up to a line), three messages, then `end`. */
const backlog = "== Set up job\nRunner: ubuntu-26.04\n== Build\nCompiling hatchrun v0.1.0\n";
const liveMessages: unknown[] = [
  { type: "events", events: [{ type: "line", text: `Compiled ${green("✓")}` }, { type: "step_finished", index: 1, summary: summary(build) }] },
  {
    type: "events",
    events: [{ type: "step_started", index: 2, name: "Test" }, { type: "line", text: "running 2 tests" }, { type: "line", text: "test a ... ok" }],
  },
  {
    type: "events",
    events: [
      { type: "line", text: "test b ... ok" },
      { type: "line", text: "all passed" },
      { type: "step_finished", index: 2, summary: summary(test) },
    ] satisfies Event[],
  },
];

const now = Date.now();
const minute = 60_000;

interface Job {
  id: number;
  name: string;
  status: "in_progress" | "completed";
  conclusion: string | null;
  started_at: number;
  completed_at: number | null;
  steps: Step[];
}

const jobs: Job[] = [
  { id: 1, name: "build (host)", status: "completed", conclusion: "failure", started_at: now - 9 * minute, completed_at: now - 8 * minute, steps: finishedSteps },
  { id: 2, name: "test (host)", status: "in_progress", conclusion: null, started_at: now - 30_000, completed_at: null, steps: liveSteps },
];

const runs = [
  run(2, 2, "ci", "push", "refs/heads/main", "completed", "success", now - 3 * 60 * minute, now - 3 * 60 * minute + 4 * minute),
  run(1, 1, "ci", "workflow_dispatch", "refs/heads/main", "in_progress", null, now - 10 * minute, null),
];

function run(id: number, number: number, workflow: string, event: string, ref: string, status: string, conclusion: string | null, created: number, completed: number | null) {
  return {
    id,
    run_number: number,
    run_attempt: 1,
    repository: "e2e/demo",
    workflow,
    event,
    ref,
    sha: "3f2a9c1d5e6b7a8091a2b3c4d5e6f708192a3b4c",
    actor: "octocat",
    status,
    conclusion,
    created_at: created,
    completed_at: completed,
  };
}

const jobJson = (job: Job) => ({
  id: job.id,
  plan_id: job.name.split(" ")[0],
  name: job.name,
  status: job.status,
  conclusion: job.conclusion,
  attempt: 1,
  started_at: job.started_at,
  completed_at: job.completed_at,
  // Rows keep a finished job's steps as summaries; a running job's are the object's views.
  steps:
    job.status === "completed"
      ? job.steps.map(summary)
      : job.steps.slice(0, 2).map((step) => ({ index: step.index, name: step.name, summary: step.index < 1 ? summary(step) : null })),
  has_log: job.status === "completed",
});

/** The dispatch inputs the mock workflow `ci` declares. */
const declared: Record<string, { kind: "string" | "boolean" | "integer" | "enum"; default?: unknown; choices?: string[] }> = {
  environment: { kind: "enum", default: "staging", choices: ["staging", "production"] },
  dry_run: { kind: "boolean", default: false },
  retries: { kind: "integer", default: 1 },
};

/** The same checks as resolve_inputs in crates/ci/src/api.rs. */
function checkInputs(given: Record<string, unknown>): string | null {
  const unknown = Object.keys(given).find((key) => !(key in declared));
  if (unknown) {
    return `input \`${unknown}\` is not declared by the workflow`;
  }
  for (const [name, input] of Object.entries(declared)) {
    const value = given[name] ?? input.default;
    if (value === undefined) {
      return `input \`${name}\` is required`;
    }
    if (input.kind === "enum" && !input.choices?.includes(String(value))) {
      return `input \`${name}\` must be one of: ${input.choices?.join(", ")}`;
    }
    const ok =
      input.kind === "boolean" ? typeof value === "boolean" : input.kind === "integer" ? Number.isInteger(value) : typeof value === "string";
    if (!ok) {
      return `input \`${name}\` has the wrong type`;
    }
  }
  return null;
}

const json = (res: ServerResponse, status: number, body: unknown, headers: Record<string, string> = {}) => {
  res.statusCode = status;
  res.setHeader("Content-Type", "application/json");
  for (const [name, value] of Object.entries(headers)) {
    res.setHeader(name, value);
  }
  res.end(JSON.stringify(body));
};

const isPrivate = (repository: string) => repository === "e2e/secret";

/** Answers a request of /api/ci. `signedIn` and `csrf` are the mock session's. */
export async function handleCi(
  req: IncomingMessage,
  res: ServerResponse,
  url: URL,
  session: { signedIn: boolean; csrf: string },
  body: unknown,
) {
  const path = url.pathname.replace(/^\/api\/ci\//, "");
  const hidden = () => json(res, 404, { error: "not found" });
  let found: RegExpExecArray | null;

  if (handleCiSettings(req, res, path, session, body)) {
    return;
  }

  if ((found = /^repos\/([^/]+)\/([^/]+)\/runs$/.exec(path))) {
    const repository = `${found[1]}/${found[2]}`;
    if (req.method !== "GET" || (repository !== "e2e/demo" && !isPrivate(repository)) || (isPrivate(repository) && !session.signedIn)) {
      return hidden();
    }
    const before = Number(url.searchParams.get("before") ?? Infinity);
    const page = (isPrivate(repository) ? [] : runs).filter((candidate) => candidate.id < before).sort((a, b) => b.id - a.id);
    return json(res, 200, { runs: page });
  }

  if ((found = /^repos\/([^/]+)\/([^/]+)\/dispatch$/.exec(path))) {
    const repository = `${found[1]}/${found[2]}`;
    if (req.method !== "POST" || (repository !== "e2e/demo" && !isPrivate(repository))) {
      return hidden();
    }
    if (!session.signedIn) {
      return json(res, 401, { error: "sign in to change runs" });
    }
    if (req.headers["x-csrf-token"] !== session.csrf) {
      return json(res, 403, { error: "missing or wrong CSRF token" });
    }
    if (isPrivate(repository)) {
      return json(res, 403, { error: "you need write access to this repository" });
    }
    const asked = (body ?? {}) as { workflow?: string; ref?: string; inputs?: Record<string, unknown> };
    if (asked.workflow !== "ci") {
      return json(res, 422, { error: `the config has no workflow \`${asked.workflow ?? ""}\`` });
    }
    const ref = asked.ref ?? "";
    if (ref !== "" && ref !== "main") {
      return json(res, 422, { error: `\`${ref}\` does not exist` });
    }
    const problem = checkInputs(asked.inputs ?? {});
    if (problem) {
      return json(res, 422, { error: problem });
    }
    const id = Math.max(...runs.map((candidate) => candidate.id)) + 1;
    runs.push(run(id, id, "ci", "workflow_dispatch", "refs/heads/main", "queued", null, Date.now(), null));
    return json(res, 202, { runs: [id] });
  }

  if ((found = /^runs\/(\d+)$/.exec(path))) {
    const wanted = runs.find((candidate) => candidate.id === Number(found?.[1]));
    if (!wanted) {
      return hidden();
    }
    return json(res, 200, { run: wanted, jobs: wanted.id === 1 ? jobs.map(jobJson) : [] });
  }

  if ((found = /^jobs\/(\d+)\/(log|log\/index)$/.exec(path))) {
    const job = jobs.find((candidate) => candidate.id === Number(found?.[1]));
    if (!job) {
      return hidden();
    }
    const finished = job.status === "completed";
    if (found[2] === "log/index") {
      return finished ? json(res, 200, indexOf(job.steps)) : json(res, 404, { error: "no index" });
    }
    const whole = finished ? text(job.steps) : backlog;
    res.statusCode = 200;
    res.setHeader("Content-Type", "text/plain; charset=utf-8");
    if (!finished) {
      res.setHeader("x-log-offset", "0");
      res.setHeader("x-log-next", String(Buffer.byteLength(whole)));
    }
    return void res.end(whole);
  }

  return hidden();
}

const GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

function frame(payload: string): Buffer {
  const data = Buffer.from(payload);
  const head =
    data.length < 126
      ? Buffer.from([0x81, data.length])
      : data.length < 65_536
        ? Buffer.from([0x81, 126, data.length >> 8, data.length & 0xff])
        : Buffer.concat([Buffer.from([0x81, 127]), Buffer.alloc(8)]);
  if (data.length >= 65_536) {
    head.writeBigUInt64BE(BigInt(data.length), 2);
  }
  return Buffer.concat([head, data]);
}

/** The live socket of /api/ci/jobs/:job/live, as the Worker answers it. */
export function serveLiveSockets(server: Server) {
  server.on("upgrade", (req: IncomingMessage, socket: Duplex) => {
    const found = /^\/api\/ci\/jobs\/(\d+)\/live$/.exec(new URL(req.url ?? "/", "http://localhost").pathname);
    if (!found) {
      return;
    }
    const refuse = (line: string) => socket.end(`HTTP/1.1 ${line}\r\nConnection: close\r\n\r\n`);
    const origin = req.headers.origin;
    if (!origin || new URL(origin).host !== req.headers.host) {
      return refuse("403 Forbidden");
    }
    const job = jobs.find((candidate) => candidate.id === Number(found[1]));
    const key = req.headers["sec-websocket-key"];
    if (!job || typeof key !== "string") {
      return refuse("404 Not Found");
    }
    if (job.status === "completed") {
      return refuse("409 Conflict");
    }
    const accept = createHash("sha1").update(key + GUID).digest("base64");
    socket.write(
      `HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${accept}\r\n\r\n`,
    );
    socket.write(frame(JSON.stringify({ type: "backlog", text: backlog })));
    const timers: NodeJS.Timeout[] = [];
    liveMessages.forEach((message, i) => {
      timers.push(setTimeout(() => socket.write(frame(JSON.stringify(message))), 800 * (i + 1)));
    });
    timers.push(
      setTimeout(() => {
        job.status = "completed";
        job.conclusion = "success";
        job.completed_at = Date.now();
        const live = runs.find((candidate) => candidate.id === 1);
        if (live) {
          live.status = "completed";
          live.conclusion = "failure";
          live.completed_at = job.completed_at;
        }
        socket.write(frame(JSON.stringify({ type: "end", conclusion: "success" })));
        socket.end(Buffer.from([0x88, 0x00]));
      }, 800 * (liveMessages.length + 1)),
    );
    socket.on("close", () => timers.forEach(clearTimeout));
    socket.on("error", () => timers.forEach(clearTimeout));
  });
}
