// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Independently committed sorted runs and resumable row assembly.
use super::*;
use sha2::{Digest, Sha256};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};

#[derive(Serialize, Deserialize)]
struct RunCommit {
    schema: u32,
    generation: ParentLossGeneration,
    sealed_phase: ParentLossPhase,
    seq: u64,
    chunk_digest: String,
    ids_digest: String,
    run_digest: String,
}

pub(super) struct Run {
    name: String,
    digest: String,
    pub ids: Vec<String>,
    source: Option<(u64, String, String)>,
}

fn private_options() -> fs::OpenOptions {
    let mut options = fs::OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

fn segment_limit() -> usize {
    #[cfg(test)]
    {
        let limit = ASSEMBLY_SEGMENT_LIMIT.with(|value| value.get());
        if limit > 0 {
            return limit.min(RECOVERY_BATCH_LIMIT);
        }
    }
    RECOVERY_BATCH_LIMIT
}

fn run_path(dir: &Path, name: &str) -> std::path::PathBuf {
    dir.join(format!("assembly-{name}.ndjson"))
}

fn commit_path(dir: &Path, seq: u64) -> std::path::PathBuf {
    dir.join(format!("assembly-run-{seq:06}.json"))
}

fn sorted_ids(bytes: &[u8]) -> Result<Vec<String>, ParentLossLedgerError> {
    let ids: RecoveryIdsFile = serde_json::from_slice(bytes)?;
    let mut ids = ids.ids;
    ids.sort();
    if ids.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(unverifiable_admission());
    }
    Ok(ids)
}

pub(super) fn cached_run(
    dir: &Path,
    generation: ParentLossGeneration,
    seq: u64,
    chunk_digest: &str,
    ids_digest: &str,
    ids_bytes: &[u8],
) -> Result<Option<Run>, ParentLossLedgerError> {
    let receipt = read_json_durable_validated::<RunCommit>(
        ArtifactId::ParentLossClosureRecovery,
        &commit_path(dir, seq),
        |r| {
            if r.schema != 1
                || r.generation != generation
                || r.sealed_phase != ParentLossPhase::Sealed
                || r.seq != seq
                || r.chunk_digest != chunk_digest
                || r.ids_digest != ids_digest
            {
                return Err("invalid assembly run commit".to_owned());
            }
            Ok(())
        },
    )?;
    let DurableRead::Present(receipt) = receipt else {
        return Ok(None);
    };
    Ok(Some(Run {
        name: format!("run-{seq:06}"),
        digest: receipt.run_digest,
        ids: sorted_ids(ids_bytes)?,
        source: Some((seq, chunk_digest.to_owned(), ids_digest.to_owned())),
    }))
}

fn encode_run(
    mut rows: Vec<PreparedAdmission>,
    ids: &[String],
) -> Result<Vec<u8>, ParentLossLedgerError> {
    rows.sort_unstable_by(|a, b| a.launch_id.cmp(&b.launch_id));
    if rows.len() > RECOVERY_BATCH_LIMIT
        || rows.len() != ids.len()
        || rows.iter().zip(ids).any(|(row, id)| row.launch_id != *id)
    {
        return Err(unverifiable_admission());
    }
    let mut bytes = Vec::new();
    for row in rows {
        serde_json::to_writer(&mut bytes, &row)?;
        bytes.push(b'\n');
    }
    Ok(bytes)
}

pub(super) fn commit_run(
    dir: &Path,
    generation: ParentLossGeneration,
    seq: u64,
    chunk_digest: &str,
    ids_digest: &str,
    ids_bytes: &[u8],
    rows: Vec<PreparedAdmission>,
) -> Result<Run, ParentLossLedgerError> {
    let ids = sorted_ids(ids_bytes)?;
    let bytes = encode_run(rows, &ids)?;
    let name = format!("run-{seq:06}");
    let digest = digest_bytes(&bytes);
    atomic_replace(
        run_path(dir, &name),
        &bytes,
        AtomicWriteOptions { mode: Some(0o600) },
    )?;
    write_json(
        commit_path(dir, seq),
        &RunCommit {
            schema: 1,
            generation,
            sealed_phase: ParentLossPhase::Sealed,
            seq,
            chunk_digest: chunk_digest.to_owned(),
            ids_digest: ids_digest.to_owned(),
            run_digest: digest.clone(),
        },
        json_options(),
    )?;
    Ok(Run {
        name,
        digest,
        ids,
        source: Some((seq, chunk_digest.to_owned(), ids_digest.to_owned())),
    })
}

