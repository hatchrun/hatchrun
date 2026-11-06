// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { ChevronDown } from "lucide-react";
import { useEffect, useState } from "react";

import { approveCiJob, getNeedsYou, type NeedsYou as NeedsYouData } from "@/api";
import { Button } from "@/components/ui/button";
import { LoadError } from "@/components/Status";
import { ago } from "@/lib/format";
import { jobUrl, runUrl } from "@/lib/urls";
import { cn } from "@/lib/utils";
import { useLoad, useNow } from "@/load";
import { Link } from "@/router";

const ROW = "flex items-center gap-3 border-t border-rule px-3.5 py-1.5";
/** Items shown before "N more". */
const SHOWN = 3;

function Row({ edge, glyph, href, label, head, rest, children }: { edge: string; glyph: string; href: string; label: string; head: string; rest: React.ReactNode; children?: React.ReactNode }) {
  return (
    <li className={ROW} style={{ boxShadow: `inset 3px 0 0 var(${edge})` }}>
      <span aria-hidden="true" className="w-3.5 shrink-0 text-center font-mono font-semibold" style={{ color: `var(${edge})` }}>
        {glyph}
      </span>
      <Link href={href} aria-label={label} className="min-w-0 flex-1 truncate font-mono text-[13px] text-fg-2 hover:text-fg hover:underline">
        <span className="font-semibold text-fg">{head}</span> · {rest}
      </Link>
      <span className="flex shrink-0 items-center gap-1.5">{children}</span>
    </li>
  );
}

/**
 * Approvals the viewer can give and default branches that fail, one
 * line each with one action: the first three, then "N more". The strip
 * collapses to its header. Renders nothing when there is nothing to do
 * (and while the first read is going). `onChange` runs after an
 * approval so the run list refreshes.
 */
export function NeedsYou({ org, onChange }: { org: string; onChange: () => void }) {
  const [loaded, reload] = useLoad<NeedsYouData>(() => getNeedsYou(org), org, true);
  const [busy, setBusy] = useState<number | null>(null);
  const [error, setError] = useState<Error | null>(null);
  const [collapsed, setCollapsed] = useState(false);
  const [all, setAll] = useState(false);
  const now = useNow(30_000);

  // The run list polls; the approvals and failing branches read again on the slow cadence.
  useEffect(() => {
    const timer = window.setInterval(reload, 30_000);
    return () => window.clearInterval(timer);
  }, [reload]);

  if (loaded.state === "error") {
    return <LoadError error={loaded.error} retry={reload} />;
  }
  if (loaded.state === "loading") {
    return null;
  }
  const { approvals, failing } = loaded.data;
  const count = approvals.length + failing.length;
  if (count === 0) {
    return null;
  }

  const approve = async (job: number) => {
    setBusy(job);
    setError(null);
    try {
      await approveCiJob(job);
      reload();
      onChange();
    } catch (err) {
      setError(err instanceof Error ? err : new Error(String(err)));
    } finally {
      setBusy(null);
    }
  };

  const rows = [
    ...approvals.map(({ run, job, since }) => (
      <Row
        key={`a${job.id}`}
        edge="--waiting"
        glyph="!"
        href={jobUrl(run.repository, run.id, job.id)}
        label={`Review approval for ${run.repository} ${run.run_number}`}
        head={job.environment ? "deploy waiting for approval" : "job waiting for approval"}
        rest={<>{run.repository} #{run.run_number} · {[job.environment ?? job.name, run.actor, ago(since, now)].join(" · ")}</>}
      >
        <Button variant="outline" size="sm" disabled={busy === job.id} onClick={() => approve(job.id)} aria-label={`Approve ${run.repository} ${run.run_number}`}>
          {busy === job.id ? "Approving" : "Approve"}
        </Button>
      </Row>
    )),
    ...failing.map(({ repository, branch, run }) => (
      <Row key={`f${repository}`} edge="--failed" glyph="✕" href={runUrl(repository, run.id)} label={`Open run ${repository} ${run.run_number}`} head={`${branch} failing`} rest={<>{repository} #{run.run_number} · {run.workflow} · {ago(run.completed_at ?? run.created_at, now)}</>} />
    )),
  ];

  return (
    <section aria-label="Needs you" className="rounded-md border border-rule bg-panel">
      <button type="button" aria-expanded={!collapsed} onClick={() => setCollapsed(!collapsed)} className="flex w-full cursor-pointer items-center gap-2 px-3.5 py-1.5 text-left">
        <ChevronDown size={14} aria-hidden="true" className={cn("text-fg-3 transition-transform", collapsed && "-rotate-90")} />
        <h2 className="section-title">Needs you</h2>
        <span className="font-mono text-[13px] text-fg-3">{count} open</span>
      </button>
      {error && (
        <p className="border-t border-rule px-3.5 py-2 font-mono text-[13px] text-failed" role="alert">
          {error.message}
        </p>
      )}
      {!collapsed && <ul>{all ? rows : rows.slice(0, SHOWN)}</ul>}
      {!collapsed && count > SHOWN && (
        <button type="button" onClick={() => setAll(!all)} className="w-full cursor-pointer border-t border-rule px-3.5 py-1.5 text-left font-mono text-[13px] text-fg-2 underline decoration-control underline-offset-3 hover:text-fg">
          {all ? "Show fewer" : `${count - SHOWN} more`}
        </button>
      )}
    </section>
  );
}
