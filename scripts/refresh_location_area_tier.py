# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc

"""Refresh the bundled area tier, retaining the last good file on upstream failure."""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import http.client
import io
import json
import math
import os
import re
import tempfile
import urllib.error
import urllib.request
import zipfile
import zlib
from pathlib import Path

from build_location_area_tier import INPUTS, build

ROOT = Path(__file__).resolve().parent.parent
GEO = Path("core/models/assets/geo")
ASSET = Path("core/crates/solstone-core-location/src/asset.rs")
COMPONENT = Path("core/crates/solstone-core-distribution/src/component_evidence.rs")
NOTICES = Path("THIRD_PARTY_NOTICES.md")
GEONAMES = "https://download.geonames.org/export/dump/"
NE_COMMIT = "ca96624a56bd078437bca8184e78163e5039ad19"
NE_BASE = f"https://raw.githubusercontent.com/nvkelso/natural-earth-vector/{NE_COMMIT}/geojson/"
NE_DIGESTS = {
    INPUTS[3]: "239eec57ac17f100a11e2536cffc56752c318b50ae765b0918ff7aab4ce8f255",
    INPUTS[4]: "22d0e3ad85eb3e27f17cabf8ba2d50e554fbc27a87796ff891d958185da62fb5",
}
MAX_DOWNLOAD = 100_000_000
MAX_CITIES_TEXT = 150_000_000


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def download(url: str) -> bytes:
    request = urllib.request.Request(
        url, headers={"User-Agent": "solstone-area-tier/1"}
    )
    with urllib.request.urlopen(request, timeout=45) as response:
        data = response.read(MAX_DOWNLOAD + 1)
        if len(data) > MAX_DOWNLOAD:
            raise ValueError(f"download exceeds {MAX_DOWNLOAD} bytes: {url}")
        length = response.headers.get("Content-Length")
        if length is not None and len(data) != int(length):
            raise ValueError(f"truncated download: {url}")
        return data


def validate_geonames(inputs: dict[str, bytes]) -> None:
    """Reject partial tables and changed schemas before the permissive builder."""
    countries = set()
    for line in inputs["countryInfo.txt"].decode("utf-8").splitlines():
        if not line or line.startswith("#"):
            continue
        fields = line.split("\t")
        if (
            len(fields) != 19
            or not re.fullmatch(r"[A-Z]{2}", fields[0])
            or not fields[4]
        ):
            raise ValueError("countryInfo.txt: unexpected row schema")
        countries.add(fields[0])
    if len(countries) < 240:
        raise ValueError("countryInfo.txt: shrunken country table")
    admins = set()
    for line in inputs["admin1CodesASCII.txt"].decode("utf-8").splitlines():
        fields = line.split("\t")
        if len(fields) != 4 or not fields[1] or not fields[3].isdigit():
            raise ValueError("admin1CodesASCII.txt: unexpected row schema")
        admins.add(fields[0])
    if len(admins) < 3700:
        raise ValueError("admin1CodesASCII.txt: shrunken division table")
    with zipfile.ZipFile(io.BytesIO(inputs["cities500.zip"])) as archive:
        info = archive.getinfo("cities500.txt")
        if info.file_size > MAX_CITIES_TEXT:
            raise ValueError("cities500.txt: uncompressed input too large")
        text = archive.read(info).decode("utf-8")  # also verifies the ZIP CRC
    ids = set()
    for line in text.splitlines():
        fields = line.split("\t")
        if len(fields) != 19:
            raise ValueError("cities500.txt: unexpected row schema")
        lat, lon = float(fields[4]), float(fields[5])
        population = int(fields[14] or 0)
        if (
            not fields[0].isdigit()
            or fields[0] in ids
            or not fields[1]
            or fields[6] != "P"
            or fields[8] not in countries
            or not math.isfinite(lat)
            or not -90 <= lat <= 90
            or not math.isfinite(lon)
            or not -180 <= lon <= 180
            or not 0 <= population <= 2**63 - 1
        ):
            raise ValueError("cities500.txt: invalid or duplicate town")
        dt.date.fromisoformat(fields[18])
        ids.add(fields[0])


def validate_growth(stats: dict, previous: dict) -> None:
    # A >1% loss in one refresh is held for inspection; fixed floors prevent
    # repeated small losses from eroding the original worldwide coverage.
    floors = {"towns": 220_000, "admin1": 3700, "countries": 240}
    for key in ("towns", "admin1", "countries", "bytes"):
        if stats[key] < max(floors.get(key, 0), previous[key] * 0.99):
            raise ValueError(
                f"shrunken area tier: {key} {previous[key]} -> {stats[key]}"
            )


def replace_once(text: str, old: str, new: str) -> str:
    if text.count(old) != 1:
        raise ValueError(f"expected one occurrence of {old!r}")
    return text.replace(old, new)


