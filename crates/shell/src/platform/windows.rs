//! Explorer's classic context menu, through per-user registry keys under
//! `HKCU\Software\Classes\SystemFileAssociations`. They add a HatchZip
//! submenu to each archive type without taking over its file association.
//! Windows 11 shows the classic menu under "Show more options"; a top-level
//! entry there needs a packaged shell extension.

use std::path::Path;

use anyhow::{Context, Result};
use winreg::RegKey;
use winreg::enums::HKEY_CURRENT_USER;

use crate::{ACTIONS, ALL_EXTENSIONS, Action, Dirs, EXTENSIONS, Surface};

const BASE: &str = r"Software\Classes\SystemFileAssociations";
const DIRECTORY: &str = r"Software\Classes\Directory";

/// The folder menus beyond the one on the directory itself: where their key
/// lives, how their command names the clicked folder, and what that place is
/// called in the registration report. The background menu has no selection,
/// so Explorer fills in the open folder through `%V`.
const FOLDER_MENUS: &[(&str, &str, &str)] = &[
    (r"shell", "\"%1\"", "folders"),
    (r"Background\shell", "%V", "the folder background"),
];

fn menu_key(ext: &str) -> String {
    format!(r"{BASE}\.{ext}\shell\HatchZip")
}

/// Writes the HatchZip submenu at `path`, one numbered item per action, each
/// command passing `target` where Explorer puts the clicked item.
fn submenu(hkcu: &RegKey, path: &str, actions: &[&Action], target: &str, exe: &str) -> Result<()> {
    // Items are numbered in their key names, so an older registration
    // whose numbers have shifted would leave duplicates behind.
    let _ = hkcu.delete_subkey_all(path);
    let (menu, _) = hkcu
        .create_subkey(path)
        .with_context(|| format!("creating HKCU\\{path}"))?;
    menu.set_value("MUIVerb", &"HatchZip")?;
    menu.set_value("Icon", &format!("\"{exe}\",0"))?;
    // An empty SubCommands value turns the entry into a submenu built
    // from its own `shell` key.
    menu.set_value("SubCommands", &"")?;
    for (i, a) in actions.iter().enumerate() {
        // Explorer orders submenu items by key name.
        let (item, _) = menu.create_subkey(format!(r"shell\{:02}{}", i + 1, a.id))?;
        item.set_value("MUIVerb", &a.label)?;
        let (command, _) = item.create_subkey("command")?;
        command.set_value("", &format!("\"{exe}\" {} {target}", a.flag))?;
    }
    Ok(())
}

pub fn current_dirs() -> Result<Dirs> {
    Ok(Dirs {
        data: std::env::temp_dir(),
    })
}

pub fn register(_dirs: &Dirs, gui: &Path) -> Result<Vec<String>> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let exe = gui.to_string_lossy();
    let mut written = Vec::new();
    let archive: Vec<&Action> = ACTIONS
        .iter()
        .filter(|a| a.on == Surface::Archive)
        .collect();
    let folder: Vec<&Action> = ACTIONS.iter().filter(|a| a.on == Surface::Folder).collect();
    for ext in EXTENSIONS {
        submenu(&hkcu, &menu_key(ext), &archive, "\"%1\"", &exe)?;
        written.push(format!("Explorer: .{ext} files"));
    }
    for (leaf, target, place) in FOLDER_MENUS {
        let path = format!(r"{DIRECTORY}\{leaf}\HatchZip");
        submenu(&hkcu, &path, &folder, target, &exe)?;
        written.push(format!("Explorer: {place}"));
    }
    Ok(written)
}

pub fn unregister(_dirs: &Dirs) -> Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let mut keys: Vec<String> = ALL_EXTENSIONS.iter().map(|ext| menu_key(ext)).collect();
    keys.extend(
        FOLDER_MENUS
            .iter()
            .map(|(leaf, _, _)| format!(r"{DIRECTORY}\{leaf}\HatchZip")),
    );
    for path in keys {
        match hkcu.delete_subkey_all(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(e).with_context(|| format!("removing HKCU\\{path}"));
            }
            _ => {}
        }
    }
    Ok(())
}

pub fn is_registered(_dirs: &Dirs) -> bool {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(menu_key(EXTENSIONS[0]))
        .is_ok()
}
