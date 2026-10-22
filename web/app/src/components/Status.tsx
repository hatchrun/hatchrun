// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import type { Loaded } from "../load";

export function Spinner({ label = "Loading" }: { label?: string }) {
  return (
    <div className="spinner-row" role="status">
      <span className="spinner" aria-hidden="true" />
      <span className="muted">{label}</span>
    </div>
  );
}

export function LoadError({ error, retry }: { error: Error; retry?: () => void }) {
  return (
    <div className="notice notice-error" role="alert">
      <p>{error.message}</p>
      {retry && (
        <button type="button" className="button button-small" onClick={retry}>
          Try again
        </button>
      )}
    </div>
  );
}

/** Spinner, error or the content, for a `useLoad` result. */
export function Await<T>({
  loaded,
  retry,
  children,
}: {
  loaded: Loaded<T>;
  retry?: () => void;
  children: (data: T) => React.ReactNode;
}) {
  if (loaded.state === "loading") {
    return <Spinner />;
  }
  if (loaded.state === "error") {
    return <LoadError error={loaded.error} retry={retry} />;
  }
  return (
    <>
      {loaded.stale && (
        <p className="font-mono text-xs text-fog" role="status">
          Could not refresh: {loaded.stale.message}. Showing the last result.
        </p>
      )}
      {children(loaded.data)}
    </>
  );
}
