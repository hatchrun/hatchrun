// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { useEffect, useRef } from "react";

import { ExternalLink } from "lucide-react";

import { getCiBilling, type CiBilling, type CiCard, type CiDay, type CiInvoice } from "@/api";
import { CardButton, dollars, SpendCapForm } from "@/components/BillingParts";
import { Page } from "@/components/Page";
import { SettingsNav } from "@/components/SettingsNav";
import { Await } from "@/components/Status";
import { cn } from "@/lib/utils";
import { useLoad } from "@/load";
import { isoDateTime, tableDate } from "@/lib/format";
import type { OrgPageProps } from "./types";

const CAP = "rounded-md border border-control bg-panel";
const HEAD = "border-b border-control px-3.5 py-2";

/** Settings, Billing (also /settings): this month's CI spend, the spend cap and the card. */
export function Billing({ org, tenant }: OrgPageProps) {
  const returning = new URLSearchParams(window.location.search).has("checkout");
  const [billing, reload] = useLoad(() => getCiBilling(org), org, returning);
  const now = new Date();
  // The server bills by UTC month, so the page reads the month in UTC too.
  const month = now.toLocaleDateString("en-US", { month: "long", timeZone: "UTC" });
  const card = billing.state === "ready" && billing.data.card !== null;
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
    <Page title="Billing" nav={<SettingsNav org={org} current="billing" admin={tenant.role === "admin"} />} meta={`${org} · billed per second · 1 to ${now.getUTCDate()} ${now.toLocaleDateString("en-US", { month: "short", timeZone: "UTC" })}`} narrow>
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
          <span className="mt-1 font-mono text-sm">{billing.card ? cardText(billing.card) : "No card"}</span>
          <span className="mt-1">
            <CardButton org={org} card={billing.card !== null} primary={false} />
          </span>
        </Figure>
      </section>

      <DailyChart days={billing.daily} now={now} month={month} />

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

      <Invoices invoices={billing.invoices} />

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
              {billing.card === null
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

const BRANDS: Record<string, string> = { visa: "Visa", mastercard: "Mastercard", amex: "American Express", discover: "Discover", diners: "Diners Club", jcb: "JCB", unionpay: "UnionPay" };

/** "Visa ending 4242, expires 08/2029"; the parts Polar did not give are left out. */
function cardText(card: CiCard): string {
  const brand = card.brand ? (BRANDS[card.brand.toLowerCase()] ?? card.brand) : null;
  const ending = card.last4 ? `ending ${card.last4}` : null;
  const expiry = card.exp_month !== null && card.exp_year !== null ? `expires ${String(card.exp_month).padStart(2, "0")}/${card.exp_year}` : null;
  const named = [brand, ending].filter(Boolean).join(" ");
  return [named, expiry].filter(Boolean).join(", ") || "Card on file";
}

const CHART = { width: 900, height: 250, left: 48, right: 890, base: 205 };

/** Spend per UTC day of this month: a bar per day, dashed outlines for days to come. */
function DailyChart({ days, now, month }: { days: CiDay[]; now: Date; month: string }) {
  const count = new Date(Date.UTC(now.getUTCFullYear(), now.getUTCMonth() + 1, 0)).getUTCDate();
  const byDay = new Map(days.map((day) => [Number(day.day.slice(8, 10)), day]));
  const peak = Math.max(100, ...days.map((day) => day.cents));
  // A round top for the scale: $1, $2, $5, $10 ... above the peak.
  const unit = 10 ** Math.floor(Math.log10(peak / 100));
  const top = ([1, 2, 5, 10].map((step) => step * unit).find((step) => step * 100 >= peak) ?? peak / 100) * 100;
  const slot = (CHART.right - CHART.left) / count;
  const width = slot * 0.68;
  const height = (cents: number) => (cents / top) * (CHART.base - 24);
  const total = days.reduce((sum, day) => sum + day.cents, 0);
  const label = `Daily spend in dollars for ${month}, hatchrun CI: ${dollars(total / 100, true)} over ${days.length} days, peak ${dollars(peak / 100, true)}`;
  return (
    <section aria-label="Daily spend" className={cn(CAP, "bg-panel")}>
      <div className={cn(HEAD, "flex flex-wrap items-baseline justify-between gap-2")}>
        <span className="cap">Spend per day, {month}</span>
        <span className="font-mono text-xs text-fg-2">
          <span aria-hidden="true" className="mr-1.5 inline-block size-2.5 rounded-[1px] bg-brand" />
          hatchrun CI
        </span>
      </div>
      <div className="overflow-x-auto px-3.5 py-3">
        <svg role="img" aria-label={label} viewBox={`0 0 ${CHART.width} ${CHART.height}`} className="block h-auto w-full min-w-[640px]">
          {[0.5, 1].map((share) => {
            const y = CHART.base - height(top * share);
            return (
              <g key={share}>
                <line x1={CHART.left} x2={CHART.right} y1={y} y2={y} className="stroke-rule" />
                <text x={CHART.left - 8} y={y + 4} textAnchor="end" fontSize={11} className="fill-fg-3 font-mono">
                  {dollars((top * share) / 100)}
                </text>
              </g>
            );
          })}
          {Array.from({ length: count }, (_, n) => {
            const day = byDay.get(n + 1);
            const x = CHART.left + n * slot + (slot - width) / 2;
            const h = day ? height(day.cents) : 0;
            const tip = day
              ? `${month} ${n + 1}: ${dollars(day.cents / 100, true)}, ${(day.seconds / 3600).toFixed(1)} runner hours`
              : `${month} ${n + 1}: not yet`;
            return (
              <g key={n}>
                <title>{tip}</title>
                {day ? (
                  <rect x={x} y={CHART.base - Math.max(h, day.cents > 0 ? 1 : 0)} width={width} height={Math.max(h, day.cents > 0 ? 1 : 0)} className="fill-brand" />
                ) : (
                  <rect x={x} y={CHART.base - 40} width={width} height={40} fill="none" strokeDasharray="3 3" className="stroke-rule" />
                )}
                {(n === 0 || (n + 1) % 5 === 0 || n + 1 === count) && (
                  <text x={x + width / 2} y={226} textAnchor="middle" fontSize={11} className="fill-fg-3 font-mono">
                    {n + 1}
                  </text>
                )}
              </g>
            );
          })}
          <line x1={CHART.left} x2={CHART.right} y1={CHART.base} y2={CHART.base} className="stroke-control" />
          <text x={(CHART.left + CHART.right) / 2} y={244} textAnchor="middle" fontSize={11} className="fill-fg-3 font-mono">
            DAY OF {month.toUpperCase()} (UTC)
          </text>
          <text x={CHART.right} y={16} textAnchor="end" fontSize={11} className="fill-fg-2 font-mono">
            peak {dollars(peak / 100, true)} / day
          </text>
        </svg>
      </div>
      <p className="m-0 border-t border-control px-3.5 py-2 font-mono text-xs text-fg-3">Hover a bar for the day. Dashed outlines are days that have not happened yet.</p>
    </section>
  );
}

const money = (cents: number) => dollars(cents / 100, true);

function Invoices({ invoices }: { invoices: CiInvoice[] }) {
  return (
    <section aria-label="Invoices" className={cn(CAP, "overflow-hidden")}>
      <div className={cn(HEAD, "bg-panel")}>
        <span className="cap">Invoices</span>
      </div>
      {invoices.length === 0 ? (
        <p className="m-0 px-3.5 py-4 font-mono text-[13px] text-fg-3">No invoices yet. The first one is issued after the month closes.</p>
      ) : (
        <div className="max-h-[520px] overflow-auto">
          <table className="w-full min-w-[640px] border-collapse">
            <thead className="sticky top-0 bg-background">
              <tr className="text-left">
                <th className="cap px-3.5 py-2.5 font-normal">Month</th>
                <th className="cap px-3 py-2.5 text-right font-normal">Total</th>
                <th className="cap px-3 py-2.5 font-normal">Status</th>
                <th className="cap px-3 py-2.5 font-normal">Issued</th>
                <th className="cap px-3.5 py-2.5 text-right font-normal">Invoice</th>
              </tr>
            </thead>
            <tbody>
              {invoices.map((invoice) => {
                const month = new Date(invoice.date).toLocaleDateString("en-US", { month: "long", year: "numeric", timeZone: "UTC" });
                const paid = invoice.status === "paid";
                return (
                  <tr key={invoice.id} className="border-t border-rule hover:bg-panel">
                    <td className="px-3.5 py-2.5 font-bold">{month}</td>
                    <td className="px-3 py-2.5 text-right font-mono text-xs">{money(invoice.amount_cents)}</td>
                    <td className="px-3 py-2.5">
                      <span className={cn("inline-flex items-center gap-1.5 rounded-sm border px-2 py-px font-mono text-xs font-bold", paid ? "border-passed text-passed" : "border-control text-fg-2")}>
                        {paid && <span aria-hidden="true">✓</span>}
                        {invoice.status.replace(/_/g, " ")}
                      </span>
                    </td>
                    <td className="px-3 py-2.5 font-mono text-xs text-fg-2" title={isoDateTime(Date.parse(invoice.date))}>
                      {tableDate(Date.parse(invoice.date))}
                    </td>
                    <td className="px-3.5 py-2.5 text-right">
                      {invoice.url === null ? (
                        <span className="font-mono text-xs text-fg-3">Not ready</span>
                      ) : (
                        <a href={invoice.url} target="_blank" rel="noreferrer" aria-label={`Open the ${month} invoice`} className="inline-flex items-center gap-1.5 font-mono text-xs text-fg quiet-underline hover:text-brand">
                          Open <ExternalLink size={11} aria-hidden="true" />
                        </a>
                      )}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
      <p className="m-0 border-t border-control px-3.5 py-2 font-mono text-xs text-fg-3">Newest first, the last 12 orders from Polar.</p>
    </section>
  );
}
