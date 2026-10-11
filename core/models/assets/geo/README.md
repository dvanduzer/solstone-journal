# area tier

`area-tier.bin` is the bundled area tier: towns from GeoNames cities500 and
country and state borders from Natural Earth, in the compact format documented
in `scripts/build_location_area_tier.py`. The journal reads it to name a
location reading after a nearby town and to draw the map app's borders. It
ships to every package as `lib/solstone_journal_models/assets/geo/area-tier.bin`
and its SHA-256 is pinned as `AREA_TIER_SHA256` in
`core/crates/solstone-core-location/src/asset.rs`. Licences:
`THIRD_PARTY_NOTICES.md` § bundled place names.

## inputs of the committed file

| input | source | sha256 |
|---|---|---|
| `cities500.zip` | https://download.geonames.org/export/dump/ (2026-10-11) | `52cd097abe516fccc5749f4e88f558ac358e63f2bfa1700dd1b8059bc00d8e5a` |
| `admin1CodesASCII.txt` | same | `1da92a6323a5fec3176f3f743bf4cf4040fd56a876da55e46fbca23c863aa60a` |
| `countryInfo.txt` | same | `c5997ea659df5bcf42509eb937064d172b2a998bb873608603e8421d714a9974` |
| `ne_10m_admin_0_countries.geojson` | https://github.com/nvkelso/natural-earth-vector, commit `ca96624a56bd078437bca8184e78163e5039ad19`, `geojson/` | `239eec57ac17f100a11e2536cffc56752c318b50ae765b0918ff7aab4ce8f255` |
| `ne_10m_admin_1_states_provinces.geojson` | same | `22d0e3ad85eb3e27f17cabf8ba2d50e554fbc27a87796ff891d958185da62fb5` |

Output: 225,397 towns, 3,777 first-level divisions, 4,596 state and province
outlines, 258 country outlines; 7,776,080 bytes, SHA-256
`a803dc5d4557b8b969c607834a3096d77803875e92c9acda7a5f9a1fd7c7aa21`.

## refreshing it

Download the five inputs into one directory, then run
`python3 scripts/build_location_area_tier.py <dir> core/models/assets/geo/area-tier.bin`.
Update `AREA_TIER_SHA256`, the component version for `area-tier` in
`core/crates/solstone-core-distribution/src/component_evidence.rs`, the digest
in `THIRD_PARTY_NOTICES.md` and the table above in the same commit. A new file changes names only where the sources
changed; every day's location document is rebuilt once, because its build
record names the area tier it used.
