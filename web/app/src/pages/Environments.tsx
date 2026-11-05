// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { Page } from "@/components/Page";
import type { OrgPageProps } from "./types";

/** Placeholder for Environments: replaced by the screen build. */
export function Environments({ org }: OrgPageProps) {
  return (
    <Page title="Environments" meta={org}>
      <p className="meta">Org environments table. Not built yet.</p>
    </Page>
  );
}
