// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { CiPages } from "./ci/RunPage";
import type { RunPageProps } from "./types";

/**
 * The run page (`/o/:org/ci/:repo/runs/:n`) and the job page (`.../jobs/:job`).
 * Placeholder: mounts the old run and job views until the screen is built.
 */
export function CiRun({ session, route }: RunPageProps) {
  return (
    <div className="mx-auto w-full max-w-[1080px]">
      <CiPages session={session} route={route} />
    </div>
  );
}
