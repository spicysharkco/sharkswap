// Tests for the strace log checker that backs the "only writes inside the
// sandbox" proof. The logs here are hand-written in strace's -f -y format.
import { test } from "node:test";
import assert from "node:assert/strict";
import { checkTrace, splitArgs, unquote } from "./check-writes.mjs";

const opts = { allow: ["/sandbox"], cwd: "/sandbox/work" };
const run = (...lines) => checkTrace(lines.join("\n") + "\n", opts);

test("splitArgs keeps quoted commas, braces and fd annotations whole", () => {
  assert.deepEqual(
    splitArgs('AT_FDCWD</a,b>, "x,\\"y", O_WRONLY|O_CREAT, 0600'),
    ["AT_FDCWD</a,b>", '"x,\\"y"', "O_WRONLY|O_CREAT", "0600"],
  );
  assert.deepEqual(splitArgs("3</d>, {tv_sec=1, tv_nsec=2}, 0"), ["3</d>", "{tv_sec=1, tv_nsec=2}", "0"]);
});

test("unquote reads octal and hex escapes", () => {
  assert.equal(unquote('"a\\040b"'), "a b");
  assert.equal(unquote('"\\x41\\n"'), "A\n");
  assert.equal(unquote("NULL"), null);
});

test("writes inside the sandbox pass, and are counted", () => {
  const r = run('10 openat(AT_FDCWD</sandbox/work>, "/sandbox/home/.x/a.json", O_WRONLY|O_CREAT|O_TRUNC, 0600) = 3</sandbox/home/.x/a.json>');
  assert.equal(r.violations.length, 0);
  assert.equal(r.inAllowed, 1);
});

test("a write outside the sandbox is a violation", () => {
  const r = run('10 openat(AT_FDCWD</sandbox/work>, "/etc/hosts", O_WRONLY|O_APPEND) = -1 EACCES (Permission denied)');
  assert.equal(r.violations.length, 1);
  assert.match(r.violations[0].why, /\/etc\/hosts/);
});

test("reading is not writing", () => {
  const r = run('10 openat(AT_FDCWD</sandbox/work>, "/home/someone/.profile", O_RDONLY|O_CLOEXEC) = 3</home/someone/.profile>');
  assert.equal(r.violations.length, 0);
  assert.equal(r.writes, 0);
});

test("a relative path resolves against the directory the call names", () => {
  const ok = run('10 openat(AT_FDCWD</sandbox/work>, "out.txt", O_WRONLY|O_CREAT, 0666) = 3</sandbox/work/out.txt>');
  assert.equal(ok.violations.length, 0);
  const bad = run('10 openat(AT_FDCWD</elsewhere>, "out.txt", O_WRONLY|O_CREAT, 0666) = 3</elsewhere/out.txt>');
  assert.equal(bad.violations.length, 1);
  const viaFd = run('10 unlinkat(7</elsewhere/dir>, "gone", 0) = 0');
  assert.match(viaFd.violations[0].why, /\/elsewhere\/dir\/gone/);
});

test("dotdot cannot climb out", () => {
  const r = run('10 mkdir("/sandbox/../outside", 0700) = 0');
  assert.equal(r.violations.length, 1);
});

test("a legacy call with a relative path uses the process's tracked directory", () => {
  const r = run(
    '10 chdir("/elsewhere") = 0',
    '10 mkdir("d", 0700) = 0',
  );
  assert.equal(r.violations.length, 1);
  assert.match(r.violations[0].why, /\/elsewhere\/d/);
  const inside = run('11 mkdir("d", 0700) = 0');
  assert.equal(inside.violations.length, 0, "falls back to the starting directory");
});

test("both ends of a rename count", () => {
  const r = run('10 rename("/sandbox/a", "/outside/b") = 0');
  assert.equal(r.violations.length, 1);
  const r2 = run('10 renameat2(AT_FDCWD</sandbox>, "/outside/a", AT_FDCWD</sandbox>, "b", RENAME_NOREPLACE) = 0');
  assert.equal(r2.violations.length, 1);
});

test("a symlink's target is data, its location is the write", () => {
  const ok = run('10 symlink("/usr/bin/real", "/sandbox/bin/cs") = 0');
  assert.equal(ok.violations.length, 0);
  const bad = run('10 symlinkat("/sandbox/x", AT_FDCWD</sandbox>, "/outside/link") = 0');
  assert.equal(bad.violations.length, 1);
});

test("/dev/null and friends are allowed", () => {
  const r = run('10 openat(AT_FDCWD</sandbox>, "/dev/null", O_WRONLY) = 3</dev/null>');
  assert.equal(r.violations.length, 0);
});

test("an unfinished call is still judged on its arguments", () => {
  const r = run('10 openat(AT_FDCWD</sandbox>, "/outside/f", O_WRONLY|O_CREAT, 0600 <unfinished ...>');
  assert.equal(r.violations.length, 1);
});

test("futimens with no path is not a path write", () => {
  const r = run('10 utimensat(5</sandbox/f>, NULL, [{tv_sec=1, tv_nsec=0}, {tv_sec=1, tv_nsec=0}], 0) = 0');
  assert.equal(r.violations.length, 0);
});

test("mkdir -p meeting an existing ancestor (EEXIST) changed nothing", () => {
  const r = run(
    '10 mkdir("/home", 0777) = -1 EEXIST (File exists)',
    '10 mkdirat(AT_FDCWD</>, "home", 0777) = -1 EEXIST (File exists)',
  );
  assert.equal(r.violations.length, 0);
  assert.equal(r.writes, 0);
});

test("a mkdir outside that succeeds, or fails another way, is still a violation", () => {
  const r = run(
    '10 mkdir("/home/new", 0777) = 0',
    '10 mkdir("/etc/x", 0777) = -1 EACCES (Permission denied)',
  );
  assert.equal(r.violations.length, 2);
});
