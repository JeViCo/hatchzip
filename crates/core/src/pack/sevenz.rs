//! The 7z half of packing: one LZMA2 block per file, and the header that
//! indexes them, written from the pack's own records.
//!
//! sevenz-rust2's writer cannot append to an archive it did not start in
//! the same run, and a resumed pack has to: the header is written once, at
//! the end, and everything before it is already on disk. So the bytes are
//! ours. They follow the same layout the crate's writer produces for a
//! non-solid archive, which the reader in `archive::sevenz` and 7-Zip both
//! take as they find it, and the header is written raw rather than
//! LZMA-encoded, which the format allows and keeps the writer here small.

use std::io::{self, Read, Write};

use anyhow::{Result, anyhow};

use lzma_rust2::{Lzma2Options, Lzma2Writer, LzmaOptions};

/// The signature header as a fresh pack starts it: a real signature and
/// version, and zeros where the end header will be pointed at. Nothing can
/// read the file as a 7z until the end is written, so a pack in progress is
/// never mistaken for a readable archive.
pub const SIGNATURE: [u8; 32] = [
    b'7', b'z', 0xBC, 0xAF, 0x27, 0x1C, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0,
];

const LZMA2_ID: u8 = 0x21;

/// One file's compressed block, as the journal's index and the header
/// record it.
pub struct Block {
    pub compressed_size: u64,
    pub compressed_crc: u32,
    pub size: u64,
    pub crc: u32,
    /// The dictionary the block was compressed with, which the header
    /// writes as the coder's properties.
    pub dict: u32,
}

/// The bytes of one index row, the fixed record a commit appends so a
/// resumed run can write the header without reading the blocks back.
pub const ROW: usize = 20;

/// The smallest dictionary that can hold the file, from 64 KiB to 8 MiB.
/// A small file should not pay for a big one, and every file gets a
/// dictionary of its own anyway: packing compresses them independently.
pub fn dictionary_for(size: u64) -> u32 {
    let mut dict = 64 << 10;
    while dict < 8 << 20 && u64::from(dict) < size {
        dict <<= 1;
    }
    dict
}

/// The LZMA2 properties byte for a power-of-two dictionary, the encoding
/// 7z defines for it.
fn props(dict: u32) -> u8 {
    let lead = dict.leading_zeros();
    let second = (dict >> (30 - lead)) - 2;
    ((19 - lead) * 2 + second) as u8
}

fn options(dict: u32) -> Lzma2Options {
    let preset = LzmaOptions::with_preset(6);
    Lzma2Options {
        lzma_options: LzmaOptions {
            dict_size: dict,
            ..preset
        },
        chunk_size: None,
    }
}

/// Counts what passes through, for the sizes and CRCs a block records.
struct Measured<R> {
    inner: R,
    size: u64,
    crc: crc32fast::Hasher,
}

impl<R> Measured<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            size: 0,
            crc: crc32fast::Hasher::new(),
        }
    }
}

impl<R: Read> Read for Measured<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.crc.update(&buf[..n]);
        self.size += n as u64;
        Ok(n)
    }
}

impl<W: Write> Write for Measured<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.crc.update(&buf[..n]);
        self.size += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Compresses one file into memory, for files small enough to be packed a
/// batch at a time.
pub fn compress<R: Read>(read: R, dict: u32) -> Result<(Block, Vec<u8>)> {
    let mut src = Measured::new(read);
    let mut encoder = Lzma2Writer::new(Vec::new(), options(dict));
    copy(&mut src, &mut encoder)?;
    let compressed = encoder
        .finish()
        .map_err(|e| anyhow!("finishing a compressed block: {e}"))?;
    Ok((
        Block {
            compressed_size: compressed.len() as u64,
            compressed_crc: crc32fast::hash(&compressed),
            size: src.size,
            crc: src.crc.finalize(),
            dict,
        },
        compressed,
    ))
}

