// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

import { signInUrl } from "../api";
import { Logo } from "../components/Logo";
import { useQuery } from "../router";

const errors: Record<string, string> = {
  github: "GitHub did not finish signing you in. Try again.",
};

export function SignIn({ returnTo }: { returnTo: string }) {
  const error = useQuery("error");
  return (
    <main className="auth">
      <div className="auth-card">
        <Logo size={40} />
        <h1>Sign in to hatchrun</h1>
        <p className="muted">
          GitHub Actions runners on fresh EC2 instances in your own AWS account.
        </p>
        {error && (
          <p className="notice notice-error" role="alert">
            {errors[error] ?? "Signing in failed. Try again."}
          </p>
        )}
        <a className="button button-primary button-wide" href={signInUrl(returnTo)}>
          <GitHubMark />
          Continue with GitHub
        </a>
        <p className="fine">
          hatchrun asks GitHub who you are and which organizations you manage. It cannot read
          your code.
        </p>
      </div>
      <p className="auth-footer muted">
        New to hatchrun? Read the <a href="https://hatchrun.io/docs/overview/">docs</a>.
      </p>
    </main>
  );
}

export function GitHubMark() {
  return (
    <svg width="18" height="18" viewBox="0 0 16 16" aria-hidden="true" fill="currentColor">
      <path d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27.68 0 1.36.09 2 .27 1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.01 8.01 0 0 0 16 8c0-4.42-3.58-8-8-8Z" />
    </svg>
  );
}
