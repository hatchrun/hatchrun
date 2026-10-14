// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

// Runner profiles: the list workflows pick from, and the form that adds
// or changes one. The Worker checks every profile and works out what it
// launches; this page only shows it.

import { useEffect, useState, type FormEvent, type ReactNode } from "react";

import {
  createProfile,
  deleteProfile,
  listRunners,
  previewProfile,
  replaceProfile,
  type Arch,
  type Estimate,
  type Family,
  type Preview,
  type Profile,
  type ProfileView,
  type Runners,
  type Tenant,
} from "../api";
import { CopyButton } from "../components/CopyButton";
import { Await } from "../components/Status";
import { useLoad } from "../load";
import { Link, navigate } from "../router";

/** `$0.090` for millionths of a dollar. */
export function dollars(micros: number): string {
  const value = micros / 1_000_000;
  return `$${value.toFixed(value < 0.01 ? 4 : value < 1 ? 3 : 2)}`;
}

const ARCH: Record<Arch, string> = { amd64: "x86-64", arm64: "Arm" };
const SOURCE: Record<ProfileView["source"], { label: string; tone: string }> = {
  builtIn: { label: "Built in", tone: "" },
  replaced: { label: "Changed", tone: " badge-accent" },
  custom: { label: "Custom", tone: " badge-success" },
};

export function RunnersPage({ tenant, item }: { tenant: Tenant; item: string | undefined }) {
  const org = tenant.org.login;
  const [loaded, reload] = useLoad(() => listRunners(org), org);
  const [runners, setRunners] = useState<Runners | null>(null);
  const data = runners ?? (loaded.state === "ready" ? loaded.data : null);

  return (
    <Await loaded={data ? { state: "ready", data } : loaded} retry={reload}>
      {(current) =>
        item === undefined ? (
          <ProfileList org={org} runners={current} />
        ) : (
          <Editor
            key={item}
            org={org}
            runners={current}
            name={item === "new" ? null : item}
            onSaved={(next) => {
              setRunners(next);
              navigate(`/o/${org}/runners`);
            }}
          />
        )
      }
    </Await>
  );
}

function ProfileList({ org, runners }: { org: string; runners: Runners }) {
  const custom = runners.profiles.filter((profile) => profile.source !== "builtIn").length;
  return (
    <section className="runners">
      <div className="page-head">
        <div>
          <p className="eyebrow">Runners</p>
          <h1>Runner profiles</h1>
        </div>
        {custom < runners.maxProfiles && (
          <Link href={`/o/${org}/runners/new`} className="button button-primary">
            New profile
          </Link>
        )}
      </div>
      <p className="muted runners-intro">
        A job names one profile in <code>runs-on</code>, optionally with overrides such as{" "}
        <code>/disk=200</code> or <code>/spot=true</code>. Prices are on-demand Linux in{" "}
        {runners.region}, before any Savings Plan or Spot discount.
      </p>
      <StackLimits runners={runners} />
      <div className="card table-wrap">
        <table className="jobs runner-table">
          <thead>
            <tr>
              <th scope="col">Profile</th>
              <th scope="col">Runner</th>
              <th scope="col">Instance type</th>
              <th scope="col">Disk</th>
              <th scope="col">Market</th>
              <th scope="col" className="numeric">
                Per hour
              </th>
              <th scope="col">
                <span className="visually-hidden">Actions</span>
              </th>
            </tr>
          </thead>
          <tbody>
            {runners.profiles.map((profile) => (
              <ProfileRow key={profile.name} org={org} profile={profile} />
            ))}
          </tbody>
        </table>
      </div>
    </section>
  );
}

function StackLimits({ runners }: { runners: Runners }) {
  if (!runners.limits) {
    return (
      <p className="notice notice-accent">
        Connect AWS to check profiles against your connect stack. Until then every family is
        offered.
      </p>
    );
  }
  return (
    <p className="fine runners-limits">
      Your connect stack allows <code>{runners.limits.instanceTypes.join(", ")}</code> and root
      volumes up to {runners.limits.volumeSizeLimit} GiB. AWS refuses anything else, so profiles
      get only those.
    </p>
  );
}

