// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { Page } from "@/components/Page";
import type { OrgPageProps } from "./types";

/** Placeholder for Secrets: replaced by the screen build. */
export function Secrets({ org }: OrgPageProps) {
  return (
    <Page title="Secrets" meta={org}>
      <p className="meta">Scope picker and secrets table. Not built yet.</p>
    </Page>
  );
}
