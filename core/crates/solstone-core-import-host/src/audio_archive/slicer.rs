// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Bitstream-accurate Ogg Opus packet slicer and playable duration calculator.
//!
//! Provides zero-re-encoding slicing of Opus streams across 5-minute Chronicle tile boundaries.
//! Slices retain at least 80 ms of decoder history before the slice point, rewrite OpusHead
//! pre-skip to the exact playable start, clamp final granule to the exact playable end, and
//! recalculate valid Ogg CRCs. Complete short members are copied directly without re-paging.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::Path;

use sha2::{Digest, Sha256};
use solstone_core_import::audio_archive::{
    AudioArchiveError, AudioArchiveMaterializer, AudioArchiveRecordMember, AudioArchiveRecordPart,
    SourceRejection,
};

pub const SAMPLE_RATE_48K: i64 = 48_000;
pub const MIN_HISTORY_SAMPLES: i64 = 3_840; // 80 ms at 48 kHz

const fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut index = 0;
    while index < table.len() {
        let mut value = (index as u32) << 24;
        let mut bit = 0;
        while bit < 8 {
            value = (value << 1)
                ^ if value & 0x8000_0000 != 0 {
                    0x04c1_1db7
                } else {
                    0
                };
            bit += 1;
        }
        table[index] = value;
        index += 1;
    }
    table
}

const OGG_CRC_TABLE: [u32; 256] = crc_table();

fn unsupported() -> AudioArchiveError {
    AudioArchiveError::SourceRejected(SourceRejection::Unsupported)
}