function ProfileRow({ org, profile }: { org: string; profile: ProfileView }) {
  const source = SOURCE[profile.source];
  const estimate = profile.estimate;
  return (
    <>
      <tr className={profile.problem ? "has-problem" : undefined}>
        <td data-label="Profile">
          <div className="profile-name">
            <code>{profile.name}</code>
            <span className={`badge${source.tone}`}>{source.label}</span>
          </div>
        </td>
        <td data-label="Runner" className="nowrap">
          <div>
            {estimate ? `${estimate.vcpus} vCPU, ${estimate.memoryGib} GiB` : `${profile.cpuMin} vCPU`}
            <div className="muted small">{ARCH[profile.arch]}</div>
          </div>
        </td>
        <td data-label="Instance type" className="nowrap">
          <div>{estimate ? <InstanceTypes estimate={estimate} /> : <span className="muted">None</span>}</div>
        </td>
        <td data-label="Disk" className="nowrap">
          {estimate ? `${estimate.diskGib} GiB` : profile.diskGib ? `${profile.diskGib} GiB` : "Default"}
        </td>
        <td data-label="Market" className="nowrap">{profile.spot ? "Spot" : "On-demand"}</td>
        <td data-label="Per hour" className="numeric">
          {estimate ? dollars(estimate.hourlyMicros) : "-"}
        </td>
        <td className="row-actions">
          <CopyButton text={`runs-on: ${profile.name}`} label="Copy runs-on" />
          <Link href={`/o/${org}/runners/${profile.name}`} className="button button-small">
            Edit<span className="visually-hidden"> {profile.name}</span>
          </Link>
        </td>
      </tr>
      {profile.problem && (
        <tr className="problem-row">
          <td colSpan={7}>
            <p className="job-error" role="note">
              {profile.problem}
            </p>
          </td>
        </tr>
      )}
    </>
  );
}

function InstanceTypes({ estimate }: { estimate: Estimate }) {
  const [first, ...rest] = estimate.instanceTypes;
  return (
    <>
      <span className="mono">{first}</span>
      {rest.length > 0 && <div className="muted small">then {rest.join(", ")}</div>}
    </>
  );
}

/** The form's fields, as typed. */
interface Draft {
  name: string;
  arch: Arch;
  families: string[];
  cpuMin: string;
  cpuMax: string;
  memoryMin: string;
  memoryMax: string;
  diskGib: string;
  spot: boolean;
  image: string;
}

const text = (value: number | null) => (value === null ? "" : String(value));
const number = (value: string) => (value.trim() === "" ? null : Number(value));

function draftOf(profile: Profile): Draft {
  return {
    name: profile.name,
    arch: profile.arch,
    families: profile.families,
    cpuMin: text(profile.cpuMin),
    cpuMax: text(profile.cpuMax),
    memoryMin: text(profile.memoryMin),
    memoryMax: text(profile.memoryMax),
    diskGib: text(profile.diskGib),
    spot: profile.spot,
    image: profile.image,
  };
}

function profileOf(draft: Draft, base: Profile | null): Profile {
  return {
    name: draft.name.trim().toLowerCase(),
    arch: draft.arch,
    families: draft.families,
    cpuMin: number(draft.cpuMin) ?? 0,
    cpuMax: number(draft.cpuMax),
    memoryMin: number(draft.memoryMin) ?? 0,
    memoryMax: number(draft.memoryMax),
    diskGib: number(draft.diskGib),
    spot: draft.spot,
    image: draft.image,
    extras: base?.extras ?? [],
    warmPool: base?.warmPool ?? 0,
  };
}

function newDraft(runners: Runners): Draft {
  return {
    name: "hatchrun-",
    arch: "arm64",
    families: ["m8g", "m7g"].filter((name) =>
      runners.families.some((family) => family.name === name && family.allowed && family.offered),
    ),
    cpuMin: "4",
    cpuMax: "",
    memoryMin: "16",
    memoryMax: "",
    diskGib: "",
    spot: false,
    image: runners.images[0] ?? "",
  };
}

