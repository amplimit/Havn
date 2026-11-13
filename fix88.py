import json


def edit(path: str, pairs: list[tuple[str, str]]) -> None:
    raw: str = open(path, "rb").read().decode("utf-8")
    crlf: bool = "\r\n" in raw
    s: str = raw.replace("\r\n", "\n")
    for old, new in pairs:
        assert s.count(old) == 1, (path, old[:70])
        s = s.replace(old, new)
    if crlf:
        s = s.replace("\n", "\r\n")
    open(path, "wb").write(s.encode("utf-8"))


edit(".github/workflows/audit.yml", [
    (
        """      # What the dashboard runs with; the lint and type-check tooling is not
      # part of a deployment. Reads package-lock.json, no install needed.
      - run: npm audit --omit=dev --audit-level=high""",
        """      # All dependencies: the README deployment installs the dev ones too and
      # runs the CSS toolchain in `npm run build`. Reads package-lock.json, so
      # no install is needed.
      - run: npm audit --audit-level=high""",
    ),
])

edit(".github/workflows/ci.yml", [
    (
        """      - run: npm run lint
      - run: npm run type-check""",
        """      # The 12 React Compiler warnings left for #87; no new ones.
      - run: npm run lint -- --max-warnings 12
      - run: npm run type-check""",
    ),
])

readme: str = open("README.md", "rb").read().decode("utf-8")
n: int = readme.count("Node.js 22+")
assert n == 2, n
readme = readme.replace("Node.js 22+", "Node.js 22.13+")
open("README.md", "wb").write(readme.encode("utf-8"))

p: str = "apps/dashboard/package.json"
pkg: dict[str, object] = json.load(open(p, encoding="utf-8"))
out: dict[str, object] = {}
for k, v in pkg.items():
    out[k] = v
    if k == "description":
        out["engines"] = {"node": ">=22.13"}
open(p, "w", encoding="utf-8", newline="\n").write(json.dumps(out, indent=2, ensure_ascii=False) + "\n")
