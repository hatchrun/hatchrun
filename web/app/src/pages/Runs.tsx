// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { Page } from "@/components/Page";
import type { OrgPageProps } from "./types";

/** Placeholder for Runs: replaced by the screen build. */
export function Runs({ org }: OrgPageProps) {
  return (
    <Page title="Runs" meta={org}>
      <p className="meta">Org runs list with filters, needs-you block and paging. Not built yet.</p>
    </Page>
  );
}
