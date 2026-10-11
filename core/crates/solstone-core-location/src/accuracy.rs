// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Naming from the location-accuracy packs: "at X", "in X" and "near X".
//!
//! The words carry the confidence. Every rule here is about the map and the
//! phone's own margin of error, never about the owner, and every place is
//! treated the same way:
//!
//! - **margin**: the accuracy the phone reported, floored at 25 m. Past 100 m,
//!   or with no accuracy, there are no venue words at all, which covers
//!   approximate location by the same rule.
//! - **at X**: the whole margin lies inside X's outline (a named building, a
//!   named area such as a park or campus, or an airport). The smallest such
//!   outline is X; the next enclosing one is context.
//! - **in X**: the whole margin lies inside a neighbourhood or town boundary.
//! - **near X**: X is a named place within the margin. At most the three
//!   closest, each with its category. It describes the map around the
//!   position, so it is true by construction.
//!
//! "at" and "near" belong to stops (a visit the phone reported). Readings
//! taken on the move get "in" at most.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::pack::{Feature, Layer, Pack, PackKind};

/// The smallest margin a position is given, in metres.
pub const MARGIN_FLOOR_M: f64 = 25.0;
/// Past this reported accuracy, a position gets no venue words.
pub const MARGIN_CAP_M: f64 = 100.0;
/// "near" names at most this many places.
pub const NEAR_LIMIT: usize = 3;

/// The margin a position's words rely on, or none past the cap.
pub fn margin_m(accuracy_m: Option<f64>) -> Option<f64> {
    let accuracy = accuracy_m.filter(|value| value.is_finite() && *value >= 0.0)?;
    (accuracy <= MARGIN_CAP_M).then(|| accuracy.max(MARGIN_FLOOR_M))
}

/// The neighbourhood and town boundaries that hold a whole margin.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct Within {
    pub neighbourhood: Option<String>,
    pub locality: Option<String>,
}

impl Within {
    pub fn is_empty(&self) -> bool {
        self.neighbourhood.is_none() && self.locality.is_none()
    }
}

/// The outline a stop was inside.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AtPlace {
    pub name: String,
    /// `building`, `land_use` or `airport`.
    pub layer: &'static str,
    /// The source's class, e.g. `train_station` or `park`; may be empty.
    pub class: String,
    /// The next named outline around it, when there is one.
    pub inside: Option<String>,
    /// Its outer rings as `[longitude, latitude]`, for the map.
    #[serde(skip)]
    pub outline: Vec<Vec<[f64; 2]>>,
}

/// A named place within a stop's margin.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NearPlace {
    pub name: String,
    /// The source's category, e.g. `coffee_shop`; may be empty.
    pub category: String,
    pub distance_m: f64,
}

/// What the packs say about one stop.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StopWords {
    pub margin_m: f64,
    pub at: Option<AtPlace>,
    pub near: Vec<NearPlace>,
}

impl StopWords {
    pub fn is_empty(&self) -> bool {
        self.at.is_none() && self.near.is_empty()
    }
}

/// The installed packs, either of which may be absent.
pub struct Packs {
    venues: Option<Pack>,
    containment: Option<Pack>,
    identity: String,
}

impl std::fmt::Debug for Packs {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.identity)
    }
}

fn divisions(features: &[Feature]) -> Within {
    let pick = |subtypes: &[&str]| {
        features
            .iter()
            .filter(|feature| feature.layer == Layer::Division)
            .find(|feature| subtypes.contains(&feature.subtype.as_str()))
            .map(|feature| feature.name.clone())
    };
    let locality = pick(&["locality"]).or_else(|| pick(&["localadmin"]));
    let neighbourhood = pick(&["microhood", "neighborhood", "macrohood"])
        .filter(|name| Some(name) != locality.as_ref());
    Within {
        neighbourhood,
        locality,
    }
}

impl Packs {
    pub fn new(venues: Option<Pack>, containment: Option<Pack>) -> Self {
        let venues = venues.filter(|pack| pack.kind() == PackKind::Venues);
        let containment = containment.filter(|pack| pack.kind() == PackKind::Containment);
        let identity = format!(
            "venues:{};containment:{}",
            venues.as_ref().map_or("-", |pack| pack.version()),
            containment.as_ref().map_or("-", |pack| pack.version())
        );
        Self {
            venues,
            containment,
            identity,
        }
    }

    /// Which pack versions these are, for build records.
    pub fn identity(&self) -> &str {
        &self.identity
    }

    pub fn has_venues(&self) -> bool {
        self.venues.is_some()
    }

    pub fn has_containment(&self) -> bool {
        self.containment.is_some()
    }

    /// Licence notices for what is installed, to show beside the names.
    pub fn attributions(&self) -> Vec<String> {
        [&self.venues, &self.containment]
            .into_iter()
            .flatten()
            .map(|pack| pack.attribution().to_owned())
            .filter(|text| !text.is_empty())
            .collect()
    }

    /// The full licence notices for what is installed.
    pub fn notices(&self) -> Vec<String> {
        [&self.venues, &self.containment]
            .into_iter()
            .flatten()
            .flat_map(|pack| pack.notices().iter().cloned())
            .collect()
    }

