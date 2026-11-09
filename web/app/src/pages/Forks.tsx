// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

// What a repository does with pull requests from forks: whether they
// run, who needs a maintainer's approval first, how many fork jobs run
// at once and for how long. The policy of a repository is changed by
// its admins; the server checks, and a member who is not one sees why.

import { useState } from "react";

import { getForkPolicy, putForkPolicy, type ForkPolicy } from "@/api";
import { Page } from "@/components/Page";
import { SettingsNav } from "@/components/SettingsNav";
import { Await } from "@/components/Status";
import { useRepoSearch } from "@/lib/useRepos";
import { useLoad } from "@/load";
import { setQuery, useQuery } from "@/router";
import { BTN_PRIMARY, FIELD } from "./formStyles";
import type { OrgPageProps } from "./types";

const LABEL = "cap flex flex-col gap-1";

const APPROVAL: [ForkPolicy["fork_approval"], string][] = [
  ["first_timers", "First-time GitHub users"],
  ["first_time_contributors", "First-time contributors to this repository"],
  ["all_outside", "Everyone but members and collaborators"],
];

export function Forks({ org, tenant }: OrgPageProps) {
  const repo = useQuery("repo");
  const [search, setSearch] = useState("");
  // Every repository of the installation, found by name: the list holds
  // the first matches and the search narrows it, so none is out of reach.
  const { repos, total, loading } = useRepoSearch(org, search, 20);
  const admin = tenant.role === "admin";

  return (
    <Page title="Fork pull requests" nav={<SettingsNav org={org} current="forks" admin={admin} />} narrow>
      <p className="m-0 font-mono text-[13px] text-fg-2">
        A pull request from a fork runs without secrets, a write token, OIDC tokens or environments. An author the approval level names needs a maintainer to approve each commit first.
        {!admin && " An admin of the repository can change its policy; the server decides who may."}
      </p>
      <label className={LABEL}>
        Find a repository
        <input
          type="search"
          value={search}
          onChange={(event) => setSearch(event.target.value)}
          placeholder="Search repositories"
          autoComplete="off"
          spellCheck={false}
          className={`${FIELD} !h-8 max-w-[360px]`}
        />
      </label>
      <ul aria-label="Repositories" className="m-0 flex list-none flex-col rounded-md border border-rule p-0">
        {repos.map((item) => (
          <li key={item.name} className="border-t border-rule first:border-t-0">
            <button
              type="button"
              aria-pressed={repo === item.name}
              onClick={() => setQuery({ repo: item.name })}
              className={`w-full cursor-pointer px-3 py-1.5 text-left font-mono text-[13px] hover:bg-raised ${repo === item.name ? "text-fg" : "text-fg-2"}`}
            >
              {item.name}
            </button>
          </li>
        ))}
        {repos.length === 0 && !loading && <li className="px-3 py-2 font-mono text-[13px] text-fg-3">No repository matches.</li>}
      </ul>
      {total > repos.length && <p className="m-0 font-mono text-xs text-fg-3">{total - repos.length} more: narrow the search.</p>}
      {repo && <Policy key={repo} repository={repo} />}
    </Page>
  );
}

function Policy({ repository }: { repository: string }) {
  const [owner = "", name = ""] = repository.split("/");
  const [loaded, reload] = useLoad(() => getForkPolicy(owner, name), repository);
  return (
    <Await loaded={loaded} retry={reload}>
      {(policy) => <PolicyForm key={JSON.stringify(policy)} owner={owner} name={name} policy={policy} />}
    </Await>
  );
}

function PolicyForm({ owner, name, policy }: { owner: string; name: string; policy: ForkPolicy }) {
  const [draft, setDraft] = useState(policy);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<{ text: string; failed: boolean } | null>(null);
  const set = <K extends keyof ForkPolicy>(key: K, value: ForkPolicy[K]) => {
    setDraft({ ...draft, [key]: value });
    setMessage(null);
  };

  const save = async () => {
    setBusy(true);
    setMessage(null);
    try {
      await putForkPolicy(owner, name, draft);
      setMessage({ text: "Saved", failed: false });
    } catch (err) {
      setMessage({ text: err instanceof Error ? err.message : String(err), failed: true });
    } finally {
      setBusy(false);
    }
  };

  return (
    <form
      className="flex flex-col gap-4 rounded-md border border-rule bg-panel px-4 py-3"
      onSubmit={(event) => {
        event.preventDefault();
        void save();
      }}
    >
      <fieldset disabled={busy} className="m-0 flex min-w-0 flex-col gap-4 border-0 p-0">
      <label className={LABEL}>
        Run pull requests from forks
        <select value={draft.fork_runs} onChange={(event) => set("fork_runs", event.target.value as ForkPolicy["fork_runs"])} className={`${FIELD} !h-8 max-w-[240px]`}>
          <option value="on">On</option>
          <option value="off">Off</option>
        </select>
      </label>
      <label className={LABEL}>
        Ask a maintainer to approve
        <select value={draft.fork_approval} onChange={(event) => set("fork_approval", event.target.value as ForkPolicy["fork_approval"])} className={`${FIELD} !h-8 max-w-[360px]`}>
          {APPROVAL.map(([value, label]) => (
            <option key={value} value={value}>
              {label}
            </option>
          ))}
        </select>
      </label>
      <div className="flex flex-wrap gap-4">
        <label className={LABEL}>
          Fork jobs at once (1 to 10)
          <input
            type="number"
            min={1}
            max={10}
            value={draft.fork_max_running}
            onChange={(event) => set("fork_max_running", Number(event.target.value))}
            className={`${FIELD} !h-8 w-[120px]`}
          />
        </label>
        <label className={LABEL}>
          Fork job timeout, minutes (5 to 60)
          <input
            type="number"
            min={5}
            max={60}
            value={draft.fork_timeout_minutes}
            onChange={(event) => set("fork_timeout_minutes", Number(event.target.value))}
            className={`${FIELD} !h-8 w-[120px]`}
          />
        </label>
      </div>
      </fieldset>
      <div className="flex items-center gap-3">
        <button type="submit" className={BTN_PRIMARY} disabled={busy}>
          {busy ? "Saving" : "Save"}
        </button>
        {message && (
          <span role={message.failed ? "alert" : "status"} className={`font-mono text-[13px] ${message.failed ? "text-failed" : "text-fg-2"}`}>
            {message.text}
          </span>
        )}
      </div>
    </form>
  );
}
