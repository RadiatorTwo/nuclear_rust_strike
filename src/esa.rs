//! The installer's archive on the CD, `setup.esa` ("ELECTRONIC_ARTS_ARCHIVE_FILE"): it holds
//! NUKEPC.EXE and the game's data folder (everything outside `worlds` and `video`, which the
//! game reads from the CD).
//!
//! Layout: the signature and a NUL, then entries up to the first file's data: name, group
//! (which install it belongs to: STRIKE_EXE_FILES, MIN_SAV_FILE, STRIKE_HUD...), both
//! NUL-terminated; flags u32, size u32, DOS time u32; method ("PKWA" or "NULL", NUL-terminated);
//! stored size u32, offset u32. "PKWA" is PKWARE's Data Compression Library ("implode"),
//! unpacked by `explode` (after Mark Adler's blast.c in zlib's contrib).

pub struct Entry {
    pub name: String,
    pub group: String,
    pub size: u32,
    pub packed: bool,
    pub stored: u32,
    pub offset: u32,
}

const SIGNATURE: &[u8] = b"ELECTRONIC_ARTS_ARCHIVE_FILE\0";

fn le32(d: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(d.get(o..o + 4)?.try_into().ok()?))
}

fn cstr(d: &[u8], p: &mut usize) -> Option<String> {
    let n = d.get(*p..)?.iter().position(|&b| b == 0)?;
    let s = d[*p..*p + n].iter().map(|&b| b as char).collect();
    *p += n + 1;
    Some(s)
}

/// The archive's entries; `head` is its start (the entries end before the first file's data).
pub fn entries(head: &[u8]) -> Option<Vec<Entry>> {
    if !head.starts_with(SIGNATURE) {
        return None;
    }
    let mut p = SIGNATURE.len();
    let mut out: Vec<Entry> = Vec::new();
    while out.first().is_none_or(|e| p < e.offset as usize) {
        let name = cstr(head, &mut p)?;
        let group = cstr(head, &mut p)?;
        let size = le32(head, p + 4)?;
        p += 12;
        let method = cstr(head, &mut p)?;
        let (stored, offset) = (le32(head, p)?, le32(head, p + 4)?);
        p += 8;
        out.push(Entry { name, group, size, packed: method == "PKWA", stored, offset });
    }
    Some(out)
}

impl Entry {
    /// The file's contents from its stored bytes.
    pub fn unpack(&self, stored: &[u8]) -> Result<Vec<u8>, String> {
        let d = if self.packed { explode(stored)? } else { stored.to_vec() };
        if d.len() != self.size as usize {
            return Err(format!("{}: {} bytes unpacked, {} expected", self.name, d.len(), self.size));
        }
        Ok(d)
    }
}

/// Huffman code: number of codes of each length (1..=13), symbols by code.
struct Huffman {
    count: [u16; 14],
    symbol: Vec<u16>,
}

impl Huffman {
    /// From the compact form: each byte is a code length (low 4 bits) and how many symbols in a
    /// row have it (high 4 bits, minus one).
    fn new(rep: &[u8]) -> Huffman {
        let lengths: Vec<u8> = rep.iter().flat_map(|&r| std::iter::repeat_n(r & 15, (r >> 4) as usize + 1)).collect();
        let mut count = [0u16; 14];
        for &l in &lengths {
            count[l as usize] += 1;
        }
        let mut offs = [0u16; 14];
        for l in 1..13 {
            offs[l + 1] = offs[l] + count[l];
        }
        let mut symbol = vec![0; lengths.len()];
        for (s, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbol[offs[l as usize] as usize] = s as u16;
                offs[l as usize] += 1;
            }
        }
        Huffman { count, symbol }
    }
}

struct Bits<'a> {
    d: &'a [u8],
    p: usize,
    buf: u32,
    cnt: u32,
}