    fn containing(&self, latitude: f64, longitude: f64, margin: f64) -> Vec<Feature> {
        self.containment
            .as_ref()
            .and_then(|pack| pack.features_containing(latitude, longitude, margin).ok())
            .unwrap_or_default()
    }

    /// The boundaries that hold a reading's whole margin.
    pub fn within(&self, latitude: f64, longitude: f64, accuracy_m: Option<f64>) -> Option<Within> {
        let margin = margin_m(accuracy_m)?;
        let within = divisions(&self.containing(latitude, longitude, margin));
        (!within.is_empty()).then_some(within)
    }

    /// Everything the packs say about a stop: its boundaries, the outline it
    /// was inside, and the places within its margin.
    pub fn stop(
        &self,
        latitude: f64,
        longitude: f64,
        accuracy_m: Option<f64>,
    ) -> Option<(Option<Within>, StopWords)> {
        let margin = margin_m(accuracy_m)?;
        let features = self.containing(latitude, longitude, margin);
        let within = Some(divisions(&features)).filter(|within| !within.is_empty());
        let mut outlines = features
            .iter()
            .filter(|feature| feature.layer != Layer::Division);
        let at = outlines.next().map(|feature| AtPlace {
            name: feature.name.clone(),
            layer: feature.layer.as_str(),
            class: feature.class.clone(),
            inside: outlines
                .find(|outer| outer.name != feature.name)
                .map(|outer| outer.name.clone()),
            outline: feature
                .polygons
                .iter()
                .filter_map(|polygon| polygon.first().cloned())
                .collect(),
        });
        let mut near: Vec<NearPlace> = Vec::new();
        if let Some(pack) = &self.venues {
            for (venue, distance) in pack
                .venues_within(latitude, longitude, margin)
                .unwrap_or_default()
            {
                // One name once: a chain's two records at one corner, or the
                // venue that shares the outline's name.
                if near.iter().any(|place| place.name == venue.name)
                    || at.as_ref().is_some_and(|at| at.name == venue.name)
                {
                    continue;
                }
                near.push(NearPlace {
                    name: venue.name,
                    category: venue.category,
                    distance_m: distance,
                });
                if near.len() == NEAR_LIMIT {
                    break;
                }
            }
        }
        Some((
            within,
            StopWords {
                margin_m: margin,
                at,
                near,
            },
        ))
    }
}

/// Where the download lives in a journal: under `cache/`, which backups skip.
pub fn accuracy_root(journal: &Path) -> PathBuf {
    journal.join("cache").join("location").join("accuracy")
}

/// The record of what is installed, written last by an install.
pub const INSTALLED_RECORD: &str = "installed.json";

/// One installed file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledFile {
    pub file: String,
    pub sha256: String,
    pub size_bytes: u64,
}

/// What `installed.json` says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Installed {
    pub version: String,
    pub files: Vec<InstalledFile>,
}

impl Installed {
    pub fn directory(&self, journal: &Path) -> PathBuf {
        accuracy_root(journal).join(&self.version)
    }

    pub fn size_bytes(&self) -> u64 {
        self.files.iter().map(|file| file.size_bytes).sum()
    }
}

