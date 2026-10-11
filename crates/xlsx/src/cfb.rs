//! The Compound File Binary (CFB/OLE2) container, implemented from Microsoft's public [MS-CFB]
//! specification: enough to tell what a non-zip spreadsheet file is (Excel wraps
//! password-protected workbooks in a CFB holding an `EncryptedPackage` stream, while Excel
//! 97-2003 `.xls` files are CFB files with a `Workbook` stream) and to read a root-level stream.
//! Every read is bounds-checked and every loop bounded.

/// The CFB signature at the start of the file.
pub(crate) const SIGNATURE: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];

const END_OF_CHAIN: u32 = 0xFFFF_FFFE;
/// Sector ids at or above this are markers (free, end of chain, FAT/DIFAT sector), not sectors.
const MAX_REG_SECT: u32 = 0xFFFF_FFFA;
/// Directory sectors followed at most (4096 entries with 512-byte sectors).
const MAX_DIR_SECTORS: usize = 1024;
/// DIFAT sectors followed at most.
const MAX_DIFAT_SECTORS: usize = 1024;

/// Stream names that mark an encrypted (password-protected) OOXML package.
const ENCRYPTED_STREAMS: [&str; 2] = ["EncryptedPackage", "EncryptionInfo"];

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    let s = b.get(at..at.checked_add(2)?)?;
    Some(u16::from_le_bytes([*s.first()?, *s.get(1)?]))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([*s.first()?, *s.get(1)?, *s.get(2)?, *s.get(3)?]))
}

/// Whether the compound file holds an encryption stream. Walks the directory through the FAT;
/// when the structure can't be followed (truncated or malformed), falls back to scanning the
/// bytes for the UTF-16LE stream names.
pub(crate) fn is_encrypted(bytes: &[u8]) -> bool {
    match directory_names(bytes) {
        Some(names) => names.iter().any(|n| ENCRYPTED_STREAMS.iter().any(|e| n.eq_ignore_ascii_case(e))),
        None => ENCRYPTED_STREAMS.iter().any(|e| contains_utf16(bytes, e)),
    }
}

fn contains_utf16(hay: &[u8], needle: &str) -> bool {
    let n: Vec<u8> = needle.encode_utf16().flat_map(u16::to_le_bytes).collect();
    !n.is_empty() && hay.windows(n.len()).any(|w| w == n.as_slice())
}

/// Names of every directory entry, or `None` when the directory can't be read.
fn directory_names(b: &[u8]) -> Option<Vec<String>> {
    if !b.starts_with(&SIGNATURE) {
        return None;
    }
    let shift = u16_at(b, 30)?;
    if shift != 9 && shift != 12 {
        return None;
    }
    let size = 1usize << shift;
    let per = size / 4;
    let sector = |id: u32| -> Option<&[u8]> {
        let start = (id as usize).checked_add(1)?.checked_mul(size)?;
        b.get(start..start.checked_add(size)?)
    };
    // The DIFAT: the FAT's sector ids, 109 in the header then chained DIFAT sectors.
    let mut difat: Vec<u32> = (0..109).filter_map(|i| u32_at(b, 76 + i * 4)).filter(|&s| s < MAX_REG_SECT).collect();
    let mut next = u32_at(b, 68)?;
    for _ in 0..MAX_DIFAT_SECTORS {
        if next >= MAX_REG_SECT {
            break;
        }
        let s = sector(next)?;
        difat.extend((0..per - 1).filter_map(|i| u32_at(s, i * 4)).filter(|&x| x < MAX_REG_SECT));
        next = u32_at(s, (per - 1) * 4)?;
    }
    let fat_next = |id: u32| -> Option<u32> {
        let idx = id as usize;
        let fat_sector = *difat.get(idx / per)?;
        u32_at(sector(fat_sector)?, (idx % per) * 4)
    };
    let mut names = Vec::new();
    let mut id = u32_at(b, 48)?;
    for _ in 0..MAX_DIR_SECTORS {
        if id == END_OF_CHAIN {
            return Some(names);
        }
        if id >= MAX_REG_SECT {
            return None;
        }
        let s = sector(id)?;
        for e in s.as_chunks::<128>().0 {
            let len = usize::from(u16_at(e, 64)?);
            if !(2..=64).contains(&len) {
                continue;
            }
            let units: Vec<u16> = (0..(len - 2) / 2).filter_map(|i| u16_at(e, i * 2)).collect();
            names.push(String::from_utf16_lossy(&units));
        }
        id = fat_next(id)?;
    }
    // A directory longer than the bound (or a cyclic chain): report what was read.
    Some(names)
}