/// Compresses one file straight into the archive, for a file too large to
/// hold its block in memory. The caller has the archive where the block
/// belongs.
pub fn stream<R: Read, W: Write>(read: R, out: W, dict: u32) -> Result<Block> {
    let mut src = Measured::new(read);
    let mut encoder = Lzma2Writer::new(Measured::new(out), options(dict));
    copy(&mut src, &mut encoder)?;
    let out = encoder
        .finish()
        .map_err(|e| anyhow!("finishing a compressed block: {e}"))?;
    Ok(Block {
        compressed_size: out.size,
        compressed_crc: out.crc.finalize(),
        size: src.size,
        crc: src.crc.finalize(),
        dict,
    })
}

fn copy(src: &mut impl Read, dst: &mut impl Write) -> Result<()> {
    let mut buf = [0u8; 256 * 1024];
    loop {
        let n = src
            .read(&mut buf)
            .map_err(|e| anyhow!("reading a file to pack: {e}"))?;
        if n == 0 {
            return Ok(());
        }
        dst.write_all(&buf[..n])
            .map_err(|e| anyhow!("compressing a file: {e}"))?;
    }
}

/// One entry the header describes: a file, an empty file or a folder, with
/// everything 7z keeps about it.
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// Windows file time, the form 7z stores.
    pub mtime: Option<u64>,
    /// Unix mode with its type bits, for the attributes field.
    pub mode: Option<u32>,
}

/// Writes a 7z number: the low bytes in little order, and a first byte
/// whose high bits say how many of them there are.
fn number(out: &mut Vec<u8>, value: u64) {
    let bits = (64 - value.leading_zeros()) as usize;
    let extra = bits.div_ceil(7).saturating_sub(1).min(8);
    if extra == 0 {
        out.push(value as u8);
        return;
    }
    // With eight extra bytes the first byte is all flags, so the value has
    // no high part left to hold in it.
    let high = if extra == 8 {
        0
    } else {
        (value >> (8 * extra)) as u8
    };
    let first = 0xFFu8 << (8 - extra) | high;
    out.push(first);
    for i in 0..extra {
        out.push((value >> (8 * i)) as u8);
    }
}

/// Writes bits as 7z does: from the top of each byte, the spare bits at
/// the end zero.
fn bits(out: &mut Vec<u8>, set: impl Fn(usize) -> bool, count: usize) {
    let mut cache = 0u8;
    let mut shift = 7;
    for i in 0..count {
        if set(i) {
            cache |= 1 << shift;
        }
        shift -= 1;
        if shift < 0 {
            out.push(cache);
            shift = 7;
            cache = 0;
        }
    }
    if shift != 7 {
        out.push(cache);
    }
}

