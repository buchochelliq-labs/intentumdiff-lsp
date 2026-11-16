"""Stage the immutable parser components the native LSP acceptance session needs.

`crates/lsp-server/tests/lsp_session.rs` drives the real `intentumdiff-lsp-server` binary
through a full editor session, which needs real parser components plus a
`parser_manifest.json` describing them. This script stages exactly those components into
--out, so CI can point `INTENTUMDIFF_TEST_WASM_DIR` at it and run the session with
`INTENTUMDIFF_LSP_REQUIRE_ACCEPTANCE=1` (missing or unverified fixtures FAIL the gate
instead of skipping it).

Trust chain (same as intentumdiff-core's scripts/provision_parser_components.py):
  REGISTRY_REF (an immutable intentumdiff-registry commit, pinned below)
    -> registry.yaml at that commit pins, per parser repo, a build `ref` and the
       component's SHA-256
    -> the `parser-wasm` artifact of the successful CI run AT that ref is downloaded and
       its bytes must match the pinned SHA-256 before anything is written.
Changing what the acceptance runs against is therefore a reviewed edit of REGISTRY_REF
(or of the registry itself), never a silent "latest build".

Outputs in --out:
  <component>.wasm          verified component bytes
  parser_manifest.json      the manifest the engine resolves parsers from
  provenance.json           registry ref, per-component sha256 + source commit, and
                            whether the bytes were registry-verified; the acceptance test
                            re-hashes every component against it.

Usage:
  GH_TOKEN=<token with contents:read + actions:read on the buchochelliq-labs parser
  repos and intentumdiff-registry> python scripts/provision_lsp_fixtures.py --out build/wasm

  Offline / local bring-up from an existing bundle (bytes still checked against the
  registry unless --no-verify; --no-verify marks the fixture unverified, which the
  required acceptance gate rejects):
  python scripts/provision_lsp_fixtures.py --out build/wasm --from-dir /path/to/bundle
"""

from __future__ import annotations

import argparse
import hashlib
import io
import json
import os
import sys
import urllib.error
import urllib.parse
import urllib.request
import zipfile
from pathlib import Path

ORG = "buchochelliq-labs"
API = "https://api.github.com"
REGISTRY_REPO = "intentumdiff-registry"
# The registry commit whose pins the LSP acceptance runs against. Same vetted component
# inventory intentumdiff-core's CI provisions from; bump it deliberately, by review.
REGISTRY_REF = "cabcb9e78bd888d08d5c2250d7e8bd3065f2c7b9"

# slug -> manifest entries for that component (language -> extensions). The acceptance
# fixtures are TypeScript; the js-ts component also serves JavaScript/TSX.
COMPONENTS: dict[str, dict[str, list[str]]] = {
    "js-ts": {
        "javascript": [".cjs", ".js", ".jsx", ".mjs"],
        "tsx": [".tsx"],
        "typescript": [".cts", ".mts", ".ts"],
    },
}


def component_file(slug: str) -> str:
    return f"{slug.replace('-', '_')}_parser.wasm"


