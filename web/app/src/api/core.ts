// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

// The request plumbing of the Worker's /api: errors, the session and its
// CSRF token (every request that changes something carries it), and the
// query and paging shapes the list routes share.

export interface User {
  id: number;
  login: string;
  name: string | null;
  avatarUrl: string;
}

export interface Session {
  user: User;
  csrfToken: string;
}

export class ApiError extends Error {
  readonly status: number;
  /** Seconds the server asked to wait before trying again (503). */
  readonly retryAfter: number | null;

  constructor(status: number, message: string, retryAfter: number | null = null) {
    super(message.charAt(0).toUpperCase() + message.slice(1));
    this.status = status;
    this.retryAfter = retryAfter;
  }
}

export async function failure(response: Response): Promise<ApiError> {
  let message = `${response.status} ${response.statusText}`;
  try {
    const answer = (await response.json()) as { error?: string };
    if (answer.error) {
      message = answer.error;
    }
  } catch {
    // Not JSON: keep the status line.
  }
  const wait = Number(response.headers.get("Retry-After"));
  return new ApiError(response.status, message, Number.isFinite(wait) && wait > 0 ? wait : null);
}

let csrfToken = "";

export async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
  const headers: Record<string, string> = { Accept: "application/json" };
  if (method !== "GET") {
    headers["X-CSRF-Token"] = csrfToken;
  }
  if (body !== undefined) {
    headers["Content-Type"] = "application/json";
  }
  const response = await fetch(path, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
    credentials: "same-origin",
  });
  if (!response.ok) {
    throw await failure(response);
  }
  if (response.status === 204) {
    return undefined as T;
  }
  return (await response.json()) as T;
}

export const get = <T>(path: string) => request<T>("GET", path);
export const post = <T>(path: string, body?: unknown) => request<T>("POST", path, body ?? {});
export const put = <T>(path: string, body?: unknown) => request<T>("PUT", path, body ?? {});
export const del = <T>(path: string) => request<T>("DELETE", path);

/** The signed-in session, or null when nobody is signed in. */
export async function getSession(): Promise<Session | null> {
  try {
    const session = await get<Session>("/api/session");
    csrfToken = session.csrfToken;
    return session;
  } catch (err) {
    if (err instanceof ApiError && err.status === 401) {
      return null;
    }
    throw err;
  }
}

export function signInUrl(returnTo: string): string {
  return `/api/auth/login?return_to=${encodeURIComponent(returnTo)}`;
}

export const signOut = () => post<undefined>("/api/auth/logout");

/** A page of a list. `next` is an opaque cursor for the page after, null on the last. */
export interface Page<T> {
  items: T[];
  total: number;
  next: string | null;
}

/** The paging query every list route takes. `limit` is 1 to 100, 50 when not set. */
export interface Paging {
  cursor?: string | null;
  limit?: number;
}

/** A query string from the parameters that are set; arrays join with commas. "" when none is. */
export function queryString(params: Record<string, string | number | boolean | string[] | null | undefined>): string {
  const query = new URLSearchParams();
  for (const [name, value] of Object.entries(params)) {
    if (value === undefined || value === null || value === "" || (Array.isArray(value) && value.length === 0)) {
      continue;
    }
    query.set(name, Array.isArray(value) ? value.join(",") : String(value));
  }
  const text = query.toString();
  return text ? `?${text}` : "";
}

const MOST_WAIT_MS = 30_000;

/** Runs `call`; on 503 waits for the Retry-After it names, twice at most. */
export async function retrying<T>(call: () => Promise<T>): Promise<T> {
  for (let attempt = 0; ; attempt++) {
    try {
      return await call();
    } catch (err) {
      if (!(err instanceof ApiError) || err.status !== 503 || attempt >= 2) {
        throw err;
      }
      await new Promise((resolve) => setTimeout(resolve, Math.min((err.retryAfter ?? 2) * 1000, MOST_WAIT_MS)));
    }
  }
}

/** The settings routes answer 404 to a caller who is no admin; say so. */
const NOT_ADMIN = "Only organization or repository admins can change these settings.";

export async function adminOnly<T>(call: () => Promise<T>): Promise<T> {
  try {
    return await call();
  } catch (err) {
    if (err instanceof ApiError && err.status === 404) {
      throw new ApiError(404, NOT_ADMIN);
    }
    throw err;
  }
}