pub(super) fn provisional_runs(
    dir: &Path,
    mut rows: Vec<PreparedAdmission>,
) -> Result<Vec<Run>, ParentLossLedgerError> {
    rows.sort_unstable_by(|a, b| a.launch_id.cmp(&b.launch_id));
    let mut result = Vec::new();
    let mut rows = rows.into_iter();
    loop {
        let part: Vec<_> = rows.by_ref().take(RECOVERY_BATCH_LIMIT).collect();
        if part.is_empty() {
            break;
        }
        let ids = part.iter().map(|r| r.launch_id.clone()).collect::<Vec<_>>();
        let bytes = encode_run(part, &ids)?;
        let name = format!("provisional-{:06}", result.len());
        atomic_replace(
            run_path(dir, &name),
            &bytes,
            AtomicWriteOptions { mode: Some(0o600) },
        )?;
        result.push(Run {
            name,
            digest: digest_bytes(&bytes),
            ids,
            source: None,
        });
    }
    Ok(result)
}

fn load_run(
    dir: &Path,
    generation: ParentLossGeneration,
    run: &Run,
) -> Result<Vec<Vec<u8>>, ParentLossLedgerError> {
    let path = run_path(dir, &run.name);
    let mut bytes = fs::read(&path)
        .ok()
        .filter(|b| digest_bytes(b) == run.digest);
    if bytes.is_none() {
        if path.exists() {
            set_aside(&path)?;
        }
        let Some((seq, chunk_digest, ids_digest)) = &run.source else {
            return Err(unverifiable_admission());
        };
        let chunk_path = dir.join(format!("audited-{seq:06}.json"));
        let source_bytes = fs::read(&chunk_path)?;
        if digest_bytes(&source_bytes) != *chunk_digest {
            set_aside(&chunk_path)?;
            set_aside(&commit_path(dir, *seq))?;
            return Err(unverifiable_admission());
        }
        let chunk: PreparedChunk = serde_json::from_slice(&source_bytes)?;
        if chunk.schema != 1
            || chunk.generation != generation
            || chunk.seq != *seq
            || chunk.ids_digest != *ids_digest
        {
            return Err(unverifiable_admission());
        }
        let repaired = encode_run(chunk.rows, &run.ids)?;
        if digest_bytes(&repaired) != run.digest {
            set_aside(&commit_path(dir, *seq))?;
            return Err(unverifiable_admission());
        }
        atomic_replace(&path, &repaired, AtomicWriteOptions { mode: Some(0o600) })?;
        bytes = Some(repaired);
    }
    let bytes = bytes.ok_or_else(unverifiable_admission)?;
    let lines: Vec<_> = bytes
        .split(|b| *b == b'\n')
        .filter(|b| !b.is_empty())
        .map(<[u8]>::to_vec)
        .collect();
    if lines.len() != run.ids.len() {
        return Err(unverifiable_admission());
    }
    Ok(lines)
}

#[derive(Serialize, Deserialize)]
struct Span {
    first_row: usize,
    last_row: usize,
    admission_start: u64,
    admission_end: u64,
    admission_digest: String,
    sealed_start: u64,
    sealed_end: u64,
    sealed_digest: String,
    sealed_rows: usize,
}

#[derive(Serialize, Deserialize)]
struct MergeCommit {
    schema: u32,
    generation: ParentLossGeneration,
    sealed_phase: ParentLossPhase,
    input_digest: String,
    positions: Vec<usize>,
    rows: usize,
    admission_end: u64,
    sealed_end: u64,
    spans: Vec<Span>,
}

