// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, FixedOffset, NaiveDate, TimeDelta, TimeZone, Utc};
use serde_json::{Map, Value};
use solstone_core_journal_io::{
    JournalRoot,
    operational_log::{OplogFormat, OplogWriter, create_oplog_at},
};

use crate::args::ThinkArgs;

pub(crate) fn mode(args: &ThinkArgs) -> &'static str {
    if args.activity.is_some() {
        "activity"
    } else if args.flush {
        "flush"
    } else if args.settle {
        "settle"
    } else if args.segments {
        "segments"
    } else if args.segment.is_some() {
        "segment"
    } else if args.weekly {
        "weekly"
    } else if args.cadence {
        "cadence"
    } else {
        "daily"
    }
}

/// Structured diagnostic writer for one think invocation.
///
/// Clones share the same append-only oplog, serialize each complete record, and
/// retain the first durability failure for the invocation boundary to surface.
pub(crate) struct RunLogWriter<W: Write = OplogWriter> {
    sink: Option<Arc<Mutex<W>>>,
    display_path: PathBuf,
    pub(crate) skip_count: usize,
    first_error: Arc<Mutex<Option<String>>>,
}

impl RunLogWriter<OplogWriter> {
    pub(crate) fn open(journal: &Path, day: &str, run: &str) -> Self {
        let display_path = journal.join("chronicle").join(day).join("health");
        let (sink, first_error) = match open_oplog(journal, day, run) {
            Ok(writer) => (Some(Arc::new(Mutex::new(writer))), None),
            Err(error) => (None, Some(error)),
        };
        Self {
            sink,
            display_path,
            skip_count: 0,
            first_error: Arc::new(Mutex::new(first_error)),
        }
    }
}

impl<W: Write> RunLogWriter<W> {
    #[cfg(test)]
    pub(crate) fn with_sink(display_path: PathBuf, sink: W) -> Self {
        Self {
            sink: Some(Arc::new(Mutex::new(sink))),
            display_path,
            skip_count: 0,
            first_error: Arc::new(Mutex::new(None)),
        }
    }

    /// Make another in-process handle to this invocation's same oplog.
    pub(crate) fn clone_for_shared_writes(&self) -> Self {
        Self {
            sink: self.sink.clone(),
            display_path: self.display_path.clone(),
            skip_count: 0,
            first_error: Arc::clone(&self.first_error),
        }
    }

    pub(crate) fn log(&mut self, event: &str, now_ms: i64, fields: Map<String, Value>) {
        self.log_checked(event, now_ms, fields, &|| Ok(()));
    }

    pub(crate) fn log_checked(
        &mut self,
        event: &str,
        now_ms: i64,
        fields: Map<String, Value>,
        checkpoint: &dyn Fn() -> Result<(), String>,
    ) {
        if event == "talent.skip" {
            self.skip_count += 1;
        }
        let Some(sink) = self.sink.as_ref() else {
            return;
        };
        let mut data = fields;
        data.insert("event".to_owned(), Value::String(event.to_owned()));
        data.insert("ts".to_owned(), Value::from(now_ms));
        let result: Result<(), String> = (|| {
            checkpoint()?;
            let mut record = serde_json::to_vec(&data).map_err(|error| error.to_string())?;
            record.push(b'\n');
            let mut sink = sink
                .lock()
                .map_err(|_| "think run log lock poisoned".to_owned())?;
            checkpoint()?;
            sink.write_all(&record).map_err(|error| error.to_string())?;
            sink.flush().map_err(|error| error.to_string())?;
            Ok(())
        })();
        if let Err(error) = result {
            self.remember_error(error);
        }
    }

    pub(crate) fn remember_error(&self, detail: String) {
        let mut first_error = self
            .first_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if first_error.is_none() {
            *first_error = Some(detail);
        }
    }

    pub(crate) fn finish(&self) -> Result<(), String> {
        let first_error = self
            .first_error
            .lock()
            .map_err(|_| "think run log failure lock poisoned".to_owned())?;
        match first_error.as_ref() {
            Some(detail) => Err(format!(
                "think run log {} failed: {detail}",
                self.display_path.display()
            )),
            None => Ok(()),
        }
    }

    pub(crate) fn summary(&mut self, now_ms: i64, message: String) {
        self.log(
            "run.summary",
            now_ms,
            Map::from_iter([("message".to_owned(), Value::String(message))]),
        );
    }
}

fn open_oplog(journal: &Path, day: &str, run: &str) -> Result<OplogWriter, String> {
    create_oplog_at(
        JournalRoot::open(journal).map_err(|error| error.to_string())?,
        "think",
        run,
        OplogFormat::Jsonl,
        current_time_on_local_day(journal, day)?,
    )
    .map_err(|error| error.to_string())
}

