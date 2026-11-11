//! The other direction: pack a folder into a 7z beside it, freeing each
//! file once its compressed copy is durable, so the disk never has to hold
//! much more than whichever of the two is larger.
//!
//! The mirror of the extraction rule: the journal never claims a file
//! before its compressed bytes are flushed to the archive, and a file is
//! never deleted before the journal claims it. Everything a resumed run
//! needs lives beside the archive, since the files themselves are gone as
//! they are packed: the journal, the list of every entry, and an index row
//! per packed block.

use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::engine::{self, Outcome, Progress};
use crate::fsutil;
use crate::{chosen, platform};
use anyhow::{Context, Result, anyhow, bail};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

mod sevenz;
use sevenz::{Block, Entry};

/// A pack journal is its own version, above every extraction journal
/// version: older builds refuse it rather than read it as one of theirs.
pub(crate) const VERSION: u32 = 9;
const KIND: &str = "pack";

/// Files this large are packed alone, straight into the archive, so their
/// blocks never sit in memory. Smaller ones are compressed a batch at a
/// time and appended in one go.
const BATCH_BYTES: u64 = 128 * 1024 * 1024;

/// Held back from every batch, so the end header, the index rows still to
/// come and the journal always fit at the finish.
const SLICE_ROOM: u64 = 1024 * 1024;

/// More than any disk holds, so the sums below stay far from overflowing.
const MAX_TOTAL: u64 = 1 << 62;

fn batch_bytes() -> u64 {
    knob("HATCHZIP_PACK_BATCH").unwrap_or(BATCH_BYTES)
}

/// A setting only the test suite uses. Release builds ignore it.
fn knob(name: &str) -> Option<u64> {
    if !cfg!(debug_assertions) {
        return None;
    }
    std::env::var(name).ok()?.parse().ok()
}

/// The least free space a run must see to hold `size` twice: LZMA2 falls
/// back to uncompressed chunks, whose overhead is a few bytes per chunk, so
/// this is a bound, not a guess.
fn expansion(size: u64) -> u64 {
    size.saturating_add(size / 1024).saturating_add(1024)
}

/// Why a pack cannot start or go on, with the numbers the message needs.
/// The CLI's exit code 3 and the space view in the app look for it.
#[derive(Debug)]
pub struct NoSpace {
    pub file: PathBuf,
    pub needed: u64,
    pub avail: u64,
}

impl std::fmt::Display for NoSpace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "packing needs {} free while it holds '{}' beside itself, {} free",
            engine::human(self.needed),
            fsutil::plain(&self.file).display(),
            engine::human(self.avail)
        )
    }
}
impl std::error::Error for NoSpace {}

#[derive(Debug)]
struct Paused;

impl std::fmt::Display for Paused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the pack was stopped")
    }
}
impl std::error::Error for Paused {}

/// One entry of the walk, and of the archive once it is packed. The list
/// file holds one per file and folder, and it is all that is left of them.
#[derive(Serialize, Deserialize, Clone)]
struct Row {
    name: String,
    is_dir: bool,
    size: u64,
    /// Windows file time, the form the archive stores.
    mtime: Option<u64>,
    /// Unix mode with its type bits. Absent where the system has none.
    mode: Option<u32>,
}

#[derive(Serialize, Deserialize, Clone)]
struct Journal {
    version: u32,
    kind: String,
    source: PathBuf,
    target: PathBuf,
    /// Every entry of the list file.
    entries: u64,
    /// Every file that has data, the ones the index holds a row for.
    blocks: u64,
    bytes_total: u64,
    /// The bytes of the files already packed, for progress.
    bytes_done: u64,
    /// The archive length once the latest batch is committed. Everything
    /// past it is a block no journal claims, and a resume cuts it off.
    committed_len: u64,
    blocks_done: u64,
    list_crc: u32,
    list_len: u64,
    /// The end header is written and the archive is whole: only the
    /// cleanup is left.
    finished: bool,
}

/// The files a pack keeps beside the archive it is building.
#[derive(Clone)]
struct Paths {
    target: PathBuf,
    list: PathBuf,
    index: PathBuf,
    journal: PathBuf,
}

impl Paths {
    fn for_target(target: &Path) -> Self {
        let with = |suffix: &str| {
            let mut s = target.as_os_str().to_owned();
            s.push(suffix);
            PathBuf::from(s)
        };
        Self {
            target: target.to_path_buf(),
            list: with(".hatchzip-list"),
            index: with(".hatchzip-index"),
            journal: with(".hatchzip"),
        }
    }
}

