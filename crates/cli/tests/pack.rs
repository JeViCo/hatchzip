//! End to end tests of packing a folder in place: the round trip through
//! HatchZip's own extraction, crash safety at both commit points, and the
//! refusals that keep a pack from destroying what it cannot take.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use common::{Files, assert_ok, hatchzip, sample_files};

fn make_tree(dir: &Path, files: &Files) -> PathBuf {
    let source = dir.join("game");
    for (name, data) in files {
        let path = source.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, data).unwrap();
    }
    fs::create_dir_all(source.join(common::EMPTY_DIR)).unwrap();
    source
}

/// The mtimes of every file, which the archive keeps and the extraction
/// puts back.
fn mtimes(dir: &Path) -> Vec<(String, SystemTime)> {
    let mut times = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else {
                times.push((
                    p.strip_prefix(dir)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                    fs::metadata(&p).unwrap().modified().unwrap(),
                ));
            }
        }
    }
    times.sort();
    times
}

fn pack(source: &Path, env: &[(&str, &str)]) -> std::process::Output {
    hatchzip(
        &[
            "pack",
            source.to_str().unwrap(),
            "--keep-free",
            "0",
            "--yes",
        ],
        env,
    )
}

fn pack_side_files(target: &Path) -> [PathBuf; 3] {
    let s = target.as_os_str().to_string_lossy().into_owned();
    [
        PathBuf::from(format!("{s}.hatchzip")),
        PathBuf::from(format!("{s}.hatchzip-list")),
        PathBuf::from(format!("{s}.hatchzip-index")),
    ]
}

fn assert_packed(dir: &Path, source: &Path, files: &Files) -> PathBuf {
    let target = dir.join("game.7z");
    assert!(target.is_file(), "the archive should exist");
    assert!(!source.exists(), "the folder should be gone");
    for p in pack_side_files(&target) {
        assert!(!p.exists(), "{} should be cleaned up", p.display());
    }
    let out = dir.join("out");
    assert_ok(&common::extract(&target, &out, &[]));
    common::assert_tree(&out, files);
    target
}

/// A folder packs into a 7z that HatchZip itself extracts back to the same
/// tree, byte for byte, with the times and the empty folder intact.
#[test]
fn packs_and_round_trips() {
    let tmp = tempfile::TempDir::new().unwrap();
    let files = sample_files();
    let source = make_tree(tmp.path(), &files);
    let before = mtimes(&source);

    assert_ok(&pack(&source, &[]));
    let target = assert_packed(tmp.path(), &source, &files);

    let out = tmp.path().join("out");
    let after = mtimes(&out);
    assert_eq!(before, after, "mtimes should survive the round trip");
    drop(target);
}

/// Every file goes into the archive under the name it had, so 7-Zip reads
/// the same tree out of it.
#[test]
fn seven_zip_reads_the_archive() {
    let tmp = tempfile::TempDir::new().unwrap();
    let files = sample_files();
    let source = make_tree(tmp.path(), &files);
    assert_ok(&pack(&source, &[]));

    let listed = hatchzip(&["list", tmp.path().join("game.7z").to_str().unwrap()], &[]);
    assert_ok(&listed);
    let text = String::from_utf8_lossy(&listed.stdout).replace('\\', "/");
    for name in files.keys() {
        assert!(
            text.contains(name.strip_prefix("game/").unwrap()),
            "{name} missing from the listing"
        );
    }
}

/// A run killed after its bytes are on disk but before the journal claims
/// them, and one killed after the journal does but before the files are
/// deleted, both resume to the same archive. Small batches make several
/// commits, and every file its own batch, so the lone-file path runs too.
#[test]
fn crashes_and_resumes() {
    for point in ["after-append", "after-journal"] {
        for after in [0, 1, 3] {
            let tmp = tempfile::TempDir::new().unwrap();
            let files = sample_files();
            let source = make_tree(tmp.path(), &files);
            let target = tmp.path().join("game.7z");
            let after = after.to_string();

            let crashed = hatchzip(
                &[
                    "pack",
                    source.to_str().unwrap(),
                    "--keep-free",
                    "0",
                    "--yes",
                ],
                &[
                    ("HATCHZIP_PACK_BATCH", "1000"),
                    ("HATCHZIP_CRASH_AT", point),
                    ("HATCHZIP_CRASH_AFTER", &after),
                ],
            );
            assert!(
                !crashed.status.success(),
                "{point}#{after} should have crashed"
            );
            assert!(target.is_file(), "{point}#{after}: no archive yet");
            for p in pack_side_files(&target) {
                assert!(p.exists(), "{point}#{after}: {} is missing", p.display());
            }

            assert_ok(&pack(&source, &[]));
            assert_packed(tmp.path(), &source, &files);
        }
    }
}