pub fn ogg_crc(data: &[u8]) -> u32 {
    let mut crc = 0u32;
    for &byte in data {
        let index = ((crc >> 24) as u8 ^ byte) as usize;
        crc = (crc << 8) ^ OGG_CRC_TABLE[index];
    }
    crc
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct RawOggPage {
    pub header_type: u8,
    pub granule_pos: i64,
    pub serial: u32,
    pub page_seq: u32,
    pub segments: Vec<u8>,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct OpusPacket {
    pub payload: Vec<u8>,
    pub sample_count: i64,
    pub start_sample: i64,
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct ParsedOggOpus {
    pub pre_skip: u16,
    pub sample_rate: u32,
    pub channels: u8,
    pub opus_head: Vec<u8>,
    pub opus_tags: Vec<u8>,
    pub packets: Vec<OpusPacket>,
    pub final_granule: i64,
    pub playable_samples: i64,
    pub playable_duration_ms: i64,
}

pub fn read_raw_ogg_pages<R: Read>(mut reader: R) -> Result<Vec<RawOggPage>, AudioArchiveError> {
    let mut pages: Vec<RawOggPage> = Vec::new();
    loop {
        let mut header = [0u8; 27];
        let mut first = [0u8; 1];
        match reader.read(&mut first) {
            Ok(0) => break,
            Ok(_) => header[0] = first[0],
            Err(error) => return Err(AudioArchiveError::Io(error)),
        }
        reader
            .read_exact(&mut header[1..])
            .map_err(|_| unsupported())?;
        if &header[..5] != b"OggS\0" || header[5] & !7 != 0 {
            return Err(unsupported());
        }
        let header_type = header[5];
        let granule_pos = i64::from_le_bytes(header[6..14].try_into().unwrap());
        let serial = u32::from_le_bytes(header[14..18].try_into().unwrap());
        let page_seq = u32::from_le_bytes(header[18..22].try_into().unwrap());
        if let Some(previous) = pages.last() {
            if previous.header_type & 4 != 0
                || header_type & 2 != 0
                || serial != previous.serial
                || previous.page_seq.checked_add(1) != Some(page_seq)
            {
                return Err(unsupported());
            }
        } else if header_type != 2 || page_seq != 0 || granule_pos != 0 {
            return Err(unsupported());
        }
        let mut segments = vec![0u8; usize::from(header[26])];
        reader
            .read_exact(&mut segments)
            .map_err(|_| unsupported())?;
        let payload_len: usize = segments.iter().map(|&size| usize::from(size)).sum();
        let mut payload = vec![0u8; payload_len];
        reader.read_exact(&mut payload).map_err(|_| unsupported())?;
        let expected_crc = u32::from_le_bytes(header[22..26].try_into().unwrap());
        header[22..26].fill(0);
        let mut checked_page = Vec::with_capacity(27 + segments.len() + payload.len());
        checked_page.extend_from_slice(&header);
        checked_page.extend_from_slice(&segments);
        checked_page.extend_from_slice(&payload);
        if ogg_crc(&checked_page) != expected_crc {
            return Err(unsupported());
        }
        pages.push(RawOggPage {
            header_type,
            granule_pos,
            serial,
            page_seq,
            segments,
            payload,
        });
    }
    if pages.last().is_none_or(|page| page.header_type & 4 == 0) {
        return Err(unsupported());
    }
    Ok(pages)
}

pub fn opus_packet_samples(packet: &[u8]) -> Option<i64> {
    if packet.is_empty() {
        return None;
    }
    let toc = packet[0];
    let config = (toc >> 3) & 0x1f;
    let frame_count_code = toc & 0x03;

    let frame_samples = if config >= 16 {
        120 << (config & 3)
    } else if config >= 12 {
        480 << (config & 1)
    } else {
        [480, 960, 1920, 2880][usize::from(config & 3)]
    };

    let frame_count = match frame_count_code {
        0 => 1,
        1 | 2 => 2,
        3 => {
            if packet.len() < 2 {
                return None;
            }
            (packet[1] & 0x3f) as i64
        }
        _ => unreachable!(),
    };

    let samples = frame_samples * frame_count;
    (frame_count > 0 && samples <= 5760).then_some(samples)
}

pub fn parse_ogg_opus_bytes(bytes: &[u8]) -> Result<ParsedOggOpus, AudioArchiveError> {
    let pages = read_raw_ogg_pages(bytes)?;
    let mut pending = Vec::new();
    let mut headers: Vec<Vec<u8>> = Vec::new();
    let mut packets: Vec<OpusPacket> = Vec::new();
    let mut timeline_samples = 0i64;
    let mut needs_continuation = false;
    for (page_index, page) in pages.iter().enumerate() {
        if (page.header_type & 1 != 0) != needs_continuation || page.segments.is_empty() {
            return Err(unsupported());
        }
        let mut offset = 0;
        let mut completed_audio = false;
        for &size in &page.segments {
            let size = usize::from(size);
            pending.extend_from_slice(&page.payload[offset..offset + size]);
            offset += size;
            needs_continuation = size == 255;
            if needs_continuation {
                continue;
            }
            if headers.len() < 2 {
                if headers.is_empty() && (page_index != 0 || page.segments.len() != 1) {
                    return Err(unsupported());
                }
                headers.push(std::mem::take(&mut pending));
            } else {
                if pending.len() / 255 + 1 > 255 {
                    return Err(unsupported());
                }
                let sample_count = opus_packet_samples(&pending).ok_or_else(unsupported)?;
                let start_sample = timeline_samples;
                timeline_samples = timeline_samples
                    .checked_add(sample_count)
                    .ok_or_else(unsupported)?;
                packets.push(OpusPacket {
                    payload: std::mem::take(&mut pending),
                    sample_count,
                    start_sample,
                });
                completed_audio = true;
            }
        }
        if completed_audio {
            let last = packets.last().ok_or_else(unsupported)?;
            if page.header_type & 4 != 0 {
                if page.granule_pos <= last.start_sample || page.granule_pos > timeline_samples {
                    return Err(unsupported());
                }
            } else if page.granule_pos != timeline_samples {
                // Nonzero initial granule offsets are outside this adapter's supported subset.
                return Err(unsupported());
            }
        } else if page.header_type & 4 != 0
            || (!packets.is_empty() && page.granule_pos != -1)
            || (packets.is_empty() && page.granule_pos != 0 && page.granule_pos != -1)
        {
            return Err(unsupported());
        }
    }
    if needs_continuation || !pending.is_empty() || headers.len() != 2 || packets.is_empty() {
        return Err(unsupported());
    }
    let opus_head = headers.remove(0);
    let opus_tags = headers.remove(0);
    // Mapping family zero covers mono/stereo Opus. Other mappings are explicitly unsupported.
    if opus_head.len() != 19
        || &opus_head[..8] != b"OpusHead"
        || opus_head[8] == 0
        || opus_head[8] > 15
        || !(1..=2).contains(&opus_head[9])
        || opus_head[18] != 0
        || opus_tags.len() < 8
        || &opus_tags[..8] != b"OpusTags"
        || opus_tags.len() / 255 + 1 > 255
    {
        return Err(unsupported());
    }
    let channels = opus_head[9];
    let pre_skip = u16::from_le_bytes(opus_head[10..12].try_into().unwrap());
    let sample_rate = u32::from_le_bytes(opus_head[12..16].try_into().unwrap());
    let final_granule = pages.last().ok_or_else(unsupported)?.granule_pos;
    let playable_samples = final_granule
        .checked_sub(i64::from(pre_skip))
        .filter(|n| *n > 0)
        .ok_or_else(unsupported)?;
    let playable_duration_ms = playable_samples / (SAMPLE_RATE_48K / 1000);
    Ok(ParsedOggOpus {
        pre_skip,
        sample_rate,
        channels,
        opus_head,
        opus_tags,
        packets,
        final_granule,
        playable_samples,
        playable_duration_ms,
    })
}

pub fn write_ogg_page<W: Write>(
    writer: &mut W,
    header_type: u8,
    granule_pos: i64,
    serial: u32,
    page_seq: u32,
    packet_payloads: &[&[u8]],
) -> io::Result<()> {
    let mut segment_table = Vec::new();
    let mut page_payload = Vec::new();

    for &packet in packet_payloads {
        let mut remaining = packet.len();
        let mut offset = 0;
        while remaining >= 255 {
            segment_table.push(255);
            page_payload.extend_from_slice(&packet[offset..offset + 255]);
            offset += 255;
            remaining -= 255;
        }
        segment_table.push(remaining as u8);
        page_payload.extend_from_slice(&packet[offset..]);
    }

    if segment_table.len() > 255 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "packet exceeds one Ogg page",
        ));
    }
    let mut header = vec![0u8; 27 + segment_table.len()];
    header[0..4].copy_from_slice(b"OggS");
    header[4] = 0; // version
    header[5] = header_type;
    header[6..14].copy_from_slice(&granule_pos.to_le_bytes());
    header[14..18].copy_from_slice(&serial.to_le_bytes());
    header[18..22].copy_from_slice(&page_seq.to_le_bytes());
    header[22..26].copy_from_slice(&0u32.to_le_bytes()); // zero crc
    header[26] = segment_table.len() as u8;
    header[27..27 + segment_table.len()].copy_from_slice(&segment_table);

    let mut checksum_data = Vec::with_capacity(header.len() + page_payload.len());
    checksum_data.extend_from_slice(&header);
    checksum_data.extend_from_slice(&page_payload);
    let crc = ogg_crc(&checksum_data);
    header[22..26].copy_from_slice(&crc.to_le_bytes());

    writer.write_all(&header)?;
    writer.write_all(&page_payload)?;
    Ok(())
}