/// Encodes the end header: the pack streams, one block per file that has
/// data, then every entry. `blocks` holds one record per such file, in the
/// order the entries list them.
pub fn end_header(entries: &[Entry], blocks: &[Block]) -> Vec<u8> {
    let streamed = entries.iter().filter(|e| !e.is_dir && e.size > 0).count();
    assert_eq!(
        streamed,
        blocks.len(),
        "the pack recorded one block per file with data"
    );

    let mut h = Vec::with_capacity(64 * 1024 + 48 * entries.len());
    h.push(0x01); // header
    h.push(0x04); // main streams info
    if !blocks.is_empty() {
        h.push(0x06); // pack info
        number(&mut h, 0);
        number(&mut h, blocks.len() as u64);
        h.push(0x09); // size
        for b in blocks {
            number(&mut h, b.compressed_size);
        }
        h.push(0x0A); // crc, all defined
        h.push(1);
        for b in blocks {
            h.extend_from_slice(&b.compressed_crc.to_le_bytes());
        }
        h.push(0x00);

        h.push(0x07); // unpack info
        h.push(0x0B); // folder
        number(&mut h, blocks.len() as u64);
        h.push(0);
        for b in blocks {
            // One coder, LZMA2, with its dictionary as the properties.
            number(&mut h, 1);
            h.push(0x20 | 1);
            h.push(LZMA2_ID);
            h.push(1);
            h.push(props(b.dict));
        }
        h.push(0x0C); // coders unpack size
        for b in blocks {
            number(&mut h, b.size);
        }
        h.push(0x00);

        h.push(0x08); // sub streams info
        h.push(0x0A); // crc, all defined
        h.push(1);
        for b in blocks {
            h.extend_from_slice(&b.crc.to_le_bytes());
        }
        h.push(0x00);
    }
    h.push(0x00);

    h.push(0x05); // files info
    number(&mut h, entries.len() as u64);
    let no_stream: Vec<&Entry> = entries.iter().filter(|e| e.is_dir || e.size == 0).collect();
    if !no_stream.is_empty() {
        h.push(0x0E); // empty stream
        let set = {
            let mut v = Vec::new();
            bits(
                &mut v,
                |i| entries[i].is_dir || entries[i].size == 0,
                entries.len(),
            );
            v
        };
        number(&mut h, set.len() as u64);
        h.extend_from_slice(&set);
        if no_stream.iter().any(|e| !e.is_dir) {
            h.push(0x0F); // empty file
            let set = {
                let mut v = Vec::new();
                bits(&mut v, |i| !no_stream[i].is_dir, no_stream.len());
                v
            };
            number(&mut h, set.len() as u64);
            h.extend_from_slice(&set);
        }
    }

    h.push(0x11); // name
    let mut names = vec![0u8];
    for e in entries {
        for c in e.name.encode_utf16() {
            names.extend_from_slice(&c.to_le_bytes());
        }
        names.extend_from_slice(&[0, 0]);
    }
    number(&mut h, names.len() as u64);
    h.extend_from_slice(&names);

    let times: Vec<u64> = entries.iter().filter_map(|e| e.mtime).collect();
    if !times.is_empty() {
        h.push(0x14); // modified time
        let mut body = Vec::new();
        if times.len() == entries.len() {
            body.push(1);
        } else {
            body.push(0);
            bits(&mut body, |i| entries[i].mtime.is_some(), entries.len());
        }
        body.push(0);
        for t in times {
            body.extend_from_slice(&t.to_le_bytes());
        }
        number(&mut h, body.len() as u64);
        h.extend_from_slice(&body);
    }

    let modes: Vec<u32> = entries.iter().filter_map(|e| e.mode).collect();
    if !modes.is_empty() {
        h.push(0x15); // windows attributes
        let mut body = Vec::new();
        if modes.len() == entries.len() {
            body.push(1);
        } else {
            body.push(0);
            bits(&mut body, |i| entries[i].mode.is_some(), entries.len());
        }
        body.push(0);
        for m in modes {
            body.extend_from_slice(&m.to_le_bytes());
        }
        number(&mut h, body.len() as u64);
        h.extend_from_slice(&body);
    }

    h.push(0x00); // files info end
    h.push(0x00); // header end
    h
}

/// The bytes that finish the archive once its end header is written at
/// `header_at`: the checksum of the start header's tail, and where the end
/// header lies. Written over the zeros the archive began with.
pub fn start_patch(header_at: u64, header: &[u8]) -> [u8; 24] {
    let mut tail = [0u8; 20];
    tail[..8].copy_from_slice(&(header_at - 32).to_le_bytes());
    tail[8..16].copy_from_slice(&(header.len() as u64).to_le_bytes());
    tail[16..].copy_from_slice(&crc32fast::hash(header).to_le_bytes());
    let mut patch = [0u8; 24];
    patch[..4].copy_from_slice(&crc32fast::hash(&tail).to_le_bytes());
    patch[4..].copy_from_slice(&tail);
    patch
}

/// Reads an index row back, the form a resumed run finds it in.
pub fn decode_row(bytes: &[u8]) -> Block {
    Block {
        compressed_size: u64::from_le_bytes(bytes[..8].try_into().unwrap()),
        compressed_crc: u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
        size: 0,
        crc: u32::from_le_bytes(bytes[12..16].try_into().unwrap()),
        dict: u32::from_le_bytes(bytes[16..20].try_into().unwrap()),
    }
}

/// Writes an index row.
pub fn encode_row(b: &Block) -> [u8; ROW] {
    let mut row = [0u8; ROW];
    row[..8].copy_from_slice(&b.compressed_size.to_le_bytes());
    row[8..12].copy_from_slice(&b.compressed_crc.to_le_bytes());
    row[12..16].copy_from_slice(&b.crc.to_le_bytes());
    row[16..20].copy_from_slice(&b.dict.to_le_bytes());
    row
}