/// Now's wall-clock time placed on `day`, at that day's own offset in the
/// owner zone, so a day across a daylight-time change keeps its own offset.
fn current_time_on_local_day(journal: &Path, day: &str) -> Result<DateTime<FixedOffset>, String> {
    let day = NaiveDate::parse_from_str(day, "%Y%m%d").map_err(|error| error.to_string())?;
    let zone = solstone_core_journal_config::owner_zone(journal);
    let wall = day.and_time(Utc::now().with_timezone(&zone).time());
    zone.from_local_datetime(&wall)
        .earliest()
        .or_else(|| {
            zone.from_local_datetime(&(wall + TimeDelta::hours(1)))
                .earliest()
        })
        .map(|instant| instant.fixed_offset())
        .ok_or_else(|| "invalid local day".to_owned())
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use serde_json::{Map, Value};

    use super::RunLogWriter;

    #[cfg(feature = "full-tests")]
    #[test]
    fn cancelled_terminal_waiting_for_the_sink_cannot_append_success() {
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            mpsc,
        };
        use std::time::Duration;
        for cancel in [false, true] {
            let mut log = RunLogWriter::with_sink(PathBuf::from("fixture"), Vec::<u8>::new());
            let sink = log.sink.as_ref().unwrap().clone();
            let held = sink.lock().unwrap();
            let cancelled = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&cancelled);
            let (checking, reached) = mpsc::channel();
            let (ended, done) = mpsc::channel();
            let worker = std::thread::spawn(move || {
                log.log_checked("talent.complete", 1, Map::new(), &|| {
                    checking.send(()).unwrap();
                    if flag.load(Ordering::Acquire) {
                        Err("cancelled".into())
                    } else {
                        Ok(())
                    }
                });
                ended.send(log.finish()).unwrap();
            });
            reached.recv_timeout(Duration::from_secs(2)).unwrap();
            assert!(done.recv_timeout(Duration::from_millis(50)).is_err());
            cancelled.store(cancel, Ordering::Release);
            drop(held);
            let result = done.recv_timeout(Duration::from_secs(5)).unwrap();
            worker.join().unwrap();
            let bytes = sink.lock().unwrap();
            if cancel {
                assert!(result.is_err());
                assert!(
                    bytes.is_empty(),
                    "no Finish may be appended after cancelled sink admission"
                );
            } else {
                result.unwrap();
                let record: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(record["event"], "talent.complete");
            }
        }
    }

    struct PartialThenError(bool);

    impl Write for PartialThenError {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.0 {
                Err(io::Error::other("partial write failed"))
            } else {
                self.0 = true;
                Ok(bytes.len().min(1))
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct WriteZero;

    impl Write for WriteZero {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Ok(0)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct FlushFailure;

    impl Write for FlushFailure {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("flush failed"))
        }
    }

    #[derive(Clone)]
    struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedBuffer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn partial_write_write_zero_and_flush_failures_are_retained() {
        let cases: Vec<(Box<dyn Write>, &str)> = vec![
            (Box::new(PartialThenError(false)), "partial write failed"),
            (Box::new(WriteZero), "failed to write whole buffer"),
            (Box::new(FlushFailure), "flush failed"),
        ];
        for (sink, expected) in cases {
            let mut writer = RunLogWriter::with_sink(PathBuf::from("run.jsonl"), sink);
            writer.log("run.start", 1, Map::new());
            assert!(
                writer.finish().unwrap_err().contains(expected),
                "expected {expected}"
            );
        }
    }

    #[test]
    fn shared_writers_keep_every_concurrent_record_complete() {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let writer =
            RunLogWriter::with_sink(PathBuf::from("run.jsonl"), SharedBuffer(Arc::clone(&bytes)));
        std::thread::scope(|scope| {
            for thread in 0..8 {
                let mut writer = writer.clone_for_shared_writes();
                scope.spawn(move || {
                    for record in 0..50 {
                        writer.log(
                            "test",
                            record,
                            Map::from_iter([
                                ("thread".to_owned(), Value::from(thread)),
                                ("record".to_owned(), Value::from(record)),
                            ]),
                        );
                    }
                });
            }
        });
        writer.finish().unwrap();
        let bytes = bytes.lock().unwrap();
        let rows = bytes
            .split(|byte| *byte == b'\n')
            .filter(|row| !row.is_empty());
        assert_eq!(rows.count(), 400);
        assert!(
            bytes
                .split(|byte| *byte == b'\n')
                .filter(|row| !row.is_empty())
                .all(|row| serde_json::from_slice::<Value>(row).is_ok())
        );
    }
}
