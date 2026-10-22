// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

// Terminal color codes in a log line, as plain data. The log view
// renders each segment as a text node inside a span with a class, so
// nothing from the log is ever parsed as HTML.

export interface Segment {
  text: string;
  /** Space-separated class names, empty for plain text. */
  className: string;
}

// CSI sequences (colors and cursor moves), and OSC strings (titles, links).
// eslint-disable-next-line no-control-regex
const ESCAPES = /\u001b\[([0-9;:]*)([\u0020-\u002f]*[\u0040-\u007e])|\u001b\][^\u0007\u001b]*(?:\u0007|\u001b\\)|\u001b[\u0040-\u005f]/g;

interface Style {
  fg: number | null;
  bg: number | null;
  bold: boolean;
  dim: boolean;
  italic: boolean;
  underline: boolean;
}

const PLAIN: Style = { fg: null, bg: null, bold: false, dim: false, italic: false, underline: false };

function classes(style: Style): string {
  const names: string[] = [];
  if (style.fg !== null) {
    names.push(`ansi-fg-${style.fg}`);
  }
  if (style.bg !== null) {
    names.push(`ansi-bg-${style.bg}`);
  }
  if (style.bold) {
    names.push("ansi-bold");
  }
  if (style.dim) {
    names.push("ansi-dim");
  }
  if (style.italic) {
    names.push("ansi-italic");
  }
  if (style.underline) {
    names.push("ansi-underline");
  }
  return names.join(" ");
}

/** Applies one `m` sequence to `style`. Extended colors (38 and 48) are skipped whole. */
function sgr(style: Style, params: string): Style {
  const codes = params === "" ? [0] : params.split(/[;:]/).map((code) => (code === "" ? 0 : Number(code)));
  let next = style;
  for (let i = 0; i < codes.length; i++) {
    const code = codes[i] ?? 0;
    if (code === 38 || code === 48) {
      i += codes[i + 1] === 5 ? 2 : codes[i + 1] === 2 ? 4 : 0;
    } else if (code === 0) {
      next = PLAIN;
    } else if (code === 1) {
      next = { ...next, bold: true };
    } else if (code === 2) {
      next = { ...next, dim: true };
    } else if (code === 3) {
      next = { ...next, italic: true };
    } else if (code === 4) {
      next = { ...next, underline: true };
    } else if (code === 22) {
      next = { ...next, bold: false, dim: false };
    } else if (code === 23) {
      next = { ...next, italic: false };
    } else if (code === 24) {
      next = { ...next, underline: false };
    } else if ((code >= 30 && code <= 37) || (code >= 90 && code <= 97)) {
      next = { ...next, fg: code };
    } else if (code === 39) {
      next = { ...next, fg: null };
    } else if ((code >= 40 && code <= 47) || (code >= 100 && code <= 107)) {
      next = { ...next, bg: code };
    } else if (code === 49) {
      next = { ...next, bg: null };
    }
  }
  return next;
}

/**
 * The segments of a line. Color and text style codes become classes;
 * every other escape sequence and the stray control characters are
 * dropped. Text before a carriage return is dropped too: it is what a
 * progress bar overwrote.
 */
export function segments(line: string): Segment[] {
  const text = line.includes("\r") ? line.slice(line.lastIndexOf("\r") + 1) : line;
  if (!text.includes("\u001b")) {
    return [{ text: clean(text), className: "" }];
  }
  const out: Segment[] = [];
  let style = PLAIN;
  let last = 0;
  const push = (end: number) => {
    const piece = clean(text.slice(last, end));
    if (piece !== "") {
      out.push({ text: piece, className: classes(style) });
    }
  };
  for (const match of text.matchAll(ESCAPES)) {
    push(match.index);
    if (match[2] === "m") {
      style = sgr(style, match[1] ?? "");
    }
    last = match.index + match[0].length;
  }
  push(text.length);
  return out;
}

/** Control characters other than a tab, and a lone escape, have no place in a line of text. */
function clean(text: string): string {
  // eslint-disable-next-line no-control-regex
  return text.replace(/[\u0000-\u0008\u000b-\u001f\u007f]/g, "");
}
