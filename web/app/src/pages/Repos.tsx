// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { Page } from "@/components/Page";
import type { OrgPageProps } from "./types";

/** Placeholder for Repos: replaced by the screen build. */
export function Repos({ org }: OrgPageProps) {
  return (
    <Page title="Repos" meta={org}>
      <p className="meta">Repository list, enable switch and search. Not built yet.</p>
    </Page>
  );
}
