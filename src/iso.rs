//! Reading files from a CD image: ISO 9660 with Joliet names when there are, or UDF (images
//! made without ISO 9660, as one of the German CD), in an `.iso` (2048 bytes a sector) or a raw
//! `.bin` (2352 bytes a sector, mode 1 or mode 2 form 1).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

/// A file or folder on the image.
#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub dir: bool,
    /// ISO 9660: the first sector of its data; UDF: the sector of its file entry
    sector: u32,
    pub size: u32,
}

pub struct Iso {
    /// bytes copied so far, and who hears of them after each file
    pub copied: u64,
    pub progress: Option<Box<dyn FnMut(u64)>>,
    file: File,
    /// bytes a sector in the file, and where its 2048 data bytes start
    sector_size: u64,
    data_at: u64,
    joliet: bool,
    /// UDF: the sector its partition starts at
    udf: Option<u32>,
    root: Entry,
}

/// The layouts an image can have: (sector size, data offset).
const LAYOUTS: [(u64, u64); 3] = [(2048, 0), (2352, 16), (2352, 24)];

impl Iso {
    /// Open a CD image; None when the file is none.
    pub fn open(path: &Path) -> Option<Iso> {
        let file = File::open(path).ok()?;
        for (sector_size, data_at) in LAYOUTS {
            let mut iso = Iso {
                copied: 0,
                progress: None,
                file: file.try_clone().ok()?,
                sector_size,
                data_at,
                joliet: false,
                udf: None,
                root: Entry::default(),
            };
            if iso.descriptors().is_some() || iso.udf_descriptors().is_some() {
                return Some(iso);
            }
        }
        None
    }

    /// The volume descriptors from sector 16: the root folder of the primary one, or of the
    /// Joliet one (UCS-2 names) when there is one.
    fn descriptors(&mut self) -> Option<()> {
        let mut found = false;
        for s in 16..64 {
            let d = self.sector(s).ok()?;
            if &d[1..6] != b"CD001" {
                return None;
            }
            match d[0] {
                1 if !found => {
                    self.root = record(&d[156..], false)?;
                    found = true;
                }
                // Joliet: a supplementary descriptor with the escape sequence %/@, %/C or %/E
                2 if d[88] == b'%' && d[89] == b'/' && b"@CE".contains(&d[90]) => {
                    self.root = record(&d[156..], true)?;
                    self.joliet = true;
                }
                0xff => break,
                _ => {}
            }
        }
        found.then_some(())
    }

    /// UDF (ECMA-167): the anchor at sector 256, the volume descriptors it points to (the
    /// partition's start, the file set's place), the file set's root folder.
    fn udf_descriptors(&mut self) -> Option<()> {
        let anchor = self.sector(256).ok()?;
        if tag(&anchor) != 2 {
            return None;
        }
        let (len, at) = (le32(&anchor, 16), le32(&anchor, 20));
        let (mut start, mut fileset) = (None, None);
        for s in at..at + len.div_ceil(2048) {
            let d = self.sector(s).ok()?;
            match tag(&d) {
                5 => start = Some(le32(&d, 188)),
                6 if le32(&d, 212) == 2048 => fileset = Some(le32(&d, 252)),
                8 => break,
                _ => {}
            }
        }
        let start = start?;
        let fsd = self.sector(start + fileset?).ok()?;
        if tag(&fsd) != 256 {
            return None;
        }
        self.udf = Some(start);
        let root = start + le32(&fsd, 404);
        let size = self.udf_entry(root).ok()?.0;
        self.root = Entry { name: String::new(), dir: true, sector: root, size };
        Some(())
    }

