//! Guesses the platform of an ISO from its first few sectors.

use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::Path,
};

use crate::jobs::Platform;

const SECTOR: usize = 2048;

/// Xbox (360) game partitions start with this volume descriptor at partition + 0x10000.
const XISO_MAGIC: &[u8; 20] = b"MICROSOFT*XBOX*MEDIA";
const XISO_HEADER: u64 = 0x10000;
/// Game partition offsets: rebuilt/trimmed, XGD2, XGD3, XGD1 (original Xbox).
const XGD_PARTITION_OFFSETS: [u64; 4] = [0, 0x0FD9_0000, 0x0208_0000, 0x1830_0000];

pub fn detect_path(path: &Path) -> io::Result<Option<Platform>> {
    detect(&mut File::open(path)?)
}

pub fn detect<R: Read + Seek>(r: &mut R) -> io::Result<Option<Platform>> {
    if is_xiso(r)? {
        return Ok(Some(Platform::X360));
    }
    if is_ps2(r)? {
        return Ok(Some(Platform::Ps2));
    }
    Ok(None)
}

/// Reads exactly `buf.len()` bytes at `offset`; `false` if the file is too short.
fn read_at<R: Read + Seek>(r: &mut R, offset: u64, buf: &mut [u8]) -> io::Result<bool> {
    r.seek(SeekFrom::Start(offset))?;
    match r.read_exact(buf) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(e),
    }
}

fn is_xiso<R: Read + Seek>(r: &mut R) -> io::Result<bool> {
    let mut buf = [0u8; XISO_MAGIC.len()];
    for offset in XGD_PARTITION_OFFSETS {
        if read_at(r, offset + XISO_HEADER, &mut buf)? && &buf == XISO_MAGIC {
            return Ok(true);
        }
    }
    Ok(false)
}

/// ISO9660 with a `SYSTEM.CNF` in the root that has a `BOOT2` line (PS1 discs use `BOOT`).
fn is_ps2<R: Read + Seek>(r: &mut R) -> io::Result<bool> {
    let mut pvd = [0u8; SECTOR];
    if !read_at(r, 16 * SECTOR as u64, &mut pvd)? || pvd[0] != 1 || &pvd[1..6] != b"CD001" {
        return Ok(false);
    }
    let root = &pvd[156..190];
    let root_lba = u32_le(&root[2..6]) as u64;
    let root_len = (u32_le(&root[10..14]) as usize).min(64 * 1024);

    let mut dir = vec![0u8; root_len];
    if !read_at(r, root_lba * SECTOR as u64, &mut dir)? {
        return Ok(false);
    }
    let Some((cnf_lba, cnf_len)) = find_entry(&dir, "SYSTEM.CNF") else {
        return Ok(false);
    };
    let mut cnf = vec![0u8; (cnf_len as usize).min(4096)];
    if !read_at(r, cnf_lba * SECTOR as u64, &mut cnf)? {
        return Ok(false);
    }
    Ok(String::from_utf8_lossy(&cnf).contains("BOOT2"))
}

/// Finds `name` in an ISO9660 directory extent, returning its (LBA, size).
fn find_entry(dir: &[u8], name: &str) -> Option<(u64, u32)> {
    let mut pos = 0;
    while pos < dir.len() {
        let rec_len = dir[pos] as usize;
        if rec_len == 0 {
            // Records never span sectors; the rest of this sector is padding.
            pos = (pos / SECTOR + 1) * SECTOR;
            continue;
        }
        let rec = dir.get(pos..pos + rec_len)?;
        if rec.len() > 33 {
            let name_len = rec[32] as usize;
            if let Some(raw) = rec.get(33..33 + name_len) {
                let entry = String::from_utf8_lossy(raw);
                let entry = entry.split(';').next().unwrap_or_default();
                if entry.eq_ignore_ascii_case(name) {
                    return Some((u32_le(&rec[2..6]) as u64, u32_le(&rec[10..14])));
                }
            }
        }
        pos += rec_len;
    }
    None
}

fn u32_le(b: &[u8]) -> u32 {
    u32::from_le_bytes(b[..4].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn dir_record(lba: u32, len: u32, name: &[u8]) -> Vec<u8> {
        let mut rec = vec![0u8; 33 + name.len()];
        if rec.len() % 2 == 1 {
            rec.push(0);
        }
        rec[0] = rec.len() as u8;
        rec[2..6].copy_from_slice(&lba.to_le_bytes());
        rec[10..14].copy_from_slice(&len.to_le_bytes());
        rec[32] = name.len() as u8;
        rec[33..33 + name.len()].copy_from_slice(name);
        rec
    }

    fn iso9660_with_system_cnf(cnf: &str) -> Vec<u8> {
        let mut img = vec![0u8; 24 * SECTOR];
        let pvd = 16 * SECTOR;
        img[pvd] = 1;
        img[pvd + 1..pvd + 6].copy_from_slice(b"CD001");
        let root = dir_record(20, SECTOR as u32, &[0]);
        img[pvd + 156..pvd + 156 + root.len()].copy_from_slice(&root);

        let mut dir = root.clone();
        dir.extend(dir_record(20, SECTOR as u32, &[1]));
        dir.extend(dir_record(21, cnf.len() as u32, b"SYSTEM.CNF;1"));
        img[20 * SECTOR..20 * SECTOR + dir.len()].copy_from_slice(&dir);
        img[21 * SECTOR..21 * SECTOR + cnf.len()].copy_from_slice(cnf.as_bytes());
        img
    }

    fn xiso_at(partition: u64) -> Vec<u8> {
        let header = (partition + XISO_HEADER) as usize;
        let mut img = vec![0u8; header + SECTOR];
        img[header..header + XISO_MAGIC.len()].copy_from_slice(XISO_MAGIC);
        img
    }

    fn detect_bytes(img: Vec<u8>) -> Option<Platform> {
        detect(&mut Cursor::new(img)).unwrap()
    }

    #[test]
    fn ps2() {
        let img = iso9660_with_system_cnf("BOOT2 = cdrom0:\\SLUS_203.12;1\nVER = 1.00\n");
        assert_eq!(detect_bytes(img), Some(Platform::Ps2));
    }

    #[test]
    fn ps1_is_not_ps2() {
        let img = iso9660_with_system_cnf("BOOT = cdrom:\\SCUS_944.55;1\nTCB = 4\n");
        assert_eq!(detect_bytes(img), None);
    }

    #[test]
    fn xiso_rebuilt() {
        assert_eq!(detect_bytes(xiso_at(0)), Some(Platform::X360));
    }

    #[test]
    fn xiso_xgd3() {
        assert_eq!(detect_bytes(xiso_at(0x0208_0000)), Some(Platform::X360));
    }

    #[test]
    fn garbage() {
        assert_eq!(detect_bytes(vec![0xAB; 64 * SECTOR]), None);
        assert_eq!(detect_bytes(vec![]), None);
    }
}
