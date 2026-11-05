// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { ChevronRight } from "lucide-react";
import { useState } from "react";

import { approveCiJob, getNeedsYou, type NeedsYou as NeedsYouData } from "@/api";
import { Button } from "@/components/ui/button";
import { LoadError } from "@/components/Status";
import { ago } from "@/lib/format";
import { jobUrl, runUrl } from "@/lib/urls";
import { useLoad, useNow } from "@/load";
import { Link } from "@/router";

const ROW = "flex flex-wrap items-center gap-x-3.5 gap-y-2 border-t border-rule px-3.5 py-[9px]";

function Row({ edge, glyph, head, rest, children }: { edge: string; glyph: string; head: string; rest: string; children: React.ReactNode }) {
  return (
    <li className={ROW} style={{ boxShadow: `inset 3px 0 0 var(${edge})` }}>
      <span aria-hidden="true" className="w-3.5 text-center font-mono font-semibold" style={{ color: `var(${edge})` }}>
        {glyph}
      </span>
      <span className="min-w-0 flex-[1_1_420px] font-mono text-[13px] text-fg-2">
        <span className="font-semibold text-fg">{head}</span> · {rest}
      </span>
      <span className="flex items-center gap-2">{children}</span>
    </li>
  );
}

/**
 * Approvals the viewer can give and default branches that fail, each
 * with one action. Renders nothing when there is nothing to do (and
 * while the first read is going). `onChange` runs after an approval so
 * the run list refreshes.
 */
export function NeedsYou({ org, onChange }: { org: string; onChange: () => void }) {
  const [loaded, reload] = useLoad<NeedsYouData>(() => getNeedsYou(org), org, true);
  const [busy, setBusy] = useState<number | null>(null);
  const [error, setError] = useState<Error | null>(null);
  const now = useNow(30_000);

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

  return (
    <section aria-label="Needs you" className="rounded-md border border-rule bg-panel">
      <div className="flex items-baseline gap-2.5 px-3.5 py-[9px]">
        <h2 className="section-title">Needs you</h2>
        <span className="font-mono text-[13px] text-fg-3">{count} open</span>
      </div>
      {error && (
        <p className="border-t border-rule px-3.5 py-2 font-mono text-[13px] text-failed" role="alert">
          {error.message}
        </p>
      )}
      <ul>
        {approvals.map(({ run, job, since }) => (
          <Row
            key={`a${job.id}`}
            edge="--waiting"
            glyph="!"
            head={job.environment ? "deploy waiting for approval" : "job waiting for approval"}
            rest={[`${run.repository} #${run.run_number}`, job.environment ?? job.name, run.actor, ago(since, now)].join(" · ")}
          >
            <Button asChild variant="outline">
              <Link href={jobUrl(run.repository, run.id, job.id)} aria-label={`Review approval for ${run.repository} ${run.run_number}`}>
                Review
              </Link>
            </Button>
            <Button variant="outline" disabled={busy === job.id} onClick={() => approve(job.id)} aria-label={`Approve ${run.repository} ${run.run_number}`}>
              {busy === job.id ? "Approving" : "Approve"}
            </Button>
          </Row>
        ))}
        {failing.map(({ repository, branch, run }) => (
          <Row key={`f${repository}`} edge="--failed" glyph="✕" head={`${branch} failing`} rest={`${repository} #${run.run_number} · ${run.workflow} · ${ago(run.completed_at ?? run.created_at, now)}`}>
            <Button asChild variant="outline">
              <Link href={runUrl(repository, run.id)} aria-label={`Open run ${repository} ${run.run_number}`}>
                Open run
                <ChevronRight aria-hidden="true" />
              </Link>
            </Button>
          </Row>
        ))}
      </ul>
    </section>
  );
}
