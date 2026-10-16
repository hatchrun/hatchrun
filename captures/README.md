# Website captures

Real captures of the dashboard for the website hero and boot section.
Only this README is in git; the media is rebuilt with one command.

## Re-run

```bash
cd web/app && npm run capture && npm run capture -- --speed 2
```

The first run writes the stills and the real-time clip, the second the
2x clip. Each takes about a minute. Needs `ffmpeg` on `PATH` (or set
`FFMPEG`) and Chromium for Playwright, once:

```bash
cd web/app && npx playwright-core install chromium-headless-shell
```

`web/app/mock/capture.ts` starts `vite --mode mock` with
`MOCK_SCENARIO=boot`, drives headless Chromium at 1440x900, device
scale 2, dark scheme, and no cursor. It makes the Jobs page poll every
250 ms instead of every 10 s, so each phase shows on the frame it
happens. The scenario lives in `web/app/mock/boot.ts`; the mock server
is a Vite dev plugin and never reaches the production bundle.

Options: `--speed 2` plays the boot at double speed, `--width 1440`
scales the clip down, `--out <dir>` writes somewhere else.

## Files

| File | What |
| --- | --- |
| `jobs-boot.mp4`, `jobs-boot.webm` | Jobs page, 2880x1800, 34 s, the job goes Waiting, Launching, Starting, Running. Muted, the last 0.6 s fades into the first so it loops. |
| `jobs-boot.gif` | The same at 720 px wide and 8 fps, as a fallback. |
| `jobs-boot-2x.*` | The same with the boot at double speed, 19 s. |
| `jobs.png` | Jobs page mid-traffic: one launching, one running, the rest done. |
| `job-running.png` | Job page of the running 8 vCPU build, with CPU and Memory charts. |
| `boot-frames/1-waiting.png` ... `4-running.png` | The Jobs page at each boot marker. |

## Mock timings

The boot job is `acme/api` on `hatchrun-4cpu-arm64`, measured on a
t4g.medium:

| Boot second | Status | What changes on screen |
| --- | --- | --- |
| 0 | Waiting | Row appears, queue slot fills |
| 6.2 | Launching | A runner slot turns to booting |
| 18.2 | Starting | Instance id appears |
| 29.5 | Running | Slot turns solid, Took reads 29 s and counts up |

The clip shows 1.5 s of the list before the job appears and holds
Running for 3 s. At `--speed 2` the boot takes 14.75 s.

The organization is `acme` with 10 runner slots, connected to AWS
account `123456789012` in `eu-west-1`. One other job (`acme/api` on
`hatchrun-8cpu-arm64`) has been running for 11 minutes, and finished
jobs go back two days. The 24-hour strip shows a busy afternoon, a
quiet night and yesterday's afternoon at the left edge.

Drive the clock by hand with `npm run dev:mock` under
`MOCK_SCENARIO=boot` and `GET /api/mock/boot?speed=2`,
`?at=18.2&freeze=1` for a still, or `?at=-1&freeze=1` to hide the job.
