// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

import { useState, type FormEvent } from "react";

import { getAwsInstructions, saveSettings, type Tenant } from "../api";
import { Await } from "../components/Status";
import { useLoad } from "../load";
import { ChoosePlan, PlanSummary, PortalLink } from "./Billing";
import { AwsSummary, ConnectAws } from "./ConnectAws";
import { InstallApp } from "./TenantPage";

export function TenantSettings({ tenant, onChange }: { tenant: Tenant; onChange: (tenant: Tenant) => void }) {
  return (
    <div className="settings">
      <Limits tenant={tenant} onChange={onChange} />
      <AwsSection tenant={tenant} onChange={onChange} />
      <section className="card panel">
        <h2>Billing</h2>
        {tenant.billing.licensed ? (
          <p>
            <PlanSummary tenant={tenant} />
          </p>
        ) : (
          <ChoosePlan tenant={tenant} onChange={(next) => next && onChange(next)} confirming={false} />
        )}
        {tenant.billing.beta && tenant.billing.subscription && (
          <p className="fine">
            <PortalLink org={tenant.org.login} label="Open the Polar customer portal" />
          </p>
        )}
      </section>
      <section className="card panel">
        <h2>GitHub App</h2>
        {tenant.installationId ? (
          <p>
            Installed on {tenant.org.login}.{" "}
            <a
              href={`https://github.com/organizations/${tenant.org.login}/settings/installations/${tenant.installationId}`}
            >
              Choose repositories or uninstall on GitHub
            </a>
          </p>
        ) : (
          <InstallApp tenant={tenant} />
        )}
      </section>
      <section className="card panel">
        <h2>Tenant</h2>
        <p className="muted">
          Your tenant ID is <code>{tenant.id}</code>. The connect stack trusts tokens for this ID
          only.
        </p>
      </section>
    </div>
  );
}

function Limits({ tenant, onChange }: { tenant: Tenant; onChange: (tenant: Tenant) => void }) {
  const [instructions] = useLoad(() => getAwsInstructions(tenant.org.login), tenant.org.login);
  const [maxRunners, setMaxRunners] = useState(String(tenant.maxRunners));
  const [everywhere, setEverywhere] = useState(tenant.allowedRegions === null);
  const [regions, setRegions] = useState<string[]>(tenant.allowedRegions ?? []);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<{ ok: boolean; text: string } | null>(null);

  const save = async (event: FormEvent) => {
    event.preventDefault();
    setBusy(true);
    setMessage(null);
    try {
      const next = await saveSettings(tenant.org.login, Number(maxRunners), everywhere ? null : regions);
      onChange(next);
      setMessage({ ok: true, text: "Saved." });
    } catch (err) {
      setMessage({ ok: false, text: err instanceof Error ? err.message : String(err) });
    } finally {
      setBusy(false);
    }
  };

  const toggle = (id: string) =>
    setRegions((was) => (was.includes(id) ? was.filter((region) => region !== id) : [...was, id]));

  return (
    <section className="card panel">
      <h2>Runners</h2>
      <form onSubmit={save}>
        <div className="field field-narrow">
          <label htmlFor="max-runners">Most runners at once</label>
          <input
            id="max-runners"
            className="input"
            type="number"
            min={0}
            max={1000}
            value={maxRunners}
            onChange={(event) => setMaxRunners(event.target.value)}
            required
          />
          <p className="field-help">Jobs past this wait for a runner to finish. 0 pauses hatchrun.</p>
        </div>
        <fieldset className="field">
          <legend className="label">Allowed regions</legend>
          <label className="check">
            <input type="checkbox" checked={everywhere} onChange={(event) => setEverywhere(event.target.checked)} />
            Every region hatchrun runs in
          </label>
          {!everywhere && (
            <Await loaded={instructions}>
              {(data) => (
                <div className="check-grid">
                  {data.regions.map((region) => (
                    <label key={region.id} className="check">
                      <input type="checkbox" checked={regions.includes(region.id)} onChange={() => toggle(region.id)} />
                      <span>
                        {region.name} <span className="muted small">{region.id}</span>
                      </span>
                    </label>
                  ))}
                </div>
              )}
            </Await>
          )}
          <p className="field-help">hatchrun connects AWS and launches runners only in these regions.</p>
        </fieldset>
        {message && (
          <p className={message.ok ? "notice notice-success" : "notice notice-error"} role="status">
            {message.text}
          </p>
        )}
        <button type="submit" className="button button-primary" disabled={busy}>
          {busy ? "Saving" : "Save"}
        </button>
      </form>
    </section>
  );
}

function AwsSection({ tenant, onChange }: { tenant: Tenant; onChange: (tenant: Tenant) => void }) {
  const [changing, setChanging] = useState(false);
  return (
    <section className="card panel">
      <h2>AWS account</h2>
      {tenant.aws && !changing ? (
        <>
          <p>
            <AwsSummary tenant={tenant} />
          </p>
          <p className="muted small mono">{tenant.aws.roleArn}</p>
          <button type="button" className="button button-small" onClick={() => setChanging(true)}>
            Connect another stack
          </button>
        </>
      ) : (
        <ConnectAws
          tenant={tenant}
          onConnected={(next) => {
            onChange(next);
            setChanging(false);
          }}
        />
      )}
    </section>
  );
}
