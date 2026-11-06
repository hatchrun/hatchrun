// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { ChevronRight } from "lucide-react";
import { useMemo, useState } from "react";

import type { CiJob, CiRun } from "@/api";
import { JobSquares } from "@/components/JobSquares";
import { StatusGlyph, ciStatus, type Status } from "@/components/StatusPill";
import { abbreviate, duration } from "@/lib/format";
import { cn } from "@/lib/utils";
import { jobUrl } from "@/lib/urls";
import { Link } from "@/router";
import { BTN_SM, FIELD } from "./runUi";

/** "test (linux, node 20)" is job "test (linux, node 20)" of the group "test". */
const MATRIX = /^(.+?) \(.*\)$/;

interface Entry {
  job: CiJob;
  status: Status;
  /** How many `needs` links lead back to a job that needs nothing, at most 2 (the design stops indenting there). */
  depth: number;
}

interface Group {
  name: string;
  depth: number;
  entries: Entry[];
  failed: number;
  passed: number;
  rank: number;
}

type Unit = { kind: "job"; entry: Entry; rank: number } | ({ kind: "group" } & Group);

/** Failures sort first, then what needs a person or runs, then the rest. */
const RANK: Record<Status, number> = { failed: 0, waiting: 1, running: 1, queued: 2, passed: 2, cancelled: 2, skipped: 2 };

const MAX_DEPTH = 2;

/** Longest chain of `needs` below each plan id, read once per run. A cycle or an unknown id counts as no needs. */
function depths(jobs: CiJob[]): Map<string, number> {
  const needs = new Map(jobs.map((job) => [job.plan_id, job.needs]));
  const memo = new Map<string, number>();
  const walk = (id: string, seen: Set<string>): number => {
    const known = memo.get(id);
    if (known !== undefined) return known;
    if (seen.has(id)) return 0;
    seen.add(id);
    const below = (needs.get(id) ?? []).filter((need) => needs.has(need)).map((need) => walk(need, seen));
    const depth = below.length === 0 ? 0 : 1 + Math.max(...below);
    memo.set(id, depth);
    return depth;
  };
  for (const id of needs.keys()) walk(id, new Set());
  return memo;
}

/** The line under a job's name: what it waits on, then what it is doing. */
function note(job: CiJob, status: Status): string {
  const needs = job.needs.length === 0 ? "no needs" : job.needs.length === 1 ? `needs ${job.needs[0]}` : `needs ${job.needs.length} jobs`;
  if (job.held === "approval") {
    return `${needs} · ${job.environment ? `waits for approval of ${job.environment}` : "waits for approval"}`;
  }
  if (job.started_at !== null) {
    return needs;
  }
  return `${needs} · ${status === "skipped" ? "skipped" : "not started"}`;
}

const needsTitle = (job: CiJob) => (job.needs.length === 0 ? "no needs" : `needs ${job.needs.join(", ")}`);

/** Left padding in px: 12 plus 14 for each level, and 16 more inside a matrix group. */
const indent = (depth: number, nested: boolean) => 12 + depth * 14 + (nested ? 16 : 0);

