// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { useState, type FormEvent } from "react";

import { connectAws, getAwsInstructions, type AwsInstructions, type Tenant } from "../api";
import { CopyButton } from "../components/CopyButton";
import { Await } from "../components/Status";
import { useLoad } from "../load";

/** The AWS step: create the connect stack, then check its role. */
export function ConnectAws({ tenant, onConnected }: { tenant: Tenant; onConnected: (tenant: Tenant) => void }) {
  const org = tenant.org.login;
  const [instructions, reload] = useLoad(() => getAwsInstructions(org), org);
  return (
    <Await loaded={instructions} retry={reload}>
      {(data) => <Steps tenant={tenant} instructions={data} onConnected={onConnected} />}
    </Await>
  );
}

function Steps({
  tenant,
  instructions,
  onConnected,
}: {
  tenant: Tenant;
  instructions: AwsInstructions;
  onConnected: (tenant: Tenant) => void;
}) {
  const [region, setRegion] = useState(
    tenant.aws?.region ??
      instructions.regions.find((candidate) => candidate.allowed && candidate.id === "us-east-1")?.id ??
      instructions.regions.find((candidate) => candidate.allowed)?.id ??
      "",
  );
  const [roleArn, setRoleArn] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const allowed = instructions.regions.filter((candidate) => candidate.allowed);
  const chosen = allowed.find((candidate) => candidate.id === region);

  const verify = async (event: FormEvent) => {
    event.preventDefault();
    setBusy(true);
    setError(null);
    try {
      onConnected(await connectAws(tenant.org.login, region, roleArn.trim()));
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="substeps">
      <p>
        Runners start in your AWS account, so you control the network, see every instance and pay
        AWS directly. A CloudFormation stack gives hatchrun a role that can only launch AWS
        Marketplace images your account subscribed to, from its own launch template, and manage
        the instances it started. No access keys are created.
      </p>

      <section className="substep">
        <h3>
          <span className="substep-number">1</span>Subscribe to the runner images
        </h3>
        <p>
          The runner images are free in AWS Marketplace; you pay AWS only for the instances. Subscribe
          this account to both once, signed in to the account you connect.
        </p>
        <p className="inline-actions">
          <a className="button" href={instructions.marketplace.amd64} target="_blank" rel="noreferrer">
            Subscribe to amd64
          </a>
          <a className="button" href={instructions.marketplace.arm64} target="_blank" rel="noreferrer">
            Subscribe to arm64
          </a>
        </p>
      </section>

      <section className="substep">
        <h3>
          <span className="substep-number">2</span>Create the stack
        </h3>
        <div className="field">
          <label htmlFor="aws-region">Region</label>
          <select
            id="aws-region"
            className="input"
            value={region}
            onChange={(event) => setRegion(event.target.value)}
          >
            {allowed.map((candidate) => (
              <option key={candidate.id} value={candidate.id}>
                {candidate.name} ({candidate.id})
              </option>
            ))}
          </select>
          {chosen && !chosen.hasImage && (
            <p className="field-help">Runner images are not published in {chosen.id} yet.</p>
          )}
        </div>
        <div className="field">
          <span className="label">Tenant ID</span>
          <div className="copy-field">
            <code>{instructions.tenantId}</code>
            <CopyButton text={instructions.tenantId} />
          </div>
          <p className="field-help">The link fills this in. Only hatchrun requests for it can use the role.</p>
        </div>
        {chosen?.launchUrl ? (
          <a className="button button-accent" href={chosen.launchUrl} target="_blank" rel="noreferrer">
            Launch Stack in {chosen.id}
          </a>
        ) : (
          <p className="notice">The connect template is not published yet, so there is no Launch Stack link.</p>
        )}
        <details className="details">
          <summary>What the stack creates</summary>
          <ul>
            <li>An IAM OIDC provider for oidc.hatchrun.io, once per account.</li>
            <li>
              A role hatchrun assumes with a five-minute token for your tenant. It can launch AWS
              Marketplace images your account subscribed to, of the instance types and volume sizes you
              allow, and stop or terminate only the instances it launched. Runner profiles offer only
              those types and sizes.
            </li>
            <li>A launch template with IMDSv2 and encrypted volumes.</li>
            <li>
              A VPC with two public subnets and a security group that allows no inbound traffic, unless
              you pick an existing VPC and subnets.
            </li>
          </ul>
          <p>
            Connecting a second region of the same account? Paste the first stack&apos;s{" "}
            <code>OidcProviderArn</code> output into the stack&apos;s OIDC provider parameter, since an
            account holds one provider per URL.
          </p>
        </details>
      </section>

      <section className="substep">
        <h3>
          <span className="substep-number">3</span>Paste the role
        </h3>
        <form onSubmit={verify}>
          <div className="field">
            <label htmlFor="role-arn">RoleArn output</label>
            <input
              id="role-arn"
              className="input input-mono"
              placeholder="arn:aws:iam::111111111111:role/hatchrun-ControlPlaneRole-..."
              value={roleArn}
              onChange={(event) => setRoleArn(event.target.value)}
              autoComplete="off"
              spellCheck={false}
              required
            />
            <p className="field-help">
              In the CloudFormation console, open the stack&apos;s Outputs tab once it shows
              CREATE_COMPLETE.
            </p>
          </div>
          {error && (
            <p className="notice notice-error" role="alert">
              {error}
            </p>
          )}
          <button type="submit" className="button button-primary" disabled={busy || roleArn.trim() === ""}>
            {busy ? "Checking the stack" : "Verify and connect"}
          </button>
        </form>
      </section>
    </div>
  );
}

/** What the AWS step says once connected. */
export function AwsSummary({ tenant }: { tenant: Tenant }) {
  if (!tenant.aws) {
    return null;
  }
  return (
    <>
      Connected in {tenant.aws.region}: launch template <code>{tenant.aws.launchTemplateId}</code>,{" "}
      {tenant.aws.subnetIds.length} subnet{tenant.aws.subnetIds.length === 1 ? "" : "s"}.
    </>
  );
}