/// A pack stopped partway refuses to be resumed as a pack of a different
/// folder, since it can only finish the one it began.
#[test]
fn a_half_pack_refuses_another_folder() {
    let tmp = tempfile::TempDir::new().unwrap();
    let files = sample_files();
    let source = make_tree(tmp.path(), &files);
    let elsewhere = tmp.path().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    let other = make_tree(&elsewhere, &files);

    let crashed = hatchzip(
        &[
            "pack",
            source.to_str().unwrap(),
            "--keep-free",
            "0",
            "--yes",
        ],
        &[
            ("HATCHZIP_PACK_BATCH", "1000"),
            ("HATCHZIP_CRASH_AT", "after-journal"),
            ("HATCHZIP_CRASH_AFTER", "1"),
        ],
    );
    assert!(!crashed.status.success());

    let refused = hatchzip(
        &[
            "pack",
            other.to_str().unwrap(),
            "-o",
            tmp.path().join("game.7z").to_str().unwrap(),
            "--keep-free",
            "0",
            "--yes",
        ],
        &[],
    );
    assert!(!refused.status.success());
    let text = String::from_utf8_lossy(&refused.stderr);
    assert!(
        text.contains("belongs to"),
        "the refusal should say whose it is: {text}"
    );

    assert_ok(&pack(&source, &[]));
    assert_packed(tmp.path(), &source, &files);
}

/// Extracting a half-packed archive refuses and names the pack it belongs
/// to, since the archive is not a 7z until the pack finishes.
#[test]
fn extraction_refuses_a_half_packed_archive() {
    let tmp = tempfile::TempDir::new().unwrap();
    let files = sample_files();
    let source = make_tree(tmp.path(), &files);
    let target = tmp.path().join("game.7z");

    let crashed = hatchzip(
        &[
            "pack",
            source.to_str().unwrap(),
            "--keep-free",
            "0",
            "--yes",
        ],
        &[
            ("HATCHZIP_PACK_BATCH", "1000"),
            ("HATCHZIP_CRASH_AT", "after-journal"),
            ("HATCHZIP_CRASH_AFTER", "1"),
        ],
    );
    assert!(!crashed.status.success());

    let refused = hatchzip(
        &[
            "extract",
            target.to_str().unwrap(),
            "--keep-free",
            "0",
            "--yes",
        ],
        &[],
    );
    assert!(!refused.status.success());
    let text = String::from_utf8_lossy(&refused.stderr);
    assert!(
        text.contains("pack"),
        "the refusal should say it is a pack in progress: {text}"
    );

    assert_ok(&pack(&source, &[]));
    assert_packed(tmp.path(), &source, &files);
}

/// A batch is only what sits in memory: a file larger than one batch is
/// streamed straight into the archive, so plenty of disk is all it needs.
#[test]
fn a_file_larger_than_a_batch_packs_when_the_disk_has_room() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut files = Files::new();
    files.insert("game/data/big.pak".into(), common::noise(2_000_000, 9));
    let source = make_tree(tmp.path(), &files);

    let packed = hatchzip(
        &[
            "pack",
            source.to_str().unwrap(),
            "--keep-free",
            "0",
            "--yes",
        ],
        &[("HATCHZIP_PACK_BATCH", "1000")],
    );
    assert_ok(&packed);
    assert_packed(tmp.path(), &source, &files);
}

/// A disk too small for the largest file refuses before anything is
/// touched: the folder and its files are all still there.
#[test]
fn refuses_when_space_runs_out() {
    let tmp = tempfile::TempDir::new().unwrap();
    let files = sample_files();
    let source = make_tree(tmp.path(), &files);

    let refused = hatchzip(
        &[
            "pack",
            source.to_str().unwrap(),
            "--keep-free",
            "0",
            "--yes",
        ],
        &[("HATCHZIP_FREE", "1000")],
    );
    assert_eq!(refused.status.code(), Some(3));
    let text = String::from_utf8_lossy(&refused.stderr);
    assert!(text.contains("free"), "should say why: {text}");
    assert!(source.join("game/data/big.pak").is_file());
}

/// An archive already at the target is never gone into, and nothing is
/// packed.
#[test]
fn refuses_an_existing_target() {
    let tmp = tempfile::TempDir::new().unwrap();
    let files = sample_files();
    let source = make_tree(tmp.path(), &files);
    let target = tmp.path().join("game.7z");
    fs::write(&target, b"not ours").unwrap();

    let refused = pack(&source, &[]);
    assert!(!refused.status.success());
    let text = String::from_utf8_lossy(&refused.stderr);
    assert!(text.contains("already exists"), "should say why: {text}");
    assert_eq!(fs::read(&target).unwrap(), b"not ours");
    assert!(source.join("game/data/big.pak").is_file());
}