def prepare_updates(
    root: Path, previous: dict, current: dict, data: bytes
) -> dict[Path, bytes]:
    updates = {GEO / "area-tier.bin": data}
    for path in (ASSET, NOTICES):
        text = (root / path).read_text()
        updates[path] = replace_once(
            text, previous["sha256"], current["sha256"]
        ).encode()
    text = (root / COMPONENT).read_text()
    old = f'let ver = "{previous["version"]}".to_string();'
    new = f'let ver = "{current["version"]}".to_string();'
    if text.count(old) != 2:
        raise ValueError("expected two area-tier component version declarations")
    updates[COMPONENT] = text.replace(old, new).encode()
    updates[GEO / "sources.json"] = (
        json.dumps(current, indent=2, sort_keys=True) + "\n"
    ).encode()
    return updates


def atomic_write(path: Path, data: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(dir=path.parent, delete=False) as stream:
        temporary = Path(stream.name)
        try:
            stream.write(data)
            stream.close()
            os.replace(temporary, path)
        finally:
            temporary.unlink(missing_ok=True)


def refresh(root: Path, cache: Path, apply: bool) -> dict:
    previous = json.loads((root / GEO / "sources.json").read_text())
    old_data = (root / GEO / "area-tier.bin").read_bytes()
    if digest(old_data) != previous["sha256"]:
        raise ValueError("committed area tier does not match sources.json")
    # Local pin drift is fatal, never disguised as an upstream fallback.
    prepare_updates(root, previous, previous, old_data)
    receipt = {
        "checked_at": dt.datetime.now(dt.timezone.utc).isoformat(),
        "previous_sha256": previous["sha256"],
        "sha256": previous["sha256"],
        "version": previous["version"],
        "applied": False,
    }
    try:
        cache.mkdir(parents=True, exist_ok=True)
        inputs = {}
        for name in INPUTS:
            if name in NE_DIGESTS:
                cached = cache / f"{NE_DIGESTS[name]}-{name}"
                data = cached.read_bytes() if cached.is_file() else b""
                if digest(data) != NE_DIGESTS[name]:
                    data = download(NE_BASE + name)
                    if digest(data) != NE_DIGESTS[name]:
                        raise ValueError(f"Natural Earth digest mismatch: {name}")
                    atomic_write(cached, data)
            else:
                data = download(GEONAMES + name)
            if (
                name in previous["input_bytes"]
                and len(data) < previous["input_bytes"][name] * 0.99
            ):
                raise ValueError(f"shrunken input: {name}")
            inputs[name] = data
        validate_geonames(inputs)
        with tempfile.TemporaryDirectory(prefix="area-tier-", dir=cache) as temporary:
            source = Path(temporary)
            for name, data in inputs.items():
                (source / name).write_bytes(data)
            data, stats = build(source)
        validate_growth(stats, previous["stats"])
    except (
        OSError,
        ValueError,
        KeyError,
        IndexError,
        zipfile.BadZipFile,
        EOFError,
        http.client.HTTPException,
        zlib.error,
    ) as error:
        receipt.update(status="retained", reason=f"{type(error).__name__}: {error}")
        return receipt
    receipt["inputs"] = {name: digest(raw) for name, raw in inputs.items()}
    receipt["input_bytes"] = {name: len(raw) for name, raw in inputs.items()}
    receipt["stats"] = stats
    receipt["sources_unchanged"] = receipt["inputs"] == previous["inputs"]
    if digest(data) == previous["sha256"]:
        receipt["status"] = "unchanged"
        return receipt
    today = dt.datetime.now(dt.timezone.utc).strftime("%Y%m%d")
    current = {
        "sha256": digest(data),
        "version": f"geonames-{today}-{digest(data)[:12]}-natural-earth-{NE_COMMIT[:7]}",
        "inputs": receipt["inputs"],
        "input_bytes": receipt["input_bytes"],
        "stats": stats,
    }
    updates = prepare_updates(root, previous, current, data)
    if apply:
        for path, contents in updates.items():
            atomic_write(root / path, contents)
    receipt.update(
        status="refreshed" if apply else "available",
        sha256=current["sha256"],
        version=current["version"],
        applied=apply,
    )
    return receipt


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--cache",
        type=Path,
        required=True,
        help="digest-checked Natural Earth input cache",
    )
    parser.add_argument(
        "--receipt",
        type=Path,
        required=True,
        help="JSON result, including retained-file reason",
    )
    parser.add_argument(
        "--apply",
        action="store_true",
        help="update this isolated checkout; default only checks",
    )
    args = parser.parse_args()
    receipt = refresh(ROOT, args.cache, args.apply)
    atomic_write(
        args.receipt, (json.dumps(receipt, indent=2, sort_keys=True) + "\n").encode()
    )
    print(json.dumps(receipt, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