/// A bound on the end header's size: names and times as long as they are,
/// every number at its widest, so the last write always fits.
fn header_room(rows: &[Row]) -> u64 {
    let names: u64 = rows.iter().map(|r| r.name.len() as u64 * 2 + 2).sum();
    let n = rows.len() as u64;
    let blocks = rows.iter().filter(|r| !r.is_dir && r.size > 0).count() as u64;
    names + 12 * n + 4 * n + 3 * (n + 7) / 8 + 80 * blocks + 64
}

fn filetime(t: SystemTime) -> Option<u64> {
    const SECS_1601_TO_1970: u64 = 11_644_473_600;
    let d = t.duration_since(UNIX_EPOCH).ok()?;
    Some((d.as_secs() + SECS_1601_TO_1970) * 10_000_000 + u64::from(d.subsec_nanos() / 100))
}

/// The archive name of a path below the source, with `/` between parts,
/// which is what every 7z holds. A `\` in a name is refused: extraction
/// reads it as a folder separator, so the round trip would rename the file.
fn archive_name(source: &Path, path: &Path) -> Result<String> {
    let mut name = String::new();
    for part in path.strip_prefix(source).unwrap_or(path).components() {
        match part {
            Component::Normal(p) => {
                let p = p
                    .to_str()
                    .context("a name that is not text cannot be packed")?;
                if p.contains('\\') {
                    bail!(
                        "'{p}' cannot be packed: a backslash in a name would come back as a folder"
                    );
                }
                if !name.is_empty() {
                    name.push('/');
                }
                name.push_str(p);
            }
            _ => bail!("a path below the folder changed while it was read"),
        }
    }
    Ok(name)
}

/// Lists the folder as it stands, refusing everything a pack cannot take:
/// links of either kind, which would either follow where they point or
/// count their bytes twice, and names the round trip would not keep.
fn walk(source: &Path) -> Result<Vec<Row>> {
    let meta = fs::metadata(source)
        .with_context(|| format!("reading {}", fsutil::plain(source).display()))?;
    if !meta.is_dir() {
        bail!("'{}' is not a folder", fsutil::plain(source).display());
    }
    let mut rows = Vec::new();
    let mut total = 0u64;
    walk_dir(source, source, &mut rows, &mut total)?;
    if total > MAX_TOTAL {
        bail!(
            "the folder holds {}, more than a pack can plan for",
            engine::human(total)
        );
    }
    Ok(rows)
}

