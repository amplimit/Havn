import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";

// The licences deny.toml allows, as contrib/notices/about.toml lists them,
// and CC-BY-4.0: caniuse-lite, the browser data Next.js builds with.
const about = readFileSync(process.argv[2], "utf8");
const listed = /^accepted = \[([^\]]*)\]/m.exec(about)?.[1] ?? "";
const allowed = new Set([...listed.matchAll(/"([^"]+)"/g)].map((m) => m[1]).concat("CC-BY-4.0"));

// The production packages of the lockfile, as installed, without the
// optional ones: the platform binaries Next.js builds with (sharp, SWC),
// which the dashboard does not ship.
const lock = JSON.parse(readFileSync("package-lock.json", "utf8"));
const found = new Map();
for (const [path, entry] of Object.entries(lock.packages)) {
  if (!path || entry.dev || entry.devOptional || entry.optional || entry.link) {
    continue;
  }
  const name = entry.name ?? path.slice(path.lastIndexOf("node_modules/") + "node_modules/".length);
  const key = `${name}@${entry.version}`;
  if (!found.has(key)) {
    found.set(key, { path });
  }
}

const wrong = [];
const parts = [];
for (const key of [...found.keys()].sort()) {
  const dep = found.get(key);
  const manifest = JSON.parse(readFileSync(join(dep.path, "package.json"), "utf8"));
  const license = typeof manifest.license === "string" ? manifest.license : (manifest.license?.type ?? "");
  const terms = license
    .replace(/[()]/g, " ")
    .split(/\s+(?:AND|OR)\s+/)
    .map((t) => t.trim())
    .filter(Boolean);
  if (terms.length === 0 || terms.some((t) => !allowed.has(t))) {
    wrong.push(`${key}: ${license || "no licence"}`);
    continue;
  }
  const files = readdirSync(dep.path)
    .filter((f) => /^(licen[cs]e|copying|notice)/i.test(f))
    .sort();
  const texts = files.map((f) => readFileSync(join(dep.path, f), "utf8").trim());
  parts.push(
    `-----------------------------------------------------------------------------\n` +
      `${key} (${license})\n\n` +
      (texts.length > 0 ? texts.join("\n\n") : `(The package carries no licence file; its package.json says ${license}.)`) +
      "\n",
  );
}
if (wrong.length > 0) {
  console.error(`npm packages under licences not on the allow list:\n  ${wrong.join("\n  ")}`);
  process.exit(1);
}
process.stdout.write(
  "\n=============================================================================\n" +
    "npm packages of the dashboard\n" +
    "=============================================================================\n\n" +
    parts.join("\n"),
);