    /// A UDF file entry: its length and where its data is: (sector, bytes) extents, or the
    /// data itself when it is in the entry.
    fn udf_entry(&mut self, n: u32) -> std::io::Result<(u32, Result<Vec<(u32, u32)>, Vec<u8>>)> {
        let d = self.sector(n)?;
        let bad = || std::io::Error::new(std::io::ErrorKind::InvalidData, "bad UDF file entry");
        // (extended) file entry: lengths of the extended attributes and allocation descriptors
        let (ea, ad) = match tag(&d) {
            261 => (le32(&d, 168), 176),
            266 => (le32(&d, 208), 216),
            _ => return Err(bad()),
        };
        let lad = le32(&d, ad - 4) as usize;
        let size = le32(&d, 56);
        let from = ad + ea as usize;
        let ads = d.get(from..from + lad).ok_or_else(bad)?;
        let start = self.udf.unwrap_or(0);
        // short or long allocation descriptors (length, block in the partition), or the data
        let extents = match le16(&d, 34) & 7 {
            0 => ads.chunks_exact(8).map(|a| (start + le32(a, 4), le32(a, 0) & 0x3fff_ffff)).collect(),
            1 => ads.chunks_exact(16).map(|a| (start + le32(a, 4), le32(a, 0) & 0x3fff_ffff)).collect(),
            3 => return Ok((size, Err(ads.to_vec()))),
            _ => return Err(bad()),
        };
        Ok((size, Ok(extents)))
    }

    /// The entries of a UDF folder: its file identifier descriptors.
    fn udf_list(&mut self, dir: &Entry) -> std::io::Result<Vec<Entry>> {
        let mut data = Vec::new();
        self.copy(dir, &mut data)?;
        let mut out = Vec::new();
        let mut at = 0;
        while at + 38 <= data.len() && tag(&data[at..]) == 257 {
            let fid = &data[at..];
            let (kind, lfi, liu) = (fid[18], fid[19] as usize, le16(fid, 36) as usize);
            let name = fid.get(38 + liu..38 + liu + lfi).map(dstring).unwrap_or_default();
            at += (38 + liu + lfi + 3) & !3;
            // deleted, or the parent folder
            if kind & (4 | 8) != 0 || name.is_empty() {
                continue;
            }
            let sector = self.udf.unwrap_or(0) + le32(fid, 24);
            let size = self.udf_entry(sector)?.0;
            out.push(Entry { name, dir: kind & 2 != 0, sector, size });
        }
        Ok(out)
    }

    fn sector(&mut self, n: u32) -> std::io::Result<[u8; 2048]> {
        let mut b = [0u8; 2048];
        self.file.seek(SeekFrom::Start(n as u64 * self.sector_size + self.data_at))?;
        self.file.read_exact(&mut b)?;
        Ok(b)
    }

    pub fn root(&self) -> Entry {
        self.root.clone()
    }

    /// The entries of a folder.
    pub fn list(&mut self, dir: &Entry) -> std::io::Result<Vec<Entry>> {
        if self.udf.is_some() {
            return self.udf_list(dir);
        }
        let mut out = Vec::new();
        for k in 0..dir.size.div_ceil(2048) {
            let d = self.sector(dir.sector + k)?;
            let mut at = 0;
            // records do not cross sectors; a length of 0 ends the sector's
            while at < 2048 && d[at] != 0 {
                let len = d[at] as usize;
                if let Some(e) = record(&d[at..(at + len).min(2048)], self.joliet).filter(|e| !e.name.is_empty()) {
                    out.push(e);
                }
                at += len;
            }
        }
        Ok(out)
    }

    /// The entry called `name` (any case) in `dir`.
    pub fn get(&mut self, dir: &Entry, name: &str) -> Option<Entry> {
        self.list(dir).ok()?.into_iter().find(|e| e.name.eq_ignore_ascii_case(name))
    }

    /// The first file called `name` in `dir` or below, with its folder.
    pub fn find(&mut self, dir: &Entry, name: &str, depth: u32) -> Option<(Entry, Entry)> {
        let list = self.list(dir).ok()?;
        if let Some(e) = list.iter().find(|e| !e.dir && e.name.eq_ignore_ascii_case(name)) {
            return Some((dir.clone(), e.clone()));
        }
        if depth == 0 {
            return None;
        }
        list.iter().filter(|e| e.dir).find_map(|d| self.find(d, name, depth - 1))
    }