/// Free-sector marker in the FAT.
const FREE_SECT: u32 = 0xFFFF_FFFF;
/// "No entry" in the directory tree.
const NO_STREAM: u32 = 0xFFFF_FFFF;
/// Streams larger than this are refused (no spreadsheet stream is this big).
const MAX_STREAM: u64 = 512 * 1024 * 1024;
/// Directory entries read at most.
const MAX_ENTRIES: usize = 65_536;

/// A parsed compound file: the FAT, the mini FAT and the directory.
pub(crate) struct Compound<'a> {
    bytes: &'a [u8],
    sector_size: usize,
    cutoff: u64,
    fat: Vec<u32>,
    mini_fat: Vec<u32>,
    entries: Vec<DirEntry>,
}

struct DirEntry {
    name: String,
    /// 1 storage, 2 stream, 5 root.
    kind: u8,
    left: u32,
    right: u32,
    child: u32,
    start: u32,
    size: u64,
}

/// Size of a mini-stream sector.
const MINI_SECTOR: usize = 64;

impl<'a> Compound<'a> {
    /// Parses the header, FAT, mini FAT and directory. Errors are plain messages.
    pub(crate) fn open(b: &'a [u8]) -> Result<Compound<'a>, String> {
        if !b.starts_with(&SIGNATURE) {
            return Err("not a compound file".into());
        }
        let sector_size = match u16_at(b, 30).ok_or("truncated header")? {
            9 => 512,
            12 => 4096,
            _ => return Err("unsupported sector size".into()),
        };
        if u16_at(b, 32).ok_or("truncated header")? != 6 {
            return Err("unsupported mini sector size".into());
        }
        let cutoff = u64::from(u32_at(b, 56).ok_or("truncated header")?);
        let sectors = (b.len() / sector_size).saturating_sub(1);
        let per = sector_size / 4;
        let mut c = Compound { bytes: b, sector_size, cutoff, fat: vec![], mini_fat: vec![], entries: vec![] };

        // The DIFAT: the FAT's sector ids, 109 in the header then chained DIFAT sectors.
        let mut fat_sectors: Vec<u32> = (0..109).filter_map(|i| u32_at(b, 76 + i * 4)).filter(|&s| s < MAX_REG_SECT).collect();
        let mut next = u32_at(b, 68).ok_or("truncated header")?;
        let mut hops = 0usize;
        while next < MAX_REG_SECT {
            hops += 1;
            if hops > MAX_DIFAT_SECTORS.min(sectors.saturating_add(1)) {
                return Err("the DIFAT chain loops".into());
            }
            let s = c.sector(next).ok_or("a DIFAT sector is outside the file")?;
            fat_sectors.extend((0..per - 1).filter_map(|i| u32_at(s, i * 4)).filter(|&x| x < MAX_REG_SECT));
            next = u32_at(s, (per - 1) * 4).ok_or("truncated DIFAT sector")?;
        }
        if fat_sectors.len() > sectors.saturating_add(1) {
            return Err("more FAT sectors than the file holds".into());
        }
        let mut fat = Vec::new();
        for id in fat_sectors {
            let s = c.sector(id).ok_or("a FAT sector is outside the file")?;
            fat.extend((0..per).filter_map(|i| u32_at(s, i * 4)));
        }
        c.fat = fat;

        // The mini FAT is an ordinary chain of sectors.
        let mini_start = u32_at(b, 60).ok_or("truncated header")?;
        if mini_start < MAX_REG_SECT {
            let raw = c.chain(mini_start, None)?;
            c.mini_fat = raw.as_chunks::<4>().0.iter().map(|q| u32::from_le_bytes(*q)).collect();
        }

        let raw = c.chain(u32_at(b, 48).ok_or("truncated header")?, None)?;
        for e in raw.as_chunks::<128>().0.iter().take(MAX_ENTRIES) {
            let len = usize::from(u16_at(e, 64).unwrap_or(0));
            let name = if (2..=64).contains(&len) {
                let units: Vec<u16> = (0..(len - 2) / 2).filter_map(|i| u16_at(e, i * 2)).collect();
                String::from_utf16_lossy(&units)
            } else {
                String::new()
            };
            let low = u64::from(u32_at(e, 120).unwrap_or(0));
            let high = u64::from(u32_at(e, 124).unwrap_or(0));
            // Version 3 writers may leave garbage in the high half of the size.
            let size = if sector_size == 512 { low } else { (high << 32) | low };
            c.entries.push(DirEntry {
                name,
                kind: e.get(66).copied().unwrap_or(0),
                left: u32_at(e, 68).unwrap_or(NO_STREAM),
                right: u32_at(e, 72).unwrap_or(NO_STREAM),
                child: u32_at(e, 76).unwrap_or(NO_STREAM),
                start: u32_at(e, 116).unwrap_or(END_OF_CHAIN),
                size,
            });
        }
        if c.entries.first().is_none_or(|r| r.kind != 5) {
            return Err("the compound file has no root entry".into());
        }
        Ok(c)
    }

    fn sector(&self, id: u32) -> Option<&'a [u8]> {
        let start = (id as usize).checked_add(1)?.checked_mul(self.sector_size)?;
        self.bytes.get(start..start.checked_add(self.sector_size)?)
    }

