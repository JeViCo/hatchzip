//! Dolphin (KDE) service menu, GNOME Files scripts and Nemo actions. Thunar
//! keeps its actions in one shared XML file, which is not edited here.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

use crate::{ACTIONS, Action, Dirs, EXTENSIONS, Surface};

#[cfg(not(feature = "rar"))]
const MIME_TYPES: &str = "application/zip;application/x-zip-compressed;application/x-7z-compressed;\
                          application/x-tar;application/x-compressed-tar;\
                          application/x-xz-compressed-tar;";
#[cfg(feature = "rar")]
const MIME_TYPES: &str = "application/zip;application/x-zip-compressed;application/x-7z-compressed;\
                          application/x-tar;application/x-compressed-tar;\
                          application/x-xz-compressed-tar;application/vnd.rar;\
                          application/x-rar;application/x-rar-compressed;";
const MARK: &str = "Added by HatchZip; `hatchzip-gui --unregister-shell` removes it.";

pub fn current_dirs() -> Result<Dirs> {
    let data = match std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => std::env::home_dir()
            .ok_or_else(|| anyhow!("no home folder to install the menu into"))?
            .join(".local/share"),
    };
    Ok(Dirs { data })
}

fn kde_file(dirs: &Dirs) -> PathBuf {
    dirs.data.join("kio/servicemenus/hatchzip.desktop")
}

/// A second servicemenu file, because Dolphin matches a menu against the
/// selection's MIME type: actions for folders can never live beside actions
/// for archives.
fn kde_folder_file(dirs: &Dirs) -> PathBuf {
    dirs.data.join("kio/servicemenus/hatchzip-folders.desktop")
}

fn nautilus_file(dirs: &Dirs, label: &str) -> PathBuf {
    dirs.data
        .join("nautilus/scripts")
        .join(format!("HatchZip - {label}"))
}

fn nemo_file(dirs: &Dirs, id: &str) -> PathBuf {
    dirs.data
        .join("nemo/actions")
        .join(format!("hatchzip-{id}.nemo_action"))
}

/// Desktop entry `Exec` keys want the program quoted, with `"`, `` ` ``, `$`
/// and `\` escaped.
fn exec_quote(path: &Path) -> String {
    let mut out = String::from("\"");
    for c in path.to_string_lossy().chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

fn sh_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

fn write(path: &Path, contents: &str, executable: bool) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, contents).with_context(|| format!("writing {}", path.display()))?;
    // Dolphin ignores service menus in a user's folder unless they are
    // executable, and GNOME Files only runs executable scripts.
    let mode = if executable { 0o755 } else { 0o644 };
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

/// One Dolphin servicemenu: what it matches plus the actions it offers.
fn kde_desktop(mime_types: &str, icon: &str, actions: &[&Action], exec: &str) -> String {
    let mut kde = format!(
        "# {MARK}\n[Desktop Entry]\nType=Service\nMimeType={mime_types}\nActions={}\n\
         X-KDE-Submenu=HatchZip\nIcon={icon}\n",
        actions.iter().map(|a| a.id).collect::<Vec<_>>().join(";") + ";"
    );
    // Every selected path goes to one window, which works through them
    // one after another, checking the space for each just before it starts.
    for a in actions {
        kde.push_str(&format!(
            "\n[Desktop Action {}]\nName={}\nIcon={icon}\nExec={exec} {} %F\n",
            a.id, a.label, a.flag
        ));
    }
    kde
}

pub fn register(dirs: &Dirs, gui: &Path) -> Result<Vec<String>> {
    let exec = exec_quote(gui);
    let archive: Vec<&Action> = ACTIONS
        .iter()
        .filter(|a| a.on == Surface::Archive)
        .collect();
    let folder: Vec<&Action> = ACTIONS.iter().filter(|a| a.on == Surface::Folder).collect();

    write(
        &kde_file(dirs),
        &kde_desktop(MIME_TYPES, "archive-extract", &archive, &exec),
        true,
    )?;
    write(
        &kde_folder_file(dirs),
        &kde_desktop("inode/directory;", "archive-insert", &folder, &exec),
        true,
    )?;

    // GNOME Files shows scripts on any selection, files and folders alike;
    // it has nothing like a MIME filter, so the pack script shows on
    // archives too and the GUI refuses them.
    for a in ACTIONS {
        // GNOME Files hands over every selected path, one per line. Split on
        // newlines only, with globbing off, so spaces and stars in names
        // survive.
        let script = format!(
            "#!/bin/sh\n# {MARK}\n[ -n \"$NAUTILUS_SCRIPT_SELECTED_FILE_PATHS\" ] || exit 0\n\
             IFS='\n'\nset -f\nexec {} {} $NAUTILUS_SCRIPT_SELECTED_FILE_PATHS\n",
            sh_quote(gui),
            a.flag
        );
        write(&nautilus_file(dirs, a.label), &script, true)?;

        let nemo = match a.on {
            Surface::Archive => format!(
                "# {MARK}\n[Nemo Action]\nName={} with HatchZip\nIcon-Name=archive-extract\n\
                 Exec={exec} {} %F\nSelection=notnone\nExtensions={};\n",
                a.label,
                a.flag,
                EXTENSIONS.join(";")
            ),
            // `dir` is Nemo's name for a directory selection.
            Surface::Folder => format!(
                "# {MARK}\n[Nemo Action]\nName={} with HatchZip\nIcon-Name=archive-insert\n\
                 Exec={exec} {} %F\nSelection=notnone\nExtensions=dir;\n",
                a.label, a.flag
            ),
        };
        write(&nemo_file(dirs, a.id), &nemo, false)?;
    }

    Ok(vec![
        format!("Dolphin: {}", kde_file(dirs).display()),
        format!(
            "GNOME Files: {}",
            dirs.data.join("nautilus/scripts").display()
        ),
        format!("Nemo: {}", dirs.data.join("nemo/actions").display()),
    ])
}

pub fn unregister(dirs: &Dirs) -> Result<()> {
    let mut paths = vec![kde_file(dirs), kde_folder_file(dirs)];
    for a in ACTIONS {
        paths.push(nautilus_file(dirs, a.label));
        paths.push(nemo_file(dirs, a.id));
    }
    for p in paths {
        match fs::remove_file(&p) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => {
                return Err(e).with_context(|| format!("removing {}", p.display()));
            }
            _ => {}
        }
    }
    Ok(())
}

pub fn is_registered(dirs: &Dirs) -> bool {
    kde_file(dirs).exists() || kde_folder_file(dirs).exists()
}
