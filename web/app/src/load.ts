// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { useCallback, useEffect, useRef, useState } from "react";

import { ApiError } from "./api";

export type Loaded<T> =
  | { state: "loading" }
  | { state: "error"; error: Error }
  | { state: "ready"; data: T; /** The last read failed; `data` is from an earlier one. */ stale?: Error };

/**
 * Runs `load` when `key` changes; `reload` runs it again. With `keep`, a
 * failed read after a good one for the same `key` keeps the good data
 * and reports the error as `stale`, so a page that refreshes itself does
 * not lose what it shows to one failed refresh. A refusal (401, 403 or
 * 404) is never kept: the viewer lost access.
 */
export function useLoad<T>(load: () => Promise<T>, key: string, keep = false): [Loaded<T>, () => void] {
  const [result, setResult] = useState<Loaded<T>>({ state: "loading" });
  const readKey = useRef<string | null>(null);
  const [round, setRound] = useState(0);

  useEffect(() => {
    let current = true;
    load().then(
      (data) => {
        if (current) {
          readKey.current = key;
          setResult({ state: "ready", data });
        }
      },
      (failure: unknown) => {
        if (!current) {
          return;
        }
        const error = failure instanceof Error ? failure : new Error(String(failure));
        const refused = error instanceof ApiError && [401, 403, 404].includes(error.status);
        setResult((last) =>
          keep && !refused && last.state === "ready" && readKey.current === key
            ? { state: "ready", data: last.data, stale: error }
            : { state: "error", error },
        );
      },
    );
    return () => {
      current = false;
    };
    // `load` is a fresh closure on every render; `key` says when it
    // asks for something else.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key, round]);

  const reload = useCallback(() => setRound((n) => n + 1), []);
  return [result, reload];
}

/** The time in Unix milliseconds, read again every `every` milliseconds. */
export function useNow(every: number): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), every);
    return () => window.clearInterval(timer);
  }, [every]);
  return now;
}