    /// Concatenates a FAT chain; with `size`, exactly that many bytes. Loops, dangling links and
    /// short chains are errors.
    fn chain(&self, start: u32, size: Option<u64>) -> Result<Vec<u8>, String> {
        let cap = size.unwrap_or(MAX_STREAM).min(MAX_STREAM);
        let max_hops = self.fat.len().min(self.bytes.len() / self.sector_size);
        let mut out = Vec::new();
        let mut id = start;
        let mut hops = 0usize;
        while id != END_OF_CHAIN && (out.len() as u64) < cap {
            hops += 1;
            if id >= MAX_REG_SECT || hops > max_hops.saturating_add(1) {
                return Err("a sector chain is broken or loops".into());
            }
            out.extend_from_slice(self.sector(id).ok_or("a sector chain points outside the file")?);
            id = *self.fat.get(id as usize).ok_or("a sector chain points outside the FAT")?;
            if id == FREE_SECT {
                return Err("a sector chain runs into a free sector".into());
            }
        }
        if let Some(size) = size {
            if (out.len() as u64) < size {
                return Err("a stream is shorter than its directory entry says".into());
            }
            out.truncate(usize::try_from(size).map_err(|_| "a stream is too large")?);
        }
        Ok(out)
    }

    /// The root storage's direct children (the directory is a tree of siblings under `child`).
    fn root_children(&self) -> impl Iterator<Item = &DirEntry> {
        let mut found = vec![];
        let mut seen = vec![false; self.entries.len()];
        let mut stack = vec![self.entries.first().map_or(NO_STREAM, |r| r.child)];
        while let Some(id) = stack.pop() {
            let i = id as usize;
            let Some(e) = self.entries.get(i) else { continue };
            match seen.get_mut(i) {
                Some(s) if !*s => *s = true,
                _ => continue,
            }
            found.push(e);
            stack.push(e.left);
            stack.push(e.right);
        }
        found.into_iter()
    }

    fn find(&self, name: &str) -> Option<&DirEntry> {
        self.root_children().find(|e| e.kind == 2 && e.name.eq_ignore_ascii_case(name))
    }

    /// Whether the root storage holds a stream called `name` (case-insensitive).
    pub(crate) fn has_stream(&self, name: &str) -> bool {
        self.find(name).is_some()
    }

    /// The bytes of the root-level stream called `name` (case-insensitive).
    pub(crate) fn stream(&self, name: &str) -> Result<Option<Vec<u8>>, String> {
        let Some(e) = self.find(name) else { return Ok(None) };
        if e.size > MAX_STREAM {
            return Err(format!("the {name} stream is too large"));
        }
        if e.size == 0 {
            return Ok(Some(vec![]));
        }
        if e.size >= self.cutoff {
            return self.chain(e.start, Some(e.size)).map(Some);
        }
        // A small stream lives in the mini stream (the root entry's data), in 64-byte sectors.
        let root = self.entries.first().ok_or("no root entry")?;
        let mini = self.chain(root.start, Some(root.size))?;
        let mut out = Vec::new();
        let mut id = e.start;
        let mut hops = 0usize;
        while (out.len() as u64) < e.size {
            hops += 1;
            if id >= MAX_REG_SECT || hops > self.mini_fat.len().saturating_add(1) {
                return Err("a mini sector chain is broken or loops".into());
            }
            let start = (id as usize).checked_mul(MINI_SECTOR).ok_or("bad mini sector")?;
            out.extend_from_slice(mini.get(start..start.saturating_add(MINI_SECTOR)).ok_or("a mini sector is outside the mini stream")?);
            id = *self.mini_fat.get(id as usize).ok_or("a mini sector chain points outside the mini FAT")?;
        }
        out.truncate(usize::try_from(e.size).map_err(|_| "a stream is too large")?);
        Ok(Some(out))
    }
}

