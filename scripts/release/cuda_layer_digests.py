"""Print the digest of each `-cuda` image layer an `ADD ... rootfs-*.tar`
made, as a registry holds it: what a pull of the image compares against
the layers it already has. `.github/workflows/cuda.yml` runs it on two
builds over different Ubuntus and wants the same lines from both
(docs/dev/docker.md § "The CUDA variant").

    python3 scripts/release/cuda_layer_digests.py <registry> <repository> <tag>

Plain http, for a registry on the runner itself.
"""

import json
import sys
import urllib.request

ACCEPT = (
    "application/vnd.oci.image.index.v1+json, "
    "application/vnd.oci.image.manifest.v1+json, "
    "application/vnd.docker.distribution.manifest.list.v2+json, "
    "application/vnd.docker.distribution.manifest.v2+json"
)


def get(url: str) -> dict:
    request = urllib.request.Request(url, headers={"Accept": ACCEPT})
    with urllib.request.urlopen(request) as response:
        return json.load(response)


def main() -> int:
    registry, repository, tag = sys.argv[1:]
    base = f"http://{registry}/v2/{repository}"
    manifest = get(f"{base}/manifests/{tag}")
    if "manifests" in manifest:
        (image,) = [
            m
            for m in manifest["manifests"]
            if m.get("platform", {}).get("architecture") == "amd64"
        ]
        manifest = get(f"{base}/manifests/{image['digest']}")
    config = get(f"{base}/blobs/{manifest['config']['digest']}")
    made = [h for h in config["history"] if not h.get("empty_layer")]
    layers = manifest["layers"]
    if len(made) != len(layers):
        print(f"{len(made)} history entries for {len(layers)} layers", file=sys.stderr)
        return 1
    found = 0
    for step, layer in zip(made, layers):
        created_by = step.get("created_by", "")
        if "ADD" in created_by and "rootfs-" in created_by:
            name = created_by.split("rootfs-", 1)[1].split()[0]
            print(f"rootfs-{name} {layer['digest']} {layer['size']}")
            found += 1
    if found != 2:
        print(f"found {found} CUDA layers, not 2", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