    /// Copy a file's contents to `out`.
    pub fn read(&mut self, f: &Entry, out: &mut impl Write) -> std::io::Result<()> {
        self.copy(f, out)?;
        self.copied += f.size as u64;
        let copied = self.copied;
        if let Some(p) = &mut self.progress {
            p(copied);
        }
        Ok(())
    }

    /// The bytes of the files in a folder and below.
    pub fn size(&mut self, dir: &Entry) -> std::io::Result<u64> {
        let mut n = 0;
        for e in self.list(dir)? {
            n += if e.dir { self.size(&e)? } else { e.size as u64 };
        }
        Ok(n)
    }

    fn copy(&mut self, f: &Entry, out: &mut impl Write) -> std::io::Result<()> {
        if self.udf.is_none() {
            return self.copy_run(f.sector, f.size, out);
        }
        let extents = match self.udf_entry(f.sector)?.1 {
            Ok(e) => e,
            Err(data) => return out.write_all(&data[..data.len().min(f.size as usize)]),
        };
        let mut left = f.size;
        for (sector, len) in extents {
            let n = len.min(left);
            self.copy_run(sector, n, out)?;
            left -= n;
        }
        Ok(())
    }

    /// `size` bytes from sector `first` on.
    fn copy_run(&mut self, first: u32, size: u32, out: &mut impl Write) -> std::io::Result<()> {
        if self.sector_size == 2048 {
            self.file.seek(SeekFrom::Start(first as u64 * 2048))?;
            std::io::copy(&mut (&mut self.file).take(size as u64), out)?;
            return Ok(());
        }
        let mut left = size as usize;
        for k in 0.. {
            if left == 0 {
                break;
            }
            let d = self.sector(first + k)?;
            let n = left.min(2048);
            out.write_all(&d[..n])?;
            left -= n;
        }
        Ok(())
    }

    /// Copy a folder with everything in it to `to`.
    pub fn extract(&mut self, dir: &Entry, to: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(to)?;
        for e in self.list(dir)? {
            let p = to.join(&e.name);
            if e.dir {
                self.extract(&e, &p)?;
            } else {
                self.read(&e, &mut std::io::BufWriter::new(File::create(&p)?))?;
            }
        }
        Ok(())
    }
}

impl Default for Entry {
    fn default() -> Self {
        Entry { name: String::new(), dir: true, sector: 0, size: 0 }
    }
}

fn le16(d: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([d[o], d[o + 1]])
}

fn le32(d: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(d[o..o + 4].try_into().unwrap())
}

/// A UDF descriptor's tag identifier.
fn tag(d: &[u8]) -> u16 {
    if d.len() < 16 { 0 } else { le16(d, 0) }
}

/// A UDF name: 8 bits a character or UTF-16 (big-endian), after a byte that says which.
fn dstring(d: &[u8]) -> String {
    match d.split_first() {
        Some((8, rest)) => rest.iter().map(|&b| b as char).collect(),
        Some((16, rest)) => String::from_utf16_lossy(&rest.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect::<Vec<_>>()),
        _ => String::new(),
    }
}

/// A directory record (no name: a folder's own and its parent's).
fn record(r: &[u8], joliet: bool) -> Option<Entry> {
    if r.len() < 34 {
        return None;
    }
    let n = r[32] as usize;
    let raw = r.get(33..33 + n)?;
    let dir = r[25] & 2 != 0;
    let entry = |name| Entry {
        name,
        dir,
        sector: u32::from_le_bytes(r[2..6].try_into().unwrap()),
        size: u32::from_le_bytes(r[10..14].try_into().unwrap()),
    };
    if n == 1 && raw[0] <= 1 {
        // a folder's records for itself and its parent (and the root's in a volume descriptor)
        return Some(entry(String::new()));
    }
    let mut name = if joliet {
        let units: Vec<u16> = raw.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
        String::from_utf16_lossy(&units)
    } else {
        raw.iter().map(|&b| b as char).collect()
    };
    // version numbers (";1") and the dot of names without an extension
    if let Some(k) = name.find(';') {
        name.truncate(k);
    }
    if !dir && name.ends_with('.') {
        name.pop();
    }
    Some(entry(name))
}
