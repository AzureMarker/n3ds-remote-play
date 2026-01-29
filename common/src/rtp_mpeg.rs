use rtp_types::RtpPacket;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};
use tokio_util::bytes::{Buf, BytesMut};
use tokio_util::codec::Decoder;

/// RTP payload type for our MPEG-1 video stream (dynamic range; value is arbitrary but must match client).
pub const RTP_MPEG_PAYLOAD_TYPE: u8 = 96;

/// How many in-flight MPEG packets we keep for out-of-order delivery across packet boundaries.
///
/// This is a safety valve against reordering between consecutive MPEG packets.
const IN_FLIGHT_WINDOW: usize = 2;

struct RtpMpegPacketState {
    total_len: usize,
    /// Number of fragments expected for the current MPEG packet.
    frag_count: u16,
    /// The raw MPEG payload data, assembled from fragments.
    buf: BytesMut,
    /// Tracks which fragment indices have already been applied (duplicate suppression).
    frag_received: Vec<bool>,
    /// Total bytes written into `buf` for the current MPEG packet.
    received_bytes: usize,
    /// Byte ranges written so far, indexed by `frag_index`.
    ///
    /// This provides a much cheaper overlap check vs tracking every byte.
    fragment_ranges: Vec<Option<(usize, usize)>>,
    start_time: Instant,
}

impl RtpMpegPacketState {
    fn new_for_packet(total_len: usize, frag_count: u16, start_time: Instant) -> Self {
        Self {
            total_len,
            frag_count,
            buf: BytesMut::zeroed(total_len),
            frag_received: vec![false; frag_count as usize],
            received_bytes: 0,
            fragment_ranges: vec![None; frag_count as usize],
            start_time,
        }
    }
}

/// Codec used with FramedRead to turn RTP datagrams into complete MPEG packets.
///
/// Input: one "frame" == one UDP datagram containing exactly one RTP packet.
/// Output: one "frame" == one complete MPEG packet payload (reassembled from fragments).
///
/// Fragment header (in RTP payload):
/// - mpeg_len (u32 BE)
/// - frag_index (u16 BE)
/// - frag_count (u16 BE)
/// - frag_offset (u32 BE)  byte offset into the MPEG packet
///
/// `frag_offset` makes out-of-order reassembly unambiguous.
#[derive(Default)]
pub struct RtpMpegPacketCodec {
    in_flight: BTreeMap<u16, RtpMpegPacketState>,
    latest_seq: u16,
}

impl RtpMpegPacketCodec {
    pub fn new() -> Self {
        Self::default()
    }

    fn evict_old_packets(&mut self, seq: u16) {
        let in_flight_keys: Vec<u16> = self.in_flight.keys().copied().collect();
        for in_flight_seq in in_flight_keys {
            if in_flight_seq + IN_FLIGHT_WINDOW as u16 <= seq {
                log::warn!(
                    "Evicting in-flight seq {} as it's too old compared to {}",
                    in_flight_seq,
                    seq
                );
                self.in_flight.remove(&in_flight_seq);
            }
        }

        self.latest_seq = std::cmp::max(self.latest_seq, seq);
    }
}

impl Decoder for RtpMpegPacketCodec {
    type Item = (BytesMut, Duration);
    type Error = anyhow::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        // We expect `src` to contain exactly one UDP datagram.
        if src.is_empty() {
            return Ok(None);
        }

        let start_time = Instant::now();
        let datagram = src.split().freeze();
        let packet = RtpPacket::parse(&datagram)?;

        if packet.payload_type() != RTP_MPEG_PAYLOAD_TYPE {
            log::warn!(
                "Ignoring RTP packet with unexpected payload type: {}",
                packet.payload_type()
            );
            return Ok(None);
        }

        // IMPORTANT: RTP sequence_number identifies the *MPEG packet*.
        // All fragments of a given MPEG packet use the same sequence number.
        let seq = packet.sequence_number();

        if seq + IN_FLIGHT_WINDOW as u16 <= self.latest_seq {
            log::warn!(
                "Ignoring RTP packet with old sequence number: {}, latest is {}",
                seq,
                self.latest_seq
            );
            return Ok(None);
        }

