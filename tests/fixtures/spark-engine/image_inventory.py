"""Emit a deterministic package freeze or SPDX 2.3 SBOM from an image."""

import argparse
import hashlib
import importlib.metadata
import json
import re
from pathlib import Path
from urllib.parse import quote


def canonical(value):
    return re.sub(r"[-_.]+", "-", value).lower()


def installed_packages():
    packages = set()
    paragraphs = Path("/var/lib/dpkg/status").read_text(encoding="utf-8").split("\n\n")
    for paragraph in paragraphs:
        fields = dict(
            line.split(": ", 1)
            for line in paragraph.splitlines()
            if ": " in line and line.split(": ", 1)[0] in {"Package", "Version", "Architecture"}
        )
        if {"Package", "Version"} <= fields.keys():
            packages.add(("deb", fields["Package"], fields["Version"], fields.get("Architecture", "")))
    for distribution in importlib.metadata.distributions():
        name = distribution.metadata.get("Name")
        if name:
            packages.add(("pypi", canonical(name), distribution.version, ""))
    return sorted(packages)


def freeze(packages):
    return "".join("\t".join(package) + "\n" for package in packages)


def spdx(packages, image_digest, created):
    frozen = freeze(packages)
    document = {
        "SPDXID": "SPDXRef-DOCUMENT",
        "creationInfo": {
            "created": created,
            "creators": ["Tool: sy-spark-image-inventory/1"],
        },
        "dataLicense": "CC0-1.0",
        "documentNamespace": (
            f"https://sy.local/spdx/{image_digest.removeprefix('sha256:')}/"
            f"{hashlib.sha256(frozen.encode()).hexdigest()}"
        ),
        "name": f"sy-spark-image-{image_digest.removeprefix('sha256:')}",
        "packages": [],
        "relationships": [],
        "spdxVersion": "SPDX-2.3",
    }
    for ecosystem, name, version, architecture in packages:
        identifier = hashlib.sha256("\0".join((ecosystem, name, version, architecture)).encode()).hexdigest()[:20]
        spdx_id = f"SPDXRef-Package-{identifier}"
        package = {
            "SPDXID": spdx_id,
            "downloadLocation": "NOASSERTION",
            "externalRefs": [
                {
                    "referenceCategory": "PACKAGE-MANAGER",
                    "referenceLocator": f"pkg:{ecosystem}/{quote(name)}@{quote(version)}",
                    "referenceType": "purl",
                }
            ],
            "filesAnalyzed": False,
            "name": name,
            "supplier": "NOASSERTION",
            "versionInfo": version,
        }
        if architecture:
            package["primaryPackagePurpose"] = "LIBRARY"
            package["summary"] = f"Installed {architecture} Debian package"
        document["packages"].append(package)
        document["relationships"].append(
            {
                "relatedSpdxElement": spdx_id,
                "relationshipType": "DESCRIBES",
                "spdxElementId": "SPDXRef-DOCUMENT",
            }
        )
    return document


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("format", choices=["freeze", "spdx"])
    parser.add_argument("--image-digest")
    parser.add_argument("--created")
    arguments = parser.parse_args()
    packages = installed_packages()
    if arguments.format == "freeze":
        print(freeze(packages), end="")
        return
    if not arguments.image_digest or not arguments.created:
        parser.error("spdx requires --image-digest and --created")
    print(json.dumps(spdx(packages, arguments.image_digest, arguments.created), sort_keys=True, separators=(",", ":")))


if __name__ == "__main__":
    main()