function Editor({
  org,
  runners,
  name,
  onSaved,
}: {
  org: string;
  runners: Runners;
  name: string | null;
  onSaved: (runners: Runners) => void;
}) {
  const existing = name === null ? null : (runners.profiles.find((profile) => profile.name === name) ?? null);
  const [draft, setDraft] = useState<Draft>(() => (existing ? draftOf(existing) : newDraft(runners)));
  const [preview, setPreview] = useState<Preview | null>(existing);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const builtIn = existing !== null && existing.source !== "custom";
  const profile = profileOf(draft, existing);
  const key = JSON.stringify(profile);

  useEffect(() => {
    const timer = window.setTimeout(() => {
      previewProfile(org, JSON.parse(key) as Profile).then(setPreview, (err: unknown) =>
        setPreview({ problem: err instanceof Error ? err.message : String(err) }),
      );
    }, 300);
    return () => window.clearTimeout(timer);
  }, [org, key]);

  if (name !== null && !existing) {
    return (
      <section className="narrow">
        <h1>No profile {name}</h1>
        <p className="muted">
          <Link href={`/o/${org}/runners`}>Back to runner profiles</Link>
        </p>
      </section>
    );
  }

  const set = (change: Partial<Draft>) => setDraft((was) => ({ ...was, ...change }));
  const act = async (call: () => Promise<Runners>) => {
    setBusy(true);
    setError(null);
    try {
      onSaved(await call());
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  };
  const save = (event: FormEvent) => {
    event.preventDefault();
    void act(() => (name === null ? createProfile(org, profile) : replaceProfile(org, name, profile)));
  };
  const remove = () => {
    const question = builtIn
      ? `Undo your changes to ${name}? Jobs get the built-in profile again.`
      : `Delete ${name}? Jobs that name it will be skipped.`;
    if (name !== null && window.confirm(question)) {
      void act(() => deleteProfile(org, name));
    }
  };

  const limits = runners.limits;
  return (
    <section className="runners">
      <div className="page-head">
        <div>
          <p className="eyebrow">
            <Link href={`/o/${org}/runners`}>Runner profiles</Link>
          </p>
          <h1>{name === null ? "New profile" : name}</h1>
        </div>
      </div>
      <div className="editor">
        <form className="card panel editor-form" onSubmit={save}>
          <div className="field">
            <label htmlFor="profile-name">Name</label>
            <input
              id="profile-name"
              className="input input-mono"
              value={draft.name}
              onChange={(event) => set({ name: event.target.value })}
              disabled={builtIn}
              autoComplete="off"
              spellCheck={false}
              required
            />
            <p className="field-help">
              {builtIn
                ? "Built-in profiles keep their name. Your changes apply to every workflow in this organization that names it."
                : "Starts with hatchrun-, then lowercase letters, digits and single hyphens."}
            </p>
          </div>

          <fieldset className="field">
            <legend className="label">Architecture</legend>
            <div className="segmented">
              {(["amd64", "arm64"] as Arch[]).map((arch) => (
                <label key={arch} className="segment">
                  <input
                    type="radio"
                    name="arch"
                    checked={draft.arch === arch}
                    onChange={() => set({ arch, families: [] })}
                  />
                  <span>
                    {ARCH[arch]} <span className="muted small mono">{arch}</span>
                  </span>
                </label>
              ))}
            </div>
          </fieldset>

          <Families
            families={runners.families.filter((family) => family.arch === draft.arch)}
            chosen={draft.families}
            region={runners.region}
            onChange={(families) => set({ families })}
          />

          <div className="field-pair">
            <NumberField id="cpu-min" label="Fewest vCPUs" value={draft.cpuMin} onChange={(cpuMin) => set({ cpuMin })} required />
            <NumberField id="cpu-max" label="Most vCPUs" value={draft.cpuMax} onChange={(cpuMax) => set({ cpuMax })} placeholder="No limit" />
          </div>
          <div className="field-pair">
            <NumberField id="memory-min" label="Least memory (GiB)" value={draft.memoryMin} onChange={(memoryMin) => set({ memoryMin })} required />
            <NumberField id="memory-max" label="Most memory (GiB)" value={draft.memoryMax} onChange={(memoryMax) => set({ memoryMax })} placeholder="No limit" />
          </div>
          <NumberField
            id="disk"
            label="Root volume (GiB)"
            value={draft.diskGib}
            onChange={(diskGib) => set({ diskGib })}
            placeholder={limits ? `${limits.rootVolumeSize}, the stack default` : "The stack default"}
            help={limits ? `Your connect stack allows up to ${limits.volumeSizeLimit} GiB.` : undefined}
          />

          <fieldset className="field">
            <legend className="label">Market</legend>
            <label className="check">
              <input type="radio" name="market" checked={!draft.spot} onChange={() => set({ spot: false })} />
              On-demand
            </label>
            <label className="check">
              <input type="radio" name="market" checked={draft.spot} onChange={() => set({ spot: true })} />
              Spot, then on-demand when Spot has no capacity
            </label>
            <p className="field-help">
              Spot usually costs well under the on-demand price, and AWS can take a Spot runner
              back with two minutes&apos; notice, which fails the job.
            </p>
          </fieldset>

          <div className="field field-narrow">
            <label htmlFor="image">Image</label>
            <select id="image" className="input" value={draft.image} onChange={(event) => set({ image: event.target.value })}>
              {runners.images.map((image) => (
                <option key={image} value={image}>
                  {image}
                </option>
              ))}
            </select>
          </div>

          {error && (
            <p className="notice notice-error" role="alert">
              {error}
            </p>
          )}
          <div className="editor-actions">
            <button type="submit" className="button button-primary" disabled={busy}>
              {busy ? "Saving" : name === null ? "Add profile" : "Save"}
            </button>
            <Link href={`/o/${org}/runners`} className="button">
              Cancel
            </Link>
            {existing && existing.source !== "builtIn" && (
              <button type="button" className="button button-link danger" onClick={remove} disabled={busy}>
                {builtIn ? "Undo changes" : "Delete profile"}
              </button>
            )}
          </div>
        </form>
        <aside className="card panel editor-preview" aria-live="polite">
          <h2>What a job gets</h2>
          <Summary name={profile.name} preview={preview} region={runners.region} />
        </aside>
      </div>
    </section>
  );
}

function NumberField({
  id,
  label,
  value,
  onChange,
  placeholder,
  help,
  required,
}: {
  id: string;
  label: string;
  value: string;
  onChange: (value: string) => void;
  placeholder?: string;
  help?: ReactNode;
  required?: boolean;
}) {
  return (
    <div className="field">
      <label htmlFor={id}>{label}</label>
      <input
        id={id}
        className="input"
        type="number"
        inputMode="numeric"
        min={1}
        value={value}
        placeholder={placeholder}
        onChange={(event) => onChange(event.target.value)}
        required={required}
      />
      {help && <p className="field-help">{help}</p>}
    </div>
  );
}

function Families({
  families,
  chosen,
  region,
  onChange,
}: {
  families: Family[];
  chosen: string[];
  region: string;
  onChange: (families: string[]) => void;
}) {
  const move = (at: number, by: number) => {
    const next = [...chosen];
    const [name] = next.splice(at, 1);
    if (name !== undefined) {
      next.splice(at + by, 0, name);
    }
    onChange(next);
  };
  const left = families.filter((family) => !chosen.includes(family.name));
  const why = (family: Family) =>
    !family.offered ? ` (not in ${region})` : !family.allowed ? " (not allowed by your stack)" : "";
  return (
    <fieldset className="field">
      <legend className="label">Instance families</legend>
      <p className="field-help families-help">
        Jobs get the smallest size that fits in the first family. The others are tried in order
        when AWS has no capacity.
      </p>
      {chosen.length > 0 && (
        <ol className="family-list">
          {chosen.map((name, at) => {
            const family = families.find((candidate) => candidate.name === name);
            return (
              <li key={name} className="family-item">
                <span className="family-rank">{at + 1}</span>
                <span className="family-name mono">{name}</span>
                <span className="muted small family-detail">
                  {family ? `${family.gibPerVcpu} GiB per vCPU${why(family)}` : "unknown family"}
                </span>
                <span className="family-buttons">
                  <button type="button" className="button button-small" onClick={() => move(at, -1)} disabled={at === 0} aria-label={`Move ${name} up`}>
                    Up
                  </button>
                  <button
                    type="button"
                    className="button button-small"
                    onClick={() => move(at, 1)}
                    disabled={at === chosen.length - 1}
                    aria-label={`Move ${name} down`}
                  >
                    Down
                  </button>
                  <button
                    type="button"
                    className="button button-small"
                    onClick={() => onChange(chosen.filter((other) => other !== name))}
                    aria-label={`Remove ${name}`}
                  >
                    Remove
                  </button>
                </span>
              </li>
            );
          })}
        </ol>
      )}
      {left.length > 0 && chosen.length < 8 && (
        <select
          className="input add-family"
          aria-label="Add an instance family"
          value=""
          onChange={(event) => event.target.value && onChange([...chosen, event.target.value])}
        >
          <option value="">Add a family</option>
          {left.map((family) => (
            <option key={family.name} value={family.name} disabled={!family.offered || !family.allowed}>
              {family.name}: {family.gibPerVcpu} GiB per vCPU
              {family.pricePerVcpu !== null ? `, ${dollars(family.pricePerVcpu)} per vCPU-hour` : ""}
              {why(family)}
            </option>
          ))}
        </select>
      )}
    </fieldset>
  );
}

function Summary({ name, preview, region }: { name: string; preview: Preview | null; region: string }) {
  const snippet = `runs-on: ${name}`;
  const estimate = preview?.estimate;
  return (
    <>
      <div className="snippet">
        <pre>
          <code>{snippet}</code>
        </pre>
        <CopyButton text={snippet} />
      </div>
      {preview?.problem ? (
        <p className="notice notice-error" role="alert">
          {preview.problem}
        </p>
      ) : estimate ? (
        <dl className="summary">
          <div>
            <dt>Instance type</dt>
            <dd>
              <InstanceTypes estimate={estimate} />
            </dd>
          </div>
          <div>
            <dt>Size</dt>
            <dd>
              {estimate.vcpus} vCPU, {estimate.memoryGib} GiB, {estimate.diskGib} GiB disk
            </dd>
          </div>
          <div>
            <dt>On-demand price</dt>
            <dd>
              <strong>{dollars(estimate.hourlyMicros)}</strong> per hour in {region}
              <div className="muted small">
                {dollars(estimate.hourlyMicros / 60)} per job minute, billed by AWS
              </div>
            </dd>
          </div>
        </dl>
      ) : (
        <p className="muted">Working it out</p>
      )}
    </>
  );
}
