// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

import { useState, type FormEvent } from "react";

import { ApiError, getPlan, openPortal, redeemInvite, startCheckout, type Plan, type Tenant } from "../api";
import { Spinner } from "../components/Status";
import { useLoad } from "../load";

const message = (err: unknown) => (err instanceof Error ? err.message : String(err));

export function formatDate(iso: string | null): string {
  if (!iso) {
    return "";
  }
  return new Date(iso).toLocaleDateString(undefined, { year: "numeric", month: "long", day: "numeric" });
}

function price(plan: Plan): string | null {
  if (plan.amount === null || !plan.currency) {
    return null;
  }
  const amount = new Intl.NumberFormat(undefined, {
    style: "currency",
    currency: plan.currency.toUpperCase(),
    minimumFractionDigits: plan.amount % 100 === 0 ? 0 : 2,
  }).format(plan.amount / 100);
  return plan.interval ? `${amount} / ${plan.interval}` : amount;
}

/** Whole months the yearly plan saves over twelve monthly payments. */
export function monthsFree(plan: Plan, plans: Plan[]): number {
  const monthly = plans.find((other) => other.interval === "month");
  if (plan.interval !== "year" || !monthly || plan.amount === null || !monthly.amount || plan.currency !== monthly.currency) {
    return 0;
  }
  return Math.max(0, Math.round((12 * monthly.amount - plan.amount) / monthly.amount));
}

/** The plan step: pay for the licence, or join the beta with a code. */
export function ChoosePlan({
  tenant,
  onChange,
  confirming,
}: {
  tenant: Tenant;
  onChange: (tenant?: Tenant) => void;
  confirming: boolean;
}) {
  const [plan] = useLoad(getPlan, "plan");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const billingOff = plan.state === "error" && plan.error instanceof ApiError && plan.error.status === 503;

  const checkout = async () => {
    setBusy(true);
    setError(null);
    try {
      const { url } = await startCheckout(tenant.org.login);
      window.location.assign(url);
    } catch (err) {
      setError(message(err));
      setBusy(false);
    }
  };

  if (confirming) {
    return <Spinner label="Confirming your payment with Polar" />;
  }

  return (
    <>
      <p>
        One licence covers every repository in <strong>{tenant.org.login}</strong>. Runners run in
        your AWS account, so you pay AWS for compute directly.
      </p>
      {error && (
        <p className="notice notice-error" role="alert">
          {error}
        </p>
      )}
      {plan.state === "loading" && <Spinner label="Loading the plan" />}
      {plan.state === "ready" && (
        <div className="plan">
          <ul className="plan-options">
            {plan.data.plans.map((option) => {
              const free = monthsFree(option, plan.data.plans);
              return (
                <li key={option.id} className="plan-option">
                  <div>
                    <div className="plan-name">
                      {option.name}
                      {free > 0 && (
                        <span className="badge badge-success">
                          {free === 1 ? "1 month free" : `${free} months free`}
                        </span>
                      )}
                    </div>
                    {option.description && <p className="muted plan-description">{option.description}</p>}
                  </div>
                  {price(option) && <div className="plan-price">{price(option)}</div>}
                </li>
              );
            })}
          </ul>
          <div className="plan-buy">
            {plan.data.plans.length > 1 && <p className="muted plan-pick">You pick one at checkout.</p>}
            <button type="button" className="button button-primary" disabled={busy} onClick={checkout}>
              {busy ? "Opening checkout" : "Continue to checkout"}
            </button>
          </div>
        </div>
      )}
      {plan.state === "error" && !billingOff && (
        <p className="notice notice-error" role="alert">
          {plan.error.message}
        </p>
      )}
      {billingOff && (
        <p className="notice notice-accent">
          hatchrun is in an invite-only beta and free while it lasts. Enter the invite code you
          were given.
        </p>
      )}
      <InviteCode tenant={tenant} onRedeemed={onChange} open={billingOff} />
      <p className="fine">
        Payments are handled by Polar, our merchant of record, who sends the invoices and handles
        sales tax.
      </p>
    </>
  );
}

function InviteCode({
  tenant,
  onRedeemed,
  open,
}: {
  tenant: Tenant;
  onRedeemed: (tenant: Tenant) => void;
  open: boolean;
}) {
  const [asked, setAsked] = useState(false);
  const shown = asked || open;
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
        <button type="button" className="button-link" onClick={() => setAsked(true)}>
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

/** What the plan step says once the tenant holds a licence. */
export function PlanSummary({ tenant }: { tenant: Tenant }) {
  const { beta, subscription } = tenant.billing;
  if (beta) {
    return <>In the invite-only beta. Nothing to pay while it lasts.</>;
  }
  if (!subscription) {
    return null;
  }
  return (
    <>
      Licence active.{" "}
      {subscription.currentPeriodEnd &&
        (subscription.cancelAtPeriodEnd
          ? `Ends on ${formatDate(subscription.currentPeriodEnd)}. `
          : `Renews on ${formatDate(subscription.currentPeriodEnd)}. `)}
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
