// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { ExternalLink } from "lucide-react";
import { useEffect, useState } from "react";

import { getCiBilling, getOrgSetup, listOrgRepos, patchEnabledRepos, type CiOrgRepo, type OrgSetup } from "@/api";
import { CardButton, dollars, FIELD, SpendCapForm } from "@/components/BillingParts";
import { Checklist, ChecklistCount, type Step } from "@/components/Checklist";
import { CopyButton } from "@/components/CopyButton";
import { Page } from "@/components/Page";
import { SettingsNav } from "@/components/SettingsNav";
import { RepoMultiSelect, type RepoSelection } from "@/components/RepoMultiSelect";
import { Await } from "@/components/Status";
import { Button } from "@/components/ui/button";
import { orgUrl } from "@/lib/urls";
import { cn } from "@/lib/utils";
import { useLoad, type Loaded } from "@/load";
import { Link } from "@/router";
import type { OrgPageProps } from "./types";

const CONFIG = `jobs:
  build:
    runs-on: ubuntu-24.04
    cache: [~/.npm]
    steps:
      - run: npm ci && npm run build
  test:
    runs-on: ubuntu-24.04
    needs: build
    cache: [~/.npm]
    steps:
      - run: npm test
`;

const CONFIG_PATH = ".hatchrun/config.yml";
const POLL_MS = 10_000;

const count = (n: number) => n.toLocaleString("en-US");

/** First run checklist (CI only). The state is read again every 10 s until every step is done. */
export function Setup({ org, tenant }: OrgPageProps) {
  const [setup, reload] = useLoad(() => getOrgSetup(org), org, true);
  const complete = setup.state === "ready" && isComplete(setup.data);
  useEffect(() => {
    if (complete) return;
    const timer = window.setInterval(reload, POLL_MS);
    return () => window.clearInterval(timer);
  }, [complete, reload]);

  return (
    <Await loaded={setup} retry={reload}>
      {(data) => <SetupSteps org={org} admin={tenant.role === "admin"} setup={data} reload={reload} />}
    </Await>
  );
}

const isComplete = (setup: OrgSetup) => setup.app.installed && (setup.enabled.all || setup.enabled.count > 0) && setup.first_run !== null && setup.billing.card;

function SetupSteps({ org, admin, setup, reload }: { org: string; admin: boolean; setup: OrgSetup; reload: () => void }) {
  const [enabled, reloadEnabled] = useLoad(() => readEnabled(org), `${org}:${setup.enabled.all}:${setup.enabled.count}`);
  const enabledCount = setup.enabled.all ? setup.app.repos : setup.enabled.count;
  const steps: Step[] = [
    {
      key: "app",
      title: "Install GitHub App",
      done: setup.app.installed,
      note: setup.app.installed ? `hatchrun app on ${org}, ${count(setup.app.repos)} ${setup.app.repos === 1 ? "repository" : "repositories"}` : `Install the hatchrun app on ${org}.`,
      aside: setup.app.installed && (
        <a href={setup.app.manage_url} className="inline-flex items-center gap-1.5 whitespace-nowrap font-mono text-xs text-fg-2 hover:text-fg" aria-label="Change repositories on GitHub">
          Change repositories <ExternalLink size={11} aria-hidden="true" />
        </a>
      ),
    },
    {
      key: "enable",
      title: "Enable CI on repositories",
      done: setup.enabled.all || setup.enabled.count > 0,
      note:
        setup.enabled.all || setup.enabled.count > 0
          ? setup.enabled.all
            ? `CI is on for all ${count(setup.app.repos)} repositories.`
            : `CI is on for ${count(enabledCount)} of ${count(setup.app.repos)} repositories.`
          : "Pick the repositories that run jobs on hatchrun.",
      children: setup.app.installed && <EnableRepos key={org} org={org} setup={setup} enabled={enabled} reloadEnabled={reloadEnabled} onChanged={reload} />,
    },
    {
      key: "config",
      title: `Add ${CONFIG_PATH}`,
      done: setup.first_run !== null,
      note: setup.first_run
        ? `First run seen: ${setup.first_run.repository}.`
        : "Commit this file to each enabled repository. This ticks when the first run is seen.",
      children: <StarterConfig enabled={enabled} />,
    },
    {
      key: "billing",
      title: "Billing",
      done: setup.billing.card,
      note: setup.billing.card
        ? `Card on file, cap ${dollars(setup.billing.cap_dollars)} per month.`
        : "Add a card. Jobs start once a card is on file.",
      children: <BillingStep org={org} setup={setup} onChanged={reload} />,
    },
  ];

  return (
    <Page
      title={`Set up ${org}`}
      nav={<SettingsNav org={org} current="setup" admin={admin} />}
      meta={<ChecklistCount steps={steps} />}
      actions={
        <Link href={orgUrl(org)} className="font-mono text-xs text-fg-2 quiet-underline hover:text-fg">
          Skip to Runs →
        </Link>
      }
      narrow
    >
      {steps.every((step) => step.done) && (
        <section aria-label="Setup complete" className="flex flex-wrap items-center justify-between gap-3 rounded-md border border-passed bg-panel px-4 py-3">
          <div className="flex items-center gap-3">
            <span aria-hidden="true" className="font-mono font-bold text-passed">
              ✓
            </span>
            <div>
              <div className="text-[15px] font-bold">Setup complete</div>
              <div className="font-mono text-xs text-fg-3">
                CI is on for {setup.enabled.all ? `all ${count(setup.app.repos)}` : count(enabledCount)} {(setup.enabled.all ? setup.app.repos : enabledCount) === 1 ? "repository" : "repositories"}, a card is on file and a run has been seen.
              </div>
            </div>
          </div>
          <Link href={orgUrl(org)} className="font-mono text-xs text-fg quiet-underline hover:text-brand">
            Go to Runs →
          </Link>
        </section>
      )}
      <Checklist steps={steps} label="CI setup steps" />
    </Page>
  );
}

