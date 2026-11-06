// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { ChevronDown } from "lucide-react";
import { useEffect, useState } from "react";

import { getActivity, type Activity, type ActivityRange } from "@/api";
import { Await } from "@/components/Status";
import { abbreviate } from "@/lib/format";
import { cn } from "@/lib/utils";
import { useLoad } from "@/load";

const RANGES: { id: ActivityRange; label: string; title: string; unit: string; axis: string[] }[] = [
  { id: "24h", label: "24h", title: "Last 24 hours, per hour", unit: "hour", axis: ["-24 h", "-18 h", "-12 h", "-6 h", "now"] },
  { id: "7d", label: "7d", title: "Last 7 days, per 6 hours", unit: "6 hours", axis: ["-7 d", "-5 d", "-3.5 d", "-2 d", "now"] },
];

const CHART_HEIGHT = 56;

const OPEN_KEY = "hatchrun.activity-open";
const readOpen = () => {
  try {
    return window.localStorage.getItem(OPEN_KEY) === "1";
  } catch {
    return false;
  }
};

/**
 * A one-line summary of concurrency now (running, queued and free slots
 * of the org's limit) and the runs started over 24 hours or 7 days,
 * which opens into the full concurrency bar and chart. The repository
 * filter scopes both; the activity API has no branch, actor or text
 * filter, so the line says so when one is set. Bars scale to the
 * largest bucket of the range.
 */
export function ActivityPanel({ org, repos, otherFilters = false }: { org: string; repos: string[]; /** A branch, actor or text filter is set: the panel cannot apply it. */ otherFilters?: boolean }) {
  const [range, setRange] = useState<ActivityRange>("24h");
  const [open, setOpen] = useState(readOpen);
  const look = RANGES.find((item) => item.id === range) ?? (RANGES[0] as (typeof RANGES)[number]);
  const [activity, reload] = useLoad(() => getActivity(org, range, repos), `${org}|${range}|${repos.join(",")}`, true);
  useEffect(() => {
    const timer = window.setInterval(reload, 15_000);
    return () => window.clearInterval(timer);
  }, [reload]);

  const toggle = () => {
    const next = !open;
    setOpen(next);
    try {
      window.localStorage.setItem(OPEN_KEY, next ? "1" : "0");
    } catch {
      // Not remembered.
    }
  };
  const scope = repos.length === 0 ? "org-wide" : repos.length === 1 ? `repo ${repos[0]}` : `${repos.length} repos`;

  return (
    <section aria-label="Concurrency now and runs over time" className="rounded-md border border-rule bg-panel">
      <button type="button" aria-expanded={open} onClick={toggle} className="flex w-full cursor-pointer flex-wrap items-center gap-x-4 gap-y-1 px-3.5 py-1.5 text-left">
        <span className="flex items-center gap-2">
          <ChevronDown size={14} aria-hidden="true" className={cn("text-fg-3 transition-transform", !open && "-rotate-90")} />
          <h2 className="section-title">Activity</h2>
        </span>
        <span className="font-mono text-[13px] text-fg-3" title={repos.join(", ") || undefined}>
          {scope}
          {otherFilters && ", branch, actor and text filters not applied"}
        </span>
        {activity.state === "ready" && <Summary data={activity.data} range={range} />}
      </button>
      {open && (
        <div className="flex flex-wrap border-t border-rule">
          <Await loaded={activity} retry={reload}>
            {(data) => (
              <>
                <Concurrency concurrency={data.concurrency} />
                <Chart data={data} look={look} range={range} onRange={setRange} />
              </>
            )}
          </Await>
        </div>
      )}
    </section>
  );
}

function Summary({ data, range }: { data: Activity; range: ActivityRange }) {
  const { running, queued, limit } = data.concurrency;
  const sum = (key: "started" | "passed" | "failed") => data.buckets.reduce((total, bucket) => total + bucket[key], 0);
  const failed = sum("failed");
  return (
    <span className="font-mono text-[13px] text-fg-3">
      <span className="text-fg">{running}</span> / {limit} slots, {queued} queued <span className="text-control">|</span> {range}: <span className="text-fg">{abbreviate(sum("started"))}</span> started,{" "}
      <span className="text-passed">{abbreviate(sum("passed"))}</span> passed, <span className={cn(failed > 0 ? "text-failed" : "text-fg-3")}>{abbreviate(failed)}</span> failed
    </span>
  );
}

