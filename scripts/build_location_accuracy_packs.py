# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc

"""Build the two optional location-accuracy packs from public Overture Maps data.

An owner who chooses "location accuracy" downloads both files. Each is the same
bytes for every journal, so the download says nothing about where anyone is,
and every lookup runs on the journal.

- ``venues.slpk``: named places (name, category, confidence, point) from
  Overture Places. Kept: confidence >= 0.5, not permanently closed, and at
  least one source other than Overture's own and the BrightQuery business
  registry. Licences: CDLA-Permissive-2.0, Apache-2.0 and CC0-1.0 by source.
- ``containment.slpk``: named outlines a position can be inside: Overture
  division areas (locality, localadmin, neighbourhood, macrohood, microhood;
  land only), every named land-use polygon, named airport outlines, and every
  named building. Licence: ODbL 1.0 (OpenStreetMap and Overture contributors).

Two modes:

    build_location_accuracy_packs.py extract --release 2026-09-23.1 --inputs DIR
    build_location_accuracy_packs.py build --release 2026-09-23.1 --inputs DIR --out DIR --version V
    build_location_accuracy_packs.py fixture FIXTURE.json --out DIR

``extract`` reads Overture's public release with DuckDB and writes the
filtered inputs as Parquet (these are what a rebuild needs, so they are what
gets mirrored). ``build`` turns them into the packs. ``fixture`` builds small
packs from a JSON file of invented features, for tests.

Container format (little-endian; ``varint`` is unsigned LEB128, ``zz`` a zigzag
varint, ``str`` a varint byte length then UTF-8; coordinates are integer
micro-degrees):

    magic    b"SLPK" + u16 format version (1) + u16 0
    u32      header length, then that many bytes of UTF-8 JSON: kind, version,
             source, licence, attribution, and the kind's lookup tables
             ("categories" for venues; "classes" and "tolerance_m" for
             containment)
    u32      tile count, then per tile, sorted by key:
             u64 key = z << 56 | x << 28 | y, u64 block offset, u32 block length
    blocks   each a raw LZMA2 stream

Tiles are a loose quadtree over longitude and latitude: at zoom z a tile spans
360/2^z by 180/2^z degrees. An item sits in the tile, at the deepest zoom where
its bounding box is no larger than a tile, that holds its box's centre, so the
item lies within that tile and its eight neighbours. Sparse sibling tiles are
merged into their parent, which keeps that property. A lookup visits, at every
zoom, the tiles touching the query box widened by one tile.

Venue block: varint count, then columns: zz delta latitude, zz delta longitude
(each from the previous venue), varint category index, u8 confidence (0-100),
str name.

Containment block: varint count, then per feature: varint class index, str
name, zz bounding box (west, south, east, north), varint polygon count, then
per polygon a varint ring count and per ring a varint point count and zz delta
longitude, zz delta latitude per point (the first from 0). The first ring of a
polygon is its outline and the rest are holes. Geometry is simplified at build
time; ``tolerance_m`` gives the most any edge moved, per layer, so a lookup can
widen its margin by it.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import lzma
import struct
import sys
from collections import defaultdict
from pathlib import Path

FORMAT_VERSION = 1
MAX_ZOOM = 16
MIN_ZOOM = 0
# Sibling tiles merge into their parent while the merged block stays under this.
MERGE_LIMIT_BYTES = 48 * 1024
METRES_PER_DEGREE = 111_195.0
# The most an edge may move when simplified, per layer, in metres.
TOLERANCE_M = {"division": 20.0, "land_use": 5.0, "airport": 5.0, "building": 1.0}
LZMA_FILTERS = [{"id": lzma.FILTER_LZMA2, "preset": 9 | lzma.PRESET_EXTREME, "dict_size": 1 << 20}]
CONTAINMENT_LAYERS = ("divisions", "land_use", "airports", "buildings")
LAYER_OF_INPUT = {"divisions": "division", "land_use": "land_use", "airports": "airport", "buildings": "building"}

VENUE_LICENCE = "CDLA-Permissive-2.0, Apache-2.0 and CC0-1.0, per source record"
VENUE_ATTRIBUTION = (
    "Places from Overture Maps Foundation (https://overturemaps.org), "
    "including data from Meta, Microsoft, Foursquare (Apache-2.0, see its NOTICE) "
    "and AllThePlaces (CC0-1.0); see https://docs.overturemaps.org/attribution/"
)
# Foursquare asks that its NOTICE travel with the data, in full.
FOURSQUARE_NOTICE = """© 2025 Foursquare Labs, Inc. All rights reserved. 

