# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc

"""Build the bundled location area tier from public GeoNames and Natural Earth files.

The area tier names a location reading at town level ("near Golden, Colorado")
and draws country and state boundaries on the map app, entirely on the journal.
It is built once from public files and shipped byte-for-byte the same to every
journal, so nothing about an owner's location is ever looked up anywhere.

Inputs (download them yourself into one directory, then pass that directory):

- ``cities500.zip``, ``admin1CodesASCII.txt`` and ``countryInfo.txt`` from
  https://download.geonames.org/export/dump/ (CC BY 4.0, https://www.geonames.org/)
- ``ne_10m_admin_0_countries.geojson`` and ``ne_10m_admin_1_states_provinces.geojson``
  from https://github.com/nvkelso/natural-earth-vector/tree/<commit>/geojson
  (public domain, https://www.naturalearthdata.com/)

The output is deterministic for a given set of input bytes. It is deliberately
not compressed: the release producer refuses compressed members outside a
declared archive slot. ``core/models/assets/geo/README.md`` records the inputs
the committed file was built from.

Format (little-endian; ``varint`` is unsigned LEB128, ``zz`` is a zigzag varint,
``str`` is a varint byte length followed by UTF-8):

    magic   b"SLAT" + u16 version (1)
    varint  country count, then per country: 2 ASCII bytes ISO code, str name
    varint  admin-1 count, then per admin-1: varint country index, str name
    varint  town count, then per town, sorted by 1-degree cell then population:
            zz delta latitude (1e-5 deg), zz delta longitude (1e-5 deg),
            varint country index, varint admin-1 index + 1 (0 = none),
            varint population, str name, str ascii name ("" when identical)
    regions admin-1 boundaries, then country boundaries, each:
            varint feature count, then per feature: str name,
            2 ASCII bytes ISO country code ("??" when unknown),
            varint ring count, per ring: varint point count, then
            zz delta longitude and zz delta latitude (1e-4 deg) per point
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import struct
import zipfile
from pathlib import Path

FORMAT_VERSION = 1
# Sections of a populated place, and historical, abandoned or destroyed places,
# are not towns a reading should be named after.
EXCLUDED_FEATURE_CODES = frozenset({"PPLX", "PPLH", "PPLQ", "PPLW", "PPLCH"})
ADMIN1_TOLERANCE_DEG = 0.01
ADMIN0_TOLERANCE_DEG = 0.02
INPUTS = (
    "cities500.zip",
    "admin1CodesASCII.txt",
    "countryInfo.txt",
    "ne_10m_admin_0_countries.geojson",
    "ne_10m_admin_1_states_provinces.geojson",
)


def varint(value: int, out: bytearray) -> None:
    if value < 0:
        raise ValueError(f"varint must be non-negative: {value}")
    while True:
        byte = value & 0x7F
        value >>= 7
        if value:
            out.append(byte | 0x80)
        else:
            out.append(byte)
            return


def zigzag(value: int, out: bytearray) -> None:
    varint((value << 1) ^ (value >> 63), out)


def put_str(value: str, out: bytearray) -> None:
    encoded = value.encode("utf-8")
    varint(len(encoded), out)
    out.extend(encoded)


def put_code(code: str, out: bytearray) -> None:
    encoded = code.encode("ascii")
    if len(encoded) != 2:
        raise ValueError(f"country code must be two ASCII letters: {code!r}")
    out.extend(encoded)


def simplify(points: list[tuple[float, float]], tolerance: float) -> list[tuple[float, float]]:
    """Douglas-Peucker on one closed ring, keeping both endpoints."""
    if len(points) < 3:
        return points
    keep = [False] * len(points)
    keep[0] = keep[-1] = True
    stack = [(0, len(points) - 1)]
    while stack:
        first, last = stack.pop()
        ax, ay = points[first]
        bx, by = points[last]
        dx, dy = bx - ax, by - ay
        norm = math.hypot(dx, dy)
        best, best_index = -1.0, -1
        for index in range(first + 1, last):
            px, py = points[index]
            if norm == 0.0:
                distance = math.hypot(px - ax, py - ay)
            else:
                distance = abs(dy * px - dx * py + bx * ay - by * ax) / norm
            if distance > best:
                best, best_index = distance, index
        if best > tolerance and best_index > 0:
            keep[best_index] = True
            stack.append((first, best_index))
            stack.append((best_index, last))
    return [point for point, kept in zip(points, keep) if kept]


def rings_of(geometry: dict) -> list[list[list[float]]]:
    if geometry["type"] == "Polygon":
        polygons = [geometry["coordinates"]]
    elif geometry["type"] == "MultiPolygon":
        polygons = geometry["coordinates"]
    else:
        raise ValueError(f"unexpected geometry {geometry['type']}")
    return [ring for polygon in polygons for ring in polygon]


def country_code(value: object) -> str:
    code = str(value or "")
    return code if len(code) == 2 and code.isalpha() else "??"


def encode_regions(features: list[tuple[str, str, dict]], tolerance: float, out: bytearray) -> int:
    varint(len(features), out)
    points = 0
    for name, code, geometry in features:
        put_str(name, out)
        put_code(code, out)
        rings = []
        for ring in rings_of(geometry):
            simplified = simplify([(float(x), float(y)) for x, y in ring], tolerance)
            if len(simplified) >= 4:
                rings.append([(round(x * 1e4), round(y * 1e4)) for x, y in simplified])
        varint(len(rings), out)
        for ring in rings:
            varint(len(ring), out)
            points += len(ring)
            previous_x = previous_y = 0
            for x, y in ring:
                zigzag(x - previous_x, out)
                zigzag(y - previous_y, out)
                previous_x, previous_y = x, y
    return points


def cell(latitude: float, longitude: float) -> int:
    return int(math.floor(latitude + 90.0)) * 360 + int(math.floor(longitude + 180.0))


def build(source: Path) -> tuple[bytes, dict[str, int]]:
    countries: dict[str, str] = {}
    for line in (source / "countryInfo.txt").read_text(encoding="utf-8").splitlines():
        if not line or line.startswith("#"):
            continue
        fields = line.split("\t")
        countries[fields[0]] = fields[4]

    admin1_names: dict[str, str] = {}
    for line in (source / "admin1CodesASCII.txt").read_text(encoding="utf-8").splitlines():
        if line:
            fields = line.split("\t")
            admin1_names[fields[0]] = fields[1]

    with zipfile.ZipFile(source / "cities500.zip") as archive:
        text = archive.read("cities500.txt").decode("utf-8")
    towns = []
    for line in text.splitlines():
        fields = line.split("\t")
        if fields[7] in EXCLUDED_FEATURE_CODES:
            continue
        towns.append(
            (
                fields[1],
                fields[2],
                float(fields[4]),
                float(fields[5]),
                fields[8],
                f"{fields[8]}.{fields[10]}",
                int(fields[14] or 0),
            )
        )

    codes = sorted({town[4] for town in towns} | set(countries))
    code_index = {code: index for index, code in enumerate(codes)}
    admin1_keys = sorted({town[5] for town in towns if town[5] in admin1_names})
    admin1_index = {key: index for index, key in enumerate(admin1_keys)}
    towns.sort(key=lambda town: (cell(town[2], town[3]), -town[6], town[0]))

    out = bytearray(b"SLAT")
    out.extend(struct.pack("<H", FORMAT_VERSION))
    varint(len(codes), out)
    for code in codes:
        put_code(code, out)
        put_str(countries.get(code, code), out)
    varint(len(admin1_keys), out)
    for key in admin1_keys:
        varint(code_index[key.split(".", 1)[0]], out)
        put_str(admin1_names[key], out)
    varint(len(towns), out)
    previous_lat = previous_lon = 0
    for name, ascii_name, latitude, longitude, code, admin1, population in towns:
        lat, lon = round(latitude * 1e5), round(longitude * 1e5)
        zigzag(lat - previous_lat, out)
        zigzag(lon - previous_lon, out)
        previous_lat, previous_lon = lat, lon
        varint(code_index[code], out)
        varint(admin1_index[admin1] + 1 if admin1 in admin1_index else 0, out)
        varint(population, out)
        put_str(name, out)
        put_str("" if ascii_name == name else ascii_name, out)

    admin1_features = []
    for feature in json.loads((source / "ne_10m_admin_1_states_provinces.geojson").read_text(encoding="utf-8"))["features"]:
        properties = feature["properties"]
        admin1_features.append((properties.get("name") or "", country_code(properties.get("iso_a2")), feature["geometry"]))
    admin0_features = []
    for feature in json.loads((source / "ne_10m_admin_0_countries.geojson").read_text(encoding="utf-8"))["features"]:
        properties = feature["properties"]
        code = country_code(properties.get("ISO_A2_EH")) if country_code(properties.get("ISO_A2")) == "??" else country_code(properties.get("ISO_A2"))
        admin0_features.append((properties.get("NAME") or "", code, feature["geometry"]))
    admin1_points = encode_regions(admin1_features, ADMIN1_TOLERANCE_DEG, out)
    admin0_points = encode_regions(admin0_features, ADMIN0_TOLERANCE_DEG, out)
    stats = {
        "countries": len(codes),
        "admin1": len(admin1_keys),
        "towns": len(towns),
        "admin1_features": len(admin1_features),
        "admin1_points": admin1_points,
        "country_features": len(admin0_features),
        "country_points": admin0_points,
        "bytes": len(out),
    }
    return bytes(out), stats


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("source", type=Path, help="directory holding the downloaded inputs")
    parser.add_argument("output", type=Path, help="area tier file to write")
    args = parser.parse_args()
    for name in INPUTS:
        digest = hashlib.sha256((args.source / name).read_bytes()).hexdigest()
        print(f"input {name} sha256 {digest}")
    data, stats = build(args.source)
    args.output.write_bytes(data)
    print(json.dumps(stats, sort_keys=True))
    print(f"output {args.output.name} sha256 {hashlib.sha256(data).hexdigest()}")


if __name__ == "__main__":
    main()
