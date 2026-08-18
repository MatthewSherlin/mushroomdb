export type TokenKind =
  | "keyword"
  | "ident"
  | "string"
  | "number"
  | "comment"
  | "punct"
  | "space";

export type Token = {
  kind: TokenKind;
  text: string;
};

const KEYWORDS = new Set([
  "and",
  "as",
  "asc",
  "by",
  "case",
  "contains",
  "create",
  "delete",
  "desc",
  "detach",
  "distinct",
  "else",
  "end",
  "ends",
  "false",
  "in",
  "is",
  "limit",
  "match",
  "merge",
  "not",
  "null",
  "on",
  "optional",
  "or",
  "order",
  "remove",
  "return",
  "set",
  "skip",
  "starts",
  "then",
  "true",
  "unwind",
  "when",
  "where",
  "with",
  "xor",
]);

const KIND_CLASS: Record<TokenKind, string | undefined> = {
  keyword: "hl-kw",
  ident: undefined,
  string: "hl-str",
  number: "hl-num",
  comment: "hl-cmt",
  punct: undefined,
  space: undefined,
};

export function tokenize(source: string): Token[] {
  const tokens: Token[] = [];
  let i = 0;
  while (i < source.length) {
    const ch = source[i]!;
    if (ch === "/" && source[i + 1] === "/") {
      const start = i;
      i += 2;
      while (i < source.length && source[i] !== "\n") {
        i += 1;
      }
      tokens.push({ kind: "comment", text: source.slice(start, i) });
      continue;
    }
    if (ch === "'" || ch === '"') {
      const quote = ch;
      const start = i;
      i += 1;
      while (i < source.length) {
        if (source[i] === "\\") {
          i += 2;
          continue;
        }
        if (source[i] === quote) {
          i += 1;
          break;
        }
        i += 1;
      }
      tokens.push({ kind: "string", text: source.slice(start, i) });
      continue;
    }
    if (isDigit(ch)) {
      const start = i;
      i += 1;
      while (i < source.length && (isDigit(source[i]!) || source[i] === ".")) {
        i += 1;
      }
      tokens.push({ kind: "number", text: source.slice(start, i) });
      continue;
    }
    if (isIdentStart(ch)) {
      const start = i;
      i += 1;
      while (i < source.length && isIdentPart(source[i]!)) {
        i += 1;
      }
      const text = source.slice(start, i);
      tokens.push({
        kind: KEYWORDS.has(text.toLowerCase()) ? "keyword" : "ident",
        text,
      });
      continue;
    }
    if (isSpace(ch)) {
      const start = i;
      i += 1;
      while (i < source.length && isSpace(source[i]!)) {
        i += 1;
      }
      tokens.push({ kind: "space", text: source.slice(start, i) });
      continue;
    }
    tokens.push({ kind: "punct", text: ch });
    i += 1;
  }
  return tokens;
}

export function highlightHtml(source: string): string {
  let html = "";
  for (const token of tokenize(source)) {
    const escaped = escapeHtml(token.text);
    const cls = KIND_CLASS[token.kind];
    if (cls === undefined) {
      html += escaped;
    } else {
      html += `<span class="${cls}">${escaped}</span>`;
    }
  }
  return html;
}

function escapeHtml(text: string): string {
  return text
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
}

function isDigit(ch: string): boolean {
  return ch >= "0" && ch <= "9";
}

function isIdentStart(ch: string): boolean {
  return (
    (ch >= "A" && ch <= "Z") ||
    (ch >= "a" && ch <= "z") ||
    ch === "_"
  );
}

function isIdentPart(ch: string): boolean {
  return isIdentStart(ch) || isDigit(ch);
}

function isSpace(ch: string): boolean {
  return ch === " " || ch === "\t" || ch === "\n" || ch === "\r";
}
