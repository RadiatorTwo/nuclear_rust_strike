//! The debug information Watcom's linker left in NUKEPC.EXE: CodeView 5 ("NB11") at the end
//! of the file. Its global publics (S_PUB32) name 2164 places in the code and ~940 in the
//! data, which gives every function's start and name without a disassembler's guesses.
//!
//! Layout: the file ends with "NB11" and the distance back to the start of the debug data,
//! which begins with "NB11" and the offset of its directory. Directory entries are
//! (subsection kind u16, module u16, offset u32, size u32); sstGlobalPub (0x12a) is a hash
//! header (16 bytes) followed by the symbol records: length u16, kind u16, then for
//! S_PUB32 (0x1009) type u32, offset u32, segment u16 and a length-prefixed name. Segment 1
//! is BEGTEXT, segment 2 DGROUP (`SEGMENTS`).

/// The start of each segment a public refers to (1 = BEGTEXT, 2 = DGROUP: one data group
/// for DGROUP and .bss).
pub const SEGMENTS: [u32; 2] = [0x410000, 0x4d0000];

/// One public: its address and name ("ObjMgr_AddObjToLists_": Watcom's register calling
/// convention adds the underscore behind, cdecl/stdcall names have it in front).
#[derive(Clone, Debug)]
pub struct Public {
    pub addr: u32,
    pub name: String,
    pub code: bool,
}

fn le16(d: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(d.get(o..o + 2)?.try_into().ok()?))
}
fn le32(d: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(d.get(o..o + 4)?.try_into().ok()?))
}

/// The publics of the EXE's CodeView data, sorted by address; empty without one.
pub fn publics(exe: &[u8]) -> Vec<Public> {
    read(exe).unwrap_or_default()
}

fn read(d: &[u8]) -> Option<Vec<Public>> {
    let n = d.len();
    if d.get(n - 8..n - 4)? != b"NB11" {
        return None;
    }
    let base = n.checked_sub(le32(d, n - 4)? as usize)?;
    if d.get(base..base + 4)? != b"NB11" {
        return None;
    }
    let dir = base + le32(d, base + 4)? as usize;
    let (hdr, ent, count) = (le16(d, dir)? as usize, le16(d, dir + 2)? as usize, le32(d, dir + 4)? as usize);
    let mut out = Vec::new();
    for k in 0..count {
        let e = dir + hdr + k * ent;
        if le16(d, e)? != 0x12a {
            continue;
        }
        let s = base + le32(d, e + 4)? as usize;
        let end = s + 16 + le32(d, s + 4)? as usize;
        let mut q = s + 16;
        while q + 4 <= end {
            let (len, kind) = (le16(d, q)? as usize, le16(d, q + 2)?);
            if kind == 0x1009 {
                let (off, seg) = (le32(d, q + 8)?, le16(d, q + 12)? as usize);
                let nl = *d.get(q + 14)? as usize;
                let name: String = d.get(q + 15..q + 15 + nl)?.iter().map(|&b| b as char).collect();
                if let Some(&start) = SEGMENTS.get(seg.wrapping_sub(1)) {
                    out.push(Public { addr: start + off, name, code: seg == 1 });
                }
            }
            q += len + 2;
        }
    }
    out.sort_by_key(|p| p.addr);
    Some(out)
}

/// Function ranges for discovery: (start, end, name), sorted. A function ends where the next
/// public in the code starts; several names at one address are joined by '/'. DirectInput's
/// data formats (`_c_dfDI...`) at the end of the code segment are data.
pub fn functions(publics: &[Public], text_end: u32) -> Vec<(u32, u32, String)> {
    let mut starts: Vec<(u32, String)> = Vec::new();
    for p in publics.iter().filter(|p| p.code && p.addr < text_end) {
        match starts.last_mut() {
            Some(last) if last.0 == p.addr => {
                last.1.push('/');
                last.1 += &p.name;
            }
            _ => starts.push((p.addr, p.name.clone())),
        }
    }
    let data_from = starts.iter().find(|s| s.1.starts_with("_c_dfDI")).map_or(text_end, |s| s.0);
    starts.retain(|s| s.0 < data_from);
    (0..starts.len())
        .map(|k| {
            let end = starts.get(k + 1).map_or(data_from, |s| s.0);
            (starts[k].0, end, starts[k].1.clone())
        })
        .collect()
}