fn walk_dir(source: &Path, dir: &Path, rows: &mut Vec<Row>, total: &mut u64) -> Result<()> {
    let children = fs::read_dir(dir)
        .with_context(|| format!("reading {}", fsutil::plain(dir).display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("reading {}", fsutil::plain(dir).display()))?;
    let mut children: Vec<_> = children
        .into_iter()
        .map(|e| (e.file_name(), e.path()))
        .collect();
    children.sort_by(|a, b| a.0.as_encoded_bytes().cmp(b.0.as_encoded_bytes()));

    for (_name, path) in children {
        let meta = fs::symlink_metadata(&path)
            .with_context(|| format!("reading {}", fsutil::plain(&path).display()))?;
        if meta.file_type().is_symlink() {
            bail!(
                "'{}' is a link, which packing cannot follow safely yet",
                fsutil::plain(&path).display()
            );
        }
        if meta.is_dir() {
            rows.push(Row {
                name: archive_name(source, &path)?,
                is_dir: true,
                size: 0,
                mtime: meta.modified().ok().and_then(filetime),
                mode: platform::mode(&meta),
            });
            walk_dir(source, &path, rows, total)?;
        } else if meta.is_file() {
            if platform::link_count(&path)? > 1 {
                bail!(
                    "'{}' has another name on this disk. Packing it would count its bytes twice, \
                     and deleting this name would not free them",
                    fsutil::plain(&path).display()
                );
            }
            *total += meta.len();
            rows.push(Row {
                name: archive_name(source, &path)?,
                is_dir: false,
                size: meta.len(),
                mtime: meta.modified().ok().and_then(filetime),
                mode: platform::mode(&meta),
            });
        } else {
            bail!(
                "'{}' is neither a file nor a folder, which packing cannot keep",
                fsutil::plain(&path).display()
            );
        }
    }
    Ok(())
}

/// The space a pack of `source` into `target` needs before it starts, and
/// what it stands to give back. Read only: nothing is touched.
pub struct SpacePlan {
    pub files: u64,
    pub folders: u64,
    pub bytes: u64,
    /// Free space the whole run never goes below, `keep_free` aside.
    pub needs: u64,
    pub avail: u64,
    pub fits: bool,
}

pub fn space_plan(source: &Path, target: &Path, keep_free: u64) -> Result<SpacePlan> {
    let rows = walk(source)?;
    let bytes: u64 = rows.iter().filter(|r| !r.is_dir).map(|r| r.size).sum();
    let largest = rows
        .iter()
        .filter(|r| !r.is_dir)
        .map(|r| r.size)
        .max()
        .unwrap_or(0);
    let needs = expansion(largest)
        + header_room(&rows)
        + list_bytes(&rows)
        + rows.len() as u64 * sevenz::ROW as u64
        + SLICE_ROOM;
    let avail = usable_space(target, keep_free)?;
    Ok(SpacePlan {
        files: rows.iter().filter(|r| !r.is_dir).count() as u64,
        folders: rows.iter().filter(|r| r.is_dir).count() as u64,
        bytes,
        needs,
        avail,
        fits: needs <= avail,
    })
}

fn usable_space(target: &Path, keep_free: u64) -> Result<u64> {
    let dir = target.parent().unwrap_or(Path::new("."));
    Ok(fsutil::available_space(dir)?.saturating_sub(keep_free))
}

fn list_bytes(rows: &[Row]) -> u64 {
    serde_json::to_vec(rows)
        .map(|v| v.len() as u64 + 16)
        .unwrap_or(0)
}

/// Where a pack of `source` puts its archive by default: beside the
/// folder, under its name.
pub fn target_for(source: &Path) -> Result<PathBuf> {
    let name = source
        .file_name()
        .and_then(|n| n.to_str())
        .with_context(|| {
            format!(
                "'{}' has no name to pack under",
                fsutil::plain(source).display()
            )
        })?;
    Ok(source.with_file_name(format!("{name}.7z")))
}

/// The bytes done and the bytes total of a pack in progress, for a progress
/// bar. Nothing is read but the journal.
pub fn progress_of(target: &Path) -> Option<(u64, u64)> {
    let paths = Paths::for_target(target);
    let journal: Journal = serde_json::from_slice(&fs::read(&paths.journal).ok()?).ok()?;
    Some((journal.bytes_done, journal.bytes_total))
}

/// The path as a person writes it, for front ends naming where a pack goes.
pub fn shown(path: &Path) -> PathBuf {
    fsutil::plain(path)
}

/// Packs `source` into a 7z, resuming a pack that was interrupted, paused
/// when `stop` is set, and refused for space before anything is touched.
pub fn pack(
    source: &Path,
    target: Option<&Path>,
    keep_free: u64,
    stop: &AtomicBool,
    progress: Progress,
) -> Result<Outcome> {
    let source = fs::canonicalize(source)
        .with_context(|| format!("finding {}", fsutil::plain(source).display()))?;
    if !source.is_dir() {
        bail!("'{}' is not a folder", fsutil::plain(&source).display());
    }
    let target = match target {
        Some(t) => std::path::absolute(t)
            .with_context(|| format!("reading the path {}", fsutil::plain(t).display()))?,
        None => target_for(&source)?,
    };
    let paths = Paths::for_target(&target);
    recover_orphan(&paths)?;

    if paths.journal.exists() {
        resume(&paths, &source, keep_free, stop, progress)
    } else {
        fresh(&paths, &source, &target, keep_free, stop, progress)
    }
}

/// A pack that stopped between creating its files and saving its journal
/// leaves them with nothing pointing at them: the archive is nothing but
/// the signature header, which no tool writes, and the side files say
/// nothing without a journal. Both are the pack's own, so the next run
/// takes them back instead of refusing.
fn recover_orphan(paths: &Paths) -> Result<()> {
    if paths.journal.exists() {
        return Ok(());
    }
    let ours = match fs::symlink_metadata(&paths.target) {
        Ok(m) if m.len() == 32 => {
            let mut head = [0u8; 32];
            File::open(&paths.target)
                .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut head))
                .is_ok()
                && head == sevenz::SIGNATURE
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        _ => false,
    };
    if !ours {
        return Ok(());
    }
    for p in [&paths.target, &paths.list, &paths.index] {
        match fs::remove_file(p) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(e).with_context(|| format!("removing {}", fsutil::plain(p).display()));
            }
            _ => {}
        }
    }
    Ok(())
}