pub fn slice_parsed_ogg_opus(
    parsed: &ParsedOggOpus,
    target_path: &Path,
    origin_ms: i64,
    length_ms: i64,
) -> Result<(), AudioArchiveError> {
    let target_start_sample = origin_ms
        .checked_mul(SAMPLE_RATE_48K / 1000)
        .filter(|v| *v >= 0)
        .ok_or_else(unsupported)?;
    let target_end_sample = origin_ms
        .checked_add(length_ms)
        .and_then(|v| v.checked_mul(SAMPLE_RATE_48K / 1000))
        .filter(|v| length_ms > 0 && *v <= parsed.playable_samples)
        .ok_or_else(unsupported)?;
    let target_start_in_stream = (parsed.pre_skip as i64) + target_start_sample;
    let target_end_in_stream = (parsed.pre_skip as i64) + target_end_sample;

    let history_threshold = target_start_in_stream.saturating_sub(MIN_HISTORY_SAMPLES);

    // Find the first packet starting at or before history_threshold
    let first_idx = parsed
        .packets
        .iter()
        .rposition(|p| p.start_sample <= history_threshold)
        .unwrap_or(0);

    let history_start_sample = parsed.packets[first_idx].start_sample;
    let new_pre_skip =
        u16::try_from(target_start_in_stream - history_start_sample).map_err(|_| unsupported())?;

    // Build rewritten OpusHead with updated pre_skip
    let mut new_opus_head = parsed.opus_head.clone();
    if new_opus_head.len() >= 12 {
        new_opus_head[10..12].copy_from_slice(&new_pre_skip.to_le_bytes());
    }

    // Select packets up to target_end_in_stream
    let mut selected_packets = Vec::new();
    for packet in &parsed.packets[first_idx..] {
        selected_packets.push(packet);
        if packet.start_sample + packet.sample_count >= target_end_in_stream {
            break;
        }
    }

    if selected_packets.is_empty() {
        return Err(AudioArchiveError::SourceRejected(
            SourceRejection::Unreadable,
        ));
    }

    if let Some(parent) = target_path.parent() {
        fs::create_dir_all(parent).map_err(AudioArchiveError::Io)?;
    }
    let mut output_file = File::create(target_path).map_err(AudioArchiveError::Io)?;
    let serial = 0x504c5553; // 'PLUS'
    let mut page_seq = 0u32;

    // Page 0: OpusHead (BOS)
    write_ogg_page(
        &mut output_file,
        0x02, // BOS
        0,
        serial,
        page_seq,
        &[&new_opus_head],
    )
    .map_err(AudioArchiveError::Io)?;
    page_seq += 1;

    // Page 1: OpusTags
    write_ogg_page(
        &mut output_file,
        0x00,
        0,
        serial,
        page_seq,
        &[&parsed.opus_tags],
    )
    .map_err(AudioArchiveError::Io)?;
    page_seq += 1;

    // Audio pages: group packets (approx 10-20 packets per page or ~200-400ms)
    let total_selected = selected_packets.len();
    let mut idx = 0;
    while idx < total_selected {
        let is_last = idx + 1 == total_selected;
        let packet = selected_packets[idx];
        let header_type = if is_last { 0x04 } else { 0x00 }; // EOS on last
        let granule_pos = if is_last {
            (new_pre_skip as i64) + (target_end_sample - target_start_sample)
        } else {
            (packet.start_sample + packet.sample_count) - history_start_sample
        };

        write_ogg_page(
            &mut output_file,
            header_type,
            granule_pos,
            serial,
            page_seq,
            &[&packet.payload],
        )
        .map_err(AudioArchiveError::Io)?;
        page_seq += 1;
        idx += 1;
    }

    output_file.flush().map_err(AudioArchiveError::Io)?;
    Ok(())
}

