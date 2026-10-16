// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import type { SVGProps } from "react";

type Props = Omit<SVGProps<SVGSVGElement>, "width" | "height"> & { size?: number };

function Stroke({ size = 12, line = 2.6, children, ...rest }: Props & { line?: number }) {
  return (
    <svg
      aria-hidden="true"
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={line}
      strokeLinecap="round"
      strokeLinejoin="round"
      {...rest}
    >
      {children}
    </svg>
  );
}

/** Leaves for another site. */
export function ExternalIcon(props: Props) {
  return (
    <Stroke size={10} {...props}>
      <path d="M14 4h6v6M20 4l-9 9" />
    </Stroke>
  );
}

/** Leaves for another site, drawn with the page it opens. */
export function OpenIcon(props: Props) {
  return (
    <Stroke size={11} line={2.4} {...props}>
      <path d="M14 4h6v6M20 4l-9 9M18 14v5a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V7a1 1 0 0 1 1-1h5" />
    </Stroke>
  );
}

export function ChevronRightIcon(props: Props) {
  return (
    <Stroke {...props}>
      <path d="M9 6l6 6-6 6" />
    </Stroke>
  );
}

export function ChevronLeftIcon(props: Props) {
  return (
    <Stroke {...props}>
      <path d="M15 6l-6 6 6 6" />
    </Stroke>
  );
}

export function UpDownIcon({ size = 12, ...rest }: Props) {
  return (
    <svg aria-hidden="true" width={size} height={size} viewBox="0 0 16 16" fill="currentColor" {...rest}>
      <path d="M4.5 6 8 2.5 11.5 6zM4.5 10 8 13.5 11.5 10z" />
    </svg>
  );
}

export function GitHubIcon({ size = 14, ...rest }: Props) {
  return (
    <svg aria-hidden="true" width={size} height={size} viewBox="0 0 16 16" fill="currentColor" {...rest}>
      <path d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27.68 0 1.36.09 2 .27 1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.013 8.013 0 0016 8c0-4.42-3.58-8-8-8z" />
    </svg>
  );
}
