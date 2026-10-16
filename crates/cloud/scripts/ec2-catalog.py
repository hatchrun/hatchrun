# SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
#
# SPDX-License-Identifier: AGPL-3.0-only

"""Writes src/catalog.rs: the instance families runner profiles can use,
their sizes and their Linux on-demand price in every region hatchrun
runs in, from the price lists behind aws.amazon.com/ec2/pricing.

    python3 scripts/ec2-catalog.py > src/catalog.rs

Prices change rarely; run it when AWS adds a family or a region.
"""

import datetime
import gzip
import json
import sys
import urllib.parse
import urllib.request

# Families profiles may name, as (family, architecture). Current
# generation general purpose (m), compute (c) and memory (r) families
# without local disks; the "flex" families are cheaper and give up
# sustained full CPU.
FAMILIES = [
    ("c7g", "Arm64"), ("m7g", "Arm64"), ("r7g", "Arm64"),
    ("c8g", "Arm64"), ("m8g", "Arm64"), ("r8g", "Arm64"),
    ("c9g", "Arm64"), ("m9g", "Arm64"), ("r9g", "Arm64"),
    ("c7i", "Amd64"), ("m7i", "Amd64"), ("r7i", "Amd64"),
    ("c7i-flex", "Amd64"), ("m7i-flex", "Amd64"),
    ("c8i", "Amd64"), ("m8i", "Amd64"), ("r8i", "Amd64"),
    ("c8i-flex", "Amd64"), ("m8i-flex", "Amd64"),
    ("c7a", "Amd64"), ("m7a", "Amd64"), ("r7a", "Amd64"),
    ("c8a", "Amd64"), ("m8a", "Amd64"), ("r8a", "Amd64"),
]

# connect::REGIONS, with the location names the price lists use.
REGIONS = [
    ("us-east-1", "US East (N. Virginia)"),
    ("us-east-2", "US East (Ohio)"),
    ("us-west-1", "US West (N. California)"),
    ("us-west-2", "US West (Oregon)"),
    ("ca-central-1", "Canada (Central)"),
    ("sa-east-1", "South America (Sao Paulo)"),
    ("eu-west-1", "EU (Ireland)"),
    ("eu-west-2", "EU (London)"),
    ("eu-west-3", "EU (Paris)"),
    ("eu-central-1", "EU (Frankfurt)"),
    ("eu-north-1", "EU (Stockholm)"),
    ("ap-south-1", "Asia Pacific (Mumbai)"),
    ("ap-southeast-1", "Asia Pacific (Singapore)"),
    ("ap-southeast-2", "Asia Pacific (Sydney)"),
    ("ap-northeast-1", "Asia Pacific (Tokyo)"),
    ("ap-northeast-2", "Asia Pacific (Seoul)"),
    ("ap-northeast-3", "Asia Pacific (Osaka)"),
]

URL = (
    "https://b0.p.awsstatic.com/pricing/2.0/meteredUnitMaps/ec2/USD/current/"
    "ec2-ondemand-without-sec-sel/{}/Linux/index.json"
)


def size_vcpus(size):
    """vCPUs of a size name, or None for metal and unusual sizes."""
    if size == "medium":
        return 1
    if size == "large":
        return 2
    if size == "xlarge":
        return 4
    if size.endswith("xlarge") and size[:-6].isdigit():
        return int(size[:-6]) * 4
    return None


def region_types(name):
    with urllib.request.urlopen(URL.format(urllib.parse.quote(name))) as response:
        body = response.read()
    # The bucket serves the lists gzipped whatever the request asks for.
    if body[:2] == b"\x1f\x8b":
        body = gzip.decompress(body)
    data = json.loads(body)
    (types,) = data["regions"].values()
    return [
        entry
        for entry in types.values()
        if entry.get("Pre Installed S/W", "NA") == "NA"
        and entry.get("License Model", "No License required") == "No License required"
    ]


def main():
    # family -> {"ratio": GiB per vCPU, "sizes": set, "prices": {region: per vCPU}}
    found = {family: {"sizes": set(), "prices": {}} for family, _ in FAMILIES}
    for region, name in REGIONS:
        for entry in region_types(name):
            family, size = entry["Instance Type"].split(".", 1)
            if family not in found:
                continue
            vcpus = size_vcpus(size)
            if vcpus is None or vcpus != int(entry["vCPU"]):
                continue
            info = found[family]
            memory = float(entry["Memory"].split()[0])
            info["ratio"] = round(memory / vcpus)
            info["sizes"].add(vcpus)
            per_vcpu = round(float(entry["price"]) / vcpus * 1_000_000)
            # Bigger sizes round the same price differently; keep the
            # smallest size's.
            if region not in info["prices"] or vcpus < info["prices"][region][0]:
                info["prices"][region] = (vcpus, per_vcpu)

    out = sys.stdout
    # REUSE-IgnoreStart: the header of the file written, not of this one.
    out.write(
        "// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>\n"
        "//\n"
        "// SPDX-License-Identifier: AGPL-3.0-only\n\n"
    )
    # REUSE-IgnoreEnd
    out.write(
        "//! Instance families runner profiles can use. Written by\n"
        "//! `scripts/ec2-catalog.py` from AWS's public price lists on "
        f"{datetime.date.today().isoformat()};\n"
        "//! run it again rather than editing this file.\n\n"
        "use crate::labels::Arch;\n\n"
        "/// An instance family.\n"
        "#[derive(Debug, Clone, Copy, PartialEq, Eq)]\n"
        "pub struct Family {\n"
        "    /// As in `m8g`.\n"
        "    pub name: &'static str,\n"
        "    /// What it runs.\n"
        "    pub arch: Arch,\n"
        "    /// Memory per vCPU in GiB.\n"
        "    pub gib_per_vcpu: u16,\n"
        "    /// vCPUs of each size, smallest first: 1 is `medium`, 2 `large`,\n"
        "    /// 4 `xlarge`, and 4N `Nxlarge`.\n"
        "    pub sizes: &'static [u16],\n"
        "    /// Linux on-demand price per vCPU-hour in millionths of a US\n"
        "    /// dollar, by region. A region it is missing from does not offer\n"
        "    /// it.\n"
        "    pub prices: &'static [(&'static str, u32)],\n"
        "}\n\n"
        "/// Every family, in the order the app lists them.\n"
        "pub const FAMILIES: &[Family] = &[\n"
    )
    for family, arch in FAMILIES:
        info = found[family]
        if not info["prices"]:
            sys.exit(f"{family}: not offered in any region")
        sizes = ", ".join(str(size) for size in sorted(info["sizes"]))
        out.write(
            f"    Family {{\n"
            f"        name: \"{family}\",\n"
            f"        arch: Arch::{arch},\n"
            f"        gib_per_vcpu: {info['ratio']},\n"
            f"        sizes: &[{sizes}],\n"
            f"        prices: &[\n"
        )
        for region, _ in REGIONS:
            if region in info["prices"]:
                out.write(f"            (\"{region}\", {info['prices'][region][1]:_}),\n")
        out.write("        ],\n    },\n")
    out.write("];\n")


main()