pub struct HostAudioArchiveMaterializer {
    pub source_root: Option<std::path::PathBuf>,
}

impl HostAudioArchiveMaterializer {
    pub fn new(source_root: Option<std::path::PathBuf>) -> Self {
        Self { source_root }
    }

    pub fn resolve_source_path(&self, member: &AudioArchiveRecordMember) -> std::path::PathBuf {
        if let Some(root) = &self.source_root {
            root.join(&member.original_relative_name)
        } else {
            std::path::PathBuf::from(&member.original_relative_name)
        }
    }
}

impl AudioArchiveMaterializer for HostAudioArchiveMaterializer {
    fn materialize(
        &self,
        member: &AudioArchiveRecordMember,
        part: &AudioArchiveRecordPart,
        target: &Path,
    ) -> Result<(), AudioArchiveError> {
        let source_path = self.resolve_source_path(member);
        if !source_path.is_file() {
            return Err(AudioArchiveError::SourceRejected(
                SourceRejection::Unreadable,
            ));
        }

        // Use one verified snapshot for the digest, parsing and staged payload.
        let bytes = fs::read(&source_path)
            .map_err(|_| AudioArchiveError::SourceRejected(SourceRejection::Unreadable))?;
        if bytes.len() as u64 != member.byte_len
            || format!("{:x}", Sha256::digest(&bytes)) != member.sha256
        {
            return Err(AudioArchiveError::SourceRejected(SourceRejection::Changed));
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(AudioArchiveError::Io)?;
        }

        // If part covers the entire member without slicing: byte copy
        let is_complete_member =
            part.origin_ms == 0 && member.duration_ms > 0 && part.length_ms >= member.duration_ms;

        if is_complete_member {
            fs::write(target, &bytes).map_err(AudioArchiveError::Io)?;
            return Ok(());
        }

        // Sliced part: packet copy
        let parsed = parse_ogg_opus_bytes(&bytes)?;
        slice_parsed_ogg_opus(&parsed, target, part.origin_ms, part.length_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn opus_toc_uses_rfc_frame_sizes_and_rejects_invalid_duration() {
        // Independent RFC 6716 section 3.1 expectations, in samples at 48 kHz.
        let expected = [
            480, 960, 1920, 2880, 480, 960, 1920, 2880, 480, 960, 1920, 2880, 480, 960, 480, 960,
            120, 240, 480, 960, 120, 240, 480, 960, 120, 240, 480, 960, 120, 240, 480, 960,
        ];
        for (config, samples) in expected.into_iter().enumerate() {
            assert_eq!(opus_packet_samples(&[(config as u8) << 3]), Some(samples));
        }
        assert_eq!(opus_packet_samples(&[]), None);
        assert_eq!(opus_packet_samples(&[3, 0]), None);
        assert_eq!(opus_packet_samples(&[3, 13]), None);
        assert_eq!(opus_packet_samples(&[3, 12]), Some(5760));
        assert_eq!(opus_packet_samples(&[3]), None);
    }
    #[test]
    fn ogg_crc_polynomial_includes_the_previously_corrupt_entries() {
        assert_eq!(OGG_CRC_TABLE[222], 0x2d15_ebe3);
        assert_eq!(OGG_CRC_TABLE[223], 0x29d4_f654);
        assert_eq!(ogg_crc(b"123456789"), 0x89a1_897f);
    }
}