fn fresh(
    paths: &Paths,
    source: &Path,
    target: &Path,
    keep_free: u64,
    stop: &AtomicBool,
    progress: Progress,
) -> Result<Outcome> {
    refuse_target(paths, source, target)?;

    let rows = walk(source)?;
    let blocks = rows.iter().filter(|r| !r.is_dir && r.size > 0).count() as u64;
    let bytes: u64 = rows.iter().filter(|r| !r.is_dir).map(|r| r.size).sum();
    let largest = rows
        .iter()
        .filter(|r| !r.is_dir)
        .map(|r| r.size)
        .max()
        .unwrap_or(0);
    let needs = expansion(largest)
        + header_room(&rows)
        + list_bytes(&rows)
        + blocks * sevenz::ROW as u64
        + SLICE_ROOM;
    let usable = usable_space(target, keep_free)?;
    if needs > usable {
        let file = rows
            .iter()
            .filter(|r| !r.is_dir)
            .max_by_key(|r| r.size)
            .map(|r| source.join(&r.name))
            .unwrap_or_else(|| source.to_path_buf());
        return Err(anyhow::Error::new(NoSpace {
            file,
            needed: needs,
            avail: usable,
        }));
    }

    let list = serde_json::to_vec(&rows).context("writing the list of files to pack")?;
    fsutil::write_atomic(&paths.list, &list)
        .with_context(|| format!("writing {}", fsutil::plain(&paths.list).display()))?;

    let created = platform::create_new_no_follow(target)
        .with_context(|| format!("creating {}", fsutil::plain(target).display()))?;
    drop(created);
    let mut archive = open_locked(target)?;
    archive.write_all(&sevenz::SIGNATURE)?;
    archive.sync_all()?;
    fsutil::sync_dir(target.parent().unwrap_or(Path::new("")))?;

    let index = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.index)
        .with_context(|| format!("creating {}", fsutil::plain(&paths.index).display()))?;

    let journal = Journal {
        version: VERSION,
        kind: KIND.to_string(),
        source: source.to_path_buf(),
        target: target.to_path_buf(),
        entries: rows.len() as u64,
        blocks,
        bytes_total: bytes,
        bytes_done: 0,
        committed_len: sevenz::SIGNATURE.len() as u64,
        blocks_done: 0,
        list_crc: crc32fast::hash(&list),
        list_len: list.len() as u64,
        finished: false,
    };
    save(paths, &journal)?;

    chosen::remember(target, source);
    let data = data_rows(&rows);
    run(
        Session {
            rows,
            data,
            source: source.to_path_buf(),
            paths: paths.clone(),
            journal,
            archive,
            index,
            keep_free,
            stop,
            progress,
        },
        Vec::new(),
    )
}

fn refuse_target(paths: &Paths, source: &Path, target: &Path) -> Result<()> {
    if fs::symlink_metadata(target).is_ok() {
        bail!(
            "'{}' already exists. A pack cannot go into it; move it away first",
            fsutil::plain(target).display()
        );
    }
    if target.strip_prefix(source).is_ok() {
        bail!(
            "'{}' would sit inside '{}', which the pack empties",
            fsutil::plain(target).display(),
            fsutil::plain(source).display()
        );
    }
    for p in [&paths.list, &paths.index, &paths.journal] {
        if fs::symlink_metadata(p).is_ok() {
            bail!(
                "'{}' is in the way, and a pack cannot know whose it is. Move it away first",
                fsutil::plain(p).display()
            );
        }
    }
    Ok(())
}

fn resume(
    paths: &Paths,
    source: &Path,
    keep_free: u64,
    stop: &AtomicBool,
    progress: Progress,
) -> Result<Outcome> {
    let (journal, rows, blocks, archive) = open_pack(paths, source)?;

    // Files a crashed run packed and journaled but never deleted.
    for row in rows
        .iter()
        .filter(|r| !r.is_dir && r.size > 0)
        .take(blocks.len())
    {
        let _ = fs::remove_file(source.join(&row.name));
    }

    let index = OpenOptions::new()
        .append(true)
        .open(&paths.index)
        .with_context(|| format!("opening {}", fsutil::plain(&paths.index).display()))?;

    let bytes_done: u64 = rows
        .iter()
        .filter(|r| !r.is_dir && r.size > 0)
        .take(blocks.len())
        .map(|r| r.size)
        .sum();

    let data = data_rows(&rows);
    run(
        Session {
            rows,
            data,
            source: source.to_path_buf(),
            paths: paths.clone(),
            journal: Journal {
                bytes_done,
                ..journal
            },
            archive,
            index,
            keep_free,
            stop,
            progress,
        },
        blocks,
    )
}

