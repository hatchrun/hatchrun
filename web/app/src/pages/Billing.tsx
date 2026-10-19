// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { useEffect, useState, type FormEvent } from "react";

import { getPlan, getTenant, openPortal, redeemInvite, startCheckout, type Plan, type Tenant } from "../api";
import { Spinner } from "../components/Status";
import { useLoad } from "../load";
import { navigate, useQuery } from "../router";

const message = (err: unknown) => (err instanceof Error ? err.message : String(err));

export function formatDate(iso: string | null): string {
  if (!iso) {
    return "";
  }
  return new Date(iso).toLocaleDateString(undefined, { year: "numeric", month: "long", day: "numeric" });
}

/** The UTC calendar month `iso` starts, such as "October". */
function monthName(iso: string): string {
  return new Date(iso).toLocaleDateString(undefined, { month: "long", timeZone: "UTC" });
}

const minutes = (count: number) => `${count.toLocaleString()} ${count === 1 ? "minute" : "minutes"}`;

/** `cents` in `currency`, with as many decimals as it needs, up to five. */
export function money(cents: number, currency: string): string {
  return new Intl.NumberFormat(undefined, {
    style: "currency",
    currency: currency.toUpperCase(),
    minimumFractionDigits: Number.isInteger(cents) && cents % 100 === 0 ? 0 : 2,
    maximumFractionDigits: 5,
  }).format(cents / 100);
}

/** The metered price per minute and the cap, when the product has them. */
export function pricing(plan: Plan): { rate: string; cap: string | null } | null {
  const unit = plan.unitAmount === null ? NaN : Number(plan.unitAmount);
  if (!plan.currency || !Number.isFinite(unit)) {
    return null;
  }
  return {
    rate: money(unit, plan.currency),
    cap: plan.capAmount === null ? null : money(plan.capAmount, plan.currency),
  };
}

/** What minutes past the free ones cost this month, at the plan's price and cap. */
export function estimate(tenant: Tenant, plan: Plan): string | null {
  const unit = plan.unitAmount === null ? NaN : Number(plan.unitAmount);
  if (!plan.currency || !Number.isFinite(unit)) {
    return null;
  }
  const { minutes: used, freeMinutes } = tenant.billing.usage;
  const cents = Math.max(0, used - freeMinutes) * unit;
  return money(plan.capAmount === null ? cents : Math.min(cents, plan.capAmount), plan.currency);
}

/** The month's job minutes against the free ones. */
export function UsageMeter({ tenant, plan }: { tenant: Tenant; plan: Plan | null }) {
  const { usage, beta, licensed } = tenant.billing;
  const share = usage.freeMinutes > 0 ? Math.min(1, usage.minutes / usage.freeMinutes) : 1;
  const over = Math.max(0, usage.minutes - usage.freeMinutes);
  const bill = licensed && !beta && plan ? estimate(tenant, plan) : null;
  return (
    <div className="usage">
      <div className="usage-head">
        <span>
          <strong className="mono">{usage.minutes.toLocaleString()}</strong> job minutes in{" "}
          {monthName(usage.monthStart)}
        </span>
        <span className="muted mono small">{minutes(usage.freeMinutes)} free</span>
      </div>
      <div
        className={over > 0 ? "usage-bar usage-bar-over" : "usage-bar"}
        role="meter"
        aria-label="Free minutes used"
        aria-valuemin={0}
        aria-valuemax={usage.freeMinutes}
        aria-valuenow={Math.min(usage.minutes, usage.freeMinutes)}
      >
        <span style={{ width: `${share * 100}%` }} />
      </div>
      <p className="muted small">
        {over > 0
          ? beta
            ? `${minutes(over)} past the free ones, not billed in the beta.`
            : `${minutes(over)} past the free ones.`
          : `${(usage.freeMinutes - usage.minutes).toLocaleString()} free ${usage.freeMinutes - usage.minutes === 1 ? "minute" : "minutes"} left.`}{" "}
        {bill && `About ${bill} for this calendar month. Polar invoices by your billing period, so the invoice can differ. `}
        The count starts again on {formatDate(usage.monthEnd)}.
      </p>
    </div>
  );
}

/** A banner for an unpaid tenant near, past or done with its free minutes. */
export function UsageNotice({ tenant }: { tenant: Tenant }) {
  const { usage, licensed } = tenant.billing;
  if (licensed) {
    return null;
  }
  const org = tenant.org.login;
  const free = `${usage.freeMinutes.toLocaleString()} free minutes`;
  const month = monthName(usage.monthStart);
  if (!usage.mayLaunch) {
    return (
      <div className="notice notice-error usage-notice" role="alert">
        <span>
          Runners stopped launching. {org} used its {free} for {month} and the grace period after
          them. Add a card to start them again, or wait until {formatDate(usage.monthEnd)}.
        </span>
        <AddCard org={org} primary />
      </div>
    );
  }
  if (usage.graceEndsAt) {
    return (
      <div className="notice notice-accent usage-notice" role="status">
        <span>
          {org} used its {free} for {month}. Runners keep launching until{" "}
          {formatDate(usage.graceEndsAt)}, then stop until you add a card.
        </span>
        <AddCard org={org} primary />
      </div>
    );
  }
  if (usage.minutes >= usage.freeMinutes * 0.8) {
    return (
      <div className="notice notice-accent usage-notice" role="status">
        <span>
          {org} used {usage.minutes.toLocaleString()} of its {free} for {month}. Add a card so
          runners keep launching past them.
        </span>
        <AddCard org={org} />
      </div>
    );
  }
  return null;
}

