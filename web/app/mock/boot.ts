// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

// The boot scenario for the website captures, on with MOCK_SCENARIO=boot.
// One organization with ten runner slots and light traffic, and one job
// that boots on the clock measured on a t4g.medium:
//
//   0 s     Waiting    GitHub queued the job
//   6.2 s   Launching  hatchrun asked EC2 for an instance
//   18.2 s  Starting   the instance exists, the runner is registering
//   29.5 s  Running    GitHub handed the job to the runner
//
// GET /api/mock/boot starts the clock. `speed` runs it faster (2 plays
// the boot in about 15 s), `at` starts it that many boot seconds in, and
// `freeze=1` holds it there for stills. Before the first call the job
// is not in the list yet.

import type { Job } from "../src/api.ts";

export const PHASES = { launching: 6.2, starting: 18.2, running: 29.5 };

const clock = { startedAt: Infinity, at: 0, speed: 1, frozen: false };

/** Seconds into the boot, or a negative number before it starts. */
function elapsed(now: number): number {
  if (clock.startedAt === Infinity) {
    return -1;
  }
  return clock.frozen ? clock.at : clock.at + ((now - clock.startedAt) / 1000) * clock.speed;
}

export function start(params: URLSearchParams, now: number) {
  clock.speed = Number(params.get("speed")) || 1;
  clock.at = Number(params.get("at")) || 0;
  clock.frozen = params.get("freeze") === "1";
  clock.startedAt = now;
  return { ...clock, phases: PHASES };
}

const avatar =
  "data:image/svg+xml;utf8,<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 2 2'><rect width='2' height='2' fill='%23e4572e'/></svg>";

export const org = { id: 103, login: "acme", avatarUrl: avatar };

export function tenant() {
  const region = "eu-west-1";
  const templateUrl = "https://hatchrun-templates.s3.us-east-1.amazonaws.com/connect.yaml";
  return {
    id: "mock103tenant00",
    org,
    installationId: org.id * 10,
    aws: {
      roleArn: "arn:aws:iam::123456789012:role/hatchrun-HatchrunRole-1XK7Q2M9ZP4LD",
      region,
      launchTemplateId: "lt-07d3c1e94b2a85f60",
      subnetIds: ["subnet-0c4f19a2e7b3d5681", "subnet-0e82b6d40f1a9c375"],
      stack: {
        version: 3,
        latest: 3,
        checkedAt: Date.now() - 3_600_000,
        changes: [],
        familyList: false,
        consoleUrl: `https://${region}.console.aws.amazon.com/cloudformation/home?region=${region}#/stacks?filteringText=hatchrun`,
        templateUrl,
        updateCommand: null,
      },
    },
    maxRunners: 10,
    allowedRegions: null,
    beta: true,
    subscription: null,
  };
}

const minute = 60_000;

const instance = (id: number) =>
  `i-0${((BigInt(id) * 2654435761n) % 2n ** 64n).toString(16).padStart(16, "0").slice(-16)}`;

/** The other jobs age from when the server started, so their clocks tick. */
const epoch = Date.now();

function job(id: number, runId: number, repository: string, label: string, fields: Partial<Job>): Job {
  return {
    id,
    repository,
    runId,
    label,
    status: "completed",
    launchState: "launched",
    runnerName: `hatchrun-${runId}-${(id % 0xffffff).toString(16)}`,
    instanceId: instance(id),
    launchError: null,
    launchAttempts: 1,
    launchRetry: null,
    nextLaunchAt: null,
    queuedAt: 0,
    updatedAt: 0,
    ...fields,
  };
}

/** The job that boots, as it stands `seconds` into the boot. */
function booting(seconds: number, now: number): Job {
  const id = 52914738201;
  const phase =
    seconds >= PHASES.running
      ? { status: "in_progress" as const, launchState: "launched" as const }
      : seconds >= PHASES.starting
        ? { status: "queued" as const, launchState: "launched" as const }
        : seconds >= PHASES.launching
          ? { status: "queued" as const, launchState: "launching" as const }
          : { status: "queued" as const, launchState: null };
  return job(id, 18630412977, "acme/api", "hatchrun-4cpu-arm64", {
    ...phase,
    runnerName: phase.launchState ? `hatchrun-18630412977-${(id % 0xffffff).toString(16)}` : null,
    instanceId: phase.launchState === "launched" ? instance(id) : null,
    launchAttempts: phase.launchState ? 1 : 0,
    // Back-dated so the browser's clock reads the boot's seconds, at any speed.
    queuedAt: now - seconds * 1000,
    updatedAt: now,
  });
}

/** The job left running, whose page has the charts. */
export const RUNNING_ID = 52914102664;