/// Everything a resumed or finished pack needs, checked before anything
/// changes: the journal belongs to `source`, the list and the index match
/// it, and the archive ends exactly at the committed length.
fn open_pack(paths: &Paths, source: &Path) -> Result<(Journal, Vec<Row>, Vec<Block>, File)> {
    let journal = load(paths)?;
    if !fsutil::same_path(&journal.source, source) {
        bail!(
            "this half-packed archive belongs to '{}', not '{}'. \
             Pack that folder to finish it",
            fsutil::plain(&journal.source).display(),
            fsutil::plain(source).display()
        );
    }
    let rows = read_list(paths, &journal)?;
    let blocks = read_index(paths, &journal, &rows)?;

    let archive = open_locked(&paths.target)?;
    let len = archive.metadata()?.len();
    if len < journal.committed_len {
        bail!(
            "'{}' is shorter than its journal says. Nothing can be told about it",
            fsutil::plain(&paths.target).display()
        );
    }
    archive.set_len(journal.committed_len)?;
    Ok((journal, rows, blocks, archive))
}

/// The rows of the files a pack compresses, in the order the manifest
/// lists them, which is the order their blocks are appended in.
fn data_rows(rows: &[Row]) -> Vec<usize> {
    rows.iter()
        .enumerate()
        .filter(|(_, r)| !r.is_dir && r.size > 0)
        .map(|(i, _)| i)
        .collect()
}

struct Session<'a> {
    rows: Vec<Row>,
    data: Vec<usize>,
    source: PathBuf,
    paths: Paths,
    journal: Journal,
    archive: File,
    index: File,
    keep_free: u64,
    stop: &'a AtomicBool,
    progress: Progress,
}

impl Session<'_> {
    fn row_path(&self, name: &str) -> PathBuf {
        self.source.join(name)
    }
}

fn run(mut s: Session, mut blocks: Vec<Block>) -> Result<Outcome> {
    let outcome = run_batches(&mut s, &mut blocks);
    let outcome = match outcome {
        Ok(()) if s.journal.finished => finish_clean(&mut s),
        Ok(()) => match finalize(&mut s, &blocks) {
            Ok(()) => finish_clean(&mut s),
            Err(e) => Err(e),
        },
        Err(e) if e.is::<Paused>() => Ok(Outcome::Paused),
        Err(e) => Err(e),
    };
    // Whatever happened, the archive ends where the journal says a
    // committed block ends, so the next run starts from solid ground.
    let _ = s.archive.set_len(s.journal.committed_len);
    outcome
}

fn run_batches(s: &mut Session, blocks: &mut Vec<Block>) -> Result<()> {
    let mut done = s.journal.blocks_done as usize;
    while done < s.data.len() {
        if s.stop.load(Ordering::Relaxed) {
            return Err(anyhow::Error::new(Paused));
        }
        let batch = choose_batch(s, done)?;
        let solo = batch.len() == 1 && s.rows[batch[0]].size > batch_bytes();
        let more = one_batch(s, &batch, solo)?;
        blocks.extend(more);
        done += batch.len();
    }
    Ok(())
}

/// The files to pack next: as many as the free space can hold beside
/// themselves, since each is deleted only once its whole block is written,
/// and never more bytes of them than a batch holds in memory. A file alone
/// over that many bytes is not refused here: it is packed straight into
/// the archive, which only the disk has to hold.
fn choose_batch(s: &Session, done: usize) -> Result<Vec<usize>> {
    let usable = usable_space(&s.paths.target, s.keep_free)?;
    let disk = usable.saturating_sub(header_room(&s.rows) + SLICE_ROOM);
    let memory = batch_bytes();
    let mut batch = Vec::new();
    let mut sum = 0u64;
    for &i in &s.data[done..] {
        let size = s.rows[i].size;
        if batch.is_empty() && expansion(size) > disk {
            return Err(anyhow::Error::new(NoSpace {
                file: s.row_path(&s.rows[i].name),
                needed: expansion(size),
                avail: usable,
            })
            .context(
                "the disk filled up or shrank while the pack ran. Free the space \
                 and run the same command to go on",
            ));
        }
        if !batch.is_empty() && sum + size > memory {
            break;
        }
        batch.push(i);
        sum += size;
    }
    Ok(batch)
}