impl MergeCommit {
    fn valid(
        &self,
        generation: ParentLossGeneration,
        input_digest: &str,
        runs: &[Run],
        total: usize,
    ) -> bool {
        if self.schema != 1
            || self.generation != generation
            || self.sealed_phase != ParentLossPhase::Sealed
            || self.input_digest != input_digest
            || self.positions.len() != runs.len()
            || self.rows > total
            || self
                .positions
                .iter()
                .zip(runs)
                .any(|(p, r)| *p > r.ids.len())
            || self.positions.iter().sum::<usize>() != self.rows
        {
            return false;
        }
        let (mut rows, mut admissions, mut sealed) = (0, 0, 0);
        for s in &self.spans {
            if s.first_row != rows
                || s.last_row <= rows
                || s.last_row > total
                || s.admission_start != admissions
                || s.admission_end < admissions
                || s.sealed_start != sealed
                || s.sealed_end < sealed
                || s.sealed_rows > s.last_row - s.first_row
            {
                return false;
            }
            rows = s.last_row;
            admissions = s.admission_end;
            sealed = s.sealed_end;
        }
        rows == self.rows
            && admissions == self.admission_end
            && sealed == self.sealed_end
            && (self.rows != total
                || self
                    .positions
                    .iter()
                    .zip(runs)
                    .all(|(p, r)| *p == r.ids.len()))
    }
}

pub(super) struct Spools {
    dir: std::path::PathBuf,
    commit: MergeCommit,
    all_ids: Vec<String>,
    sealed_ids: Vec<String>,
}

fn spool_paths(dir: &Path) -> [std::path::PathBuf; 2] {
    [
        dir.join("assembly-admissions.body"),
        dir.join("assembly-sealed.body"),
    ]
}

fn reset(dir: &Path) -> Result<(), ParentLossLedgerError> {
    for path in spool_paths(dir)
        .into_iter()
        .chain([dir.join("assembly-progress.json")])
    {
        if path.exists() {
            set_aside(&path)?;
        }
    }
    Ok(())
}