/// Compresses one segment of a streamed file: its own LZMA2 run, starting
/// with a dictionary and state reset, which is legal at any point of an
/// LZMA2 stream. The trailing end marker goes unless this is the last
/// segment, so the pieces a packed stream is built from read as one.
/// Returns the compressed bytes, the bytes consumed, and their CRC.
pub fn compress_segment(
    read: impl Read,
    dict: u32,
    first: bool,
    last: bool,
) -> Result<(Vec<u8>, u64, u32)> {
    let mut src = Measured::new(read);
    let mut encoder = Lzma2Writer::new(Vec::new(), options(dict));
    copy(&mut src, &mut encoder)?;
    let mut out = encoder
        .finish()
        .map_err(|e| anyhow!("finishing a compressed segment: {e}"))?;
    if !last {
        let end = out
            .pop()
            .expect("the LZMA2 end marker is at least one byte");
        assert_eq!(end, 0, "the LZMA2 end marker is one zero byte");
    }
    let _ = first;
    Ok((out, src.size, src.crc.finalize()))
}

/// Joins two CRC-32 values of consecutive byte ranges into the CRC of the
/// whole, the way zlib does since 1.3: crc1 shifted forward by len2 bytes
/// (x^(len2 * 8) mod p, applied by multmodp), crc2 XORed on. What a
/// streamed file's journal carries across a restart.
pub fn crc32_combine(crc1: u32, crc2: u32, len2: u64) -> u32 {
    if len2 == 0 {
        return crc1;
    }
    crc32_combine_op(crc1, crc2, crc32_shift_op(len2))
}

/// The operator crc32_combine builds for a length: x^(len2 * 8) mod p.
fn crc32_shift_op(len2: u64) -> u32 {
    // x2n_table[k] = x^(2^(k+1)) mod p, k counted from the low bits of n.
    fn multmodp(a: u32, mut b: u32) -> u32 {
        let mut m = 1u32 << 31;
        let mut p = 0u32;
        loop {
            if a & m != 0 {
                p ^= b;
                if a & (m - 1) == 0 {
                    break;
                }
            }
            m >>= 1;
            b = if b & 1 != 0 {
                (b >> 1) ^ 0xedb8_8320
            } else {
                b >> 1
            };
        }
        p
    }

    let mut table = [0u32; 32];
    let mut square = 1u32 << 30;
    for slot in table.iter_mut() {
        *slot = square;
        square = multmodp(square, square);
    }
    #[cfg(test)]
    if len2 == 700_001 {
        eprintln!("rust table head: {:x?}", &table[..6]);
        eprintln!(
            "rust mm tests: {:x} {:x} {:x}",
            multmodp(0xC000_0000, 0x1234_5678),
            multmodp(0x55dc_1152, 0x74be_b8ea),
            multmodp(0xbc3b_33ac, 0x74be_b8ea)
        );
    }

    let mut q = 1u32 << 31;
    // k starts at 3: the table holds powers of x^2, and len2 counts bytes,
    // which are x^8, so the bits of len2 are worth x^(8 * 2^k) each.
    let mut k = 3usize;
    let mut len2 = len2;
    while len2 != 0 {
        if len2 & 1 != 0 {
            q = multmodp(table[k & 31], q);
        }
        len2 >>= 1;
        k += 1;
    }
    q
}

