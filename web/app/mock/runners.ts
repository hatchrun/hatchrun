// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

// Runner profiles for the mock API: a few families and a rough copy of
// the Worker's choice of instance type (crates/cloud/src/instance.rs),
// good enough to work on the Runners page.

type Arch = "amd64" | "arm64";
type Capacity = "on-demand" | "spot" | "spot-with-fallback";

interface Profile {
  name: string;
  arch: Arch;
  categories: string[];
  minGeneration: number | null;
  families: string[];
  cpuMin: number;
  cpuMax: number | null;
  memoryMin: number;
  memoryMax: number | null;
  diskGib: number | null;
  capacity: Capacity;
  image: string;
  extras: string[];
  warmPool: number;
}

// [name, arch, GiB per vCPU, largest vCPUs, us-east-1 price per vCPU-hour in millionths]
const FAMILIES: [string, Arch, number, number, number][] = [
  ["c7g", "arm64", 2, 64, 36_300],
  ["m7g", "arm64", 4, 64, 40_800],
  ["r7g", "arm64", 8, 64, 53_600],
  ["c8g", "arm64", 2, 192, 39_880],
  ["m8g", "arm64", 4, 192, 44_880],
  ["r8g", "arm64", 8, 192, 58_910],
  ["c7i", "amd64", 2, 192, 44_625],
  ["m7i", "amd64", 4, 192, 50_400],
  ["r7i", "amd64", 8, 192, 66_150],
  ["c8i", "amd64", 2, 384, 46_855],
  ["m8i", "amd64", 4, 384, 52_920],
  ["r8i", "amd64", 8, 384, 69_460],
  ["m8a", "amd64", 4, 192, 50_100],
  ["c9g", "arm64", 2, 192, 37_900],
  ["m9g", "arm64", 4, 192, 42_600],
];
const category = (family: string) => family.match(/^[a-z]+/)?.[0] ?? "";
const generation = (family: string) => Number(family.match(/^[a-z]+(\d+)/)?.[1] ?? 0);
const SIZES = [1, 2, 4, 8, 16, 32, 48, 64, 96, 128, 192, 384];
const IMAGE = "ubuntu-26.04";
const STACK = {
  instanceTypes: ["*.medium", "*.large", "*.xlarge", "*.2xlarge", "*.4xlarge", "*.8xlarge"],
  excludedInstanceTypes: ["p*", "g*", "inf*", "trn*", "dl*", "f*", "vt*", "mac*", "*.metal*"],
  volumeSizeLimit: 200,
  rootVolumeSize: 30,
};

const sizeName = (vcpus: number) =>
  vcpus === 1 ? "medium" : vcpus === 2 ? "large" : vcpus === 4 ? "xlarge" : `${vcpus / 4}xlarge`;

const like = (pattern: string, type: string) =>
  new RegExp(`^${pattern.replace(".", "\\.").replace("*", ".*")}$`).test(type);
const allows = (connected: boolean, type: string) =>
  !connected ||
  (STACK.instanceTypes.some((pattern) => like(pattern, type)) &&
    !STACK.excludedInstanceTypes.some((pattern) => like(pattern, type)));

function builtIn(cpu: number, arch: Arch, name?: string): Profile {
  return {
    name: name ?? (arch === "amd64" ? `hatchrun-${cpu}cpu` : `hatchrun-${cpu}cpu-arm64`),
    arch,
    categories: ["m"],
    minGeneration: null,
    families: [],
    cpuMin: cpu,
    cpuMax: cpu,
    memoryMin: cpu * 4,
    memoryMax: null,
    diskGib: null,
    capacity: "on-demand",
    image: IMAGE,
    extras: [],
    warmPool: 0,
  };
}

const BUILT_INS: Profile[] = [
  builtIn(2, "amd64", "hatchrun"),
  ...[2, 4, 8, 16].map((cpu) => builtIn(cpu, "amd64")),
  ...[2, 4, 8, 16].map((cpu) => builtIn(cpu, "arm64")),
];

/** Each org's own profiles, by login. */
const custom = new Map<string, Profile[]>([
  [
    "acme-corp",
    [
      {
        ...builtIn(16, "arm64", "hatchrun-rust"),
        categories: ["c", "m"],
        minGeneration: 8,
        cpuMax: null,
        memoryMin: 32,
        diskGib: 150,
        capacity: "spot-with-fallback",
      },
      { ...builtIn(4, "amd64", "hatchrun-e2e"), categories: [], families: ["r8i"], memoryMin: 32 },
    ],
  ],
]);

