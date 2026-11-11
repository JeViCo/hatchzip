//! The command line. File manager menus call these exact forms, so changing
//! one breaks every installed right-click entry. Each task form takes one
//! archive or several, which run one after another; packing takes a single
//! folder.

use std::path::PathBuf;

use clap::{ArgGroup, Parser};

#[derive(Parser, Debug)]
#[command(
    name = "hatchzip-gui",
    version,
    about = "Extract large archives in place, removing the archive progressively.",
    group(ArgGroup::new("what").args([
        "archive",
        "extract_here",
        "extract_to",
        "extract_files",
        "test",
        "pack_here",
        "register_shell",
        "unregister_shell",
    ])),
    group(ArgGroup::new("extracting").args(["extract_here", "extract_to", "extract_files"]))
)]
pub struct Args {
    /// The archive to open, or any part of a split archive
    pub archive: Option<PathBuf>,
    /// Extract into the folder each archive is in
    #[arg(long, value_name = "ARCHIVE", num_args = 1..)]
    pub extract_here: Option<Vec<PathBuf>>,
    /// Extract into a new folder named after each archive
    #[arg(long, value_name = "ARCHIVE", num_args = 1..)]
    pub extract_to: Option<Vec<PathBuf>>,
    /// Ask for a folder, then extract every archive into it
    #[arg(long, value_name = "ARCHIVE", num_args = 1..)]
    pub extract_files: Option<Vec<PathBuf>>,
    /// Check every file for damage, without writing anything
    #[arg(long, value_name = "ARCHIVE", num_args = 1..)]
    pub test: Option<Vec<PathBuf>>,
    /// Pack the folder into an archive beside it, removing the folder
    /// progressively
    #[arg(long, value_name = "FOLDER", num_args = 1)]
    pub pack_here: Option<PathBuf>,
    /// With an extract option: keep the archive, removing nothing
    #[arg(long, requires = "extracting")]
    pub keep_archive: bool,
    /// Add HatchZip to the file manager's right-click menu, then exit
    #[arg(long)]
    pub register_shell: bool,
    /// Remove HatchZip from the file manager's right-click menu, then exit
    #[arg(long)]
    pub unregister_shell: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskKind {
    ExtractHere,
    ExtractTo,
    /// Into a folder the person picks, which is asked for first when it is
    /// `None`.
    ExtractFiles(Option<PathBuf>),
    /// Pack one folder into an archive beside it. The folder itself rides
    /// in `archives`, like every other kind's paths.
    PackHere,
    Test,
}

/// What a right-click asked for.
#[derive(Debug, PartialEq, Eq)]
pub struct TaskArgs {
    pub kind: TaskKind,
    /// One or more, done in this order.
    pub archives: Vec<PathBuf>,
    /// Extract without removing the archives.
    pub keep_archive: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Launch {
    /// The main window, empty or with an archive open.
    Main(Option<PathBuf>),
    /// A small window that runs one task, started from a right-click.
    Task(TaskArgs),
    /// Installers run these; no window opens.
    RegisterShell,
    UnregisterShell,
}

impl Args {
    pub fn launch(self) -> Launch {
        let task = |kind, archives| {
            Launch::Task(TaskArgs {
                kind,
                archives,
                keep_archive: self.keep_archive,
            })
        };
        if self.register_shell {
            Launch::RegisterShell
        } else if self.unregister_shell {
            Launch::UnregisterShell
        } else if let Some(a) = self.extract_here {
            task(TaskKind::ExtractHere, a)
        } else if let Some(a) = self.extract_to {
            task(TaskKind::ExtractTo, a)
        } else if let Some(a) = self.extract_files {
            task(TaskKind::ExtractFiles(None), a)
        } else if let Some(a) = self.test {
            task(TaskKind::Test, a)
        } else if let Some(a) = self.pack_here {
            task(TaskKind::PackHere, vec![a])
        } else {
            Launch::Main(self.archive)
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    fn parse(args: &[&str]) -> Result<Launch, clap::Error> {
        let mut full = vec!["hatchzip-gui"];
        full.extend_from_slice(args);
        Args::try_parse_from(full).map(Args::launch)
    }

    #[test]
    fn command_line_is_well_formed() {
        Args::command().debug_assert();
    }

    fn task(kind: TaskKind, archives: &[&str], keep_archive: bool) -> Launch {
        Launch::Task(TaskArgs {
            kind,
            archives: archives.iter().map(PathBuf::from).collect(),
            keep_archive,
        })
    }

    #[test]
    fn every_documented_form_parses() {
        assert_eq!(parse(&[]).unwrap(), Launch::Main(None));
        assert_eq!(
            parse(&["game.zip"]).unwrap(),
            Launch::Main(Some("game.zip".into()))
        );
        assert_eq!(
            parse(&["--extract-here", "a b/game.7z.001"]).unwrap(),
            task(TaskKind::ExtractHere, &["a b/game.7z.001"], false)
        );
        assert_eq!(
            parse(&["--extract-to", "game.zip"]).unwrap(),
            task(TaskKind::ExtractTo, &["game.zip"], false)
        );
        assert_eq!(
            parse(&["--extract-files", "game.zip"]).unwrap(),
            task(TaskKind::ExtractFiles(None), &["game.zip"], false)
        );
        assert_eq!(
            parse(&["--test", "game.tar"]).unwrap(),
            task(TaskKind::Test, &["game.tar"], false)
        );
        assert_eq!(
            parse(&["--pack-here", "game"]).unwrap(),
            task(TaskKind::PackHere, &["game"], false)
        );
        assert_eq!(
            parse(&["--pack-here=game"]).unwrap(),
            task(TaskKind::PackHere, &["game"], false)
        );
        assert_eq!(
            parse(&["--extract-here=game.zip"]).unwrap(),
            task(TaskKind::ExtractHere, &["game.zip"], false)
        );
    }

    /// What the "keep archive" menu entries run, and what file managers
    /// that hand over every selected file at once pass.
    #[test]
    fn keeping_and_several_archives_parse() {
        assert_eq!(
            parse(&["--keep-archive", "--extract-here", "game.zip"]).unwrap(),
            task(TaskKind::ExtractHere, &["game.zip"], true)
        );
        assert_eq!(
            parse(&["--extract-to", "game.zip", "--keep-archive"]).unwrap(),
            task(TaskKind::ExtractTo, &["game.zip"], true)
        );
        assert_eq!(
            parse(&["--keep-archive", "--extract-here", "a.zip", "b c.7z.001"]).unwrap(),
            task(TaskKind::ExtractHere, &["a.zip", "b c.7z.001"], true)
        );
        assert_eq!(
            parse(&["--test", "a.zip", "b.tar"]).unwrap(),
            task(TaskKind::Test, &["a.zip", "b.tar"], false)
        );
        // Keeping only means something when extracting.
        assert!(parse(&["--keep-archive"]).is_err());
        assert!(parse(&["--keep-archive", "--test", "a.zip"]).is_err());
        assert!(parse(&["--keep-archive", "a.zip"]).is_err());
        assert!(parse(&["--keep-archive", "--pack-here", "a"]).is_err());
    }

    #[test]
    fn installers_can_add_and_remove_the_menu() {
        assert_eq!(parse(&["--register-shell"]).unwrap(), Launch::RegisterShell);
        assert_eq!(
            parse(&["--unregister-shell"]).unwrap(),
            Launch::UnregisterShell
        );
        assert!(parse(&["--register-shell", "--unregister-shell"]).is_err());
        assert!(parse(&["--register-shell", "a.zip"]).is_err());
    }

    #[test]
    fn one_thing_at_a_time() {
        assert!(parse(&["--test", "a.zip", "--extract-to", "b.zip"]).is_err());
        assert!(parse(&["a.zip", "--extract-here", "b.zip"]).is_err());
        assert!(parse(&["--extract-here"]).is_err());
        assert!(parse(&["a.zip", "b.zip"]).is_err());
        // Packing takes exactly one folder.
        assert!(parse(&["--pack-here"]).is_err());
        assert!(parse(&["--pack-here", "a", "b"]).is_err());
        assert!(parse(&["--pack-here", "a", "--test", "b.zip"]).is_err());
        assert!(parse(&["a", "--pack-here", "b"]).is_err());
    }
}