/// Compresses one batch, commits it and deletes its files. On return the
/// journal claims every file of the batch, or none of them.
fn one_batch(s: &mut Session, batch: &[usize], solo: bool) -> Result<Vec<Block>> {
    let start_len = s.journal.committed_len;
    let mut appended = Vec::new();

    if solo {
        let row = s.rows[batch[0]].clone();
        let path = s.row_path(&row.name);
        check_still(&row, &path)?;
        let src = File::open(&path).with_context(|| {
            format!(
                "reading {}. If it is gone for good, keep what is packed so far with \
                 `hatchzip pack <folder> --finish`",
                fsutil::plain(&path).display()
            )
        })?;
        s.archive.seek(SeekFrom::Start(start_len))?;
        let block = sevenz::stream(src, &mut s.archive, sevenz::dictionary_for(row.size))
            .with_context(|| format!("packing {}", fsutil::plain(&path).display()))?;
        check_still(&row, &path)?;
        appended.push(block);
    } else {
        let packed: Vec<(usize, Block, Vec<u8>)> = batch
            .par_iter()
            .map(|&i| {
                let row = &s.rows[i];
                let path = s.source.join(&row.name);
                let src = File::open(&path).with_context(|| {
                    format!(
                        "reading {}. If it is gone for good, keep what is packed so far with \
                         `hatchzip pack <folder> --finish`",
                        fsutil::plain(&path).display()
                    )
                })?;
                let (block, bytes) = sevenz::compress(src, sevenz::dictionary_for(row.size))
                    .with_context(|| format!("packing {}", fsutil::plain(&path).display()))?;
                Ok((i, block, bytes))
            })
            .collect::<Result<_>>()?;
        for (i, _, _) in &packed {
            check_still(&s.rows[*i], &s.row_path(&s.rows[*i].name))?;
        }
        s.archive.seek(SeekFrom::Start(start_len))?;
        for (_, _, bytes) in &packed {
            s.archive
                .write_all(bytes)
                .with_context(|| format!("writing {}", fsutil::plain(&s.paths.target).display()))?;
        }
        appended = packed.into_iter().map(|(_, b, _)| b).collect();
    }

    s.archive
        .sync_all()
        .with_context(|| format!("flushing {}", fsutil::plain(&s.paths.target).display()))?;
    engine::crash_point("after-append");

    for block in &appended {
        s.index
            .write_all(&sevenz::encode_row(block))
            .with_context(|| format!("writing {}", fsutil::plain(&s.paths.index).display()))?;
    }
    s.index
        .sync_data()
        .with_context(|| format!("flushing {}", fsutil::plain(&s.paths.index).display()))?;

    let batch_bytes: u64 = batch.iter().map(|&i| s.rows[i].size).sum();
    let journal = Journal {
        blocks_done: s.journal.blocks_done + batch.len() as u64,
        committed_len: s.archive.stream_position()?,
        bytes_done: s.journal.bytes_done + batch_bytes,
        ..s.journal.clone()
    };
    save(&s.paths, &journal)?;
    s.journal = journal;
    engine::crash_point("after-journal");

    for &i in batch {
        let path = s.row_path(&s.rows[i].name);
        match fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(e).with_context(|| {
                    format!(
                        "deleting {}. The archive is fine; run the same command to go on",
                        fsutil::plain(&path).display()
                    )
                });
            }
            _ => {}
        }
    }
    (s.progress.on_bytes)(s.journal.bytes_done);
    Ok(appended)
}

/// A file must not change between reading it and deleting it, or the pack
/// would throw away what was never packed.
fn check_still(row: &Row, path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)
        .with_context(|| format!("reading {}", fsutil::plain(path).display()))?;
    if meta.len() != row.size || meta.modified().ok().and_then(filetime) != row.mtime {
        bail!(
            "'{}' changed while it was being packed. Nothing was deleted; pack it again \
             once it is still, or keep what is packed so far with \
             `hatchzip pack <folder> --finish`",
            fsutil::plain(path).display()
        );
    }
    Ok(())
}

fn finalize(s: &mut Session, blocks: &[Block]) -> Result<()> {
    let entries: Vec<Entry> = s.rows.iter().map(row_entry).collect();
    let end = write_header(&mut s.archive, &s.paths.target, &entries, blocks)?;

    let journal = Journal {
        finished: true,
        committed_len: end,
        ..s.journal.clone()
    };
    save(&s.paths, &journal)?;
    s.journal = journal;
    Ok(())
}

/// The header's form of a walk row.
fn row_entry(r: &Row) -> Entry {
    Entry {
        name: r.name.clone(),
        is_dir: r.is_dir,
        size: r.size,
        mtime: r.mtime,
        mode: r.mode.map(|m| 0x8000 | (m << 16)),
    }
}