/** Opens a Polar checkout, which adds the card that pays past the free minutes. */
export function AddCard({ org, primary = false }: { org: string; primary?: boolean }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const checkout = async () => {
    setBusy(true);
    setError(null);
    try {
      const { url } = await startCheckout(org);
      window.location.assign(url);
    } catch (err) {
      setError(message(err));
      setBusy(false);
    }
  };
  return (
    <span className="add-card">
      <button
        type="button"
        className={primary ? "button button-primary button-small" : "button button-small"}
        disabled={busy}
        onClick={checkout}
      >
        {busy ? "Opening checkout" : "Add a card"}
      </button>
      {error && (
        <span className="field-error" role="alert">
          {" "}
          {error}
        </span>
      )}
    </span>
  );
}

/** The Billing section of Settings: the month's minutes, the price, and the card. */
export function BillingPanel({ tenant, onChange }: { tenant: Tenant; onChange: (tenant: Tenant) => void }) {
  const [plan] = useLoad(getPlan, "plan");
  const checkout = useQuery("checkout");
  const org = tenant.org.login;
  const { beta, licensed, usage } = tenant.billing;
  const confirming = checkout !== null && !licensed;
  const product = plan.state === "ready" ? (plan.data.plans[0] ?? null) : null;
  const price = product ? pricing(product) : null;

  // Back from Polar's checkout: the webhook may land a moment after the
  // browser does, so look again for half a minute.
  useEffect(() => {
    if (!confirming) {
      return;
    }
    let tries = 0;
    const timer = window.setInterval(() => {
      tries += 1;
      getTenant(org).then(onChange, () => undefined);
      if (tries >= 10) {
        window.clearInterval(timer);
        navigate(`/o/${org}/settings`, true);
      }
    }, 3000);
    return () => window.clearInterval(timer);
  }, [confirming, org, onChange]);

  return (
    <>
      {checkout && licensed && (
        <p className="notice notice-success" role="status">
          Card added. {org} pays per minute past its free ones.
        </p>
      )}
      <UsageMeter tenant={tenant} plan={product} />
      {confirming ? (
        <Spinner label="Confirming your card with Polar" />
      ) : beta ? (
        <p>In the invite-only beta. Minutes are counted and nothing is billed.</p>
      ) : licensed ? (
        <p>
          <PlanSummary tenant={tenant} />
        </p>
      ) : (
        <>
          <p>
            Every organization gets {minutes(usage.freeMinutes)} a month without a card.
            {price &&
              ` Past them, hatchrun costs ${price.rate} a minute${price.cap ? `, and never more than ${price.cap} a month` : ""}.`}{" "}
            Runners run in your cloud account, so your provider bills compute directly, with no
            markup.
          </p>
          <p className="inline-actions">
            <AddCard org={org} primary />
          </p>
          <InviteCode tenant={tenant} onRedeemed={onChange} />
        </>
      )}
      <p className="fine">
        Payments are handled by Polar, our merchant of record, who sends the invoices and handles
        sales tax.
      </p>
    </>
  );
}

function InviteCode({ tenant, onRedeemed }: { tenant: Tenant; onRedeemed: (tenant: Tenant) => void }) {
  const [shown, setShown] = useState(false);
  const [code, setCode] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    setBusy(true);
    setError(null);
    try {
      onRedeemed(await redeemInvite(tenant.org.login, code));
    } catch (err) {
      setError(message(err));
    } finally {
      setBusy(false);
    }
  };

  if (!shown) {
    return (
      <p className="invite-toggle">
        <button type="button" className="button-link" onClick={() => setShown(true)}>
          Have a beta invite code?
        </button>
      </p>
    );
  }
  return (
    <form className="inline-form" onSubmit={submit}>
      <label htmlFor="invite-code">Beta invite code</label>
      <div className="inline-form-row">
        <input
          id="invite-code"
          className="input"
          value={code}
          onChange={(event) => setCode(event.target.value)}
          autoComplete="off"
          spellCheck={false}
          required
        />
        <button type="submit" className="button" disabled={busy || code.trim() === ""}>
          {busy ? "Checking" : "Join the beta"}
        </button>
      </div>
      {error && (
        <p className="field-error" role="alert">
          {error}
        </p>
      )}
    </form>
  );
}

/** What Billing says once the tenant pays per minute. */
export function PlanSummary({ tenant }: { tenant: Tenant }) {
  const { subscription } = tenant.billing;
  if (!subscription) {
    return null;
  }
  return (
    <>
      Card on file, paying per minute past the free ones.{" "}
      {subscription.currentPeriodEnd &&
        (subscription.cancelAtPeriodEnd
          ? `Ends on ${formatDate(subscription.currentPeriodEnd)}. `
          : `Next invoice on ${formatDate(subscription.currentPeriodEnd)}. `)}
      <PortalLink org={tenant.org.login} />
    </>
  );
}

export function PortalLink({ org, label = "Manage billing" }: { org: string; label?: string }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const open = async () => {
    setBusy(true);
    setError(null);
    try {
      const { url } = await openPortal(org);
      window.location.assign(url);
    } catch (err) {
      setError(message(err));
      setBusy(false);
    }
  };
  return (
    <>
      <button type="button" className="button-link" disabled={busy} onClick={open}>
        {busy ? "Opening Polar" : label}
      </button>
      {error && <span className="field-error"> {error}</span>}
    </>
  );
}
