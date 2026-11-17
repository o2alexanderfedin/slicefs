//! On-disk formats of the arena spike (plan section 3): arena header page,
//! record v2, idx v2. Also the record v1 decoder of `chunk-store`, for crash
//! test 1.

use chunk_store::{ObjectId, ID_LEN};

/// Data starts after the 4 KiB header page.
pub const HEADER_PAGE: u64 = 4096;
pub const ARENA_MAGIC: &[u8; 4] = b"SFAR";
pub const IDX_MAGIC: &[u8; 4] = b"SFIX";
pub const FORMAT_VERSION: u16 = 2;

/// Record marker: "SFR" + 0xA5 (little endian u32 0xA5_52_46_53).
pub const MARKER: u32 = 0xA552_4653;
/// marker(4) + id(28) + flags(1) + raw(4) + stored(4)
pub const REC_HEADER: usize = 4 + ID_LEN + 9;
pub const REC_CRC: usize = 4;
/// A record payload larger than this is refused by the decoder (bounds
/// check before the CRC). Larger objects get an arena of their own, whose
/// decoder bound is the arena length.
pub const MAX_STORED: u32 = 64 * 1024;
pub const FLAG_ZSTD: u8 = 1;
/// id(28) + offset(8) + stored(4) + raw(4) + flags(1)
pub const IDX_ENTRY: usize = ID_LEN + 17;
/// magic(4) + version(2) + num(8) + state(1) + hwm(8) + count(4)
pub const IDX_HEAD: usize = 27;

/// Unpadded record length.
pub fn rec_len(stored: u32) -> usize {
    REC_HEADER + stored as usize + REC_CRC
}

/// Record length padded to `granule`.
pub fn padded_len(stored: u32, granule: u32) -> u64 {
    let n = rec_len(stored) as u64;
    let g = granule.max(1) as u64;
    n.div_ceil(g) * g
}

pub fn encode_record(id: &ObjectId, flags: u8, raw: u32, payload: &[u8], granule: u32) -> Vec<u8> {
    let plen = padded_len(payload.len() as u32, granule) as usize;
    let mut r = Vec::with_capacity(plen);
    r.extend_from_slice(&MARKER.to_le_bytes());
    r.extend_from_slice(&id.0);
    r.push(flags);
    r.extend_from_slice(&raw.to_le_bytes());
    r.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    r.extend_from_slice(payload);
    let crc = crc32c::crc32c(&r);
    r.extend_from_slice(&crc.to_le_bytes());
    r.resize(plen, 0);
    r
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecHead {
    pub id: ObjectId,
    pub flags: u8,
    pub raw: u32,
    pub stored: u32,
}

/// Decode the record at `buf[0]`: marker first, then bounds (`stored <=
/// max_stored` and the record inside `buf`), then CRC. `buf` must end at the
/// end of the arena (or of the read window; the caller re-reads on `Short`).
pub fn decode_record(buf: &[u8], max_stored: u32) -> Decoded {
    if buf.len() < 4 {
        return Decoded::Short;
    }
    if u32::from_le_bytes(buf[..4].try_into().unwrap()) != MARKER {
        return Decoded::Bad;
    }
    if buf.len() < REC_HEADER + REC_CRC {
        return Decoded::Short;
    }
    let id = ObjectId(buf[4..4 + ID_LEN].try_into().unwrap());
    let o = 4 + ID_LEN;
    let flags = buf[o];
    let raw = u32::from_le_bytes(buf[o + 1..o + 5].try_into().unwrap());
    let stored = u32::from_le_bytes(buf[o + 5..o + 9].try_into().unwrap());
    if stored > max_stored {
        return Decoded::Bad;
    }
    let total = rec_len(stored);
    if buf.len() < total {
        return Decoded::Short;
    }
    let body = total - REC_CRC;
    let crc = u32::from_le_bytes(buf[body..total].try_into().unwrap());
    if crc32c::crc32c(&buf[..body]) != crc {
        return Decoded::Bad;
    }
    Decoded::Ok(RecHead { id, flags, raw, stored })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decoded {
    Ok(RecHead),
    /// Not enough bytes in the buffer (end of window or of file).
    Short,
    /// Not a record here.
    Bad,
}

/// The payload slice of a decoded record buffer.
pub fn payload(buf: &[u8], stored: u32) -> &[u8] {
    &buf[REC_HEADER..REC_HEADER + stored as usize]
}

/// Record v1 of `chunk-store` (no marker): id, flags, raw, stored, payload,
/// crc. Only for crash test 1 (an all-zero tail must fail decode).
pub fn decode_record_v1(buf: &[u8]) -> Option<(ObjectId, u32)> {
    const H: usize = ID_LEN + 9;
    if buf.len() < H + 4 {
        return None;
    }
    let stored = u32::from_le_bytes(buf[ID_LEN + 5..ID_LEN + 9].try_into().unwrap());
    let total = H + stored as usize + 4;
    if buf.len() < total {
        return None;
    }
    let crc = u32::from_le_bytes(buf[total - 4..total].try_into().unwrap());
    (crc32c::crc32c(&buf[..total - 4]) == crc).then(|| (ObjectId(buf[..ID_LEN].try_into().unwrap()), stored))
}

// ── Arena header ──────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArenaHeader {
    pub num: u64,
    pub size: u64,
    pub granule: u32,
}

pub fn encode_header(h: &ArenaHeader) -> Vec<u8> {
    let mut b = Vec::with_capacity(HEADER_PAGE as usize);
    b.extend_from_slice(ARENA_MAGIC);
    b.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    b.extend_from_slice(&h.num.to_le_bytes());
    b.extend_from_slice(&h.size.to_le_bytes());
    b.extend_from_slice(&h.granule.to_le_bytes());
    let crc = crc32c::crc32c(&b);
    b.extend_from_slice(&crc.to_le_bytes());
    b.resize(HEADER_PAGE as usize, 0);
    b
}

pub fn decode_header(b: &[u8]) -> Option<ArenaHeader> {
    if b.len() < 30 || &b[..4] != ARENA_MAGIC {
        return None;
    }
    let crc = u32::from_le_bytes(b[26..30].try_into().unwrap());
    if crc32c::crc32c(&b[..26]) != crc || u16::from_le_bytes(b[4..6].try_into().unwrap()) != FORMAT_VERSION {
        return None;
    }
    Some(ArenaHeader {
        num: u64::from_le_bytes(b[6..14].try_into().unwrap()),
        size: u64::from_le_bytes(b[14..22].try_into().unwrap()),
        granule: u32::from_le_bytes(b[22..26].try_into().unwrap()),
    })
}

// ── idx v2 ────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdxState {
    /// Written at close: the arena is resumable from `hwm`.
    Open = 0,
    Sealed = 1,
    /// Reuse mode (spike C).
    Checkpoint = 2,
}