/** The first page of enabled repositories: enough to pick one for the starter file. */
async function readEnabled(org: string): Promise<CiOrgRepo[]> {
  return (await listOrgRepos(org, { enabled: true, limit: 50 })).items;
}

function EnableRepos({ org, setup, enabled, reloadEnabled, onChanged }: { org: string; setup: OrgSetup; enabled: Loaded<CiOrgRepo[]>; reloadEnabled: () => void; onChanged: () => void }) {
  // Kept here so the message survives the form remounting.
  const [result, setResult] = useState<SaveResult | null>(null);
  return (
    <EnableForm
      org={org}
      result={result}
      setResult={setResult}
      total={setup.app.repos}
      allOn={setup.enabled.all}
      enabledCount={setup.enabled.count}
      onChanged={() => {
        reloadEnabled();
        onChanged();
      }}
      failed={enabled.state === "error" ? reloadEnabled : null}
    />
  );
}

type SaveResult = { ok: boolean; text: string };

/**
 * Changes the enabled set with PATCH: the picked repositories are added
 * or removed, the rest are left as they are. The saved set is never read
 * in full, so the form opens at once on an org with thousands of repositories.
 */
function EnableForm({
  org,
  total,
  allOn,
  enabledCount,
  result,
  setResult,
  onChanged,
  failed,
}: {
  org: string;
  total: number;
  allOn: boolean;
  enabledCount: number;
  result: SaveResult | null;
  setResult: (result: SaveResult | null) => void;
  onChanged: () => void;
  failed: (() => void) | null;
}) {
  const [draft, setDraft] = useState<RepoSelection>({ all: false, repos: [] });
  const [busy, setBusy] = useState(false);
  const n = draft.all ? total : draft.repos.length;

  const change = async (body: { all?: boolean; add?: string[]; remove?: string[] }, text: string) => {
    setBusy(true);
    setResult(null);
    try {
      await patchEnabledRepos(org, body);
      setResult({ ok: true, text });
      setDraft({ all: false, repos: [] });
      onChanged();
    } catch (err) {
      setResult({ ok: false, text: err instanceof Error ? err.message : String(err) });
    } finally {
      setBusy(false);
    }
  };
  const enable = () => change(draft.all ? { all: true } : { add: draft.repos }, `${count(n)} enabled`);
  const disable = () => change({ remove: draft.repos }, `${count(n)} disabled`);

  return (
    <div className="sticky bottom-0 flex flex-col gap-2">
      <div className="flex flex-wrap items-center gap-2">
        <RepoMultiSelect org={org} value={draft} onChange={setDraft} label="Choose repositories to change" />
        <span className="font-mono text-xs text-fg-3">{allOn ? `CI is on for all ${count(total)}` : `${count(enabledCount)} enabled now`}</span>
      </div>
      <div className="flex flex-wrap items-center gap-x-3 gap-y-2 border-t border-rule bg-panel pt-2">
        <Button disabled={busy || n === 0} onClick={() => void enable()} className="font-mono text-xs uppercase tracking-[0.04em]">
          {busy ? "Saving" : draft.all ? `Enable CI on all ${count(total)} repositories` : n === 0 ? "Select repositories to enable" : `Enable CI on ${count(n)} ${n === 1 ? "repository" : "repositories"}`}
        </Button>
        <Button variant="outline" disabled={busy || draft.all || n === 0} onClick={() => void disable()} className="font-mono text-xs uppercase tracking-[0.04em]">
          {draft.all || n === 0 ? "Disable selected" : `Disable on ${count(n)}`}
        </Button>
        <span className="font-mono text-xs text-fg-3">{draft.all ? `all ${count(total)} selected` : `${count(n)} of ${count(total)} selected`}</span>
        {result && (
          <span role="status" className={cn("font-mono text-xs font-bold", result.ok ? "text-passed" : "text-failed-soft")}>
            {result.text}
          </span>
        )}
        {failed && (
          <button type="button" onClick={failed} className="cursor-pointer font-mono text-xs text-fg-2 quiet-underline hover:text-fg">
            Retry loading enabled repositories
          </button>
        )}
        <Link href={orgUrl(org, "repositories")} className="font-mono text-xs text-fg-2 quiet-underline hover:text-fg">
          Filter in Repositories →
        </Link>
      </div>
    </div>
  );
}