        let mut payload = packet.payload();
        if payload.len() < 12 {
            // Not enough for fragment header; drop.
            log::warn!("RTP packet payload too short for MPEG fragment header; dropping");
            return Ok(None);
        }

        // Fragment header: u32 mpeg_len BE, u16 frag_index BE, u16 frag_count BE, u32 frag_offset BE
        let total_len = payload.get_u32() as usize;
        let frag_index = payload.get_u16();
        let frag_count = payload.get_u16();
        let frag_offset = payload.get_u32() as usize;
        let frag_bytes = payload;

        if frag_count == 0 {
            log::warn!("RTP packet has frag_count == 0; dropping");
            return Ok(None);
        }
        if frag_index >= frag_count {
            log::warn!(
                "RTP packet has invalid frag_index {} >= frag_count {}; dropping",
                frag_index,
                frag_count
            );
            return Ok(None);
        }

        // Get or create in-flight state for this MPEG packet id.
        self.in_flight.entry(seq).or_insert_with(|| {
            RtpMpegPacketState::new_for_packet(total_len, frag_count, start_time)
        });
        self.evict_old_packets(seq);

        let state = self.in_flight.get_mut(&seq).expect("state must exist");

        // If parameters change for an existing seq, drop that packet.
        if state.frag_count != frag_count || state.total_len != total_len {
            log::warn!(
                "Dropping MPEG packet seq {} due to header mismatch: existing total_len {} frag_count {}, got total_len {} frag_count {}",
                seq,
                state.total_len,
                state.frag_count,
                total_len,
                frag_count
            );
            self.in_flight.remove(&seq);
            return Ok(None);
        }

        // Bounds check for declared offset.
        if frag_offset >= state.total_len {
            log::warn!(
                "RTP fragment offset {} is out of bounds (total_len {}); dropping current MPEG packet",
                frag_offset,
                state.total_len
            );
            self.in_flight.remove(&seq);
            return Ok(None);
        }
        if frag_offset.saturating_add(frag_bytes.len()) > state.total_len {
            log::warn!(
                "RTP fragment (offset {}, len {}) exceeds total_len {}; dropping current MPEG packet",
                frag_offset,
                frag_bytes.len(),
                state.total_len
            );
            self.in_flight.remove(&seq);
            return Ok(None);
        }

        // Ignore duplicates but keep the current packet state.
        if state.frag_received[frag_index as usize] {
            log::warn!(
                "Duplicate fragment ignored for MPEG packet seq {} (frag_index {})",
                seq,
                frag_index
            );
            return Ok(None);
        }

        // Compute range for this fragment.
        let start = frag_offset;
        let end = frag_offset + frag_bytes.len();

        // Detect overlap/double-write by comparing this fragment's byte range with
        // all previously-seen fragment ranges.
        //
        // This protects against corrupted headers / sender bugs.
        for range in &state.fragment_ranges {
            let Some((r_start, r_end)) = range else {
                continue;
            };

            // Half-open interval overlap check: [start, end) overlaps [r_start, r_end)
            if start < *r_end && *r_start < end {
                log::warn!(
                    "Dropping MPEG packet seq {} due to overlapping fragment (frag_index {} offset {} len {})",
                    seq,
                    frag_index,
                    frag_offset,
                    frag_bytes.len(),
                );
                self.in_flight.remove(&seq);
                return Ok(None);
            }
        }

        // Copy bytes into their final position.
        state.buf[start..end].copy_from_slice(frag_bytes);
        state.frag_received[frag_index as usize] = true;
        state.fragment_ranges[frag_index as usize] = Some((start, end));
        state.received_bytes = state.received_bytes.saturating_add(frag_bytes.len());

        // Finished accepting this fragment
        log::trace!(
            "Rx #{} {}/{} (l={}) {:?}",
            seq,
            frag_index + 1,
            frag_count,
            frag_bytes.len(),
            start_time.elapsed()
        );

        // Check if the packet is complete
        if !state.frag_received.iter().all(|v| *v) {
            // Waiting for more fragments
            return Ok(None);
        }

