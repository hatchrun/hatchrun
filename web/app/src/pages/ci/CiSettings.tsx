// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

// An organization's CI page: billing, then its secrets and variables.

import { useState, type FormEvent } from "react";

import {
  getCiBilling,
  openPortal,
  setCiCap,
  startCheckout,
  type CiBilling,
} from "@/api";
import { Await } from "@/components/Status";
import { useLoad } from "@/load";
import { SecretsTable } from "./SecretsTable";

const dollars = (cents: number) =>
  new Intl.NumberFormat("en-US", { style: "currency", currency: "USD" }).format(cents / 100);

export function CiSettings({ org }: { org: string }) {
  const [billing, reload] = useLoad(() => getCiBilling(org), org);
  return (
    <div className="flex flex-col gap-8">
      <section aria-label="Billing" className="flex flex-col gap-3">
        <h1 className="display text-[26px] leading-[1.05]">CI</h1>
        <Await loaded={billing} retry={reload}>
          {(data) => <BillingCard org={org} billing={data} onChange={reload} />}
        </Await>
      </section>
      <SecretsTable scope={{ org }} kind="secrets" />
      <SecretsTable scope={{ org }} kind="vars" />
    </div>
  );
}

function BillingCard({ org, billing, onChange }: { org: string; billing: CiBilling; onChange: () => void }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Both buttons leave for the hatchrun.io checkout or portal, which return to this page.
  const go = async (open: (org: string) => Promise<{ url: string }>) => {
    setBusy(true);
    setError(null);
    try {
      window.location.assign((await open(org)).url);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
      setBusy(false);
    }
  };

  return (
    <div className="card panel flex flex-col gap-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <p>
          {billing.card ? "A card is on file." : "Add a card to run CI jobs. Jobs wait until you do."}
        </p>
        {billing.card ? (
          <button type="button" className="button" disabled={busy} onClick={() => void go((o) => openPortal(o, "ci"))}>
            Manage billing
          </button>
        ) : (
          <button type="button" className="button button-primary" disabled={busy} onClick={() => void go((o) => startCheckout(o, "ci"))}>
            Add a card
          </button>
        )}
      </div>
      {error && (
        <p className="notice notice-error" role="alert">
          {error}
        </p>
      )}
      <dl className="grid grid-cols-1 gap-4 sm:grid-cols-3">
        <Figure label="Free minutes used" title={`${billing.minutes.toLocaleString()} runner minutes; larger runners count more`}>
          {billing.units.toLocaleString()} of {billing.freeUnits.toLocaleString()}
        </Figure>
        <Figure label="Spend this month">{dollars(billing.spendCents)}</Figure>
        <Figure label="Jobs running">
          {billing.running} of {billing.maxRunning}
        </Figure>
      </dl>
      <CapForm org={org} current={billing.capDollars} min={billing.minCapDollars} max={billing.maxCapDollars} onSaved={onChange} />
    </div>
  );
}

function Figure({ label, title, children }: { label: string; title?: string; children: React.ReactNode }) {
  return (
    <div title={title}>
      <dt className="label">{label}</dt>
      <dd className="font-mono text-lg">{children}</dd>
    </div>
  );
}

function CapForm({ org, current, min, max, onSaved }: { org: string; current: number; min: number; max: number; onSaved: () => void }) {
  const [cap, setCap] = useState(String(current));
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);

  const save = async (event: FormEvent) => {
    event.preventDefault();
    setSaved(false);
    const dollars = Number(cap);
    if (cap.trim() === "" || !Number.isInteger(dollars) || dollars < min || dollars > max) {
      setProblem(`Set a cap in whole dollars from $${min} to $${max.toLocaleString()}`);
      return;
    }
    setBusy(true);
    setProblem(null);
    try {
      await setCiCap(org, dollars);
      setSaved(true);
      onSaved();
    } catch (err) {
      setProblem(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <form onSubmit={save} aria-label="Monthly spend cap" noValidate>
      <div className="field field-narrow">
        <label className="label" htmlFor="ci-cap">
          Monthly spend cap (USD)
        </label>
        <input
          id="ci-cap"
          className="input"
          type="number"
          inputMode="numeric"
          step={1}
          min={min}
          max={max}
          value={cap}
          onChange={(event) => setCap(event.target.value)}
          aria-invalid={problem !== null}
          aria-describedby={problem ? "ci-cap-error" : "ci-cap-help"}
        />
        {problem ? (
          <p id="ci-cap-error" className="field-error" role="alert">
            {problem}
          </p>
        ) : (
          <p id="ci-cap-help" className="field-help">
            Jobs wait once this month's spend reaches the cap.
          </p>
        )}
      </div>
      {saved && (
        <p className="notice notice-success" role="status">
          Saved.
        </p>
      )}
      <button type="submit" className="button" disabled={busy}>
        {busy ? "Saving" : "Save cap"}
      </button>
    </form>
  );
}