/** The key of a YAML line decides its colour, as in the design. */
const keyed = /^\s*(jobs|build|test|steps|cache|needs|runs-on):/;

function StarterConfig({ enabled }: { enabled: Loaded<CiOrgRepo[]> }) {
  const repos = enabled.state === "ready" ? enabled.data : [];
  const [chosen, setChosen] = useState<string | null>(null);
  const repo = repos.find((candidate) => candidate.name === chosen) ?? repos[0];
  const lines = CONFIG.trimEnd().split("\n");
  const newFile = repo
    ? `https://github.com/${repo.name}/new/${encodeURIComponent(repo.default_branch)}?filename=${encodeURIComponent(CONFIG_PATH)}&value=${encodeURIComponent(CONFIG)}`
    : null;

  return (
    <>
      <div className="overflow-hidden rounded-md border border-control">
        <div className="flex items-center justify-between gap-2 border-b border-control bg-background py-1.5 pr-2 pl-3">
          <span className="cap">{CONFIG_PATH}</span>
          <CopyButton text={CONFIG} label="Copy config" />
        </div>
        <div className="overflow-x-auto bg-background">
          <pre className="m-0 min-w-[420px] py-2.5 font-mono text-xs leading-[1.7]">
            {lines.map((line, index) => (
              <div key={index} className="flex">
                <span aria-hidden="true" className="w-10 flex-none pr-3.5 text-right text-fg-3 select-none">
                  {index + 1}
                </span>
                <span className={cn("whitespace-pre", keyed.test(line) || /^\s*-\s/.test(line) ? "text-fg" : "text-fg-2")}>{line}</span>
              </div>
            ))}
          </pre>
        </div>
      </div>
      <div className="flex flex-wrap items-center gap-x-3 gap-y-2">
        {repos.length > 1 && (
          <select
            aria-label="Repository to add the file to"
            className={cn(FIELD, "max-w-[260px]")}
            value={repo?.name}
            onChange={(event) => setChosen(event.target.value)}
          >
            {repos.map((candidate) => (
              <option key={candidate.name} value={candidate.name}>
                {candidate.name}
              </option>
            ))}
          </select>
        )}
        {newFile ? (
          <Button asChild>
            <a href={newFile} target="_blank" rel="noreferrer" className="font-mono text-xs uppercase tracking-[0.04em]">
              Add on GitHub <ExternalLink aria-hidden="true" />
            </a>
          </Button>
        ) : (
          <Button disabled className="font-mono text-xs uppercase tracking-[0.04em]">
            Add on GitHub
          </Button>
        )}
        <span className="font-mono text-xs text-fg-3">
          {repo ? `opens ${repo.name} on GitHub with the file filled in, you commit it` : "enable a repository first"}
        </span>
      </div>
    </>
  );
}

function BillingStep({ org, setup, onChanged }: { org: string; setup: OrgSetup; onChanged: () => void }) {
  const [billing, reload] = useLoad(() => getCiBilling(org), `${org}:${setup.billing.cap_dollars}`);
  return (
    <>
      <div className="font-mono text-xs text-fg-3">
        per second, 60 s minimum per job.{" "}
        <Link href={orgUrl(org, "settings/billing")} className="text-fg quiet-underline">
          Pricing and usage →
        </Link>
      </div>
      <div className="flex flex-wrap items-end gap-x-6 gap-y-3">
        <div>
          <CardButton org={org} card={setup.billing.card} />
        </div>
        <Await loaded={billing} retry={reload}>
          {(data) => (
            <SpendCapForm
              key={data.capDollars}
              org={org}
              current={data.capDollars}
              min={data.minCapDollars}
              max={data.maxCapDollars}
              onSaved={onChanged}
              id="setup-cap"
            />
          )}
        </Await>
      </div>
      <div className="font-mono text-xs text-fg-3">
        Jobs stop starting at {dollars(setup.billing.cap_dollars)} a month until you raise the cap.
      </div>
    </>
  );
}
