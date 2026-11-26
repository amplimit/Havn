// The part of THIRD-PARTY-NOTICES.txt cargo-about does not write: code the
// binaries are linked with from outside the cargo graph (the Rust standard
// library, and musl and libunwind as zig builds them), and the dashboard's
// npm packages. Run in apps/dashboard after its build, with
//   node notices.mjs <repo> <COPYRIGHT-library.html> <zig directory>
// It writes to stdout, and fails when a licence is not on deny.toml's list.
import { existsSync, readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";

const [repo, rustCopyright, zig] = process.argv.slice(2);
if (!repo || !rustCopyright || !zig) {
  console.error("usage: node notices.mjs <repo> <COPYRIGHT-library.html> <zig directory>");
  process.exit(2);
}

const quoted = (list) => [...list.matchAll(/"([^"]+)"/g)].map((m) => m[1]).sort();
// deny.toml's [licenses] allow list, which contrib/notices/about.toml
// repeats for cargo-about: they must be the same.
const deny = readFileSync(join(repo, "deny.toml"), "utf8");
const denied = quoted(/^\[licenses\][^[]*?^allow = \[([^\]]*)\]/ms.exec(deny)?.[1] ?? "");
const about = readFileSync(join(repo, "contrib/notices/about.toml"), "utf8");
const accepted = quoted(/^accepted = \[([^\]]*)\]/m.exec(about)?.[1] ?? "");
if (denied.length === 0 || denied.join("\n") !== accepted.join("\n")) {
  console.error(
    `contrib/notices/about.toml's accepted list differs from deny.toml's allow list:\n` +
      `  deny.toml: ${denied.join(", ")}\n  about.toml: ${accepted.join(", ")}`,
  );
  process.exit(1);
}
const allowed = new Set(denied);
// Licences allowed for one package only, by name: caniuse-lite is the
// browser data Next.js builds with; tar, a tool Next.js vendors.
const allowedFor = { "caniuse-lite": ["CC-BY-4.0"], tar: ["BlueOak-1.0.0"] };

const wrong = [];
const terms = (license) =>
  license
    .replace(/[()]/g, " ")
    .split(/\s+(?:AND|OR)\s+/)
    .map((t) => t.trim())
    .filter(Boolean);
const licensed = (name, license) => {
  const ok = terms(license);
  if (ok.length === 0 || ok.some((t) => !allowed.has(t) && !(allowedFor[name] ?? []).includes(t))) {
    wrong.push(`${name}: ${license || "no licence"}`);
    return false;
  }
  return true;
};
const licenceFiles = (dir) =>
  readdirSync(dir)
    .filter((f) => /^(licen[cs]e|copying|notice)/i.test(f))
    .sort()
    .map((f) => readFileSync(join(dir, f), "utf8").trim());
const entry = (title, texts, fallback) =>
  "-----------------------------------------------------------------------------\n" +
  `${title}\n\n` +
  (texts.length > 0 ? texts.join("\n\n") : fallback) +
  "\n";
const section = (title, parts) =>
  "\n=============================================================================\n" +
  `${title}\n` +
  "=============================================================================\n\n" +
  parts.join("\n");
const licenceOf = (manifest) =>
  typeof manifest.license === "string" ? manifest.license : (manifest.license?.type ?? "");

// Linked in from outside the cargo graph: the standard library as the Rust
// project lists it, and musl libc, LLVM's libunwind and zig's compiler_rt,
// which zig builds for the musl targets.
const html = readFileSync(rustCopyright, "utf8");
const entities = { amp: "&", lt: "<", gt: ">", quot: '"', apos: "'", nbsp: " " };
const rustText = html
  .replace(/<(script|style)[\s\S]*?<\/\1>/gi, "")
  .replace(/<[^>]+>/g, "")
  .replace(/&#(\d+);/g, (_, n) => String.fromCodePoint(Number(n)))
  .replace(/&#x([0-9a-f]+);/gi, (_, n) => String.fromCodePoint(parseInt(n, 16)))
  .replace(/&([a-z]+);/gi, (m, n) => entities[n.toLowerCase()] ?? m)
  .split("\n")
  .map((l) => l.trimEnd())
  .join("\n")
  .replace(/\n{3,}/g, "\n\n")
  .trim();
const linked = [
  entry("The Rust standard library (std, core, alloc and the crates they are built from)", [rustText], ""),
  entry("musl libc, as zig builds it", [readFileSync(join(zig, "lib/libc/musl/COPYRIGHT"), "utf8").trim()], ""),
  entry("LLVM libunwind, as zig builds it", [readFileSync(join(zig, "lib/libunwind/LICENSE.TXT"), "utf8").trim()], ""),
  entry("zig's compiler_rt, which zig links where the Rust code needs it", [readFileSync(join(zig, "LICENSE"), "utf8").trim()], ""),
];

// The production packages of the lockfile, as installed, without the
// optional ones: the platform binaries Next.js builds with (sharp, SWC),
// which the dashboard does not ship.
const lock = JSON.parse(readFileSync("package-lock.json", "utf8"));
const found = new Map();
for (const [path, item] of Object.entries(lock.packages)) {
  if (!path || item.dev || item.devOptional || item.optional || item.link) {
    continue;
  }
  const name = item.name ?? path.slice(path.lastIndexOf("node_modules/") + "node_modules/".length);
  found.set(`${name}@${item.version}`, { name, path });
}
// Development packages whose output ships: tailwindcss, whose preflight
// is in the dashboard's CSS.
for (const name of ["tailwindcss"]) {
  const path = `node_modules/${name}`;
  const version = JSON.parse(readFileSync(join(path, "package.json"), "utf8")).version;
  found.set(`${name}@${version}`, { name, path });
}
const npm = [];
for (const key of [...found.keys()].sort()) {
  const { name, path } = found.get(key);
  const license = licenceOf(JSON.parse(readFileSync(join(path, "package.json"), "utf8")));
  if (licensed(name, license)) {
    npm.push(
      entry(`${key} (${license})`, licenceFiles(path), `(The package carries no licence file; its package.json says ${license}.)`),
    );
  }
}

// The packages Next.js vendors into itself (next/dist/compiled): its
// client runtime, which the dashboard ships, is built from some of them.
const compiled = "node_modules/next/dist/compiled";
const vendored = [];
for (const top of readdirSync(compiled).sort()) {
  const dirs = top.startsWith("@") ? readdirSync(join(compiled, top)).map((d) => join(top, d)) : [top];
  for (const dir of dirs) {
    const at = join(compiled, dir);
    const texts = existsSync(at) ? licenceFiles(at) : [];
    if (texts.length === 0) {
      continue;
    }
    const manifest = join(at, "package.json");
    const license = existsSync(manifest) ? licenceOf(JSON.parse(readFileSync(manifest, "utf8"))) : "";
    // Without a licence field, its licence file is what there is.
    if (license && !licensed(dir, license)) {
      continue;
    }
    vendored.push(entry(`next/dist/compiled/${dir}${license ? ` (${license})` : ""}`, texts, ""));
  }
}

if (wrong.length > 0) {
  console.error(`packages under licences not on the allow list:\n  ${wrong.join("\n  ")}`);
  process.exit(1);
}
process.stdout.write(
  section("Linked into the binaries from outside the crates above", linked) +
    section("npm packages of the dashboard havn-gateway embeds", npm) +
    section("Packages Next.js vendors into itself", vendored),
);
