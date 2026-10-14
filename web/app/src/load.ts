// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

import { useCallback, useEffect, useState } from "react";

export type Loaded<T> =
  | { state: "loading" }
  | { state: "error"; error: Error }
  | { state: "ready"; data: T };

/** Runs `load` when `key` changes; `reload` runs it again. */
export function useLoad<T>(load: () => Promise<T>, key: string): [Loaded<T>, () => void] {
  const [result, setResult] = useState<Loaded<T>>({ state: "loading" });
  const [round, setRound] = useState(0);

  useEffect(() => {
    let current = true;
    load().then(
      (data) => current && setResult({ state: "ready", data }),
      (error: unknown) =>
        current && setResult({ state: "error", error: error instanceof Error ? error : new Error(String(error)) }),
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