/// Read `installed.json`, if the journal has a complete install.
pub fn read_installed(journal: &Path) -> Option<Installed> {
    let bytes = std::fs::read(accuracy_root(journal).join(INSTALLED_RECORD)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn open_installed(journal: &Path, installed: &Installed) -> Packs {
    let directory = installed.directory(journal);
    let mut venues = None;
    let mut containment = None;
    for file in &installed.files {
        let path = directory.join(&file.file);
        // The download verified every byte; a size that moved since means
        // the file was changed outside the journal, so it is not used.
        if std::fs::metadata(&path).map(|metadata| metadata.len()).ok() != Some(file.size_bytes) {
            continue;
        }
        match Pack::open(&path) {
            Ok(pack) if pack.kind() == PackKind::Venues => venues = Some(pack),
            Ok(pack) if pack.kind() == PackKind::Containment => containment = Some(pack),
            _ => {}
        }
    }
    Packs::new(venues, containment)
}

type LoadedPacks = Option<(PathBuf, Installed, Arc<Packs>)>;
static LOADED: Mutex<LoadedPacks> = Mutex::new(None);

/// The packs installed in this journal, opened once and reopened when the
/// install changes. `None` when the owner has not downloaded them.
pub fn installed_packs(journal: &Path) -> Option<Arc<Packs>> {
    let installed = read_installed(journal);
    let mut loaded = LOADED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(installed) = installed else {
        *loaded = None;
        return None;
    };
    if let Some((path, current, packs)) = loaded.as_ref()
        && path == journal
        && current == &installed
    {
        return Some(Arc::clone(packs));
    }
    let packs = Arc::new(open_installed(journal, &installed));
    if !packs.has_venues() && !packs.has_containment() {
        *loaded = None;
        return None;
    }
    *loaded = Some((journal.to_path_buf(), installed, Arc::clone(&packs)));
    Some(packs)
}

/// Close whatever this process has open, before files are removed.
pub fn release_loaded() {
    *LOADED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::pack::tests::fixture;

    const LAT: f64 = 39.7527;
    const LON: f64 = -105.0003;

    pub(crate) fn fixture_packs() -> Packs {
        Packs::new(
            Some(fixture("venues.slpk")),
            Some(fixture("containment.slpk")),
        )
    }

    fn offset(east_m: f64, north_m: f64) -> (f64, f64) {
        (
            LAT + north_m / 111_195.0,
            LON + east_m / (111_195.0 * LAT.to_radians().cos()),
        )
    }

    #[test]
    fn the_margin_is_floored_and_capped() {
        assert_eq!(margin_m(Some(5.0)), Some(25.0));
        assert_eq!(margin_m(Some(60.0)), Some(60.0));
        assert_eq!(margin_m(Some(100.0)), Some(100.0));
        assert_eq!(margin_m(Some(100.5)), None);
        assert_eq!(margin_m(Some(2000.0)), None);
        assert_eq!(margin_m(None), None);
        assert_eq!(margin_m(Some(f64::NAN)), None);
    }

    #[test]
    fn a_stop_inside_a_building_is_at_it_and_near_what_is_within_its_margin() {
        let packs = fixture_packs();
        let (lat, lon) = offset(-45.0, 0.0);
        let (within, words) = packs.stop(lat, lon, Some(12.0)).unwrap();
        assert_eq!(words.margin_m, 25.0);
        let at = words.at.unwrap();
        assert_eq!(at.name, "Invented Station");
        assert_eq!(at.inside.as_deref(), Some("Invented Plaza"));
        assert_eq!(
            within.unwrap(),
            Within {
                neighbourhood: Some("Lower Invented".to_owned()),
                locality: Some("Inventedville".to_owned())
            }
        );
        // The closest invented place is over 60 m away, past the 25 m margin.
        assert!(words.near.is_empty());
    }

    #[test]
    fn near_lists_at_most_three_closest_with_categories() {
        let packs = fixture_packs();
        let (_, words) = packs.stop(LAT, LON, Some(95.0)).unwrap();
        let near: Vec<(&str, &str)> = words
            .near
            .iter()
            .map(|place| (place.name.as_str(), place.category.as_str()))
            .collect();
        assert_eq!(
            near,
            [
                ("Invented Coffee", "coffee_shop"),
                ("Invented Diner", "restaurant"),
                ("Invented Books", "bookstore")
            ]
        );
        // The plaza reaches 150 m each way, so a 95 m margin is inside it.
        assert_eq!(words.at.unwrap().name, "Invented Plaza");
    }

    #[test]
    fn past_the_cap_there_are_no_venue_words() {
        let packs = fixture_packs();
        assert!(packs.stop(LAT, LON, Some(150.0)).is_none());
        assert!(packs.stop(LAT, LON, None).is_none());
        assert!(packs.within(LAT, LON, Some(2000.0)).is_none());
    }

    #[test]
    fn a_reading_gets_only_its_boundaries() {
        let packs = fixture_packs();
        let within = packs.within(LAT, LON, Some(10.0)).unwrap();
        assert_eq!(within.locality.as_deref(), Some("Inventedville"));
        assert_eq!(within.neighbourhood.as_deref(), Some("Lower Invented"));
    }

    #[test]
    fn identity_names_both_versions() {
        assert_eq!(
            fixture_packs().identity(),
            "venues:fixture-1;containment:fixture-1"
        );
        let venues_only = Packs::new(Some(fixture("venues.slpk")), None);
        assert_eq!(venues_only.identity(), "venues:fixture-1;containment:-");
        assert!(
            venues_only
                .stop(LAT, LON, Some(40.0))
                .unwrap()
                .1
                .at
                .is_none()
        );
    }

    #[test]
    fn an_installed_record_opens_its_files_and_follows_changes() {
        let journal = tempfile::tempdir().unwrap();
        assert!(installed_packs(journal.path()).is_none());
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/accuracy");
        let installed = Installed {
            version: "fixture-1".to_owned(),
            files: ["venues.slpk", "containment.slpk"]
                .iter()
                .map(|name| InstalledFile {
                    file: (*name).to_owned(),
                    sha256: String::new(),
                    size_bytes: std::fs::metadata(source.join(name)).unwrap().len(),
                })
                .collect(),
        };
        let directory = installed.directory(journal.path());
        std::fs::create_dir_all(&directory).unwrap();
        for file in &installed.files {
            std::fs::copy(source.join(&file.file), directory.join(&file.file)).unwrap();
        }
        let record = accuracy_root(journal.path()).join(INSTALLED_RECORD);
        std::fs::write(&record, serde_json::to_vec(&installed).unwrap()).unwrap();
        let packs = installed_packs(journal.path()).unwrap();
        assert!(packs.has_venues() && packs.has_containment());
        std::fs::remove_file(&record).unwrap();
        assert!(installed_packs(journal.path()).is_none());
    }
}
