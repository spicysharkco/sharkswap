// Reads an strace log and fails if a process wrote anywhere outside the allowed
// directories. test/run.sh produces the log by tracing the whole CLI suite
// (`strace -f -y ...`), so this is the proof that a switch, a save, a remove or
// a rotation touches only the throwaway HOME the tests give it.
//
//   node test/check-writes.mjs --allow DIR [--allow DIR ...] --cwd DIR TRACE
//   node test/check-writes.mjs --calls      (the system calls to trace, for strace -e trace=)
//
// "Write" here means any call that creates, changes, renames or deletes a file
// system entry: open with a write flag, creat, mkdir, rename, unlink, rmdir,
// symlink, link, chmod, chown, truncate, utimensat, mknod, setxattr. A path the
// checker cannot resolve counts as outside, so a gap fails loudly instead of
// passing quietly.
import { readFileSync, realpathSync } from "node:fs";
import { posix } from "node:path";

export const TRACED_CALLS = [
  "open", "openat", "openat2", "creat", "mkdir", "mkdirat", "rename", "renameat", "renameat2",
  "unlink", "unlinkat", "rmdir", "symlink", "symlinkat", "link", "linkat", "chmod", "fchmodat", "fchmodat2",
  "chown", "lchown", "fchownat", "truncate", "utimensat", "utimes", "futimesat", "mknod", "mknodat",
  "setxattr", "lsetxattr", "removexattr", "lremovexattr", "chdir",
];

// Where a call keeps its paths: [dirfd index | null, path index] pairs.
const PATH_ARGS = {
  open: [[null, 0]], openat: [[0, 1]], openat2: [[0, 1]], creat: [[null, 0]],
  mkdir: [[null, 0]], mkdirat: [[0, 1]],
  rename: [[null, 0], [null, 1]], renameat: [[0, 1], [2, 3]], renameat2: [[0, 1], [2, 3]],
  unlink: [[null, 0]], unlinkat: [[0, 1]], rmdir: [[null, 0]],
  symlink: [[null, 1]], symlinkat: [[1, 2]],
  link: [[null, 0], [null, 1]], linkat: [[0, 1], [2, 3]],
  chmod: [[null, 0]], fchmodat: [[0, 1]], fchmodat2: [[0, 1]],
  chown: [[null, 0]], lchown: [[null, 0]], fchownat: [[0, 1]],
  truncate: [[null, 0]], utimensat: [[0, 1]], utimes: [[null, 0]], futimesat: [[0, 1]],
  mknod: [[null, 0]], mknodat: [[0, 1]],
  setxattr: [[null, 0]], lsetxattr: [[null, 0]], removexattr: [[null, 0]], lremovexattr: [[null, 0]],
};

const OPEN_WRITE = /\b(O_WRONLY|O_RDWR|O_CREAT|O_TRUNC|O_APPEND|O_TMPFILE)\b/;
const ALWAYS_OK = [/^\/dev\/(null|tty|zero|stdout|stderr|stdin|urandom|random)$/, /^\/dev\/fd\//, /^\/proc\//];

// Split the argument text of one call at top-level commas.
export function splitArgs(text) {
  const out = [];
  let depth = 0, cur = "", i = 0;
  while (i < text.length) {
    const c = text[i];
    if (c === '"') {            // a quoted string, with C escapes; strace may append "..."
      let j = i + 1;
      while (j < text.length && text[j] !== '"') j += text[j] === "\\" ? 2 : 1;
      cur += text.slice(i, j + 1);
      i = j + 1;
      continue;
    }
    if (c === "<") {            // fd path from -y: 3</some/path>, AT_FDCWD</cwd>
      const stop = findFdEnd(text, i);
      cur += text.slice(i, stop + 1);
      i = stop + 1;
      continue;
    }
    if (c === "{" || c === "[" || c === "(") depth++;
    if (c === "}" || c === "]" || c === ")") depth--;
    if (c === "," && depth === 0) { out.push(cur.trim()); cur = ""; i++; continue; }
    cur += c; i++;
  }
  if (cur.trim() !== "") out.push(cur.trim());
  return out;
}

// The closing ">" of an fd annotation: the first ">" that is followed by "," or the end.
function findFdEnd(text, from) {
  for (let k = from + 1; k < text.length; k++) {
    if (text[k] === ">" && (k + 1 === text.length || text[k + 1] === "," || text[k + 1] === ")")) return k;
  }
  return text.length - 1;
}

// A strace string literal back into a JS string (octal, hex and the usual escapes).
export function unquote(lit) {
  const m = /^"((?:[^"\\]|\\.)*)"/.exec(lit);
  if (!m) return null;
  const bytes = [];
  const s = m[1];
  for (let i = 0; i < s.length; i++) {
    if (s[i] !== "\\") { bytes.push(...Buffer.from(s[i])); continue; }
    const n = s[++i];
    if (/[0-7]/.test(n)) {
      let oct = n;
      while (oct.length < 3 && /[0-7]/.test(s[i + 1] ?? "")) oct += s[++i];
      bytes.push(parseInt(oct, 8) & 255);
    } else if (n === "x") {
      let hex = "";
      while (hex.length < 2 && /[\da-f]/i.test(s[i + 1] ?? "")) hex += s[++i];
      bytes.push(parseInt(hex, 16));
    } else {
      bytes.push({ n: 10, t: 9, r: 13, a: 7, b: 8, f: 12, v: 11 }[n] ?? n.charCodeAt(0));
    }
  }
  return Buffer.from(bytes).toString("utf8");
}

