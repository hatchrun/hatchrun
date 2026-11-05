// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

// The Worker's tenant routes: the organizations the viewer can open,
// the GitHub App installations, and the billing portal hand-offs.

import { get, post } from "./core";

/** An installation of the App on an organization the user can see. */
export interface InstallationChoice {
  id: number;
  login: string;
  avatarUrl: string;
  suspended: boolean;
  /** The organization's login when the user is already in its tenant. */
  tenant: string | null;
}

export interface Installations {
  /** Installs the App on another account, tied to this session. */
  installUrl: string;
  installations: InstallationChoice[];
}

export interface TenantSummary {
  id: string;
  login: string;
  avatarUrl: string;
}

export interface Tenant {
  id: string;
  org: { id: number; login: string; avatarUrl: string };
  installationId: number | null;
  installUrl: string;
  /** The viewer's role in the organization. */
  role: "admin" | "member";
}

export const listInstallations = () => get<Installations>("/api/installations");
export const listTenants = () => get<TenantSummary[]>("/api/tenants");
export const getTenant = (org: string) => get<Tenant>(`/api/tenants/${encodeURIComponent(org)}`);
export const createTenant = (installationId: number) => post<Tenant>("/api/tenants", { installationId });

const tenantPath = (org: string) => `/api/tenants/${encodeURIComponent(org)}`;
// `back` sends Polar's return to the CI page instead of Settings.
export const startCheckout = (org: string, back?: "ci") =>
  post<{ url: string }>(`${tenantPath(org)}/checkout${back ? "?return=ci" : ""}`);
export const openPortal = (org: string, back?: "ci") =>
  post<{ url: string }>(`${tenantPath(org)}/portal${back ? "?return=ci" : ""}`);