        // Double check that we received all bytes
        if state.received_bytes != state.total_len {
            log::warn!(
                "Dropping MPEG packet seq {}: completed fragment set but received_bytes {} != total_len {}",
                seq,
                state.received_bytes,
                state.total_len
            );
            self.in_flight.remove(&seq);
            return Ok(None);
        }

        // Packet is complete. Remove from in-flight list and return the assembled buffer.
        let mut completed = self.in_flight.remove(&seq).expect("exists");
        let out = completed.buf.split_to(completed.total_len);
        let duration = completed.start_time.elapsed();

        log::debug!("Rx #{} COMPLETE in {:?}", seq, duration);

        Ok(Some((out, duration)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_log::test;
    use tokio_util::bytes::{BufMut, BytesMut};

    fn build_rtp_datagram(
        payload_type: u8,
        seq: u16,
        mpeg_len: u32,
        frag_index: u16,
        frag_count: u16,
        frag_offset: u32,
        frag_bytes: &[u8],
    ) -> BytesMut {
        // Minimal RTP header (12 bytes): V=2, P=0, X=0, CC=0
        // Marker=0, PT=payload_type
        // Seq, Timestamp=0, SSRC=0
        let mut buf = BytesMut::with_capacity(12 + 12 + frag_bytes.len());
        buf.put_u8(0x80);
        buf.put_u8(payload_type & 0x7F);
        buf.put_u16(seq);
        buf.put_u32(0);
        buf.put_u32(0);

        // Our MPEG fragment header: total_len, frag_index, frag_count, frag_offset (all BE)
        buf.put_u32(mpeg_len);
        buf.put_u16(frag_index);
        buf.put_u16(frag_count);
        buf.put_u32(frag_offset);
        buf.extend_from_slice(frag_bytes);
        buf
    }

    #[test]
    fn rtp_mpeg_codec_single_fragment_packet() {
        let mut codec = RtpMpegPacketCodec::new();

        let payload = b"hello-mpeg";
        let mut datagram = build_rtp_datagram(
            RTP_MPEG_PAYLOAD_TYPE,
            10,
            payload.len() as u32,
            0,
            1,
            0,
            payload,
        );

        let (out, _) = codec
            .decode(&mut datagram)
            .unwrap()
            .expect("expected output");
        assert_eq!(out.as_ref(), payload);
        assert!(datagram.is_empty(), "datagram should be consumed");
    }

    #[test]
    fn rtp_mpeg_codec_multi_fragment_reassembles_in_order() {
        let mut codec = RtpMpegPacketCodec::new();

        let full = b"abcdefghijklmnopqrstuvwxyz";
        let f0 = &full[..10];
        let f1 = &full[10..20];
        let f2 = &full[20..];

        let mut d0 = build_rtp_datagram(RTP_MPEG_PAYLOAD_TYPE, 100, full.len() as u32, 0, 3, 0, f0);
        assert!(codec.decode(&mut d0).unwrap().is_none());

        let mut d1 =
            build_rtp_datagram(RTP_MPEG_PAYLOAD_TYPE, 100, full.len() as u32, 1, 3, 10, f1);
        assert!(codec.decode(&mut d1).unwrap().is_none());

        let mut d2 =
            build_rtp_datagram(RTP_MPEG_PAYLOAD_TYPE, 100, full.len() as u32, 2, 3, 20, f2);
        let (out, _) = codec.decode(&mut d2).unwrap().expect("expected output");
        assert_eq!(out.as_ref(), full);
    }

    #[test]
    fn rtp_mpeg_codec_out_of_order_fragments_reassemble() {
        let mut codec = RtpMpegPacketCodec::new();

        let full = b"abcdefghijklmnopqrstuvwxyz";
        let f0 = &full[..10];
        let f1 = &full[10..20];
        let f2 = &full[20..];

        let mut d0 = build_rtp_datagram(RTP_MPEG_PAYLOAD_TYPE, 10, full.len() as u32, 0, 3, 0, f0);
        let mut d1 = build_rtp_datagram(RTP_MPEG_PAYLOAD_TYPE, 10, full.len() as u32, 1, 3, 10, f1);
        let mut d2 = build_rtp_datagram(RTP_MPEG_PAYLOAD_TYPE, 10, full.len() as u32, 2, 3, 20, f2);

        assert!(codec.decode(&mut d0).unwrap().is_none());
        assert!(codec.decode(&mut d2).unwrap().is_none());
        let (out, _) = codec.decode(&mut d1).unwrap().expect("expected output");
        assert_eq!(out.as_ref(), full);
    }

    #[test]
    fn rtp_mpeg_codec_out_of_order_fragments_reverse_order_reassemble() {
        let mut codec = RtpMpegPacketCodec::new();

        let full = b"abcdefghijklmnopqrstuvwxyz";
        let f0 = &full[..10];
        let f1 = &full[10..20];
        let f2 = &full[20..];

        let mut d0 = build_rtp_datagram(RTP_MPEG_PAYLOAD_TYPE, 10, full.len() as u32, 0, 3, 0, f0);
        let mut d1 = build_rtp_datagram(RTP_MPEG_PAYLOAD_TYPE, 10, full.len() as u32, 1, 3, 10, f1);
        let mut d2 = build_rtp_datagram(RTP_MPEG_PAYLOAD_TYPE, 10, full.len() as u32, 2, 3, 20, f2);

        assert!(codec.decode(&mut d2).unwrap().is_none());
        assert!(codec.decode(&mut d1).unwrap().is_none());
        let (out, _) = codec.decode(&mut d0).unwrap().expect("expected output");
        assert_eq!(out.as_ref(), full);
    }

    #[test]
    fn rtp_mpeg_codec_evicts_old_packets() {
        let mut codec = RtpMpegPacketCodec::new();

        let full = b"0123456789abcdef";
        let f0 = &full[..8];
        let f1 = &full[8..];

        // Send first fragment of seq 1
        let mut d0 = build_rtp_datagram(RTP_MPEG_PAYLOAD_TYPE, 200, full.len() as u32, 0, 2, 0, f0);
        assert!(codec.decode(&mut d0).unwrap().is_none());

        // Simulate loss of seq 1 by sending seq 2 packet
        let mut d2 = build_rtp_datagram(RTP_MPEG_PAYLOAD_TYPE, 201, full.len() as u32, 0, 2, 0, f0);
        assert!(codec.decode(&mut d2).unwrap().is_none());

        // Simulate loss of seq 2 by sending seq 3 packet
        let mut d4 = build_rtp_datagram(RTP_MPEG_PAYLOAD_TYPE, 202, full.len() as u32, 0, 2, 0, f0);
        assert!(codec.decode(&mut d4).unwrap().is_none());

        // Now send the second fragment of seq 1, which should be discarded
        let mut d1 = build_rtp_datagram(RTP_MPEG_PAYLOAD_TYPE, 200, full.len() as u32, 1, 2, 8, f1);
        assert!(
            codec.decode(&mut d1).unwrap().is_none(),
            "expected no output due to eviction"
        );

        // Complete seq 2 and 3 packets
        let mut d3 = build_rtp_datagram(RTP_MPEG_PAYLOAD_TYPE, 201, full.len() as u32, 1, 2, 8, f1);
        let (out2, _) = codec.decode(&mut d3).unwrap().expect("expected output");
        assert_eq!(out2.as_ref(), full);

        let mut d5 = build_rtp_datagram(RTP_MPEG_PAYLOAD_TYPE, 202, full.len() as u32, 1, 2, 8, f1);
        let (out3, _) = codec.decode(&mut d5).unwrap().expect("expected output");
        assert_eq!(out3.as_ref(), full);
    }

    #[test]
    fn rtp_mpeg_codec_ignores_unexpected_payload_type() {
        let mut codec = RtpMpegPacketCodec::new();

        let payload = b"nope";
        let mut datagram = build_rtp_datagram(97, 1, payload.len() as u32, 0, 1, 0, payload);
        assert!(codec.decode(&mut datagram).unwrap().is_none());
    }

    #[test]
    fn rtp_mpeg_codec_duplicate_fragment_is_ignored() {
        let mut codec = RtpMpegPacketCodec::new();

        let full = b"abcdefghijklmnopqrstuvwxyz";
        let f0 = &full[..10];
        let f1 = &full[10..];

        let mut d0 = build_rtp_datagram(RTP_MPEG_PAYLOAD_TYPE, 1, full.len() as u32, 0, 2, 0, f0);
        let mut d0_dup = d0.clone();
        let mut d1 = build_rtp_datagram(RTP_MPEG_PAYLOAD_TYPE, 1, full.len() as u32, 1, 2, 10, f1);

        // First fragment will not result in output
        assert_eq!(codec.decode(&mut d0).unwrap(), None);

        // Duplicate fragment should be ignored
        assert_eq!(codec.decode(&mut d0_dup).unwrap(), None);

        // Second fragment should complete the packet
        let (out, _) = codec.decode(&mut d1).unwrap().expect("expected output");
        assert_eq!(out.as_ref(), full);
    }

    #[test]
    fn rtp_mpeg_codec_overlap_drops_packet() {
        let mut codec = RtpMpegPacketCodec::new();

        let full = b"abcdefghijklmnopqrstuvwxyz";
        let f0 = &full[..10];
        let f1 = &full[10..];

        let mut d0 = build_rtp_datagram(RTP_MPEG_PAYLOAD_TYPE, 10, full.len() as u32, 0, 2, 0, f0);
        assert!(codec.decode(&mut d0).unwrap().is_none());

        // Overlap: frag_index is 1 but the offset overlaps the first fragment
        let mut overlap =
            build_rtp_datagram(RTP_MPEG_PAYLOAD_TYPE, 10, full.len() as u32, 1, 2, 5, f1);
        assert!(codec.decode(&mut overlap).unwrap().is_none());

        // Sending the last fragment should not produce output since the packet was dropped
        let mut d1 = build_rtp_datagram(RTP_MPEG_PAYLOAD_TYPE, 10, full.len() as u32, 1, 2, 10, f1);
        assert!(codec.decode(&mut d1).unwrap().is_none());
    }

    #[test]
    fn rtp_mpeg_codec_missing_fragment_never_completes() {
        let mut codec = RtpMpegPacketCodec::new();

        let full = b"abcdefghijklmnopqrstuvwxyz";
        let f0 = &full[..10];
        let f2 = &full[20..];

        let mut d0 = build_rtp_datagram(RTP_MPEG_PAYLOAD_TYPE, 10, full.len() as u32, 0, 3, 0, f0);
        assert!(codec.decode(&mut d0).unwrap().is_none());

        // Skip frag 1 entirely.
        let mut d2 = build_rtp_datagram(RTP_MPEG_PAYLOAD_TYPE, 11, full.len() as u32, 2, 3, 20, f2);
        assert!(codec.decode(&mut d2).unwrap().is_none());
    }

    #[test]
    fn rtp_mpeg_codec_out_of_order_across_packet_boundary_is_ok() {
        let mut codec = RtpMpegPacketCodec::new();

        // Packet A (seq=10): 2 fragments
        let a = b"abcdefghijklmnop";
        let a0 = &a[..8];
        let a1 = &a[8..];

        // Packet B (seq=11): single fragment
        let b = b"uvwxyz";

        // Receive first fragment of A
        let mut a_frag0 =
            build_rtp_datagram(RTP_MPEG_PAYLOAD_TYPE, 10, a.len() as u32, 0, 2, 0, a0);
        assert!(codec.decode(&mut a_frag0).unwrap().is_none());

        // Then receive full packet B
        // FIXME: This should not return the packet until we've either completed or given up on A.
        let mut b0 = build_rtp_datagram(RTP_MPEG_PAYLOAD_TYPE, 11, b.len() as u32, 0, 1, 0, b);
        let (out_b, _) = codec.decode(&mut b0).unwrap().expect("expected packet B");
        assert_eq!(&out_b[..], b);

        // Then receive the last fragment of A
        let mut a_frag1 =
            build_rtp_datagram(RTP_MPEG_PAYLOAD_TYPE, 10, a.len() as u32, 1, 2, 8, a1);
        let (out_a, _) = codec
            .decode(&mut a_frag1)
            .unwrap()
            .expect("expected packet A");
        assert_eq!(&out_a[..], a);
    }
}