/// Appends the end header for `entries` and their `blocks` and points the
/// start header at it. Returns the archive length once both are durable,
/// which is what a resume trusts.
fn write_header(
    archive: &mut File,
    target: &Path,
    entries: &[Entry],
    blocks: &[Block],
) -> Result<u64> {
    let header = sevenz::end_header(entries, blocks);
    archive.seek(SeekFrom::End(0))?;
    archive
        .write_all(&header)
        .with_context(|| format!("writing {}", fsutil::plain(target).display()))?;
    // The end of the archive is where the header stopped, not where the
    // start-header patch leaves the write position. It is the length a
    // resume must trust, so it is taken before the patch seek moves.
    let end = archive.stream_position()?;
    let header_at = end - header.len() as u64;
    let patch = sevenz::start_patch(header_at, &header);
    archive.seek(SeekFrom::Start(8))?;
    archive
        .write_all(&patch)
        .with_context(|| format!("writing {}", fsutil::plain(target).display()))?;
    archive
        .sync_all()
        .with_context(|| format!("flushing {}", fsutil::plain(target).display()))?;
    Ok(end)
}

/// What finishing early kept, and where the rest of the folder stands.
pub struct Finished {
    pub target: PathBuf,
    /// The files the archive holds, and the files the walk saw.
    pub packed: u64,
    pub total: u64,
    pub bytes: u64,
}

/// Ends a pack in progress the only way short of finishing it: everything
/// packed so far becomes a readable 7z, and everything the walk had not
/// reached stays in the folder, untouched. The journal goes away, so this
/// is not undoable either; nothing is deleted, so nothing is lost.
pub fn finish(source: &Path, target: Option<&Path>) -> Result<Finished> {
    let source = fs::canonicalize(source)
        .with_context(|| format!("finding {}", fsutil::plain(source).display()))?;
    let target = match target {
        Some(t) => std::path::absolute(t)
            .with_context(|| format!("reading the path {}", fsutil::plain(t).display()))?,
        None => target_for(&source)?,
    };
    let paths = Paths::for_target(&target);
    if !paths.journal.exists() {
        bail!(
            "there is no pack in progress beside '{}', so there is nothing to finish early",
            fsutil::plain(&target).display()
        );
    }
    let (mut journal, rows, blocks, mut archive) = open_pack(&paths, &source)?;
    if journal.finished {
        bail!(
            "'{}' is already whole. Run `hatchzip pack` on '{}' without --finish to delete \
             the folder and be done",
            fsutil::plain(&paths.target).display(),
            fsutil::plain(&source).display()
        );
    }
    let data = data_rows(&rows);
    let packed = blocks.len() as u64;

    // Nothing is packed yet, so the half-made archive holds nothing and
    // the folder is untouched: finishing here only means cancelling.
    if packed == 0 {
        drop(archive);
        fs::remove_file(&paths.target)
            .with_context(|| format!("removing {}", fsutil::plain(&paths.target).display()))?;
        let _ = fs::remove_file(&paths.index);
        let _ = fs::remove_file(&paths.list);
        chosen::forget(&paths.target);
        fs::remove_file(&paths.journal)
            .with_context(|| format!("removing {}", fsutil::plain(&paths.journal).display()))?;
        return Ok(Finished {
            target,
            packed: 0,
            total: data.len() as u64,
            bytes: 0,
        });
    }

    let entries: Vec<Entry> = rows
        .iter()
        .filter(|r| !r.is_dir && r.size > 0)
        .take(blocks.len())
        .map(row_entry)
        .collect();
    let end = write_header(&mut archive, &paths.target, &entries, &blocks)?;

    journal.finished = true;
    journal.committed_len = end;
    save(&paths, &journal)?;
    drop(archive);

    // The folder stays: whatever was not packed is still in it, and the
    // archive is opened over it like any other 7z.
    let _ = fs::remove_file(&paths.index);
    let _ = fs::remove_file(&paths.list);
    chosen::forget(&paths.target);
    fs::remove_file(&paths.journal)
        .with_context(|| format!("removing {}", fsutil::plain(&paths.journal).display()))?;
    Ok(Finished {
        target,
        packed,
        total: data.len() as u64,
        bytes: journal.bytes_done,
    })
}