/// Whether the compound file holds an Excel 97-2003 workbook stream (`Workbook`, or `Book` for
/// Excel 5/95).
pub(crate) fn has_workbook_stream(bytes: &[u8]) -> bool {
    Compound::open(bytes).is_ok_and(|c| c.has_stream("Workbook") || c.has_stream("Book"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A minimal valid compound file (512-byte sectors: FAT at sector 0, directory at sector 1)
    /// whose directory holds a root entry plus the given stream names.
    pub(crate) fn build(streams: &[&str]) -> Vec<u8> {
        let mut b = vec![0u8; 512 * 3];
        b[..8].copy_from_slice(&SIGNATURE);
        b[24..26].copy_from_slice(&0x3Eu16.to_le_bytes()); // minor version
        b[26..28].copy_from_slice(&3u16.to_le_bytes()); // major version
        b[28..30].copy_from_slice(&0xFFFEu16.to_le_bytes()); // byte order
        b[30..32].copy_from_slice(&9u16.to_le_bytes()); // sector shift
        b[32..34].copy_from_slice(&6u16.to_le_bytes()); // mini sector shift
        b[44..48].copy_from_slice(&1u32.to_le_bytes()); // FAT sectors
        b[48..52].copy_from_slice(&1u32.to_le_bytes()); // first directory sector
        b[60..64].copy_from_slice(&END_OF_CHAIN.to_le_bytes()); // no mini FAT
        b[68..72].copy_from_slice(&END_OF_CHAIN.to_le_bytes()); // no DIFAT sectors
        for i in 0..109 {
            b[76 + i * 4..80 + i * 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        }
        b[76..80].copy_from_slice(&0u32.to_le_bytes()); // FAT lives in sector 0
        // FAT (sector 0, at byte 512): sector 0 is a FAT sector, sector 1 ends its chain.
        for i in 0..128 {
            b[512 + i * 4..516 + i * 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        }
        b[512..516].copy_from_slice(&0xFFFF_FFFDu32.to_le_bytes());
        b[516..520].copy_from_slice(&END_OF_CHAIN.to_le_bytes());
        // Directory (sector 1, at byte 1024).
        for (k, name) in std::iter::once("Root Entry").chain(streams.iter().copied()).take(4).enumerate() {
            let e = 1024 + k * 128;
            let units: Vec<u16> = name.encode_utf16().take(31).collect();
            for (i, u) in units.iter().enumerate() {
                b[e + i * 2..e + i * 2 + 2].copy_from_slice(&u.to_le_bytes());
            }
            b[e + 64..e + 66].copy_from_slice(&((units.len() as u16 + 1) * 2).to_le_bytes());
            b[e + 66] = if k == 0 { 5 } else { 2 };
        }
        b
    }

    #[test]
    fn reads_directory_names() {
        let b = build(&["EncryptionInfo", "EncryptedPackage"]);
        assert_eq!(directory_names(&b), Some(vec!["Root Entry".into(), "EncryptionInfo".into(), "EncryptedPackage".into()]));
        assert!(is_encrypted(&b));
        assert!(!is_encrypted(&build(&["Workbook", "\u{5}SummaryInformation"])));
    }

    #[test]
    fn hostile_headers_never_panic() {
        let mut b = build(&["Workbook"]);
        for at in [30usize, 48, 68, 76, 512, 516, 1024 + 64] {
            for v in [0u32, 1, 7, 0xFFFF_FFFA, 0xFFFF_FFFF, 0x7FFF_FFFF] {
                let mut c = b.clone();
                c[at..at + 4].copy_from_slice(&v.to_le_bytes());
                let _ = is_encrypted(&c);
                for cut in [0usize, 8, 100, 600, 1100] {
                    let _ = is_encrypted(&c[..cut.min(c.len())]);
                }
            }
        }
        // A directory chain that loops back on itself stops at the bound.
        b[516..520].copy_from_slice(&1u32.to_le_bytes());
        assert!(!is_encrypted(&b));
    }
}
