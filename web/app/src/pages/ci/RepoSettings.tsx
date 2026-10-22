// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

// A repository's CI secrets and variables, at /ci/:owner/:repo/settings.
// They need admin access to the repository: anyone else gets a 404.

import { type Session } from "@/api";
import { ChevronLeftIcon } from "@/components/icons";
import { Shell } from "@/components/Shell";
import { Link } from "@/router";
import { SignIn } from "../SignIn";
import { SecretsTable } from "./SecretsTable";

export function RepoSettings({ session, owner, repo }: { session: Session | null; owner: string; repo: string }) {
  if (session === null) {
    return <SignIn returnTo={`/ci/${owner}/${repo}/settings`} />;
  }
  const scope = { owner, repo };
  return (
    <Shell
      user={session.user}
      nav={
        <Link href={`/ci/${owner}/${repo}`} className="font-mono text-sm text-bone hover:text-chick">
          {owner}/{repo}
        </Link>
      }
    >
      <div className="mx-auto flex w-full max-w-[1080px] flex-col gap-8">
        <div>
          <Link href={`/ci/${owner}/${repo}`} className="inline-flex items-center gap-1.5 font-mono text-xs text-bone hover:text-chick">
            <ChevronLeftIcon className="text-chick" />
            Runs
          </Link>
          <h1 className="display text-[26px] leading-[1.05]">Settings</h1>
          <p className="muted">Secrets and variables for {owner}/{repo}. Repository admins only.</p>
        </div>
        <SecretsTable scope={scope} kind="secrets" />
        <SecretsTable scope={scope} kind="vars" />
      </div>
    </Shell>
  );
}
