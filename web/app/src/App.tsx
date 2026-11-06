// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { useEffect, useState } from "react";

import { ApiError, getSession, getTenant, type Session, type Tenant } from "./api";
import { Shell } from "./components/Shell";
import { Await } from "./components/Status";
import { useLoad } from "./load";
import { Billing } from "./pages/Billing";
import { CiRun } from "./pages/CiRun";
import { Environments } from "./pages/Environments";
import { Home } from "./pages/Home";
import { Kit } from "./pages/Kit";
import { Repos } from "./pages/Repos";
import { Runs } from "./pages/Runs";
import { Secrets } from "./pages/Secrets";
import { Audit } from "./pages/Audit";
import { Profile } from "./pages/Profile";
import { Setup } from "./pages/Setup";
import { SignIn } from "./pages/SignIn";
import { legacyCiRedirect, matchOrg, navigate, useLocation, type OrgRoute } from "./router";

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
  // Old /ci/:owner/:repo URLs live under the organization now.
  const moved = legacyCiRedirect(path);

  useEffect(() => {
    if (moved) {
      navigate(moved, true);
    } else if (signedIn && path === "/signin") {
      navigate("/", true);
    }
  }, [signedIn, path, moved]);

  if (state.kind === "loading" || moved) {
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
  const session = state.session;
  const route = matchOrg(path);

  if (route?.page === "run" && route.repo && route.run) {
    // Runs of public repositories read without signing in.
    return session ? (
      <OrgRoute session={session} route={route} />
    ) : (
      <Shell user={null} publicRepo={`${route.org}/${route.repo}`}>
        <CiRun session={null} role={null} route={{ org: route.org, repo: route.repo, run: route.run, job: route.job }} />
      </Shell>
    );
  }
  if (session === null) {
    return <SignIn returnTo={path === "/signin" ? "/" : location} />;
  }
  if (route) {
    return <OrgRoute session={session} route={route} />;
  }
  return (
    <Shell user={session.user}>
      <div className="mx-auto w-full max-w-[1080px] pt-3">
        {path === "/new" ? <Home stay /> : path === "/" ? <Home /> : path === "/profile" ? <Profile user={session.user} /> : <NotFound />}
      </div>
    </Shell>
  );
}

/** A page under /o/:org: reads the tenant for the top bar and the role, then mounts the page. */
function OrgRoute({ session, route }: { session: Session; route: OrgRoute }) {
  const { user } = session;
  const [tenant, reload] = useLoad(() => getTenant(route.org), route.org);
  const publicRun = route.page === "run" && route.repo !== null && route.run !== null;

  if (tenant.state === "ready") {
    return (
      <Shell user={user} org={{ login: tenant.data.org.login, role: tenant.data.role }}>
        <OrgPage session={session} tenant={tenant.data} route={route} />
      </Shell>
    );
  }
  // A viewer outside the organization still reads a public repository's run.
  if (publicRun && tenant.state === "error" && tenant.error instanceof ApiError && [403, 404].includes(tenant.error.status)) {
    return (
      <Shell user={user} publicRepo={`${route.org}/${route.repo}`}>
        <CiRun session={session} role={null} route={{ org: route.org, repo: route.repo ?? "", run: route.run ?? "", job: route.job }} />
      </Shell>
    );
  }
  return (
    <Shell user={user} org={{ login: route.org, role: null }}>
      <Await loaded={tenant} retry={reload}>
        {() => null}
      </Await>
    </Shell>
  );
}

function OrgPage({ session, tenant, route }: { session: Session; tenant: Tenant; route: OrgRoute }) {
  const props = { org: tenant.org.login, tenant, user: session.user };
  switch (route.page) {
    case "runs":
      return <Runs {...props} />;
    case "repos":
      return <Repos {...props} />;
    case "secrets":
      return <Secrets {...props} />;
    case "environments":
      return <Environments {...props} />;
    case "audit":
      return <Audit {...props} />;
    case "kit":
      return <Kit {...props} />;
    case "setup":
      return <Setup {...props} />;
    case "settings":
    case "billing":
      return <Billing {...props} />;
    case "run":
      return <CiRun session={session} role={tenant.role} route={{ org: route.org, repo: route.repo ?? "", run: route.run ?? "", job: route.job }} />;
  }
}

function NotFound() {
  return (
    <section className="narrow">
      <h1>Nothing here</h1>
      <p className="muted">That page does not exist.</p>
    </section>
  );
}
