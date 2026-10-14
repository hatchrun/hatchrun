// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

import { useEffect, useState } from "react";

import { getSession, type Session } from "./api";
import { Shell } from "./components/Shell";
import { Home } from "./pages/Home";
import { SignIn } from "./pages/SignIn";
import { navigate, useLocation } from "./router";

type State = { kind: "loading" } | { kind: "error"; message: string } | { kind: "ready"; session: Session | null };

export function App() {
  const location = useLocation();
  const [state, setState] = useState<State>({ kind: "loading" });

  useEffect(() => {
    getSession().then(
      (session) => setState({ kind: "ready", session }),
      (err: unknown) => setState({ kind: "error", message: err instanceof Error ? err.message : String(err) }),
    );
  }, []);

  const path = location.split("?")[0] ?? "/";
  const signedIn = state.kind === "ready" && state.session !== null;

  useEffect(() => {
    if (signedIn && path === "/signin") {
      navigate("/", true);
    }
  }, [signedIn, path]);

  if (state.kind === "loading") {
    return <div className="loading" aria-busy="true" />;
  }
  if (state.kind === "error") {
    return (
      <main className="auth">
        <div className="auth-card">
          <h1>hatchrun is not answering</h1>
          <p className="muted">{state.message}</p>
          <button type="button" className="button" onClick={() => window.location.reload()}>
            Try again
          </button>
        </div>
      </main>
    );
  }
  if (state.session === null) {
    return <SignIn returnTo={path === "/signin" ? "/" : location} />;
  }
  return (
    <Shell user={state.session.user}>
      <Home user={state.session.user} />
    </Shell>
  );
}
