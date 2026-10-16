// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { useEffect, useState } from "react";

import { getSession, getTenant, type Session } from "./api";
import { OrgSwitcher } from "./components/OrgSwitcher";
import { Shell } from "./components/Shell";
import { Await } from "./components/Status";
import { useLoad } from "./load";
import { Home } from "./pages/Home";
import { SignIn } from "./pages/SignIn";
import { TenantPage } from "./pages/TenantPage";
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

  const user = state.session.user;
  const tenantRoute = /^\/o\/([A-Za-z0-9-]+)(?:\/([a-z]+)(?:\/([a-z0-9-]+))?)?\/?$/.exec(path);
  if (tenantRoute?.[1]) {
    return <TenantRoute user={user} org={tenantRoute[1]} section={tenantRoute[2]} item={tenantRoute[3]} />;
  }
  return (
    <Shell user={user}>
      <div className="mx-auto w-full max-w-[1080px] pt-3">
        {path === "/new" ? <Home stay /> : path === "/" ? <Home /> : <NotFound />}
      </div>
    </Shell>
  );
}

function TenantRoute({
  user,
  org,
  section,
  item,
}: {
  user: Session["user"];
  org: string;
  section: string | undefined;
  item: string | undefined;
}) {
  const [tenant, reload] = useLoad(() => getTenant(org), org);
  if (tenant.state === "ready") {
    return (
      <TenantPage key={tenant.data.org.login} user={user} tenant={tenant.data} section={section} item={item} />
    );
  }
  return (
    <Shell user={user} nav={<OrgSwitcher current={{ login: org, avatarUrl: "" }} />}>
      <Await loaded={tenant} retry={reload}>
        {() => null}
      </Await>
    </Shell>
  );
}

function NotFound() {
  return (
    <section className="narrow">
      <h1>Nothing here</h1>
      <p className="muted">That page does not exist.</p>
    </section>
  );
}
