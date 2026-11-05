// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { Page } from "@/components/Page";
import type { OrgPageProps } from "./types";

/** Placeholder for Setup: replaced by the screen build. */
export function Setup({ org }: OrgPageProps) {
  return (
    <Page title="Setup" meta={org}>
      <p className="meta">First run checklist. Not built yet.</p>
    </Page>
  );
}
