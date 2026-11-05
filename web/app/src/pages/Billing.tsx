// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { Page } from "@/components/Page";
import { CiSettings } from "./ci/CiSettings";
import type { OrgPageProps } from "./types";

/** Placeholder for Settings and Settings, Billing: shows the old CI billing panel until the screen is built. */
export function Billing({ org }: OrgPageProps) {
  return (
    <Page title="Settings" meta={org} narrow>
      <CiSettings org={org} />
    </Page>
  );
}
