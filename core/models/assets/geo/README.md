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

[`sources.json`](sources.json) records input SHA-256 hashes, output counts,
the output digest and component version. GeoNames inputs come from
https://download.geonames.org/export/dump/. Natural Earth inputs are pinned to
commit `ca96624a56bd078437bca8184e78163e5039ad19` of
https://github.com/nvkelso/natural-earth-vector, under `geojson/`.

## refreshing it

In an isolated preparation checkout, run:

```sh
python3 scripts/refresh_location_area_tier.py --cache /var/tmp/area-tier-inputs --receipt /var/tmp/area-tier-refresh.json --apply
```

The command downloads current GeoNames inputs and caches Natural Earth by its
pinned digest. It rejects truncated, malformed or substantially shrunken data
and retains the last good file, recording the reason in the receipt. Local pin
drift returns nonzero. Without `--apply`, it checks for an
update and writes only the cache and receipt.

When output changes, it updates the file, `AREA_TIER_SHA256`, both `area-tier`
component version declarations, the notices digest and `sources.json` together.
Commit them together before the release cut. Identical output leaves them
untouched, even if the ZIP metadata changed. A data-only update does not itself
call for a release; each selected journal release carries the refreshed file.

A new digest causes one pass over location days to re-read and re-render their
surviving readings with current names. The small build record changes; the
location document and its search index change only when the rendered words do.
Days without location are skipped. Unchanged output causes no extra pass.
