// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { LoaderCircle } from "lucide-react";

import type { Loaded } from "../load";
import { Button } from "./ui/button";

export function Spinner({ label = "Loading" }: { label?: string }) {
  return (
    <div className="flex items-center gap-2 py-4 text-fg-2" role="status">
      <LoaderCircle size={16} aria-hidden="true" className="animate-spin motion-reduce:animate-none" />
      <span>{label}</span>
    </div>
  );
}

export function LoadError({ error, retry }: { error: Error; retry?: () => void }) {
  return (
    <div className="flex flex-wrap items-center gap-3 error-line rounded-xs px-3 py-2" role="alert">
      <p className="m-0 min-w-0 flex-1">{error.message}</p>
      {retry && (
        <Button variant="outline" size="sm" onClick={retry}>
          Try again
        </Button>
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
        <p className="font-mono text-xs text-fg-3" role="status">
          Could not refresh: {loaded.stale.message}. Showing the last result.
        </p>
      )}
      {children(loaded.data)}
    </>
  );
}