export function JobsPanel({ run, jobs, selected, now }: { run: CiRun; jobs: CiJob[]; selected: number | null; now: number }) {
  const [q, setQ] = useState("");
  const [failedFirst, setFailedFirst] = useState(true);
  const [expanded, setExpanded] = useState<Record<string, boolean>>({});

  const units = useMemo(() => {
    const depth = depths(jobs);
    const needle = q.trim().toLowerCase();
    const entries = jobs
      .filter((job) => needle === "" || job.name.toLowerCase().includes(needle))
      .map((job): Entry => ({ job, status: ciStatus(job.status, job.conclusion, job.held), depth: Math.min(MAX_DEPTH, depth.get(job.plan_id) ?? 0) }));
    const bases = new Map<string, Entry[]>();
    for (const entry of entries) {
      const base = MATRIX.exec(entry.job.name)?.[1];
      if (base !== undefined) {
        bases.set(base, [...(bases.get(base) ?? []), entry]);
      }
    }
    const out: Unit[] = [];
    const placed = new Set<string>();
    for (const entry of entries) {
      const base = MATRIX.exec(entry.job.name)?.[1];
      const members = base === undefined ? undefined : bases.get(base);
      if (base !== undefined && members !== undefined && members.length > 1) {
        if (!placed.has(base)) {
          placed.add(base);
          const sorted = failedFirst ? members.slice().sort((a, b) => RANK[a.status] - RANK[b.status]) : members;
          out.push({
            kind: "group",
            name: base,
            depth: Math.min(...members.map((m) => m.depth)),
            entries: sorted,
            failed: members.filter((m) => m.status === "failed").length,
            passed: members.filter((m) => m.status === "passed").length,
            rank: Math.min(...members.map((m) => RANK[m.status])),
          });
        }
      } else {
        out.push({ kind: "job", entry, rank: RANK[entry.status] });
      }
    }
    return failedFirst ? out.slice().sort((a, b) => a.rank - b.rank) : out;
  }, [jobs, q, failedFirst]);

  const isOpen = (group: Group) =>
    expanded[group.name] ?? (group.failed > 0 || q.trim() !== "" || group.entries.some((e) => e.job.id === selected));

  return (
    <section aria-label="Jobs in this run" className="flex min-h-0 w-[320px] shrink-0 flex-col overflow-hidden rounded-md border border-rule bg-panel max-lg:w-full max-lg:max-h-[420px]">
      <div className="flex items-baseline justify-between gap-2 border-b border-rule px-3 pt-2.5 pb-2">
        <span className="cap">Jobs</span>
        <span className="font-mono text-[13px] text-fg-2">{abbreviate(jobs.length)} in this run</span>
      </div>
      <div className="border-b border-rule px-3 py-2">
        <JobSquares jobs={run.jobs} />
      </div>
      <div className="flex gap-2 border-b border-rule p-2">
        <input
          type="search"
          aria-label="Find job"
          placeholder="Find job"
          value={q}
          onChange={(event) => setQ(event.target.value)}
          className={cn(FIELD, "min-w-0 flex-1")}
        />
        <button type="button" className={BTN_SM} aria-pressed={failedFirst} title="Show failed jobs first" onClick={() => setFailedFirst(!failedFirst)}>
          Failed first
        </button>
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto py-1">
        {units.map((unit) =>
          unit.kind === "job" ? (
            <JobRow key={unit.entry.job.id} entry={unit.entry} run={run} selected={selected} now={now} />
          ) : (
            <div key={`g:${unit.name}`}>
              <button
                type="button"
                aria-expanded={isOpen(unit)}
                aria-label={`${unit.name}, ${unit.entries.length} jobs, ${unit.passed} passed, ${unit.failed} failed`}
                title={unit.name}
                onClick={() => setExpanded({ ...expanded, [unit.name]: !isOpen(unit) })}
                style={{ paddingLeft: indent(unit.depth, false) }}
                className="flex w-full cursor-pointer items-center gap-2 pr-3 py-1.5 text-left hover:bg-raised"
              >
                <ChevronRight size={12} aria-hidden="true" className={cn("shrink-0 text-fg-3 transition-transform", isOpen(unit) && "rotate-90")} />
                <span className="min-w-0 flex-1 truncate font-mono text-[13px] font-bold">{unit.name}</span>
                <span className="font-mono text-[13px] text-fg-3">{unit.entries.length}</span>
                <span className="font-mono text-[13px] text-passed">{unit.passed}</span>
                {unit.failed > 0 && <span className="font-mono text-[13px] text-failed">{unit.failed}✕</span>}
              </button>
              {isOpen(unit) &&
                unit.entries.map((entry) => <JobRow key={entry.job.id} entry={entry} run={run} selected={selected} now={now} nested label={entry.job.name.slice(unit.name.length + 1)} />)}
            </div>
          ),
        )}
        {units.length === 0 && <p className="px-3 py-3 font-mono text-[13px] text-fg-3">No job matches that name.</p>}
      </div>
      <div className="border-t border-rule px-3 py-1.5 font-mono text-[13px] text-fg-3">billed per second</div>
    </section>
  );
}

function JobRow({
  entry,
  run,
  selected,
  now,
  nested = false,
  label,
}: {
  entry: Entry;
  run: CiRun;
  selected: number | null;
  now: number;
  nested?: boolean;
  label?: string;
}) {
  const { job, status } = entry;
  const level = nested ? 0 : entry.depth;
  const current = job.id === selected;
  return (
    <Link
      href={jobUrl(run.repository, run.id, job.id)}
      aria-current={current ? "true" : undefined}
      title={`${job.name}, ${needsTitle(job)}`}
      style={{ paddingLeft: indent(level, nested) }}
      className={cn("group flex items-center gap-2 border-l-2 py-1.5 pr-3 hover:bg-raised", current ? "border-l-brand bg-raised" : "border-l-transparent")}
    >
      {level > 0 && (
        <span aria-hidden="true" className="-mr-1 font-mono text-[13px] text-fg-3">
          └
        </span>
      )}
      <StatusGlyph status={status} />
      <span className="flex min-w-0 flex-1 flex-col">
        <span className="truncate font-mono text-[13px] font-bold group-hover:text-brand">{label ?? job.name}</span>
        <span title={needsTitle(job)} className={cn("truncate font-mono text-xs", status === "waiting" ? "text-waiting" : "text-fg-3")}>
          {note(job, status)}
        </span>
      </span>
      {job.started_at !== null && <span className="font-mono text-xs text-fg-3">{duration((job.completed_at ?? now) - job.started_at)}</span>}
    </Link>
  );
}
