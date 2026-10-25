// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { useEffect, useState, type FormEvent } from "react";

import { getTenant, openPortal, redeemInvite, startCheckout, type Tenant } from "../api";
import { Spinner } from "../components/Status";
import { runnerTime } from "../lib/utils";
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

/** `cents` in `currency`, with as many decimals as it needs, up to five. */
export function money(cents: number, currency: string): string {
  return new Intl.NumberFormat(undefined, {
    style: "currency",
    currency: currency.toUpperCase(),
    minimumFractionDigits: Number.isInteger(cents) && cents % 100 === 0 ? 0 : 2,
    maximumFractionDigits: 5,
  }).format(cents / 100);
}

/** The runner price per minute, such as "$0.001". */
function rate(tenant: Tenant): string {
  return money(tenant.billing.usage.priceMicrosPerMinute / 10_000, "usd");
}

/** The month's runner time, and what it costs when the tenant pays. */
export function UsageMeter({ tenant }: { tenant: Tenant }) {
  const { usage, beta, licensed } = tenant.billing;
  return (
    <div className="usage">
      <div className="usage-head">
        <span>
          <strong className="mono">{runnerTime(usage.seconds)}</strong> of runner time in{" "}
          {monthName(usage.monthStart)}
        </span>
        {licensed && !beta && (
          <span className="muted mono small">{money(usage.spendCents, "usd")} so far</span>
        )}
      </div>
      <p className="muted small">
        {beta
          ? "Not billed in the beta."
          : `Billed per second, at least a minute per job, at ${rate(tenant)} a minute.`}{" "}
        {licensed && !beta && "Polar invoices by your billing period, so the invoice can differ. "}
        The count starts again on {formatDate(usage.monthEnd)}.
      </p>
    </div>
  );
}

/** A banner for a tenant with no card and no beta, which launches no runners. */
export function UsageNotice({ tenant }: { tenant: Tenant }) {
  if (tenant.billing.licensed) {
    return null;
  }
  const org = tenant.org.login;
  return (
    <div className="notice notice-accent usage-notice" role="status">
      <span>
        Runners launch once {org} adds a card. hatchrun costs {rate(tenant)} a minute, billed per
        second.
      </span>
      <AddCard org={org} primary />
    </div>
  );
}

/** Opens a Polar checkout, which adds the card that pays for runner time. */
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

/** The Billing section of Settings: the month's runner time, the price, and the card. */
export function BillingPanel({ tenant, onChange }: { tenant: Tenant; onChange: (tenant: Tenant) => void }) {
  const checkout = useQuery("checkout");
  const org = tenant.org.login;
  const { beta, licensed } = tenant.billing;
  const confirming = checkout !== null && !licensed;

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
          Card added. {org} pays per second of runner time.
        </p>
      )}
      <UsageMeter tenant={tenant} />
      {confirming ? (
        <Spinner label="Confirming your card with Polar" />
      ) : beta ? (
        <p>In the invite-only beta. Runner time is counted and nothing is billed.</p>
      ) : licensed ? (
        <p>
          <PlanSummary tenant={tenant} />
        </p>
      ) : (
        <>
          <p>
            Runners launch once you add a card. hatchrun costs {rate(tenant)} a minute of runner
            time, billed per second with a one minute minimum per job. Runners run in your cloud
            account, so your provider bills compute directly, with no markup.
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

/** What Billing says once the tenant pays per second. */
export function PlanSummary({ tenant }: { tenant: Tenant }) {
  const { subscription } = tenant.billing;
  if (!subscription) {
    return null;
  }
  return (
    <>
      Card on file, paying per second of runner time.{" "}
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