const fdBase = (arg) => {                // AT_FDCWD</cwd> or 4</dir>
  const m = /^(AT_FDCWD|\d+)<(.*)>$/s.exec(arg);
  return m ? { cwd: m[1] === "AT_FDCWD", dir: m[2] } : null;
};

export function checkTrace(text, { allow, cwd }) {
  const roots = allow.map((a) => a.replace(/\/+$/, ""));
  const inside = (p) => roots.some((r) => p === r || p.startsWith(r + "/"));
  const cwds = new Map();
  const violations = [];
  let seen = 0, writes = 0, inAllowed = 0;
  const pending = new Map();

  for (const raw of text.split("\n")) {
    if (!raw) continue;
    const lm = /^(?:(\d+)\s+)?(\w+)\((.*)$/.exec(raw);
    if (!lm) continue;                   // exit lines, signals, resumed halves
    const pid = lm[1] ?? "0";
    const name = lm[2];
    if (!TRACED_CALLS.includes(name)) continue;
    let body = lm[3];
    const unfinished = body.includes("<unfinished ...>");
    body = body.replace(/\s*<unfinished \.\.\.>\s*$/, "");
    // Strip " = ret" (and anything after) when the call completed on this line.
    let ret = null;
    const rm = /\)\s+=\s+(-?\d+|\?)(?:<.*?>)?(?:\s+\w+.*)?$/.exec(body);
    if (!unfinished && rm) { ret = rm[1]; body = body.slice(0, rm.index); }
    else if (body.endsWith(")")) body = body.slice(0, -1);
    const args = splitArgs(body);
    seen++;
    pending.delete(pid);

    // Learn the working directory of this process.
    const first = fdBase(args[0] ?? "");
    if (first?.cwd) cwds.set(pid, first.dir);
    const second = fdBase(args[2] ?? "");
    if (second?.cwd) cwds.set(pid, second.dir);

    if (name === "chdir") {
      const p = unquote(args[0] ?? "");
      if (p !== null && ret === "0") cwds.set(pid, p.startsWith("/") ? posix.normalize(p) : posix.join(cwds.get(pid) ?? cwd, p));
      continue;
    }

    if (["open", "openat", "openat2"].includes(name) && !OPEN_WRITE.test(body)) continue;
    // `mkdir -p` (GNU coreutils) walks every ancestor from / and gets EEXIST for
    // each one already there. A directory that already exists was not created,
    // so that call changed nothing. Any other failed write still counts.
    if ((name === "mkdir" || name === "mkdirat") && ret === "-1" && /\)\s+=\s+-1\s+EEXIST\b/.test(raw)) continue;
    writes++;

    for (const [dirIdx, pathIdx] of PATH_ARGS[name]) {
      const lit = args[pathIdx];
      if (lit === undefined || lit === "NULL") continue;   // futimens(fd, ...): no path
      const p = unquote(lit);
      if (p === null) { violations.push({ raw, why: "unparsed path argument" }); continue; }
      let abs;
      if (p.startsWith("/")) abs = posix.normalize(p);
      else {
        const base = dirIdx === null ? { cwd: true, dir: cwds.get(pid) ?? cwd } : fdBase(args[dirIdx] ?? "");
        if (!base) { violations.push({ raw, why: "relative path against an unknown directory" }); continue; }
        abs = posix.normalize(posix.join(base.dir, p));
      }
      if (ALWAYS_OK.some((re) => re.test(abs))) continue;
      if (inside(abs)) { inAllowed++; continue; }
      violations.push({ raw, why: `writes ${abs}` });
    }
  }
  return { seen, writes, inAllowed, violations };
}

function main(argv) {
  if (argv[0] === "--calls") {         // the list for strace's -e trace=, so there is one copy of it
    console.log(TRACED_CALLS.join(","));
    return 0;
  }
  const allow = [], rest = [];
  let cwd = process.cwd();
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === "--allow") {
      const d = argv[++i];
      allow.push(d);
      try { allow.push(realpathSync(d)); } catch { /* not created: only the literal path */ }
    } else if (argv[i] === "--cwd") cwd = argv[++i];
    else rest.push(argv[i]);
  }
  if (allow.length === 0 || rest.length !== 1) {
    console.error("usage: check-writes.mjs --allow DIR [--allow DIR ...] [--cwd DIR] TRACE");
    return 2;
  }
  const r = checkTrace(readFileSync(rest[0], "utf8"), { allow: [...new Set(allow)], cwd });
  console.log(`traced ${r.seen} file-changing calls, ${r.writes} writes, ${r.inAllowed} inside the sandbox, ${r.violations.length} outside`);
  if (r.seen === 0 || r.inAllowed === 0) {
    console.error("check-writes: the trace holds no sandbox writes at all; strace did not see the run");
    return 3;
  }
  for (const v of r.violations.slice(0, 40)) console.error(`OUTSIDE: ${v.why}\n  ${v.raw.slice(0, 300)}`);
  return r.violations.length === 0 ? 0 : 1;
}

if (import.meta.url === `file://${process.argv[1]}`) process.exit(main(process.argv.slice(2)));