pub(super) fn merge(
    dir: &Path,
    generation: ParentLossGeneration,
    authority: &ClosingAuthority<'_>,
    runs: &[Run],
    all_ids: &[String],
    provisional: &HashMap<String, AdmissionSourceSnapshot>,
) -> Result<Option<Spools>, ParentLossLedgerError> {
    if runs.iter().map(|run| run.ids.len()).sum::<usize>() != all_ids.len() {
        return Err(unverifiable_admission());
    }
    let sorted_provisional: BTreeMap<_, _> = provisional.iter().collect();
    let run_bindings: Vec<_> = runs
        .iter()
        .map(|r| (&r.name, &r.digest, &r.ids, &r.source))
        .collect();
    let input_digest = digest_bytes(&serde_json::to_vec(&(
        all_ids,
        run_bindings,
        sorted_provisional,
    ))?);
    let progress_path = dir.join("assembly-progress.json");
    let retained = read_json_durable_validated::<MergeCommit>(
        ArtifactId::ParentLossClosureRecovery,
        &progress_path,
        |p| {
            if p.valid(generation, &input_digest, runs, all_ids.len()) {
                Ok(())
            } else {
                Err("invalid assembly merge commit".to_owned())
            }
        },
    )?;
    let mut progress = match retained {
        DurableRead::Present(p) => p,
        _ => {
            reset(dir)?;
            MergeCommit {
                schema: 1,
                generation,
                sealed_phase: ParentLossPhase::Sealed,
                input_digest,
                positions: vec![0; runs.len()],
                rows: 0,
                admission_end: 0,
                sealed_end: 0,
                spans: Vec::new(),
            }
        }
    };
    let paths = spool_paths(dir);
    let mut files = Vec::new();
    for (path, end) in paths
        .iter()
        .zip([progress.admission_end, progress.sealed_end])
    {
        let file = private_options().truncate(false).open(path)?;
        if file.metadata()?.len() < end {
            reset(dir)?;
            return Ok(None);
        }
        // A crash may leave bytes beyond the independent commit. They were never authoritative.
        file.set_len(end)?;
        files.push(file);
    }
    let ready_at_entry = progress.rows == all_ids.len();
    if !ready_at_entry {
        let mut heap = BinaryHeap::new();
        for (index, run) in runs.iter().enumerate() {
            if let Some(id) = run.ids.get(progress.positions[index]) {
                heap.push(Reverse((id.clone(), index)));
            }
        }
        let mut cache = HashMap::new();
        while progress.rows < all_ids.len() {
            if Instant::now() >= authority.deadline {
                return Ok(None);
            }
            let first_row = progress.rows;
            let mut admission_bytes = Vec::new();
            let mut sealed_bytes = Vec::new();
            let mut sealed_rows = 0;
            while progress.rows - first_row < segment_limit() && progress.rows < all_ids.len() {
                if Instant::now() >= authority.deadline {
                    break;
                }
                let Reverse((id, index)) = heap.pop().ok_or_else(unverifiable_admission)?;
                if id != all_ids[progress.rows] {
                    return Err(unverifiable_admission());
                }
                if let std::collections::hash_map::Entry::Vacant(entry) = cache.entry(index) {
                    entry.insert(load_run(dir, generation, &runs[index])?);
                }
                let row: PreparedAdmission =
                    serde_json::from_slice(&cache[&index][progress.positions[index]])?;
                if row.launch_id != id {
                    return Err(unverifiable_admission());
                }
                #[cfg(test)]
                ASSEMBLY_CONSUMED_ROWS.with(|count| count.set(count.get() + 1));
                admission_bytes.extend_from_slice(row.closure.as_bytes());
                admission_bytes.push(b'\n');
                if let Some(sealed) = row.sealed {
                    sealed_bytes.extend_from_slice(sealed.as_bytes());
                    sealed_bytes.push(b'\n');
                    sealed_rows += 1;
                }
                progress.positions[index] += 1;
                progress.rows += 1;
                if let Some(next) = runs[index].ids.get(progress.positions[index]) {
                    heap.push(Reverse((next.clone(), index)));
                }
            }
            if first_row == progress.rows {
                return Ok(None);
            }
            for ((file, end), bytes) in files
                .iter_mut()
                .zip([progress.admission_end, progress.sealed_end])
                .zip([&admission_bytes, &sealed_bytes])
            {
                file.seek(SeekFrom::Start(end))?;
                file.write_all(bytes)?;
                file.sync_all()?;
                #[cfg(test)]
                if ASSEMBLY_WRITE_FAULT.with(|fault| fault.get()) == 1 {
                    return Ok(None);
                }
            }
            #[cfg(test)]
            if ASSEMBLY_WRITE_FAULT.with(|fault| fault.get()) == 2 {
                return Ok(None);
            }
            let span = Span {
                first_row,
                last_row: progress.rows,
                admission_start: progress.admission_end,
                admission_end: progress.admission_end + admission_bytes.len() as u64,
                admission_digest: digest_bytes(&admission_bytes),
                sealed_start: progress.sealed_end,
                sealed_end: progress.sealed_end + sealed_bytes.len() as u64,
                sealed_digest: digest_bytes(&sealed_bytes),
                sealed_rows,
            };
            progress.admission_end = span.admission_end;
            progress.sealed_end = span.sealed_end;
            progress.spans.push(span);
            write_json(&progress_path, &progress, json_options())?;
            #[cfg(test)]
            if ASSEMBLY_STOP_AFTER_SEGMENT.with(|remaining| {
                let n = remaining.get();
                if n == 0 {
                    return false;
                }
                remaining.set(n - 1);
                n == 1
            }) {
                return Ok(None);
            }
        }
        if !heap.is_empty()
            || !progress
                .positions
                .iter()
                .zip(runs)
                .all(|(p, r)| *p == r.ids.len())
        {
            return Err(unverifiable_admission());
        }
        // Large finalization gets a fresh slice rather than competing with the last merge segment.
        if all_ids.len() > RECOVERY_BATCH_LIMIT {
            return Ok(None);
        }
    }
    let mut spools = Spools {
        dir: dir.to_owned(),
        commit: progress,
        all_ids: all_ids.to_vec(),
        sealed_ids: Vec::new(),
    };
    let mut ordinal = 0;
    let mut sealed_ids = Vec::new();
    spools.consume(false, |bytes| {
        for row in bytes.split(|b| *b == b'\n').filter(|row| !row.is_empty()) {
            let admission: AdmissionClosure = serde_json::from_slice(row)?;
            if spools.all_ids.get(ordinal) != Some(&admission.launch_id) {
                return Err(unverifiable_admission());
            }
            if admission.instance.is_some() && admission.uid.is_some() {
                sealed_ids.push(admission.launch_id);
            }
            ordinal += 1;
        }
        Ok(())
    })?;
    if ordinal != all_ids.len() {
        return Err(unverifiable_admission());
    }
    spools.sealed_ids = sealed_ids;
    Ok(Some(spools))
}