/// An index entry: what the idx stores for each record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IdxEntry {
    pub id: ObjectId,
    pub off: u64,
    pub stored: u32,
    pub raw: u32,
    pub flags: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Idx {
    pub num: u64,
    pub state: IdxState,
    pub hwm: u64,
    pub entries: Vec<IdxEntry>,
}

pub fn encode_idx(idx: &Idx) -> Vec<u8> {
    let mut sorted: Vec<&IdxEntry> = idx.entries.iter().collect();
    sorted.sort_by_key(|e| e.id);
    let mut b = Vec::with_capacity(IDX_HEAD + sorted.len() * IDX_ENTRY + 4);
    b.extend_from_slice(IDX_MAGIC);
    b.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    b.extend_from_slice(&idx.num.to_le_bytes());
    b.push(idx.state as u8);
    b.extend_from_slice(&idx.hwm.to_le_bytes());
    b.extend_from_slice(&(sorted.len() as u32).to_le_bytes());
    for e in sorted {
        b.extend_from_slice(&e.id.0);
        b.extend_from_slice(&e.off.to_le_bytes());
        b.extend_from_slice(&e.stored.to_le_bytes());
        b.extend_from_slice(&e.raw.to_le_bytes());
        b.push(e.flags);
    }
    let crc = crc32c::crc32c(&b);
    b.extend_from_slice(&crc.to_le_bytes());
    b
}

pub fn decode_idx(b: &[u8]) -> Option<Idx> {
    if b.len() < IDX_HEAD + 4 || &b[..4] != IDX_MAGIC {
        return None;
    }
    let (body, crc) = b.split_at(b.len() - 4);
    if crc32c::crc32c(body) != u32::from_le_bytes(crc.try_into().unwrap()) {
        return None;
    }
    if u16::from_le_bytes(body[4..6].try_into().unwrap()) != FORMAT_VERSION {
        return None;
    }
    let num = u64::from_le_bytes(body[6..14].try_into().unwrap());
    let state = match body[14] {
        0 => IdxState::Open,
        1 => IdxState::Sealed,
        2 => IdxState::Checkpoint,
        _ => return None,
    };
    let hwm = u64::from_le_bytes(body[15..23].try_into().unwrap());
    let n = u32::from_le_bytes(body[23..27].try_into().unwrap()) as usize;
    if body.len() != IDX_HEAD + n * IDX_ENTRY {
        return None;
    }
    let entries = body[IDX_HEAD..]
        .chunks_exact(IDX_ENTRY)
        .map(|e| {
            let o = ID_LEN;
            IdxEntry {
                id: ObjectId(e[..ID_LEN].try_into().unwrap()),
                off: u64::from_le_bytes(e[o..o + 8].try_into().unwrap()),
                stored: u32::from_le_bytes(e[o + 8..o + 12].try_into().unwrap()),
                raw: u32::from_le_bytes(e[o + 12..o + 16].try_into().unwrap()),
                flags: e[o + 16],
            }
        })
        .collect();
    Some(Idx { num, state, hwm, entries })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_round_trip_with_padding() {
        let id = ObjectId::of(b"x");
        for g in [1u32, 64, 512, 4096] {
            let r = encode_record(&id, 0, 3, b"abc", g);
            assert_eq!(r.len() as u64, padded_len(3, g));
            assert_eq!(decode_record(&r, MAX_STORED), Decoded::Ok(RecHead { id, flags: 0, raw: 3, stored: 3 }));
        }
    }

    #[test]
    fn idx_and_header_round_trip() {
        let h = ArenaHeader { num: 7, size: 64 << 20, granule: 1 };
        assert_eq!(decode_header(&encode_header(&h)), Some(h));
        let idx = Idx {
            num: 7,
            state: IdxState::Open,
            hwm: 12345,
            entries: vec![IdxEntry { id: ObjectId::of(b"a"), off: 4096, stored: 10, raw: 20, flags: 1 }],
        };
        assert_eq!(decode_idx(&encode_idx(&idx)), Some(idx));
    }
}
