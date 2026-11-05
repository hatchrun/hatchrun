// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { useEffect, useRef } from "react";

import { getCiBilling, type CiBilling } from "@/api";
import { CardButton, dollars, SpendCapForm } from "@/components/BillingParts";
import { Page } from "@/components/Page";
import { Await } from "@/components/Status";
import { cn } from "@/lib/utils";
import { useLoad } from "@/load";
import type { OrgPageProps } from "./types";

const CAP = "rounded-md border border-control bg-panel";
const HEAD = "border-b border-control px-3.5 py-2";

/** Settings, Billing (also /settings): this month's CI spend, the spend cap and the card. */
export function Billing({ org }: OrgPageProps) {
  const returning = new URLSearchParams(window.location.search).has("checkout");
  const [billing, reload] = useLoad(() => getCiBilling(org), org, returning);
  const now = new Date();
  // The server bills by UTC month, so the page reads the month in UTC too.
  const month = now.toLocaleDateString("en-US", { month: "long", timeZone: "UTC" });
  const card = billing.state === "ready" ? billing.data.card : false;
  const tries = useRef(0);
  useEffect(() => {
    // Polar confirms the card by webhook, a moment after the redirect back.
    if (!returning || card) {
      return;
    }
    const timer = window.setInterval(() => {
      tries.current += 1;
      reload();
      if (tries.current >= 10) {
        window.clearInterval(timer);
      }
    }, 3000);
    return () => window.clearInterval(timer);
  }, [returning, card, reload]);
  return (
    <Page title="Billing" meta={`${org} · billed per second · 1 to ${now.getUTCDate()} ${now.toLocaleDateString("en-US", { month: "short", timeZone: "UTC" })}`} narrow>
      <Await loaded={billing} retry={reload}>
        {(data) => <BillingBody org={org} billing={data} month={month} now={now} reload={reload} />}
      </Await>
    </Page>
  );
}

function BillingBody({ org, billing, month, now, reload }: { org: string; billing: CiBilling; month: string; now: Date; reload: () => void }) {
  const spent = billing.spendCents / 100;
  const daysInMonth = new Date(Date.UTC(now.getUTCFullYear(), now.getUTCMonth() + 1, 0)).getUTCDate();
  const projected = (spent / now.getUTCDate()) * daysInMonth;
  const percent = billing.capDollars > 0 ? Math.min(100, Math.round((spent / billing.capDollars) * 100)) : 0;
  const reached = spent >= billing.capDollars;

  return (
    <>
      <section aria-label="This month" className={cn(CAP, "flex flex-wrap")}>
        <Figure label="Month to date" className="flex-[1_1_200px]">
          <span className="font-mono text-[26px] leading-8 font-bold">{dollars(spent, true)}</span>
        </Figure>
        <Figure label={`Projected for ${month}`} className="flex-[1_1_200px] border-l border-control">
          <span className="font-mono text-[26px] leading-8 font-bold">{dollars(projected)}</span>
          <span className="font-mono text-[11px] text-fg-3">at the current daily rate</span>
        </Figure>
        <Figure label="Card on file" className="flex-[1_1_240px] border-l border-control">
          <span className="mt-1 font-mono text-sm">{billing.card ? "Card on file" : "No card"}</span>
          <span className="mt-1">
            <CardButton org={org} card={billing.card} primary={false} />
          </span>
        </Figure>
      </section>

      <section aria-label="Line items" className={cn(CAP, "overflow-hidden")}>
        <div className={cn(HEAD, "bg-panel")}>
          <span className="cap">Line items, {month}</span>
        </div>
        <div className="overflow-x-auto">
          <table className="w-full min-w-[560px] border-collapse">
            <thead>
              <tr className="bg-background text-left">
                <th className="cap px-3.5 py-2.5 font-normal">Product</th>
                <th className="cap px-3 py-2.5 font-normal">Unit</th>
                <th className="cap px-3 py-2.5 text-right font-normal">Quantity</th>
                <th className="cap px-3.5 py-2.5 text-right font-normal">Cost</th>
              </tr>
            </thead>
            <tbody>
              <tr className="border-t border-rule hover:bg-panel">
                <td className="px-3.5 py-2.5 font-bold shadow-[inset_3px_0_0_var(--brand)]">hatchrun CI</td>
                <td className="px-3 py-2.5 font-mono text-xs text-fg-2">2 vCPU seconds</td>
                <td className="px-3 py-2.5 text-right font-mono text-xs">{billing.units.toLocaleString("en-US")}</td>
                <td className="px-3.5 py-2.5 text-right font-mono text-xs">{dollars(spent, true)}</td>
              </tr>
              <tr className="border-t border-control bg-panel">
                <td colSpan={3} className="cap px-3.5 py-2.5 text-fg">
                  Total month to date
                </td>
                <td className="px-3.5 py-2.5 text-right font-mono text-[13px] font-bold">{dollars(spent, true)}</td>
              </tr>
            </tbody>
          </table>
        </div>
        <p className="m-0 border-t border-control px-3.5 py-2.5 font-mono text-xs text-fg-3">
          Billed per second, 60 s minimum per job. {billing.running} of {billing.maxRunning} jobs running now.
        </p>
      </section>

      <section aria-label="CI spend cap" className={CAP}>
        <div className={HEAD}>
          <span className="cap">CI spend cap</span>
        </div>
        <div className="flex flex-col gap-3.5 px-3.5 pt-3.5 pb-4">
          <div>
            <div className="mb-2 flex flex-wrap justify-between gap-1.5 font-mono text-xs text-fg-3">
              <span>
                <b className="text-fg">{dollars(spent, true)}</b> of <b className="text-fg">{dollars(billing.capDollars)}</b> used
              </span>
              <b className="text-fg">{percent}%</b>
            </div>
            <div
              role="progressbar"
              aria-label="CI spend this month against the cap"
              aria-valuemin={0}
              aria-valuemax={100}
              aria-valuenow={percent}
              className="h-2.5 overflow-hidden rounded-sm border border-control bg-background"
            >
              <div className={cn("h-full", reached ? "bg-waiting" : "bg-brand")} style={{ width: `${percent}%` }} />
            </div>
            <p className={cn("m-0 mt-2 font-mono text-xs", reached ? "text-waiting" : "text-fg-3")}>
              {!billing.card
                ? "A card is required. New CI jobs wait until one is added."
                : reached
                  ? "Cap reached. New CI jobs queue until you raise it."
                  : "New CI jobs start until the month reaches the cap."}
            </p>
          </div>
          <SpendCapForm key={billing.capDollars} org={org} current={billing.capDollars} min={billing.minCapDollars} max={billing.maxCapDollars} onSaved={reload} />
          <span className="font-mono text-xs text-fg-3">At the cap, new CI jobs queue until you raise it. Running jobs finish.</span>
        </div>
      </section>
    </>
  );
}

function Figure({ label, className, children }: { label: string; className?: string; children: React.ReactNode }) {
  return (
    <div className={cn("flex flex-col items-start gap-1 px-[18px] py-3.5", className)}>
      <span className="cap">{label}</span>
      {children}
    </div>
  );
}
