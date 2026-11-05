// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import type { Session, Tenant, User } from "@/api";
import type { CiRoute } from "@/router";

/** What every page under /o/:org gets from the router (App.tsx). */
export interface OrgPageProps {
  /** The organization's login. */
  org: string;
  tenant: Tenant;
  user: User;
}

/** A run or job page, which also reads signed out for a public repository. */
export interface RunPageProps {
  session: Session | null;
  route: CiRoute;
}