impl Spools {
    /// Each buffer is authenticated against a separate writer commit before it
    /// supplies output bytes. Do not mint a final digest from a mutable reread.
    fn consume(
        &self,
        sealed: bool,
        mut consume: impl FnMut(&[u8]) -> Result<(), ParentLossLedgerError>,
    ) -> Result<(), ParentLossLedgerError> {
        let path = &spool_paths(&self.dir)[usize::from(sealed)];
        let mut file = fs::File::open(path)?;
        let expected_end = if sealed {
            self.commit.sealed_end
        } else {
            self.commit.admission_end
        };
        if file.metadata()?.len() != expected_end {
            return Err(unverifiable_admission());
        }
        for span in &self.commit.spans {
            let (start, end, expected) = if sealed {
                (span.sealed_start, span.sealed_end, &span.sealed_digest)
            } else {
                (
                    span.admission_start,
                    span.admission_end,
                    &span.admission_digest,
                )
            };
            let mut bytes = vec![0; (end - start) as usize];
            file.read_exact(&mut bytes)?;
            #[cfg(test)]
            ASSEMBLY_SPOOL_READ_BYTES.with(|count| count.set(count.get() + bytes.len()));
            if digest_bytes(&bytes) != *expected {
                reset(&self.dir)?;
                return Err(unverifiable_admission());
            }
            let actual_rows = bytes
                .split(|b| *b == b'\n')
                .filter(|row| !row.is_empty())
                .count();
            let expected_rows = if sealed {
                span.sealed_rows
            } else {
                span.last_row - span.first_row
            };
            if actual_rows != expected_rows {
                reset(&self.dir)?;
                return Err(unverifiable_admission());
            }
            consume(&bytes)?;
        }
        Ok(())
    }

    pub(super) fn write_array(
        &self,
        target: &Path,
        sealed: bool,
        prefix: &[u8],
        suffix: &[u8],
    ) -> Result<String, ParentLossLedgerError> {
        let temporary = target.with_extension("assembling");
        let mut file = BufWriter::with_capacity(
            64 * 1024,
            private_options().truncate(true).open(&temporary)?,
        );
        let mut digest = Sha256::new();
        let mut write = |bytes: &[u8]| -> Result<(), ParentLossLedgerError> {
            file.write_all(bytes)?;
            digest.update(bytes);
            Ok(())
        };
        write(prefix)?;
        let expected_ids = if sealed {
            &self.sealed_ids
        } else {
            &self.all_ids
        };
        let mut ordinal = 0;
        let mut first = true;
        self.consume(sealed, |bytes| {
            for row in bytes.split(|b| *b == b'\n').filter(|row| !row.is_empty()) {
                let id = expected_ids
                    .get(ordinal)
                    .ok_or_else(unverifiable_admission)?;
                let expected_prefix = format!("{{\"launch_id\":{},", serde_json::to_string(id)?);
                if !row.starts_with(expected_prefix.as_bytes()) {
                    return Err(unverifiable_admission());
                }
                ordinal += 1;
                if !first {
                    write(b",")?;
                }
                first = false;
                write(row)?;
            }
            Ok(())
        })?;
        if ordinal != expected_ids.len() {
            return Err(unverifiable_admission());
        }
        write(suffix)?;
        file.flush()?;
        file.get_ref().sync_all()?;
        fs::rename(&temporary, target)?;
        fs::File::open(&self.dir)?.sync_all()?;
        Ok(format!("{:x}", digest.finalize()))
    }

