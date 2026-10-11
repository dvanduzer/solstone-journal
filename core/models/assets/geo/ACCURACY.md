# location accuracy packs

The optional "location accuracy" download is two files an owner fetches from
the update origin when they choose it in the map app. They are not bundled with
the journal and are not in this directory; only their pins are in the source
(`solstone-core-assets` catalog unit `location-accuracy`).

- `venues.slpk`: named places from Overture Maps Places, with name, category,
  confidence and position. Kept: confidence of at least 0.5, not permanently
  closed, and at least one source other than Overture's own and the BrightQuery
  business registry. Licences per source record: CDLA-Permissive-2.0,
  Apache-2.0 (Foursquare, whose NOTICE travels in the file's header) and
  CC0-1.0.
- `containment.slpk`: named outlines from Overture Maps divisions (locality,
  localadmin, neighbourhood, macrohood, microhood), land use, airports and
  buildings. Licence: ODbL 1.0.

`scripts/build_location_accuracy_packs.py` documents the format and builds both:

```sh
build_location_accuracy_packs.py extract --release <overture-release> --inputs DIR
build_location_accuracy_packs.py build --release <overture-release> --inputs DIR --out OUT --version <version>
```

`extract` reads Overture's public release and writes the filtered inputs as
Parquet; `build` turns them into the two packs and a receipt with the input
and output digests. Overture keeps a release for about 60 days, so the inputs a
published version was built from are kept alongside it. The journal reads the
packs with `solstone-core-location::pack` and names stops with
`solstone-core-location::accuracy`.

Test fixtures in `core/crates/solstone-core-location/fixtures/accuracy/` are
built by the same script's `fixture` mode from invented features.
