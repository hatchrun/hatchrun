// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

import type { User } from "../api";

export function Home({ user }: { user: User }) {
  return (
    <section className="narrow">
      <h1>Welcome, {user.name ?? user.login}</h1>
      <p className="muted">
        Organization setup opens here soon. Until then, there is nothing to configure.
      </p>
    </section>
  );
}