    pub(super) fn existing_ledger_matches(
        &self,
        existing: &[u8],
    ) -> Result<bool, ParentLossLedgerError> {
        // Keep one immutable legacy byte snapshot through comparison and staging.
        // A valid non-array root was inequality in the original whole-Value comparator.
        if existing.iter().find(|b| !b.is_ascii_whitespace()) != Some(&b'[') {
            let _: serde_json::Value = serde_json::from_slice(existing)?;
            return Ok(false);
        }
        let mut expected_rows = Vec::new();
        let mut ordinal = 0;
        self.consume(true, |bytes| {
            for row in bytes.split(|b| *b == b'\n').filter(|r| !r.is_empty()) {
                let id = self
                    .sealed_ids
                    .get(ordinal)
                    .ok_or_else(unverifiable_admission)?;
                let prefix = format!("{{\"launch_id\":{},", serde_json::to_string(id)?);
                if !row.starts_with(prefix.as_bytes()) {
                    return Err(unverifiable_admission());
                }
                ordinal += 1;
            }
            // Only compact row bytes are retained; no whole-ledger Value tree.
            expected_rows.extend(
                bytes
                    .split(|b| *b == b'\n')
                    .filter(|r| !r.is_empty())
                    .map(<[u8]>::to_vec),
            );
            Ok(())
        })?;
        if ordinal != self.sealed_ids.len() {
            return Err(unverifiable_admission());
        }
        json_array_matches(existing, &expected_rows)
    }
}

fn json_array_matches(
    existing: &[u8],
    expected_rows: &[Vec<u8>],
) -> Result<bool, ParentLossLedgerError> {
    if existing.iter().find(|b| !b.is_ascii_whitespace()) != Some(&b'[') {
        let _: serde_json::Value = serde_json::from_slice(existing)?;
        return Ok(false);
    }
    struct Compare<'a>(&'a [Vec<u8>]);
    impl<'de> serde::de::Visitor<'de> for Compare<'_> {
        type Value = bool;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a ledger array")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<bool, A::Error> {
            let mut equal = true;
            let mut count = 0;
            while let Some(row) = seq.next_element::<serde_json::Value>()? {
                if let Some(expected) = self.0.get(count) {
                    let expected: serde_json::Value =
                        serde_json::from_slice(expected).map_err(serde::de::Error::custom)?;
                    equal &= row == expected;
                } else {
                    equal = false;
                }
                count += 1;
            }
            Ok(equal && count == self.0.len())
        }
    }
    let mut decoder = serde_json::Deserializer::from_slice(existing);
    let equal = serde::de::Deserializer::deserialize_seq(&mut decoder, Compare(expected_rows))?;
    decoder.end()?;
    Ok(equal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incremental_ledger_comparison_matches_the_whole_value_oracle() {
        let arrays = [
            "[]",
            "[null]",
            "[{}]",
            "[{\"a\":null}]",
            "[{\"n\":1}]",
            "[{\"n\":1.0}]",
            "[{\"nested\":{\"x\":1,\"y\":\"text\"}}]",
            "[ {\"nested\": {\"y\":\"text\",\"x\":1} } ]",
            "[{\"a\":1,\"a\":2}]",
            "[{\"a\":2}]",
            "[{\"uid\":501,\"parent_launch_id\":null}]",
            "[{\"uid\":501}]",
            "[{\"uid\":502,\"parent_launch_id\":null}]",
            "[{},null]",
            "[null,{}]",
            "[{\"unknown\":true}]",
        ];
        for expected in arrays {
            let oracle: serde_json::Value = serde_json::from_str(expected).unwrap();
            let rows: Vec<_> = oracle
                .as_array()
                .unwrap()
                .iter()
                .map(|row| serde_json::to_vec(row).unwrap())
                .collect();
            for actual in arrays
                .into_iter()
                .chain(["null", "{}", "true", "0", "\"text\""])
            {
                let old: serde_json::Value = serde_json::from_str(actual).unwrap();
                assert_eq!(
                    json_array_matches(actual.as_bytes(), &rows).unwrap(),
                    old == oracle,
                    "expected={expected}, actual={actual}"
                );
            }
            for malformed in [
                "[",
                "[] trailing",
                "[{\"unequal\":true},",
                "[{\"unequal\":true}] trailing",
            ] {
                assert!(json_array_matches(malformed.as_bytes(), &rows).is_err());
            }
        }
    }
}
