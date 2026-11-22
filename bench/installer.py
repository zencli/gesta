#!/usr/bin/env python3
"""Check that install.sh can actually find a release asset, without installing anything.

The installer's prebuilt path was broken for every machine and every release, and it failed in the way
that reads as a fact about the machine rather than about the script: it asked for `gesta-<platform>`
when the release carries `gesta-<version>-<platform>.tar.gz`, got a 404, and printed "no prebuilt binary
for this machine". A user without Rust then saw an install failure and a correct impression that the
tool did not support them.

This checks the two ends of the name still agree: the tag a release publishes, and the asset names that
release actually carries. It reads the published release through the API, so it needs a network; without
one it skips rather than failing, because a runner with no egress should not report a defect.

Run: python3 bench/installer.py
"""

import json
import os
import re
import subprocess
import sys
import urllib.error
import urllib.request

_PROJECT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = "zencli/gesta"
SCRIPT = os.path.join(_PROJECT, "install.sh")

results = []


def record(name, ok, detail=""):
    results.append((name, ok, detail))
    print(f"  [{'PASS' if ok else 'FAIL'}] {name}")
    if not ok and detail:
        for line in str(detail).strip().splitlines()[:6]:
            print(f"         {line}")


def fetch(url):
    r = urllib.request.Request(url, headers={"User-Agent": "gesta-installer-check"})
    with urllib.request.urlopen(r, timeout=45) as x:
        return json.loads(x.read().decode("utf-8", "replace"))


def platform_for(script_text):
    """The platform string install.sh would compute on this machine, from its own case arms."""
    m_os = re.search(r"Linux\)\s*os_part=([a-z-]+)", script_text)
    linux = m_os.group(1) if m_os else "unknown-linux-gnu"
    m_arch = re.search(r"x86_64\|amd64\)\s*arch_part=([a-z0-9_]+)", script_text)
    arch = m_arch.group(1) if m_arch else "x86_64"
    return f"{arch}-{linux}"


def main() -> int:
    if not os.path.exists(SCRIPT):
        print(f"install.sh not found at {SCRIPT}")
        return 1
    text = open(SCRIPT, encoding="utf-8").read()

    # Static checks: these catch the shape of the bug without a network at all.
    record(
        "the asset name includes the version",
        'name="gesta-$bare-$plat' in text,
        "the release writes gesta-<version>-<platform>.tar.gz; a name without the version is a 404",
    )
    record(
        "the tag's leading v is stripped for the file name",
        "${VERSION#v}" in text,
        "v0.11.1 is the tag and 0.11.1 is the file; they are not the same string",
    )
    record(
        "the archive extension is read outside the platform subshell",
        "archive_ext" in text,
        "anything platform() sets in a command substitution is gone before the caller reads it",
    )
    record(
        "the archive is unpacked rather than executed directly",
        "tar -xzf" in text and "unzip" in text,
        "the asset is a .tar.gz / .zip, so a downloaded file is not a binary",
    )

    # The PowerShell installer is a second copy of the same name, and it carried the same defect: it
    # asked for `gesta-<platform>.exe`, which no release has ever carried.
    ps1 = os.path.join(_PROJECT, "install.ps1")
    if os.path.exists(ps1):
        ptext = open(ps1, encoding="utf-8").read()
        record(
            "install.ps1 builds the same versioned zip name",
            "gesta-$bare-$plat.zip" in ptext and "-replace '^v'" in ptext,
            "the Windows installer is a second copy of the same mapping and drifts independently",
        )
        record(
            "install.ps1 unpacks the archive",
            "Expand-Archive" in ptext,
            "the Windows asset is a .zip, so a downloaded file is not an executable",
        )

    # Live check against the published release, when the network allows it.
    try:
        rel = fetch(f"https://api.github.com/repos/{REPO}/releases/latest")
    except (urllib.error.URLError, urllib.error.HTTPError, OSError, json.JSONDecodeError) as e:
        print(f"  [SKIP] the published release could not be read: {e}")
        passed = sum(1 for _, ok, _ in results if ok)
        print(f"\n{passed}/{len(results)} static installer checks pass; the live check did not run")
        return 0 if passed == len(results) else 1

    tag = rel.get("tag_name", "")
    assets = [a.get("name", "") for a in rel.get("assets", [])]
    plat = platform_for(text)
    expected = f"gesta-{tag.lstrip('v')}-{plat}.tar.gz"

    record(
        f"the release {tag} carries the name the installer builds",
        expected in assets,
        f"installer would ask for {expected}\nrelease carries: {sorted(assets)}",
    )
    record(
        "no asset is named without its version",
        not any(re.fullmatch(r"gesta-[a-z0-9_]+-unknown-linux-gnu(\.tar\.gz)?", a) for a in assets),
        f"{sorted(assets)}",
    )

    passed = sum(1 for _, ok, _ in results if ok)
    total = len(results)
    print(f"\n{passed}/{total} installer checks pass")
    if passed != total:
        print("\nfailing:")
        for n, ok, _ in results:
            if not ok:
                print(f"  - {n}")
    return 0 if passed == total else 1


if __name__ == "__main__":
    sys.exit(main())