/// zlib's crc32_combine_op: multmodp(op, crc1) ^ crc2.
fn crc32_combine_op(crc1: u32, crc2: u32, op: u32) -> u32 {
    if op == 0 {
        return 0;
    }
    fn multmodp(a: u32, mut b: u32) -> u32 {
        let mut m = 1u32 << 31;
        let mut p = 0u32;
        loop {
            if a & m != 0 {
                p ^= b;
                if a & (m - 1) == 0 {
                    break;
                }
            }
            m >>= 1;
            b = if b & 1 != 0 {
                (b >> 1) ^ 0xedb8_8320
            } else {
                b >> 1
            };
        }
        p
    }
    multmodp(op, crc1) ^ crc2
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, is_dir: bool, size: u64) -> Entry {
        Entry {
            name: name.to_string(),
            is_dir,
            size,
            mtime: Some(133_000_000_000_000_000),
            mode: Some(if is_dir { 0o040755 } else { 0o100644 } | 0x8000),
        }
    }

    /// A header the reference reader parses into exactly the entries and
    /// blocks it was written from, the round trip every packed archive
    /// depends on.
    #[test]
    fn the_reference_reader_parses_the_header() {
        let dir = std::env::temp_dir().join(format!("hatchzip-pack-header-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let entries = vec![
            entry("docs", true, 0),
            entry("docs/empty.txt", false, 0),
            entry("docs/notes.txt", false, 5),
            entry("run.bin", false, 1 << 20),
        ];
        let blocks = vec![
            Block {
                compressed_size: 3,
                compressed_crc: 1,
                size: 5,
                crc: 2,
                dict: 64 << 10,
            },
            Block {
                compressed_size: 999_999,
                compressed_crc: 3,
                size: 1 << 20,
                crc: 4,
                dict: 1 << 20,
            },
        ];
        let header = end_header(&entries, &blocks);
        let mut file = SIGNATURE.to_vec();
        file.extend_from_slice(&[0u8; 2 << 20]);
        let header_at = file.len() as u64;
        file.extend_from_slice(&header);
        let patch = start_patch(header_at, &header);
        file[8..32].copy_from_slice(&patch);
        let path = dir.join("made.7z");
        std::fs::write(&path, &file).unwrap();

        let archive = sevenz_rust2::Archive::read(
            &mut std::fs::File::open(&path).unwrap(),
            &sevenz_rust2::Password::empty(),
        )
        .unwrap();
        assert_eq!(archive.files.len(), 4);
        assert!(archive.files[0].is_directory());
        assert!(!archive.files[1].is_directory());
        assert_eq!(archive.files[1].size(), 0);
        assert_eq!(archive.files[2].size(), 5);
        assert_eq!(archive.files[2].crc, 2);
        assert_eq!(archive.files[3].size(), 1 << 20);
        assert_eq!(archive.files[3].crc, 4);
        assert_eq!(archive.pack_sizes(), &[3, 999_999]);
        assert_eq!(archive.files[2].name(), "docs/notes.txt");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The header of a tree with nothing but folders and empty files still
    /// parses, with no streams at all.
    #[test]
    fn an_empty_stream_header_parses() {
        let dir = std::env::temp_dir().join(format!("hatchzip-pack-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let entries = vec![entry("only", true, 0)];
        let header = end_header(&entries, &[]);
        let mut file = SIGNATURE.to_vec();
        let header_at = file.len() as u64;
        file.extend_from_slice(&header);
        let patch = start_patch(header_at, &header);
        file[8..32].copy_from_slice(&patch);
        let path = dir.join("empty.7z");
        std::fs::write(&path, &file).unwrap();

        let archive = sevenz_rust2::Archive::read(
            &mut std::fs::File::open(&path).unwrap(),
            &sevenz_rust2::Password::empty(),
        )
        .unwrap();
        assert_eq!(archive.files.len(), 1);
        assert!(archive.files[0].is_directory());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The properties byte matches what 7z defines for each dictionary
    /// size: the dictionary is the byte's own encoding, read back.
    #[test]
    fn properties_bytes_name_their_dictionary() {
        let mut dict = 64 << 10;
        while dict <= 8 << 20 {
            let p = props(dict);
            let from_props = (2 | u32::from(p & 1)) << (p / 2 + 11);
            assert_eq!(from_props, dict, "props {p} for dictionary {dict}");
            dict <<= 1;
        }
    }

    /// Segments concatenate into one LZMA2 stream: a decoder reads the
    /// pieces stripped of their end markers, then the last piece whole,
    /// and sees exactly the bytes fed.
    #[test]
    fn segments_read_as_one_stream() {
        let data = common_noise(3_000_000);
        let step = 1 << 20;
        let mut stream: Vec<u8> = Vec::new();
        let mut at = 0;
        while at < data.len() {
            let last = at + step >= data.len();
            let (piece, fed, crc) = compress_segment(
                &data[at..(at + step).min(data.len())],
                1 << 20,
                at == 0,
                last,
            )
            .unwrap();
            assert_eq!(fed, u64::try_from(step.min(data.len() - at)).unwrap());
            let _ = crc;
            stream.extend_from_slice(&piece);
            at += step;
        }

        let mut decoded = Vec::new();
        let mut reader = lzma_rust2::Lzma2Reader::new(&stream[..], 1 << 20, None);
        reader.read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded, data);
    }

    /// The combine is the CRC of the concatenation, at any split.
    #[test]
    fn crcs_combine_like_concatenation() {
        let a = common_noise(700_001);
        let b = common_noise(1_000_000);
        // A split inside a: the pieces join back to exactly a.
        for split in [0usize, 1, 12345, a.len()] {
            let joined = crc32_combine(
                crc32fast::hash(&a[..split]),
                crc32fast::hash(&a[split..]),
                (a.len() - split) as u64,
            );
            eprintln!(
                "split {split}: joined {:08x} crc1 {:08x} crc2 {:08x} len2 {} shift {:08x} whole {:08x}",
                joined,
                crc32fast::hash(&a[..split]),
                crc32fast::hash(&a[split..]),
                a.len() - split,
                crc32_shift_op((a.len() - split) as u64),
                crc32fast::hash(&a)
            );
            assert_eq!(joined, crc32fast::hash(&a), "split at {split}");
        }
        // The whole of a and the whole of b join to a followed by b.
        let mut whole = a.clone();
        whole.extend_from_slice(&b);
        let joined = crc32_combine(crc32fast::hash(&a), crc32fast::hash(&b), b.len() as u64);
        assert_eq!(joined, crc32fast::hash(&whole));
        assert_eq!(
            crc32_combine(crc32fast::hash(&a), 0, 0),
            crc32fast::hash(&a)
        );
        // Reference values from the same arithmetic in zlib's multmodp and
        // x2nmodp, run against this data in isolation.
        assert_eq!(crc32_shift_op(700_001), 0x55dc_1152);
        assert_eq!(
            crc32_combine(
                crc32fast::hash(&a[..1]),
                crc32fast::hash(&a[1..]),
                (a.len() - 1) as u64
            ),
            crc32fast::hash(&a)
        );
        eprintln!("shift op: {:x}", crc32_shift_op(700_001));
        eprintln!("crc a: {:x}", crc32fast::hash(&a));
    }

    /// Deterministic bytes for the tests above.
    fn common_noise(len: usize) -> Vec<u8> {
        let mut x = 0x1234_5678u32;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect()
    }

    /// Numbers round trip through the encoding 7z defines.
    #[test]
    fn numbers_round_trip() {
        for v in [
            0u64,
            1,
            0x7F,
            0x80,
            0x123,
            0x14523,
            u64::from(u32::MAX),
            u64::MAX,
        ] {
            let mut out = Vec::new();
            number(&mut out, v);
            let mut bytes: &[u8] = &out;
            let mut first = [0u8; 1];
            std::io::Read::read_exact(&mut bytes, &mut first).unwrap();
            let mut value = 0u64;
            let mut mask = 0x80u32;
            let mut i = 0;
            while i < 8 {
                if u32::from(first[0]) & mask == 0 {
                    value = u64::from(first[0] & (mask - 1) as u8) << (8 * i);
                    for k in 0..i {
                        let mut b = [0u8; 1];
                        std::io::Read::read_exact(&mut bytes, &mut b).unwrap();
                        value |= u64::from(b[0]) << (8 * k);
                    }
                    break;
                }
                mask >>= 1;
                i += 1;
            }
            if i == 8 {
                let mut b = [0u8; 8];
                std::io::Read::read_exact(&mut bytes, &mut b).unwrap();
                value = u64::from_le_bytes(b);
            }
            assert_eq!(value, v, "encoding {out:02X?}");
        }
    }
}