/** Finished jobs: repository, label, minutes ago it was queued, seconds it took. */
const FINISHED: [string, string, number, number][] = [
  ["acme/web", "hatchrun-2cpu", 4, 108],
  ["acme/api", "hatchrun-4cpu-arm64", 17, 252],
  ["acme/docs", "hatchrun", 26, 52],
  ["acme/mobile", "hatchrun-8cpu-arm64", 38, 571],
  ["acme/web", "hatchrun-2cpu", 44, 121],
  ["acme/infra", "hatchrun-2cpu-arm64", 63, 87],
  ["acme/api", "hatchrun-4cpu-arm64", 71, 246],
  ["acme/data", "hatchrun-16cpu", 96, 838],
  ["acme/web", "hatchrun-2cpu", 118, 99],
  ["acme/api", "hatchrun-4cpu-arm64", 142, 263],
  ["acme/mobile", "hatchrun-8cpu-arm64", 171, 604],
  ["acme/docs", "hatchrun", 205, 49],
];

export function jobs(now: number): Job[] {
  const seconds = elapsed(now);
  const older = Array.from({ length: 36 }, (_, i): [string, string, number, number] => {
    const [repository = "acme/api", label = "hatchrun", , took = 120] = FINISHED[i % FINISHED.length] ?? [];
    return [repository, label, 240 + i * 38, took + ((i * 37) % 60)];
  });
  return [
    ...(seconds >= 0 ? [booting(seconds, now)] : []),
    job(RUNNING_ID, 18630377410, "acme/api", "hatchrun-8cpu-arm64", {
      status: "in_progress",
      queuedAt: epoch - 11.3 * minute,
      updatedAt: epoch - 11 * minute,
    }),
    ...[...FINISHED, ...older].map(([repository, label, ago, took], i) =>
      job(52913987345 - i * 48_211 + ((i * 7919) % 997), 18630297412 - i * 9_377 + ((i * 104_729) % 89), repository, label, {
        queuedAt: epoch - ago * minute,
        updatedAt: epoch - ago * minute + took * 1000,
      }),
    ),
  ];
}

/**
 * A day of 15 minute buckets with the shape of a small team: this
 * afternoon busy, the night quiet, yesterday afternoon at the left edge.
 * Counted back from now so the strip looks the same whenever it is shot.
 */
export function activity(now: number, day: Job[]) {
  const bucketMs = 900_000;
  const since = (Math.floor(now / bucketMs) + 1 - 96) * bucketMs;
  let seed = 7;
  const random = () => {
    seed = (seed * 1103515245 + 12345) % 2147483648;
    return seed / 2147483648;
  };
  const finished = Array.from({ length: 96 }, (_, i) => {
    const hoursAgo = (95 - i) / 4;
    const busy = hoursAgo < 7 || hoursAgo > 19 ? 1 : hoursAgo < 9 || hoursAgo > 17 ? 0.35 : 0.05;
    return Math.round(random() * 3.6 * busy);
  });
  for (const job of day) {
    const i = Math.floor((job.updatedAt - since) / bucketMs);
    if (job.status === "completed" && i >= 0 && i < 96) {
      finished[i] = (finished[i] ?? 0) + 1;
    }
  }
  return { since, bucketMs, finished, failed: finished.map(() => 0) };
}

/** Eleven minutes of a Rust build on 8 vCPU: fetch, a long compile, then tests. */
export function windows(job: Job, now: number) {
  const gib = 1024 ** 3;
  const start = Math.round(job.queuedAt / 1000) + 35;
  return Array.from({ length: 66 }, (_, i) => {
    const wobble = 6 * Math.sin(i * 1.3) + 4 * Math.sin(i * 0.37);
    const cpu = i < 5 ? 22 + wobble : i < 44 ? 84 + wobble : 41 + 1.6 * wobble;
    const mem = i < 5 ? 1.4 + i * 0.3 : i < 44 ? 3 + (i - 5) * 0.21 + 0.4 * Math.sin(i) : 6.2 + 0.6 * Math.sin(i / 2);
    return {
      at: start + (i + 1) * 10,
      seconds: 10,
      cpuMin: Math.max(2, cpu - 18 - (i % 3) * 4),
      cpuAvg: cpu,
      cpuMax: Math.min(100, cpu + 9 + (i % 4) * 2),
      cpuSteal: 0.2,
      memMax: mem * gib,
      memTotal: 32 * gib,
      cgroupPeak: 11.6 * gib,
      oomKills: 0,
      diskUsed: (14 + i * 0.06) * gib,
      netRx: i < 5 ? 160e6 : 1.5e6,
      netTx: 0.8e6,
    };
  }).filter((w) => w.at * 1000 <= now);
}
