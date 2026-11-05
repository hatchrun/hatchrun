// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

// The card and spend cap controls shared by the Setup checklist and the Billing page.

import { useState, type FormEvent } from "react";

import { openPortal, setCiCap, startCheckout } from "@/api";
import { cn } from "@/lib/utils";
import { Button } from "./ui/button";

/** A text input of the console: 32px, mono, control border. */
export const FIELD =
  "h-8 rounded-sm border border-control bg-background px-2.5 font-mono text-[13px] text-fg placeholder:text-fg-3 hover:border-fg-3 aria-[invalid=true]:border-failed";

export const dollars = (value: number, cents = false) =>
  new Intl.NumberFormat("en-US", { style: "currency", currency: "USD", minimumFractionDigits: cents ? 2 : 0, maximumFractionDigits: 2 }).format(value);

/**
 * "Add a card" (Polar checkout) or "Manage card" (Polar portal). Both
 * leave for polar and return to the billing page.
 */
export function CardButton({ org, card, primary = true }: { org: string; card: boolean; primary?: boolean }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const go = async () => {
    setBusy(true);
    setError(null);
    try {
      window.location.assign((await (card ? openPortal(org) : startCheckout(org))).url);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
      setBusy(false);
    }
  };
  return (
    <span className="inline-flex flex-wrap items-center gap-3">
      <Button variant={card || !primary ? "outline" : "default"} disabled={busy} onClick={() => void go()} className={cn(!card && primary && "font-mono text-xs uppercase tracking-[0.04em]")}>
        {card ? "Manage card" : "Add a card"}
      </Button>
      {error && (
        <span role="alert" className="font-mono text-xs text-failed-soft">
          {error}
        </span>
      )}
    </span>
  );
}

/** The monthly cap in whole dollars within the server's range. */
export function SpendCapForm({
  org,
  current,
  min,
  max,
  onSaved,
  id = "ci-cap",
}: {
  org: string;
  current: number;
  min: number;
  max: number;
  onSaved: () => void;
  id?: string;
}) {
  const [cap, setCap] = useState(String(current));
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);

  const save = async (event: FormEvent) => {
    event.preventDefault();
    setSaved(false);
    const value = Number(cap.replace(/,/g, ""));
    if (!/^[0-9,]+$/.test(cap.trim()) || !Number.isInteger(value) || value < min || value > max) {
      setProblem(`Set a cap in whole dollars from ${dollars(min)} to ${dollars(max)}`);
      return;
    }
    setBusy(true);
    setProblem(null);
    try {
      await setCiCap(org, value);
      setSaved(true);
      onSaved();
    } catch (err) {
      setProblem(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <form onSubmit={save} aria-label="Monthly spend cap" noValidate className="flex flex-wrap items-end gap-x-3 gap-y-2">
      <div className="flex flex-col gap-1">
        <label className="cap" htmlFor={id}>
          Monthly cap, USD
        </label>
        <input
          id={id}
          className={cn(FIELD, "w-40")}
          type="text"
          inputMode="numeric"
          value={cap}
          onChange={(event) => {
            setCap(event.target.value);
            setSaved(false);
          }}
          aria-invalid={problem !== null}
          aria-describedby={problem ? `${id}-error` : undefined}
        />
      </div>
      <Button type="submit" disabled={busy} className="font-mono text-xs uppercase tracking-[0.04em]">
        {busy ? "Saving" : "Save cap"}
      </Button>
      {saved && (
        <span role="status" className="inline-flex h-8 items-center gap-1.5 font-mono text-xs text-passed">
          <span aria-hidden="true">✓</span>Cap saved
        </span>
      )}
      {problem && (
        <p id={`${id}-error`} role="alert" className="m-0 basis-full font-mono text-xs text-failed-soft">
          {problem}
        </p>
      )}
    </form>
  );
}