/// What a stopped start of a pack left behind, with no journal to name it,
/// is the pack's own: the next run takes it back rather than refuse.
#[test]
fn recovers_its_own_abandoned_start() {
    let tmp = tempfile::TempDir::new().unwrap();
    let files = sample_files();
    let source = make_tree(tmp.path(), &files);
    let target = tmp.path().join("game.7z");
    fs::write(
        &target,
        b"7z\xBC\xAF\x27\x1C\x00\x04"
            .iter()
            .chain(std::iter::repeat_n(&0, 24))
            .cloned()
            .collect::<Vec<u8>>(),
    )
    .unwrap();
    fs::write(tmp.path().join("game.7z.hatchzip-list"), b"stale").unwrap();

    assert_ok(&pack(&source, &[]));
    assert_packed(tmp.path(), &source, &files);
}

/// The archive goes where it is told, under any name.
#[test]
fn packs_into_a_given_target() {
    let tmp = tempfile::TempDir::new().unwrap();
    let files = sample_files();
    let source = make_tree(tmp.path(), &files);
    let target = tmp.path().join("elsewhere/game-copy.7z");
    fs::create_dir_all(target.parent().unwrap()).unwrap();

    assert_ok(&hatchzip(
        &[
            "pack",
            source.to_str().unwrap(),
            "-o",
            target.to_str().unwrap(),
            "--keep-free",
            "0",
            "--yes",
        ],
        &[],
    ));
    assert!(target.is_file());
    assert!(!source.exists());
    for p in pack_side_files(&target) {
        assert!(!p.exists(), "{} should be cleaned up", p.display());
    }
    let out = tmp.path().join("out");
    assert_ok(&common::extract(&target, &out, &[]));
    common::assert_tree(&out, &files);
}

/// A folder with nothing but empty folders and files packs and extracts,
/// with no data blocks at all.
#[test]
fn packs_a_tree_of_nothing() {
    let tmp = tempfile::TempDir::new().unwrap();
    let source = tmp.path().join("empty-game");
    fs::create_dir_all(source.join("saves")).unwrap();
    fs::write(source.join("empty.txt"), b"").unwrap();

    assert_ok(&pack(&source, &[]));
    assert!(!source.exists());

    let target = tmp.path().join("empty-game.7z");
    let out = tmp.path().join("out");
    assert_ok(&common::extract(&target, &out, &[]));
    assert!(out.join("empty.txt").is_file());
    assert!(out.join("saves").is_dir());
}

/// Links are refused before anything is packed: following one would pack
/// where it points, and a file with several names would be counted twice
/// and freed once.
#[cfg(unix)]
#[test]
fn refuses_links() {
    use std::os::unix::fs::symlink;

    let tmp = tempfile::TempDir::new().unwrap();
    let files = sample_files();
    let source = make_tree(tmp.path(), &files);
    symlink("/etc/hostname", source.join("game/link")).unwrap();
    fs::hard_link(
        source.join("game/data/big.pak"),
        source.join("game/data/twin.pak"),
    )
    .unwrap();

    let refused = pack(&source, &[]);
    assert!(!refused.status.success());
    let text = String::from_utf8_lossy(&refused.stderr);
    assert!(
        text.contains("link") || text.contains("another name"),
        "should say why: {text}"
    );
    assert!(source.join("game/data/big.pak").is_file());
    assert!(!tmp.path().join("game.7z").exists());
}

/// Files that appear while the pack runs are left in place and named,
/// since the archive holds only what the walk saw.
#[test]
fn leaves_files_that_appeared() {
    let tmp = tempfile::TempDir::new().unwrap();
    let files = sample_files();
    let source = make_tree(tmp.path(), &files);

    // The pack stops after one commit, a file appears, and the resume
    // finishes without taking it.
    let crashed = hatchzip(
        &[
            "pack",
            source.to_str().unwrap(),
            "--keep-free",
            "0",
            "--yes",
        ],
        &[
            ("HATCHZIP_PACK_BATCH", "1000"),
            ("HATCHZIP_CRASH_AT", "after-journal"),
            ("HATCHZIP_CRASH_AFTER", "1"),
        ],
    );
    assert!(!crashed.status.success());
    let appeared = source.join("game/late.txt");
    fs::write(&appeared, b"too late").unwrap();

    let done = pack(&source, &[]);
    assert_ok(&done);
    assert!(appeared.is_file(), "the late file should still be there");
    assert!(source.join("game").is_dir(), "its folder stays too");
    let text = String::from_utf8_lossy(&done.stderr);
    assert!(
        text.contains("stayed"),
        "the warning should name what stayed: {text}"
    );
}