impl Bits<'_> {
    fn byte(&mut self) -> Result<u32, String> {
        let b = *self.d.get(self.p).ok_or("compressed data ends early")?;
        self.p += 1;
        Ok(b as u32)
    }

    fn bits(&mut self, n: u32) -> Result<u32, String> {
        let mut v = self.buf;
        while self.cnt < n {
            v |= self.byte()? << self.cnt;
            self.cnt += 8;
        }
        self.buf = v >> n;
        self.cnt -= n;
        Ok(v & ((1 << n) - 1))
    }

    /// One symbol; the codes are stored with their bits inverted.
    fn decode(&mut self, h: &Huffman) -> Result<u16, String> {
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        let (mut buf, mut left) = (self.buf, self.cnt);
        let mut len = 1usize;
        loop {
            while left > 0 {
                left -= 1;
                code |= ((buf & 1) ^ 1) as i32;
                buf >>= 1;
                let c = h.count[len] as i32;
                if code < first + c {
                    self.buf = buf;
                    self.cnt = (self.cnt.wrapping_sub(len as u32)) & 7;
                    return Ok(h.symbol[(index + code - first) as usize]);
                }
                index += c;
                first = (first + c) << 1;
                code <<= 1;
                len += 1;
            }
            left = (14 - len as u32).min(8);
            if left == 0 {
                return Err("bad code in compressed data".into());
            }
            buf = self.byte()?;
        }
    }
}

const LITERALS: [u8; 98] = [
    11, 124, 8, 7, 28, 7, 188, 13, 76, 4, 10, 8, 12, 10, 12, 10, 8, 23, 8, 9, 7, 6, 7, 8, 7, 6, 55, 8, 23, 24, 12, 11, 7, 9,
    11, 12, 6, 7, 22, 5, 7, 24, 6, 11, 9, 6, 7, 22, 7, 11, 38, 7, 9, 8, 25, 11, 8, 11, 9, 12, 8, 12, 5, 38, 5, 38, 5, 11, 7,
    5, 6, 21, 6, 10, 53, 8, 7, 24, 10, 27, 44, 253, 253, 253, 252, 252, 252, 13, 12, 45, 12, 45, 12, 61, 12, 45, 44, 173,
];
const LENGTHS: [u8; 6] = [2, 35, 36, 53, 38, 23];
const DISTANCES: [u8; 7] = [2, 20, 53, 230, 247, 151, 248];
const BASE: [u16; 16] = [3, 2, 4, 5, 6, 7, 8, 9, 10, 12, 16, 24, 40, 72, 136, 264];
const EXTRA: [u8; 16] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8];

/// Unpack PKWARE DCL "imploded" data.
pub fn explode(d: &[u8]) -> Result<Vec<u8>, String> {
    use std::sync::OnceLock;
    static CODES: OnceLock<[Huffman; 3]> = OnceLock::new();
    let [lit, len, dist] = CODES.get_or_init(|| [Huffman::new(&LITERALS), Huffman::new(&LENGTHS), Huffman::new(&DISTANCES)]);
    let mut s = Bits { d, p: 0, buf: 0, cnt: 0 };
    let coded = s.bits(8)?;
    let dict = s.bits(8)?;
    if coded > 1 || !(4..=6).contains(&dict) {
        return Err("not PKWARE DCL data".into());
    }
    let mut out = Vec::with_capacity(d.len() * 3);
    loop {
        if s.bits(1)? != 0 {
            let sym = s.decode(len)? as usize;
            let n = BASE[sym] as usize + s.bits(EXTRA[sym] as u32)? as usize;
            if n == 519 {
                return Ok(out);
            }
            let low = if n == 2 { 2 } else { dict };
            let back = ((s.decode(dist)? as usize) << low) + s.bits(low)? as usize + 1;
            if back > out.len() {
                return Err("distance too far back in compressed data".into());
            }
            for _ in 0..n {
                out.push(out[out.len() - back]);
            }
        } else {
            out.push(if coded != 0 { s.decode(lit)? as u8 } else { s.bits(8)? as u8 });
        }
    }
}