function choose(profile: Profile, connected: boolean) {
  if (connected && profile.diskGib !== null && profile.diskGib > STACK.volumeSizeLimit) {
    return {
      problem: `${profile.name} asks for a ${profile.diskGib} GiB root volume and the connect stack allows at most ${STACK.volumeSizeLimit} GiB. Raise VolumeSizeLimit on the stack or ask for less.`,
    };
  }
  const fits: { type: string; vcpus: number; ratio: number; price: number; generation: number; allowed: boolean }[] = [];
  const kinds = FAMILIES.filter(
    ([name, arch]) =>
      arch === profile.arch &&
      (profile.families.length > 0
        ? profile.families.includes(name)
        : profile.categories.length === 0 || profile.categories.includes(category(name))) &&
      (profile.minGeneration === null || generation(name) >= profile.minGeneration),
  );
  for (const family of kinds) {
    const [name, , ratio, largest, price] = family;
    const sizes = SIZES.filter(
      (vcpus) =>
        vcpus <= largest &&
        vcpus >= profile.cpuMin &&
        (profile.cpuMax === null || vcpus <= profile.cpuMax) &&
        vcpus * ratio >= profile.memoryMin &&
        (profile.memoryMax === null || vcpus * ratio <= profile.memoryMax),
    );
    const allowed = sizes.find((vcpus) => allows(connected, `${name}.${sizeName(vcpus)}`));
    const vcpus = allowed ?? sizes[0];
    if (vcpus !== undefined) {
      fits.push({
        type: `${name}.${sizeName(vcpus)}`,
        vcpus,
        ratio,
        price,
        generation: generation(name),
        allowed: allowed !== undefined,
      });
    }
  }
  const ok = fits
    .filter((fit) => fit.allowed)
    .sort((a, b) => b.generation - a.generation || a.vcpus * a.price - b.vcpus * b.price);
  const first = ok[0];
  if (!first) {
    return {
      problem: fits.length
        ? `The connect stack allows none of the instance types that fit ${profile.name} (${fits.map((fit) => fit.type).join(", ")}). It allows ${STACK.instanceTypes.join(", ")} except ${STACK.excludedInstanceTypes.join(", ")}. Change InstanceTypes on the stack or change the profile.`
        : `us-east-1 offers no instance types for ${profile.arch} that fit ${profile.name}`,
    };
  }
  return {
    estimate: {
      instanceTypes: ok.map((fit) => fit.type),
      vcpus: first.vcpus,
      memoryGib: first.vcpus * first.ratio,
      diskGib: profile.diskGib ?? STACK.rootVolumeSize,
      hourlyMicros: first.vcpus * first.price,
    },
  };
}

function view(profile: Profile, source: string, connected: boolean) {
  const result = choose(profile, connected);
  return { ...profile, source, estimate: result.estimate ?? null, problem: result.problem ?? null };
}

/** GET /api/tenants/<org>/runners */
export function runners(org: string, region: string | null) {
  const connected = region !== null;
  const own = custom.get(org) ?? [];
  const profiles = [
    ...BUILT_INS.map((profile) => {
      const mine = own.find((candidate) => candidate.name === profile.name);
      return mine ? view(mine, "replaced", connected) : view(profile, "builtIn", connected);
    }),
    ...own
      .filter((profile) => !BUILT_INS.some((candidate) => candidate.name === profile.name))
      .sort((a, b) => a.name.localeCompare(b.name))
      .map((profile) => view(profile, "custom", connected)),
  ];
  return {
    region: region ?? "us-east-1",
    connected,
    limits: connected ? STACK : null,
    images: [IMAGE],
    families: FAMILIES.map(([name, arch, ratio, largest, price]) => ({
      name,
      category: category(name),
      generation: generation(name),
      special: false,
      arch,
      gibPerVcpu: ratio,
      sizes: SIZES.filter((vcpus) => vcpus <= largest).map((vcpus) => [sizeName(vcpus), vcpus]),
      offered: true,
      allowed: SIZES.some((vcpus) => vcpus <= largest && allows(connected, `${name}.${sizeName(vcpus)}`)),
      pricePerVcpu: price,
    })),
    profiles,
    maxProfiles: 50,
  };
}

function check(profile: Profile): string | null {
  if (!/^hatchrun(-[a-z0-9]+)*$/.test(profile.name)) {
    return "Name the profile hatchrun-something: lowercase letters, digits and single hyphens, at most 64 characters";
  }
  if (!(profile.cpuMin >= 1)) {
    return "Ask for 1 to 384 vCPUs";
  }
  if (!(profile.memoryMin >= 1)) {
    return "Ask for 1 to 3072 GiB of memory";
  }
  return null;
}

/** POST /api/tenants/<org>/runners/preview */
export function preview(profile: Profile, region: string | null) {
  const problem = check(profile);
  return problem ? { problem } : view(profile, "custom", region !== null);
}

/** POST, PUT and DELETE under /api/tenants/<org>/runners. */
export function change(
  org: string,
  region: string | null,
  method: string,
  name: string | null,
  profile: Profile | null,
): [number, unknown] {
  const own = custom.get(org) ?? [];
  custom.set(org, own);
  if (profile) {
    const problem = check(profile) ?? (region !== null ? (choose(profile, true).problem ?? null) : null);
    if (problem) {
      return [422, { error: problem }];
    }
  }
  const isBuiltIn = (candidate: string) => BUILT_INS.some((builtIn) => builtIn.name === candidate);
  const at = name === null ? -1 : own.findIndex((candidate) => candidate.name === name);
  if (method === "DELETE") {
    if (at < 0) {
      return [404, { error: `no runner profile ${name}` }];
    }
    own.splice(at, 1);
  } else if (method === "POST" && profile) {
    if (isBuiltIn(profile.name)) {
      return [409, { error: `${profile.name} is built in. Edit it to change it for ${org}.` }];
    }
    if (own.some((candidate) => candidate.name === profile.name)) {
      return [409, { error: `${profile.name} exists already` }];
    }
    own.push(profile);
  } else if (method === "PUT" && profile && name !== null) {
    if (at < 0 && !isBuiltIn(name)) {
      return [404, { error: `no runner profile ${name}` }];
    }
    if (at >= 0) {
      own.splice(at, 1);
    }
    own.push(profile);
  }
  return [200, runners(org, region)];
}