class _StripAuthOnRedirect(urllib.request.HTTPRedirectHandler):
    """Drop Authorization when a redirect leaves the API host (artifact zips 302 to blob
    storage, which rejects a bearer token it did not issue)."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        new = super().redirect_request(req, fp, code, msg, headers, newurl)
        if new is not None and (
            urllib.parse.urlsplit(newurl).netloc != urllib.parse.urlsplit(req.full_url).netloc
        ):
            new.remove_header("Authorization")
        return new


_OPENER = urllib.request.build_opener(_StripAuthOnRedirect)


def _get(url: str, token: str, accept: str = "application/vnd.github+json") -> bytes:
    req = urllib.request.Request(
        url,
        headers={
            "Authorization": f"Bearer {token}",
            "Accept": accept,
            "User-Agent": "intentumdiff-lsp-provision",
        },
    )
    with _OPENER.open(req, timeout=120) as resp:
        return resp.read()


def load_registry_pins(token: str) -> tuple[dict[str, str], dict[str, str]]:
    """({component filename: sha256}, {parser repo: pinned build ref}) at REGISTRY_REF."""
    try:
        import yaml
    except ImportError:  # pragma: no cover - environment problem
        sys.exit("registry verification needs PyYAML (pip install 'pyyaml>=6.0')")
    raw = _get(
        f"{API}/repos/{ORG}/{REGISTRY_REPO}/contents/registry.yaml?ref={REGISTRY_REF}",
        token,
        accept="application/vnd.github.raw",
    )
    document = yaml.safe_load(raw.decode("utf-8"))
    pins: dict[str, str] = {}
    refs: dict[str, str] = {}
    for plugin, entry in (document.get("plugins") or {}).items():
        pins.update(entry.get("wasm_checksums") or {})
        if entry.get("ref"):
            refs[plugin] = entry["ref"]
    if not pins:
        sys.exit("registry.yaml carries no wasm_checksums - refusing to 'verify' nothing")
    return pins, refs


def fetch_component(slug: str, token: str, ref: str) -> tuple[str, bytes]:
    """The component bytes from the `parser-wasm` artifact of the successful run AT `ref`.
    Returns (source commit, payload). Never writes."""
    repo = f"intentumdiff-{slug}-parser"
    wanted = component_file(slug)
    runs = json.loads(_get(
        f"{API}/repos/{ORG}/{repo}/actions/runs?status=success&head_sha={ref}&per_page=100",
        token,
    )).get("workflow_runs") or []
    problems = []
    for run in runs:
        if run.get("head_sha") != ref:
            continue
        artifacts = json.loads(_get(run["artifacts_url"], token)).get("artifacts", [])
        artifact = next((a for a in artifacts if a["name"] == "parser-wasm"), None)
        if artifact is None or artifact.get("expired"):
            problems.append(f"run {run['id']}: no live 'parser-wasm' artifact")
            continue
        with zipfile.ZipFile(io.BytesIO(_get(artifact["archive_download_url"], token))) as zf:
            for member in zf.namelist():
                name = Path(member).name
                for prefix in ("intentumdiff_", "intentdiff_"):
                    if name.startswith(prefix):
                        name = name[len(prefix):]
                        break
                if name == wanted:
                    return run["head_sha"], zf.read(member)
        problems.append(f"run {run['id']}: artifact has no {wanted}")
    detail = "; ".join(problems) or "no successful run at that ref"
    raise RuntimeError(f"{repo}: cannot obtain {wanted} at pinned ref {ref[:8]} - {detail}")


def write_manifest(out: Path, slugs: list[str]) -> None:
    parsers = {}
    extension_index = {}
    for slug in slugs:
        for language, extensions in COMPONENTS[slug].items():
            parsers[language] = {
                "extensions": extensions,
                "plugin_id": language,
                "wasm": component_file(slug),
            }
            for ext in extensions:
                extension_index[ext] = language
    manifest = {"schema_version": 1, "parsers": parsers, "extension_index": extension_index}
    (out / "parser_manifest.json").write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--out", required=True, help="staging dir (INTENTUMDIFF_TEST_WASM_DIR)")
    parser.add_argument("--from-dir", help="take component bytes from this local bundle "
                                           "instead of downloading them")
    parser.add_argument("--no-verify", action="store_true",
                        help="do not check bytes against the registry pins (fixture is "
                             "marked unverified; the required gate rejects it)")
    args = parser.parse_args()

    token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN") or ""
    if not token and not (args.from_dir and args.no_verify):
        sys.exit("need GH_TOKEN/GITHUB_TOKEN with read access to the registry and parser repos")
    pins, refs = ({}, {}) if args.no_verify else load_registry_pins(token)

    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    slugs = sorted(COMPONENTS)
    provenance = {"registry_repo": f"{ORG}/{REGISTRY_REPO}", "registry_ref": REGISTRY_REF,
                  "verified": not args.no_verify, "components": {}}
    failures = []
    for slug in slugs:
        name = component_file(slug)
        ref = refs.get(f"intentumdiff-{slug}-parser")
        try:
            if args.from_dir:
                payload, source = (Path(args.from_dir) / name).read_bytes(), "local"
            else:
                if not ref:
                    raise RuntimeError(f"intentumdiff-{slug}-parser has no pinned ref in the registry")
                source, payload = fetch_component(slug, token, ref)
        except (OSError, RuntimeError, urllib.error.HTTPError) as exc:
            failures.append(f"{slug}: {exc}")
            continue
        digest = hashlib.sha256(payload).hexdigest()
        if not args.no_verify:
            pinned = pins.get(name)
            if pinned is None:
                failures.append(f"{slug}: {name} is not pinned at registry {REGISTRY_REF[:8]}")
                continue
            if pinned != digest:
                failures.append(f"{slug}: {name} CHECKSUM MISMATCH - registry pins {pinned}, "
                                f"got {digest}")
                continue
        (out / name).write_bytes(payload)
        provenance["components"][name] = {"sha256": digest, "source": source, "ref": ref}
        print(f"staged {name} sha256={digest} source={source}"
              f"{'' if not args.no_verify else ' (UNVERIFIED)'}")

    if failures:
        print(f"\nFAILED to stage {len(failures)} component(s):", file=sys.stderr)
        for failure in failures:
            print(f"  {failure}", file=sys.stderr)
        sys.exit(1)
    write_manifest(out, slugs)
    (out / "provenance.json").write_text(json.dumps(provenance, indent=2, sort_keys=True) + "\n")
    print(f"\n{len(slugs)} component(s) staged in {out}")


if __name__ == "__main__":
    main()
