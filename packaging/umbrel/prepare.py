#!/usr/bin/env python3
"""Render a store package using an actual release's multiarchitecture index."""
import argparse
import hashlib
import json
import re
import subprocess
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("version")
parser.add_argument("destination", type=Path, help="parent directory for fedimintcard-till")
parser.add_argument("--revision", help="use the sha-FULL_COMMIT image tag for this package version")
parser.add_argument("--digest", required=True, help="sha256 index digest from the release job")
parser.add_argument("--submission-url", required=True, help="official store pull request URL")
args = parser.parse_args()
if not re.fullmatch(r"https://github\.com/getumbrel/umbrel-apps/pull/[1-9][0-9]*", args.submission_url):
    parser.error("expected a pull request URL in getumbrel/umbrel-apps")
if not re.fullmatch(r"(0|[1-9][0-9]*)(\.(0|[1-9][0-9]*)){2}", args.version):
    parser.error("expected an x.y.z release version")
if not re.fullmatch(r"sha256:[0-9a-f]{64}", args.digest):
    parser.error("expected the digest of the released index")
if args.revision and not re.fullmatch(r"[0-9a-f]{40}", args.revision):
    parser.error("expected the full 40-character source commit")
image_tag = f"sha-{args.revision}" if args.revision else args.version
tag = f"ghcr.io/fedimintcard/till:{image_tag}"
raw = subprocess.check_output(["docker", "buildx", "imagetools", "inspect", "--raw", tag])
digest = "sha256:" + hashlib.sha256(raw).hexdigest()
if digest != args.digest:
    parser.error(f"{tag} resolved to {digest}, expected {args.digest}")
manifest = json.loads(raw)
platforms = {m["platform"]["architecture"] for m in manifest.get("manifests", [])
             if m.get("platform", {}).get("os") == "linux"}
if not {"amd64", "arm64"} <= platforms:
    parser.error("both linux/amd64 and linux/arm64 are required")
src = Path(__file__).resolve().parent / "fedimintcard-till"
out = args.destination / src.name
out.mkdir(parents=True, exist_ok=False)
for item in src.glob("*.in"):
    (out / item.stem).write_text(item.read_text().replace("@VERSION@", args.version)
                                .replace("@IMAGE_TAG@", image_tag).replace("@DIGEST@", args.digest).replace("@SUBMISSION@", args.submission_url))
(out / "data" / "wallet").mkdir(parents=True)
(out / "data" / "wallet" / ".gitkeep").touch()
print(out)
