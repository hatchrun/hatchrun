// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

// Records the website captures of the Jobs and job pages against the
// boot scenario in boot.ts: `npm run capture` from web/app. It starts
// the mock server, drives headless Chromium at 1440x900 and device
// scale 2, and writes stills and the boot clip to captures/ at the
// repository root. `-- --speed 2` records the clip at double speed as
// jobs-boot-2x.*, and `-- --width 1440` scales it down. Needs ffmpeg on PATH (or FFMPEG) and Chromium from
// `npx playwright-core install chromium-headless-shell`.

import { spawn, spawnSync } from "node:child_process";
import { mkdirSync, rmSync, statSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { parseArgs } from "node:util";

import { chromium, type Page } from "playwright-core";

import { PHASES, RUNNING_ID } from "./boot.ts";

const { values: args } = parseArgs({
  options: {
    speed: { type: "string", default: "1" },
    port: { type: "string", default: "5199" },
    width: { type: "string", default: "2880" },
    out: { type: "string", default: join(import.meta.dirname, "../../../captures") },
  },
});
const speed = Number(args.speed);
const width = Number(args.width);
const out = args.out;
const base = `http://127.0.0.1:${args.port}`;
const ffmpeg = process.env.FFMPEG ?? "ffmpeg";

/** Seconds of the list before the job shows up, and of Running at the end. */
const PRE = 1.5;
const HOLD = 3;
/** Seconds the end of the clip fades into its start, so it loops. */
const SEAM = 0.6;
/** How often the page polls during capture, instead of every 10 s. */
const POLL_MS = 250;

const server = spawn(join(import.meta.dirname, "../node_modules/.bin/vite"), ["--mode", "mock", "--port", args.port, "--strictPort", "--host", "127.0.0.1"], {
  cwd: join(import.meta.dirname, ".."),
  env: { ...process.env, MOCK_SCENARIO: "boot" },
  stdio: ["ignore", "ignore", "inherit"],
});
process.on("exit", () => server.kill());

const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

async function clock(query: string) {
  const res = await fetch(`${base}/api/mock/boot?${query}`);
  if (!res.ok) {
    throw new Error(`mock clock: ${res.status} ${await res.text()}`);
  }
}

async function ready() {
  for (let i = 0; i < 100; i++) {
    try {
      if ((await fetch(`${base}/api/session`)).ok) {
        return;
      }
    } catch {
      // not listening yet
    }
    await sleep(200);
  }
  throw new Error("the mock server did not start");
}

/** Waits until the newest row says `text`, then for the page to settle. */
async function rowSays(page: Page, text: string) {
  await page.locator("tbody tr").first().getByText(text).waitFor();
  await sleep(600);
}

function run(command: string, argv: string[]) {
  const result = spawnSync(command, argv, { stdio: ["ignore", "ignore", "inherit"] });
  if (result.status !== 0) {
    throw new Error(`${command} ${argv.join(" ")} failed`);
  }
}

await ready();
mkdirSync(join(out, "boot-frames"), { recursive: true });
const browser = await chromium.launch();
const context = await browser.newContext({
  viewport: { width: 1440, height: 900 },
  deviceScaleFactor: 2,
  colorScheme: "dark",
});
// The Jobs page polls every 10 s. Poll often enough that each phase
// shows when it happens.
await context.addInitScript(`{
  const every = window.setInterval.bind(window);
  window.setInterval = (fn, ms, ...rest) => every(fn, ms === 10000 ? ${POLL_MS} : ms, ...rest);
}`);
const page = await context.newPage();
const jobs = `${base}/o/acme`;

if (speed === 1) {
  const frames: [string, number, string][] = [
    ["1-waiting", 0, "Waiting"],
    ["2-launching", PHASES.launching, "Launching"],
    ["3-starting", PHASES.starting, "Starting"],
    ["4-running", PHASES.running, "Running"],
  ];
  for (const [name, at, text] of frames) {
    await clock(`at=${at}&freeze=1`);
    await page.goto(jobs);
    await rowSays(page, text);
    await page.screenshot({ path: join(out, "boot-frames", `${name}.png`) });
  }

  await clock("at=10&freeze=1");
  await page.goto(jobs);
  await rowSays(page, "Launching");
  await page.screenshot({ path: join(out, "jobs.png") });

  await page.goto(`${base}/o/acme/jobs/${RUNNING_ID}`);
  await page.getByRole("region", { name: "Memory" }).locator("svg").first().waitFor();
  await sleep(600);
  await page.screenshot({ path: join(out, "job-running.png") });
}

// The clip: the list without the job, the boot, then Running for a while.
const stem = `jobs-boot${speed === 1 ? "" : `-${speed}x`}${width === 2880 ? "" : `-${width}`}`;
const raw = join(out, `.${stem}-frames`);
rmSync(raw, { recursive: true, force: true });
mkdirSync(raw);
await clock("at=-1&freeze=1");
await page.goto(jobs);
await page.locator("tbody tr").first().waitFor();
await sleep(1500);

const cdp = await context.newCDPSession(page);
const shots: { file: string; at: number }[] = [];
cdp.on("Page.screencastFrame", ({ data, sessionId }) => {
  const file = join(raw, `${String(shots.length).padStart(5, "0")}.png`);
  shots.push({ file, at: performance.now() });
  writeFileSync(file, Buffer.from(data, "base64"));
  void cdp.send("Page.screencastFrameAck", { sessionId });
});
await cdp.send("Page.startScreencast", { format: "png", maxWidth: 2880, maxHeight: 1800 });
const began = performance.now();
await sleep(PRE * 1000);
await clock(`speed=${speed}`);
await sleep((PHASES.running / speed + HOLD) * 1000);
await cdp.send("Page.stopScreencast");
const ended = performance.now();
await browser.close();
server.kill();

// Each frame lasts until the next one; the first starts the clip.
const list = shots
  .map((shot, i) => {
    const from = i === 0 ? began : shot.at;
    const to = shots[i + 1]?.at ?? ended;
    return `file '${shot.file}'\nduration ${((to - from) / 1000).toFixed(3)}\n`;
  })
  .join("");
writeFileSync(join(raw, "list.txt"), `${list}file '${shots[shots.length - 1]?.file}'\n`);
const length = (ended - began) / 1000;

const master = join(raw, "master.mp4");
run(ffmpeg, ["-y", "-loglevel", "error", "-f", "concat", "-safe", "0", "-i", join(raw, "list.txt"),
  "-vf", `fps=30,scale=${width}:-2:flags=lanczos,format=yuv420p`, "-c:v", "libx264", "-crf", "10", master]);
const loop = `[0]trim=start=${SEAM},setpts=PTS-STARTPTS[a];[0]trim=end=${SEAM},setpts=PTS-STARTPTS[b];` +
  `[a][b]xfade=transition=fade:duration=${SEAM}:offset=${(length - 2 * SEAM).toFixed(3)}`;
run(ffmpeg, ["-y", "-loglevel", "error", "-i", master, "-filter_complex", `${loop},format=yuv420p`, "-an",
  "-c:v", "libx264", "-preset", "slow", "-crf", "24", "-tune", "stillimage", "-movflags", "+faststart",
  join(out, `${stem}.mp4`)]);
run(ffmpeg, ["-y", "-loglevel", "error", "-i", master, "-filter_complex", `${loop},format=yuv420p`, "-an",
  "-c:v", "libvpx-vp9", "-crf", "36", "-b:v", "0", "-row-mt", "1", join(out, `${stem}.webm`)]);
run(ffmpeg, ["-y", "-loglevel", "error", "-i", master, "-filter_complex",
  `${loop},fps=8,scale=720:-1:flags=lanczos,split[x][y];[x]palettegen=max_colors=64[p];[y][p]paletteuse=dither=none`,
  "-loop", "0", join(out, `${stem}.gif`)]);
rmSync(raw, { recursive: true, force: true });

for (const file of [`${stem}.mp4`, `${stem}.webm`, `${stem}.gif`]) {
  console.log(`${file}  ${(statSync(join(out, file)).size / 1e6).toFixed(2)} MB`);
}
console.log(`${shots.length} frames over ${length.toFixed(1)} s, boot at ${speed}x`);
process.exit(0);