The Foursquare OS Places dataset (the “Data”) is licensed under the Apache License, Version 2.0 (the "License"). You may not use, modify, or distribute the Data except in compliance with the License. 
 
As set forth more fully in the License, if you use, modify, or distribute the Data, you must:
* provide recipients with a copy of the License.
* if applicable, include prominent notices to the extent you’ve changed the Data.
* preserve attribution to Foursquare, including preserving the full content of this NOTICE.txt file.
 
To ensure appropriate attribution to Foursquare, we recommend the following:
* if using/distributing the Data in flat file form as-is or after making changes/modifications: include this NOTICE.txt file, which may be modified to include an additional notice of your changes/modifications, if any. 
* if using/distributing the Data in API form as-is or after making changes/modifications: include a copy of the content from this NOTICE.txt file prominently in your developer documentation for such API, which may be modified to include an additional notice of your changes/modifications, if any. 
 
You may obtain a copy of the License at: http://www.apache.org/licenses/LICENSE-2.0. Unless required by applicable law or agreed to in writing, the Data distributed under the License is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.

See the License for the specific language governing permissions and limitations under the License.

We also encourage you to join our Placemaker community (https://opensource.foursquare.com/placemaker/)—where you can contribute and provide suggestions to improve the accuracy of the Data for future releases for yourself and others."""
VENUE_NOTICES = [
    FOURSQUARE_NOTICE,
    "Changes by sol pbc: places are filtered (confidence of at least 0.5, not "
    "permanently closed, at least one source other than a business registry) and "
    "reduced to name, category, confidence and position, re-encoded into this file.",
    "Licences: CDLA-Permissive-2.0 https://cdla.dev/permissive-2-0/ ; "
    "Apache-2.0 https://www.apache.org/licenses/LICENSE-2.0 ; "
    "CC0-1.0 https://creativecommons.org/publicdomain/zero/1.0/. "
    "The full texts of CDLA-Permissive-2.0 and Apache-2.0 follow.",
]
# Both licences ask for their text to travel with the data, not a link.
LICENCE_TEXTS = Path(__file__).resolve().parent.parent / "licenses"
VENUE_LICENCE_TEXTS = ("CDLA-Permissive-2.0.txt", "Apache-2.0.txt")
CONTAINMENT_LICENCE = "ODbL-1.0"
CONTAINMENT_NOTICES = [
    "This database is made available under the Open Database License: "
    "https://opendatacommons.org/licenses/odbl/1-0/. Rights in individual contents "
    "stay with their sources, under the licences listed at "
    "https://docs.overturemaps.org/attribution/.",
    "Some outlines are adapted from sources available under the Creative Commons "
    "Attribution 4.0 International licence (CC BY 4.0, "
    "https://creativecommons.org/licenses/by/4.0/): geoBoundaries; Esri Community Maps "
    "contributors; Land Information New Zealand (LINZ); Google Open Buildings; Qian Shi "
    "et al., building outlines for East Asian countries (doi:10.5281/zenodo.8174931); and "
    "work derived from BTN 2024, ign.es.",
    "Changes by sol pbc: named outlines are selected from Overture Maps divisions, "
    "land use, airports and buildings, simplified (tolerance per layer in "
    "tolerance_m) and re-encoded into this file by "
    "scripts/build_location_accuracy_packs.py in the solstone journal source.",
]
CONTAINMENT_ATTRIBUTION = (
    "Outlines © OpenStreetMap contributors and Overture Maps Foundation, available "
    "under the Open Database License (https://opendatacommons.org/licenses/odbl/1-0/); "
    "see https://docs.overturemaps.org/attribution/"
)

OWN_DATASETS = "('Overture', 'Overture-signals')"


def extract_queries(base: str) -> dict[str, str]:
    return {
        "places": f"""SELECT names.primary AS name, basic_category AS category, confidence,
                ST_X(geometry) AS lon, ST_Y(geometry) AS lat
            FROM read_parquet('{base}/theme=places/type=place/*')
            WHERE names.primary IS NOT NULL AND confidence >= 0.5
              AND coalesce(operating_status, '') <> 'permanently_closed'
              AND ST_GeometryType(geometry) = 'POINT'
              AND len(list_filter(sources, s -> s.dataset NOT IN {OWN_DATASETS}
                                               AND s.dataset <> 'BrightQuery')) > 0""",
        "divisions": f"""SELECT names.primary AS name, subtype, class, ST_AsWKB(geometry) AS wkb
            FROM read_parquet('{base}/theme=divisions/type=division_area/*')
            WHERE is_land AND names.primary IS NOT NULL
              AND subtype IN ('locality', 'localadmin', 'neighborhood', 'macrohood', 'microhood')""",
        "land_use": f"""SELECT names.primary AS name, subtype, class, ST_AsWKB(geometry) AS wkb
            FROM read_parquet('{base}/theme=base/type=land_use/*')
            WHERE names.primary IS NOT NULL
              AND ST_GeometryType(geometry) IN ('POLYGON', 'MULTIPOLYGON')""",
        "airports": f"""SELECT names.primary AS name, subtype, class, ST_AsWKB(geometry) AS wkb
            FROM read_parquet('{base}/theme=base/type=infrastructure/*')
            WHERE subtype = 'airport' AND class LIKE '%airport%' AND names.primary IS NOT NULL
              AND ST_GeometryType(geometry) IN ('POLYGON', 'MULTIPOLYGON')""",
        "buildings": f"""SELECT names.primary AS name, subtype, class, ST_AsWKB(geometry) AS wkb
            FROM read_parquet('{base}/theme=buildings/type=building/*')
            WHERE names.primary IS NOT NULL
              AND ST_GeometryType(geometry) IN ('POLYGON', 'MULTIPOLYGON')""",
    }


# --------------------------------------------------------------------- encoding


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


def text(value: str, out: bytearray) -> None:
    data = value.encode("utf-8")
    varint(len(data), out)
    out.extend(data)


def micro(degrees: float) -> int:
    return int(round(degrees * 1_000_000))


def tile_size(z: int) -> tuple[float, float]:
    return 360.0 / (1 << z), 180.0 / (1 << z)


def tile_of(lon: float, lat: float, z: int) -> tuple[int, int]:
    n = 1 << z
    x = min(n - 1, max(0, int((lon + 180.0) / 360.0 * n)))
    y = min(n - 1, max(0, int((lat + 90.0) / 180.0 * n)))
    return x, y


def tile_for_box(west: float, south: float, east: float, north: float) -> tuple[int, int, int]:
    width, height = east - west, north - south
    zoom = MIN_ZOOM
    for z in range(MAX_ZOOM, MIN_ZOOM - 1, -1):
        tile_w, tile_h = tile_size(z)
        if width <= tile_w and height <= tile_h:
            zoom = z
            break
    x, y = tile_of((west + east) / 2.0, (south + north) / 2.0, zoom)
    return zoom, x, y


def merge_sparse(tiles: dict, size_of) -> dict:
    """Fold sibling tiles into their parent while the merged block stays small."""

    def size(items: list) -> int:
        return sum(size_of(item) for item in items)

    for z in range(MAX_ZOOM, MIN_ZOOM, -1):
        parents: dict[tuple[int, int, int], list[tuple[int, int, int]]] = defaultdict(list)
        for key in [key for key in tiles if key[0] == z]:
            parents[(z - 1, key[1] >> 1, key[2] >> 1)].append(key)
        for parent, children in parents.items():
            total = size(tiles.get(parent, [])) + sum(size(tiles[child]) for child in children)
            if total > MERGE_LIMIT_BYTES:
                continue
            merged = tiles.setdefault(parent, [])
            for child in children:
                merged.extend(tiles.pop(child))
    return tiles


def write_pack(path: Path, header: dict, blocks: dict[tuple[int, int, int], bytes]) -> dict:
    header_bytes = json.dumps(header, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode("utf-8")
    keys = sorted(blocks, key=lambda key: (key[0] << 56) | (key[1] << 28) | key[2])
    directory_size = 4 + 20 * len(keys)
    offset = 4 + 4 + 4 + len(header_bytes) + directory_size
    entries = bytearray()
    entries += struct.pack("<I", len(keys))
    payload: list[bytes] = []
    for key in keys:
        block = blocks[key]
        entries += struct.pack("<QQI", (key[0] << 56) | (key[1] << 28) | key[2], offset, len(block))
        payload.append(block)
        offset += len(block)
    digest = hashlib.sha256()
    temporary = path.with_name(f".{path.name}.part")
    with temporary.open("wb") as handle:
        for piece in (b"SLPK", struct.pack("<HH", FORMAT_VERSION, 0), struct.pack("<I", len(header_bytes)), header_bytes, bytes(entries), *payload):
            handle.write(piece)
            digest.update(piece)
    temporary.replace(path)
    return {"file": path.name, "sha256": digest.hexdigest(), "size_bytes": path.stat().st_size, "tiles": len(keys)}


def compress(raw: bytes) -> bytes:
    return lzma.compress(raw, format=lzma.FORMAT_RAW, filters=LZMA_FILTERS)


def compress_all(raws: dict) -> dict:
    """Compress every block, in parallel when there are many."""
    keys = list(raws)
    if len(keys) < 64:
        return {key: compress(raws[key]) for key in keys}
    import multiprocessing
    import os

    with multiprocessing.Pool(max(1, (os.cpu_count() or 2) // 2)) as pool:
        compressed = pool.map(compress, (raws[key] for key in keys), chunksize=16)
    return dict(zip(keys, compressed))


# ----------------------------------------------------------------------- venues


def encode_venue(name: str, category: int, confidence: float, lon: float, lat: float) -> tuple[tuple[int, int, int], tuple]:
    key = tile_for_box(lon, lat, lon, lat)
    return key, (micro(lat), micro(lon), category, max(0, min(100, int(round(confidence * 100)))), name)


def venue_block(rows: list[tuple]) -> bytes:
    rows = sorted(rows)
    raw = bytearray()
    varint(len(rows), raw)
    previous = 0
    for row in rows:
        zigzag(row[0] - previous, raw)
        previous = row[0]
    previous = 0
    for row in rows:
        zigzag(row[1] - previous, raw)
        previous = row[1]
    for row in rows:
        varint(row[2], raw)
    raw.extend(bytes(row[3] for row in rows))
    for row in rows:
        text(row[4], raw)
    return bytes(raw)


def final_tiles(sizes: dict[tuple[int, int, int], int]) -> set[tuple[int, int, int]]:
    """The tiles points end up in after sparse siblings merge, from sizes alone."""
    tiles = {key: [size] for key, size in sizes.items()}
    return set(merge_sparse(tiles, lambda size: size))


def build_venues_streaming(connection, places: Path, out: Path, header: dict) -> dict:
    """Build the venue pack without holding every venue in memory.

    Every venue is a point, so it starts in its zoom-16 tile. DuckDB sums each
    tile's size, the merge runs on those sums alone, and the venues are then
    read back sorted by their final tile, one block at a time.
    """
    n = 1 << MAX_ZOOM
    connection.execute(f"""CREATE OR REPLACE TEMP TABLE venue AS
        SELECT name, coalesce(category, '') AS category, confidence, lon, lat,
               least({n - 1}, greatest(0, floor((lon + 180.0) / 360.0 * {n})))::BIGINT AS x,
               least({n - 1}, greatest(0, floor((lat + 90.0) / 180.0 * {n})))::BIGINT AS y
        FROM '{places}'""")
    sizes = {
        (MAX_ZOOM, x, y): size
        for x, y, size in connection.execute(
            "SELECT x, y, sum(12 + strlen(name))::BIGINT FROM venue GROUP BY x, y"
        ).fetchall()
    }
    finals = final_tiles(sizes)
    owner = {}
    for _, x, y in sizes:
        for z in range(MAX_ZOOM, MIN_ZOOM - 1, -1):
            shift = MAX_ZOOM - z
            key = (z, x >> shift, y >> shift)
            if key in finals:
                owner[(x, y)] = (key[0] << 56) | (key[1] << 28) | key[2]
                break
    del sizes
    import pyarrow

    owners = pyarrow.table({
        "x": pyarrow.array([x for x, _ in owner], pyarrow.int64()),
        "y": pyarrow.array([y for _, y in owner], pyarrow.int64()),
        "tile": pyarrow.array(list(owner.values()), pyarrow.uint64()),
    })
    del owner
    connection.register("tile_owners", owners)
    connection.execute("CREATE OR REPLACE TEMP TABLE tile_owner AS SELECT * FROM tile_owners")
    connection.unregister("tile_owners")
    del owners
    categories = [row[0] for row in connection.execute("SELECT DISTINCT category FROM venue ORDER BY category").fetchall()]
    category_index = {name: index for index, name in enumerate(categories)}
    count = connection.execute("SELECT count(*) FROM venue").fetchone()[0]
    reader = connection.execute(
        """SELECT o.tile, v.name, v.category, v.confidence, v.lon, v.lat
           FROM venue v JOIN tile_owner o USING (x, y) ORDER BY o.tile"""
    ).to_arrow_reader(200_000)

    def raw_blocks():
        current, rows = None, []
        for batch in reader:
            columns = batch.to_pydict()
            for tile, name, category, confidence, lon, lat in zip(
                columns["tile"], columns["name"], columns["category"], columns["confidence"], columns["lon"], columns["lat"]
            ):
                if tile != current and rows:
                    yield current, venue_block(rows)
                    rows = []
                current = tile
                rows.append(encode_venue(name, category_index[category], confidence, lon, lat)[1])
        if rows:
            yield current, venue_block(rows)

    header = {**header, "kind": "venues", "items": count, "categories": categories}
    return write_pack_streaming(out, header, raw_blocks())


def write_pack_streaming(path: Path, header: dict, raw_blocks) -> dict:
    """Write a pack from (tile key, raw block) pairs arriving in key order."""
    import multiprocessing
    import os
    import tempfile

    keys: list[int] = []
    sizes: list[int] = []
    with tempfile.TemporaryFile(dir=path.parent) as spool:
        with multiprocessing.Pool(max(1, (os.cpu_count() or 2) // 2)) as pool:
            pending = ((key, raw) for key, raw in raw_blocks)

            def compressed():
                batch = []
                for item in pending:
                    batch.append(item)
                    if len(batch) == 256:
                        yield from zip((key for key, _ in batch), pool.map(compress, [raw for _, raw in batch]))
                        batch = []
                if batch:
                    yield from zip((key for key, _ in batch), pool.map(compress, [raw for _, raw in batch]))

            for key, block in compressed():
                if keys and key <= keys[-1]:
                    raise ValueError("blocks must arrive in tile order")
                keys.append(key)
                sizes.append(len(block))
                spool.write(block)
        header_bytes = json.dumps(header, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode("utf-8")
        offset = 4 + 4 + 4 + len(header_bytes) + 4 + 20 * len(keys)
        entries = bytearray(struct.pack("<I", len(keys)))
        for key, size in zip(keys, sizes):
            entries += struct.pack("<QQI", key, offset, size)
            offset += size
        digest = hashlib.sha256()
        temporary = path.with_name(f".{path.name}.part")
        with temporary.open("wb") as handle:
            for piece in (b"SLPK", struct.pack("<HH", FORMAT_VERSION, 0), struct.pack("<I", len(header_bytes)), header_bytes, bytes(entries)):
                handle.write(piece)
                digest.update(piece)
            spool.seek(0)
            for chunk in iter(lambda: spool.read(1 << 20), b""):
                handle.write(chunk)
                digest.update(chunk)
        temporary.replace(path)
    return {"file": path.name, "sha256": digest.hexdigest(), "size_bytes": path.stat().st_size, "tiles": len(keys)}


# ------------------------------------------------------------------ containment


def encode_feature(class_index: int, name: str, polygons: list[list[list[tuple[float, float]]]]) -> tuple[tuple[int, int, int], bytes] | None:
    points = [point for polygon in polygons for ring in polygon for point in ring]
    if not points:
        return None
    west = min(point[0] for point in points)
    east = max(point[0] for point in points)
    south = min(point[1] for point in points)
    north = max(point[1] for point in points)
    raw = bytearray()
    varint(class_index, raw)
    text(name, raw)
    for value in (west, south, east, north):
        zigzag(micro(value), raw)
    varint(len(polygons), raw)
    for polygon in polygons:
        varint(len(polygon), raw)
        for ring in polygon:
            varint(len(ring), raw)
            previous_lon = previous_lat = 0
            for lon, lat in ring:
                lon_i, lat_i = micro(lon), micro(lat)
                zigzag(lon_i - previous_lon, raw)
                zigzag(lat_i - previous_lat, raw)
                previous_lon, previous_lat = lon_i, lat_i
    return tile_for_box(west, south, east, north), bytes(raw)


def containment_block(features: list[bytes]) -> bytes:
    raw = bytearray()
    varint(len(features), raw)
    for feature in sorted(features):
        raw.extend(feature)
    return bytes(raw)


def build_containment(features, out: Path, header: dict) -> dict:
    """``features`` yields (layer, subtype, class, name, polygons)."""
    classes: dict[tuple[str, str, str], int] = {}
    tiles: dict[tuple[int, int, int], list[bytes]] = defaultdict(list)
    count = 0
    for layer, subtype, klass, name, polygons in features:
        index = classes.setdefault((layer, subtype or "", klass or ""), len(classes))
        encoded = encode_feature(index, name, polygons)
        if encoded is None:
            continue
        key, raw = encoded
        tiles[key].append(raw)
        count += 1
    tiles = merge_sparse(dict(tiles), len)
    ordered = [list(item) for item, _ in sorted(classes.items(), key=lambda item: item[1])]
    header = {**header, "kind": "containment", "items": count, "classes": ordered, "tolerance_m": TOLERANCE_M}
    blocks = compress_all({key: containment_block(tiles.pop(key)) for key in list(tiles)})
    return write_pack(out, header, blocks)


# ------------------------------------------------------------------------ modes


def rings_of(geometry) -> list[list[list[tuple[float, float]]]]:
    polygons = list(geometry.geoms) if geometry.geom_type == "MultiPolygon" else [geometry]
    result = []
    for polygon in polygons:
        if polygon.is_empty:
            continue
        rings = [list(polygon.exterior.coords)[:-1]] + [list(ring.coords)[:-1] for ring in polygon.interiors]
        rings = [[(float(x), float(y)) for x, y in ring] for ring in rings if len(ring) >= 3]
        if rings and len(rings[0]) >= 3:
            result.append(rings)
    return result


def run_extract(arguments) -> None:
    import duckdb

    base = f"s3://overturemaps-us-west-2/release/{arguments.release}"
    inputs = Path(arguments.inputs)
    inputs.mkdir(parents=True, exist_ok=True)
    connection = duckdb.connect()
    connection.execute("INSTALL httpfs; LOAD httpfs; INSTALL spatial; LOAD spatial; SET s3_region='us-west-2';")
    for layer, query in extract_queries(base).items():
        if arguments.layer and layer != arguments.layer:
            continue
        target = inputs / f"{layer}.parquet"
        connection.execute(f"COPY ({query}) TO '{target}' (FORMAT PARQUET, COMPRESSION ZSTD, ROW_GROUP_SIZE 100000)")
        print(f"{layer}: {connection.execute(f'SELECT count(*) FROM {chr(39)}{target}{chr(39)}').fetchone()[0]} rows", flush=True)


def input_manifest(inputs: Path, names) -> dict:
    manifest = {}
    for name in names:
        path = inputs / f"{name}.parquet"
        digest = hashlib.sha256()
        with path.open("rb") as handle:
            for chunk in iter(lambda: handle.read(1 << 20), b""):
                digest.update(chunk)
        manifest[path.name] = {"sha256": digest.hexdigest(), "size_bytes": path.stat().st_size}
    return manifest


def run_build(arguments) -> None:
    import duckdb
    import shapely

    inputs, out = Path(arguments.inputs), Path(arguments.out)
    out.mkdir(parents=True, exist_ok=True)
    # A bounded memory budget: the venue table spills to disk, not RAM.
    scratch = out / ".duckdb-scratch"
    scratch.mkdir(exist_ok=True)
    connection = duckdb.connect(str(scratch / "build.duckdb"))
    connection.execute(f"SET memory_limit='{arguments.memory_limit}'; SET threads=8; SET temp_directory='{scratch}'")
    receipt = {"version": arguments.version, "release": arguments.release, "packs": []}

    def common(names) -> dict:
        source = {"overture": arguments.release, "inputs": input_manifest(inputs, names)}
        receipt.setdefault("inputs", {}).update(source["inputs"])
        return {"version": arguments.version, "source": source, "format": FORMAT_VERSION}

    if arguments.only in (None, "venues"):
        venue = build_venues_streaming(
            connection,
            inputs / "places.parquet",
            out / "venues.slpk",
            {
                **common(["places"]),
                "licence": VENUE_LICENCE,
                "attribution": VENUE_ATTRIBUTION,
                "notices": VENUE_NOTICES + [(LICENCE_TEXTS / name).read_text() for name in VENUE_LICENCE_TEXTS],
            },
        )
        receipt["packs"].append(venue)
        print(json.dumps(venue), flush=True)

    if arguments.only in (None, "containment"):
        def features():
            for name in CONTAINMENT_LAYERS:
                layer = LAYER_OF_INPUT[name]
                tolerance = TOLERANCE_M[layer] / METRES_PER_DEGREE
                reader = connection.execute(f"SELECT name, subtype, class, wkb FROM '{inputs / (name + '.parquet')}'").to_arrow_reader(50_000)
                for batch in reader:
                    columns = batch.to_pydict()
                    geometries = shapely.from_wkb(columns["wkb"], on_invalid="ignore")
                    simplified = shapely.simplify(geometries, tolerance, preserve_topology=True)
                    for feature_name, subtype, klass, geometry in zip(columns["name"], columns["subtype"], columns["class"], simplified):
                        if geometry is None or geometry.is_empty or geometry.geom_type not in ("Polygon", "MultiPolygon"):
                            continue
                        polygons = rings_of(geometry)
                        if polygons:
                            yield layer, subtype, klass, feature_name, polygons

        containment = build_containment(features(), out / "containment.slpk", {**common(CONTAINMENT_LAYERS), "licence": CONTAINMENT_LICENCE, "attribution": CONTAINMENT_ATTRIBUTION, "notices": CONTAINMENT_NOTICES})
        receipt["packs"].append(containment)
        print(json.dumps(containment), flush=True)

    (out / f"receipt-{arguments.only or 'all'}.json").write_text(json.dumps(receipt, indent=2, sort_keys=True) + "\n")


def run_fixture(arguments) -> None:
    global MERGE_LIMIT_BYTES
    MERGE_LIMIT_BYTES = arguments.merge_limit
    fixture = json.loads(Path(arguments.fixture).read_text())
    out = Path(arguments.out)
    out.mkdir(parents=True, exist_ok=True)
    common = {"version": fixture["version"], "source": {"fixture": Path(arguments.fixture).name}, "format": FORMAT_VERSION}
    features = (
        (f["layer"], f.get("subtype", ""), f.get("class", ""), f["name"], [[[tuple(point) for point in ring] for ring in polygon] for polygon in f["polygons"]])
        for f in fixture["features"]
    )
    import duckdb
    import tempfile

    connection = duckdb.connect()
    connection.execute("CREATE TABLE fixture_venues (name VARCHAR, category VARCHAR, confidence DOUBLE, lon DOUBLE, lat DOUBLE)")
    connection.executemany(
        "INSERT INTO fixture_venues VALUES (?, ?, ?, ?, ?)",
        [(v["name"], v["category"], v["confidence"], v["lon"], v["lat"]) for v in fixture["venues"]],
    )
    with tempfile.TemporaryDirectory() as scratch:
        places = Path(scratch) / "places.parquet"
        connection.execute(f"COPY fixture_venues TO '{places}' (FORMAT PARQUET)")
        venues = build_venues_streaming(connection, places, out / "venues.slpk", {**common, "licence": "fixture", "attribution": "invented test data"})
    print(json.dumps(venues))
    print(json.dumps(build_containment(features, out / "containment.slpk", {**common, "licence": "fixture", "attribution": "invented test data"})))


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    modes = parser.add_subparsers(dest="mode", required=True)
    extract = modes.add_parser("extract")
    extract.add_argument("--release", required=True)
    extract.add_argument("--inputs", required=True)
    extract.add_argument("--layer", choices=("places", *CONTAINMENT_LAYERS))
    build = modes.add_parser("build")
    build.add_argument("--release", required=True)
    build.add_argument("--inputs", required=True)
    build.add_argument("--out", required=True)
    build.add_argument("--version", required=True)
    build.add_argument("--only", choices=("venues", "containment"))
    build.add_argument("--memory-limit", default="8GB", help="DuckDB's memory budget")
    fixture = modes.add_parser("fixture")
    fixture.add_argument("fixture")
    fixture.add_argument("--out", required=True)
    fixture.add_argument("--merge-limit", type=int, default=MERGE_LIMIT_BYTES, help="0 keeps every item at its own tile")
    arguments = parser.parse_args(argv)
    {"extract": run_extract, "build": run_build, "fixture": run_fixture}[arguments.mode](arguments)
    return 0


if __name__ == "__main__":
    sys.exit(main())