function Concurrency({ concurrency }: { concurrency: Activity["concurrency"] }) {
  const { running, queued, limit } = concurrency;
  const free = Math.max(0, limit - running);
  const parts = [
    { key: "running", label: "running", n: running, className: "bg-running" },
    { key: "free", label: "free", n: free, className: "border border-control bg-transparent" },
    { key: "queued", label: "queued", n: queued, className: "bg-waiting/60" },
  ];
  return (
    <div className="flex min-w-[320px] flex-[1_1_380px] flex-col gap-3 px-4.5 py-4">
      <div className="flex items-baseline justify-between gap-3">
        <h2 className="section-title">Concurrency</h2>
        <span className="font-mono text-[13px] text-fg-3">
          <span className="text-lg font-semibold text-fg">{running}</span> / {limit} slots
        </span>
      </div>
      <div role="img" aria-label={parts.map((part) => `${part.n} ${part.label}`).join(", ")} className="flex h-[22px] gap-0.5">
        {parts
          .filter((part) => part.n > 0)
          .map((part) => (
            <span key={part.key} title={`${part.n} ${part.label}`} style={{ flex: `${part.n} 1 0` }} className={cn("min-w-[3px] rounded-xs", part.className)} />
          ))}
        {running + queued + free === 0 && <span className="flex-1 rounded-xs border border-control" />}
      </div>
      <div className="flex flex-wrap gap-x-4.5 gap-y-1.5 font-mono text-[13px] text-fg-3">
        {parts.map((part) => (
          <span key={part.key} className="inline-flex items-center gap-1.5">
            <span aria-hidden="true" className={cn("size-2.5 rounded-[1px]", part.className)} />
            <span className="text-fg">{part.n}</span> {part.label}
          </span>
        ))}
      </div>
    </div>
  );
}

function Chart({ data, look, range, onRange }: { data: Activity; look: (typeof RANGES)[number]; range: ActivityRange; onRange: (range: ActivityRange) => void }) {
  const started = data.buckets.reduce((sum, bucket) => sum + bucket.started, 0);
  const passed = data.buckets.reduce((sum, bucket) => sum + bucket.passed, 0);
  const failed = data.buckets.reduce((sum, bucket) => sum + bucket.failed, 0);
  const peak = Math.max(1, ...data.buckets.map((bucket) => bucket.started));
  const px = (n: number) => Math.round((n / peak) * CHART_HEIGHT);
  const stamp = (at: number) =>
    new Date(at).toLocaleString("en-US", range === "24h" ? { hour: "2-digit", minute: "2-digit", hour12: false } : { weekday: "short", hour: "2-digit", minute: "2-digit", hour12: false });
  return (
    <div className="flex min-w-[420px] flex-[2_1_460px] flex-col gap-2.5 border-l border-rule px-4.5 py-4 max-lg:border-t max-lg:border-l-0">
      <div className="flex flex-wrap items-baseline justify-between gap-x-3 gap-y-1.5">
        <div className="flex items-center gap-3">
          <h2 className="section-title">{look.title}</h2>
          <div role="group" aria-label="Chart range" className="flex overflow-hidden rounded-sm border border-control font-mono text-xs">
            {RANGES.map((item) => (
              <button
                key={item.id}
                type="button"
                aria-pressed={item.id === range}
                onClick={() => onRange(item.id)}
                className={cn("cursor-pointer px-2.5 py-1", item.id === range ? "bg-fg text-background" : "text-fg-2 hover:text-fg")}
              >
                {item.label}
              </button>
            ))}
          </div>
        </div>
        <span className="font-mono text-[13px] text-fg-3">
          <span className="text-lg font-semibold text-fg">{abbreviate(started)}</span> started <span className="text-control">|</span> <span className="text-passed">{abbreviate(passed)}</span> passed{" "}
          <span className="text-control">|</span> <span className={cn("text-failed", failed > 0 && "text-lg font-semibold")}>{abbreviate(failed)}</span> failed
        </span>
      </div>
      <div className="relative">
        <span className="absolute top-[-2px] left-0 z-[1] bg-panel pr-1.5 font-mono text-xs text-fg-3">
          peak {abbreviate(peak === 1 && started === 0 ? 0 : peak)} / {look.unit}
        </span>
        <div aria-hidden="true" className="absolute top-1.5 right-0 left-0 border-t border-dashed border-rule" />
        <div
          role="img"
          aria-label={`Runs started per ${look.unit} over the ${range === "24h" ? "last 24 hours" : "last 7 days"}: ${started} started, ${passed} passed, ${failed} failed, peak ${peak} in one bucket`}
          style={{ height: CHART_HEIGHT + 6 }}
          className="relative flex items-end gap-0.5 border-b border-rule pt-1.5"
        >
          {data.buckets.map((bucket) => (
            <span key={bucket.at} title={`${stamp(bucket.at)}: ${bucket.started} started, ${bucket.passed} passed, ${bucket.failed} failed`} className="flex min-h-px flex-1 flex-col justify-end">
              <span className="bg-failed" style={{ height: px(bucket.failed) }} />
              <span className="min-h-px bg-passed/70" style={{ height: px(bucket.passed), minHeight: bucket.passed > 0 ? 1 : 0 }} />
              <span className="bg-fg-3/40" style={{ height: px(Math.max(0, bucket.started - bucket.passed - bucket.failed)) }} />
            </span>
          ))}
        </div>
      </div>
      <div className="flex justify-between font-mono text-xs text-fg-3">
        {look.axis.map((label) => (
          <span key={label}>{label}</span>
        ))}
      </div>
    </div>
  );
}