/// Deletes what the finished archive has already taken: the empty files,
/// then the folders from the deepest up to the source itself. Whatever
/// appeared while the pack ran stays where it is, and is named.
fn finish_clean(s: &mut Session) -> Result<Outcome> {
    for row in &s.rows {
        if row.is_dir || row.size > 0 {
            continue;
        }
        let path = s.row_path(&row.name);
        match fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(e)
                    .with_context(|| format!("deleting {}", fsutil::plain(&path).display()));
            }
            _ => {}
        }
    }
    let mut leftover = Vec::new();
    let mut dirs: Vec<&Row> = s.rows.iter().filter(|r| r.is_dir).collect();
    dirs.sort_by_key(|r| std::cmp::Reverse(r.name.len()));
    for row in dirs {
        let path = s.row_path(&row.name);
        match fs::remove_dir(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => leftover.push(path),
            Ok(()) => {}
        }
    }
    match fs::remove_dir(&s.source) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => leftover.push(s.source.clone()),
        Ok(()) => {}
    }

    let warning = (!leftover.is_empty()).then(|| {
        let mut said: Vec<String> = leftover
            .iter()
            .take(3)
            .map(|p| format!("'{}'", fsutil::plain(p).display()))
            .collect();
        let more = leftover.len() - said.len();
        if more > 0 {
            said.push(format!("and {more} more"));
        }
        format!(
            "the archive is whole, but these folders hold files that were not there when \
             the pack began, and stayed: {}",
            said.join(", ")
        )
    });

    let _ = fs::remove_file(&s.paths.index);
    let _ = fs::remove_file(&s.paths.list);
    chosen::forget(&s.paths.target);
    fs::remove_file(&s.paths.journal)
        .with_context(|| format!("removing {}", fsutil::plain(&s.paths.journal).display()))?;
    (s.progress.on_bytes)(s.journal.bytes_total);
    Ok(Outcome::Finished(warning))
}

fn save(paths: &Paths, journal: &Journal) -> Result<()> {
    let bytes = serde_json::to_vec(journal).context("saving the pack journal")?;
    fsutil::write_atomic(&paths.journal, &bytes)
        .with_context(|| format!("saving {}", fsutil::plain(&paths.journal).display()))
}

fn load(paths: &Paths) -> Result<Journal> {
    let bytes = fs::read(&paths.journal)
        .with_context(|| format!("reading {}", fsutil::plain(&paths.journal).display()))?;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).context("reading the pack journal")?;
    match value.get("version").and_then(|v| v.as_u64()) {
        Some(v) if v == VERSION as u64 => {}
        Some(v) => bail!(
            "the journal of this pack was written by {} HatchZip",
            if v > VERSION as u64 {
                "a newer"
            } else {
                "an older"
            }
        ),
        None => bail!("the journal of this pack is damaged"),
    }
    if value.get("kind").and_then(|k| k.as_str()) != Some(KIND) {
        bail!(
            "'{}' is not the journal of a pack",
            fsutil::plain(&paths.journal).display()
        );
    }
    serde_json::from_value(value)
        .with_context(|| format!("reading {}", fsutil::plain(&paths.journal).display()))
}

fn read_list(paths: &Paths, journal: &Journal) -> Result<Vec<Row>> {
    let bytes = fs::read(&paths.list)
        .with_context(|| format!("reading {}", fsutil::plain(&paths.list).display()))?;
    if bytes.len() as u64 != journal.list_len || crc32fast::hash(&bytes) != journal.list_crc {
        bail!(
            "'{}' does not match the journal, and without it the archive cannot be finished",
            fsutil::plain(&paths.list).display()
        );
    }
    serde_json::from_slice(&bytes)
        .with_context(|| format!("reading {}", fsutil::plain(&paths.list).display()))
}

/// An index row keeps what the header cannot take from the list: the
/// compressed size and checksum, the file's checksum, and the dictionary.
/// The file's own size comes from the list, which is read with it.
fn read_index(paths: &Paths, journal: &Journal, list: &[Row]) -> Result<Vec<Block>> {
    let bytes = fs::read(&paths.index)
        .with_context(|| format!("reading {}", fsutil::plain(&paths.index).display()))?;
    let done = journal.blocks_done as usize;
    if bytes.len() < done * sevenz::ROW {
        bail!(
            "'{}' holds less than the journal claims was packed",
            fsutil::plain(&paths.index).display()
        );
    }
    let sizes = list
        .iter()
        .filter(|r| !r.is_dir && r.size > 0)
        .map(|r| r.size);
    Ok(bytes[..done * sevenz::ROW]
        .chunks_exact(sevenz::ROW)
        .map(sevenz::decode_row)
        .zip(sizes)
        .map(|(mut b, size)| {
            b.size = size;
            b
        })
        .collect())
}

fn open_locked(path: &Path) -> Result<File> {
    platform::open_exclusive(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::WouldBlock {
            anyhow!(
                "'{}' is in use by another program or HatchZip process",
                fsutil::plain(path).display()
            )
        } else {
            anyhow::Error::new(e).context(format!("opening {}", fsutil::plain(path).display()))
        }
    })
}
