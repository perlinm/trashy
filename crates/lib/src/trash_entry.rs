use std::borrow::Cow;
use std::io;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use log::warn;
use snafu::{ResultExt, Snafu, OptionExt, ensure};

use crate::percent_path::{self, PercentPath};
use crate::trash_info::{self, TrashInfo};
use crate::utils::{self, convert_to_str, move_path, read_dir_path, remove_path};
use crate::{TRASH_DIR, TRASH_FILE_DIR, TRASH_INFO_DIR, TRASH_INFO_EXT};

/// Represents an entry in the trash directory. Includes the file path and the trash info path.
#[derive(Debug)]
pub struct TrashEntry {
    info_path: PathBuf,
    file_path: PathBuf,
}

#[derive(Snafu, Debug)]
pub enum Error {
    #[snafu(display("The trash info path {} does not exist", path.display()))]
    ExistsInfoPath { path: PathBuf },

    #[snafu(display("The trash file path {} does not exist", path.display()))]
    ExistsFilePath { path: PathBuf },

    #[snafu(display("There is not a file name for path {}", path.display()))]
    NoFileName { path: PathBuf },

    #[snafu(context(false))]
    DecodePercentPath { source: percent_path::Error },

    #[snafu(display("Failed to read entries from path {}: {}", path.display(), source))]
    ReadDirPath { source: io::Error, path: PathBuf },

    #[snafu(display("The path {} was not found", path.display()))]
    NotFound { path: PathBuf },

    #[snafu(context(false))]
    ParseTrashInfo { source: trash_info::Error },

    #[snafu(context(false))]
    #[snafu(display("Utils error: {}", source))]
    Utils { source: utils::Error },

    #[snafu(display("Moving path {} into the trash directory when it is already there", path.display()))]
    CreationInTrash { path: PathBuf },
}

type Result<T, E = Error> = ::std::result::Result<T, E>;

/// Represents an entry in the trash. Holds the paths of the file and the .trashinfo file that make
/// up one TrashEntry
impl TrashEntry {
    /// Constructor for TrashEntry struct. Fails if the name or path is not a valid TrashEntry
    /// name.
    pub fn new(name: impl AsRef<Path>) -> Result<TrashEntry> {
        let name = name.as_ref();
        let name = name.file_name().context(NoFileName { path: name })?;

        // the info path is the name and the info dir with an extension
        let mut info_path = TRASH_INFO_DIR.join(name);
        info_path.set_extension(&TRASH_INFO_EXT);

        // same for file path
        let file_path = TRASH_FILE_DIR.join(name);

        // create the trash entry struct and check if it is valid
        let trash_entry = TrashEntry {
            info_path,
            file_path,
        };
        trash_entry.is_valid()?;

        Ok(trash_entry)
    }

    pub fn info_path(&self) -> &Path {
        &self.info_path
    }

    pub fn file_path(&self) -> &Path {
        &self.file_path
    }

    /// Create a trash entry from a path not in the trash directory. Will move the path to the
    /// trash files directory. Can fail if attempting to move something already in the trash
    /// directory
    pub(crate) fn create(path: impl AsRef<Path>, existing: &[TrashEntry]) -> Result<TrashEntry> {
        let path = path.as_ref();

        if in_trash_dir(path) {
            CreationInTrash { path }.fail()?;
        }
        let name = find_name_trash_entry(path, existing)?;
        let name = name.as_ref();

        // create the trash info file
        TrashInfo::new(PercentPath::from_path(path)?, None).save(name)?;

        // move the path the the trash file dir
        move_path(path, TRASH_FILE_DIR.join(name))?;

        // return the trash entry that was created
        Ok(TrashEntry::new(name)?)
    }

    /// Check if paths in TrashEntry is valid.
    pub fn is_valid(&self) -> Result<()> {
        ensure!(
            self.info_path.exists(),
            ExistsInfoPath {
                path: &self.info_path
            }
        );
        ensure!(
            self.file_path.exists(),
            ExistsFilePath {
                path: &self.file_path
            }
        );
        Ok(())
    }

    /// Restores the trash entry
    pub fn restore(self) -> Result<()> {
        self.is_valid()?;
        let trash_info = TrashInfo::parse_from_path(self.info_path)?;
        let original_path = trash_info.percent_path().decoded()?;

        move_path(&self.file_path, original_path.as_ref())?;
        remove_path(&self.file_path)?;
        Ok(())
    }

    /// Removes the trash_entry
    pub fn remove(self) -> Result<()> {
        self.is_valid()?;
        remove_path(self.info_path)?;
        remove_path(self.file_path)?;
        Ok(())
    }

    pub fn parse_trash_info(&self) -> Result<TrashInfo, trash_info::Error> {
        TrashInfo::parse_from_path(&self.info_path)
    }
}

pub fn read_dir_trash_entries() -> Result<impl Iterator<Item = TrashEntry>> {
    let res = read_dir_path(&TRASH_FILE_DIR);
    if let Err(ref e) = res {
        if e.kind() == ErrorKind::NotFound {
            NotFound { path: &*TRASH_FILE_DIR }.fail()?;
        }
    }
    let iter = res.context(ReadDirPath { path: &*TRASH_FILE_DIR })?
        .map(|path| TrashEntry::new(path))
        .inspect(|res| {
            if let Some(e) = res.as_ref().err() {
                warn!("{}", e);
            }
        })
        .filter_map(Result::ok);
    Ok(iter)
}

fn find_name_trash_entry<'a, T>(path: &'a T, existing: &[TrashEntry]) -> Result<Cow<'a, str>>
where
    T: AsRef<Path> + ?Sized,
{
    let path = convert_to_str(path.as_ref())?;
    let existing_names: Vec<_> = existing
        .into_iter()
        .map(|trash_entry| {
            trash_entry
                .file_path
                .file_name()
                .expect("Need to have file name")
        })
        .map(|p| convert_to_str(p.as_ref()).unwrap()) // TODO: fix unwrap
        .collect();

    let res = find_name(path, &existing_names)?;
    Ok(res)
}

fn find_name<'a, T>(path: &'a T, existing: &[&str]) -> Result<Cow<'a, str>>
where
    T: AsRef<Path> + ?Sized,
{
    let name = path.as_ref().file_name().expect("Must have filename");
    let name = convert_to_str(name.as_ref())?;

    let res = (0..1000)
        .map(|num| {
            if num == 0 {
                Cow::Borrowed(name)
            } else {
                Cow::Owned(format!("{}_{}", name, num))
            }
        })
        .find(|new_path| !contains_contains(existing, new_path))
        .expect("BUG: timeout is too small for find_name");
    Ok(res)
}

fn contains_contains(slice: &[&str], item: &str) -> bool {
    slice.into_iter().any(|s| s.contains(item))
}

fn in_trash_dir(path: impl AsRef<Path>) -> bool {
    path.as_ref()
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p == *TRASH_DIR)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::{contains_all_elements, temp_file_iter};
    use anyhow::{Context, Result};
    use std::io::Write;
    use tempfile::{tempdir, NamedTempFile};

    #[test]
    fn find_names_test() -> Result<()> {
        assert_eq!(find_name("vim.log", &["vim.log", "vim.log2"])?, "vim.log_1");
        Ok(())
    }

    #[test]
    fn find_names_test_2_test() -> Result<()> {
        assert_eq!(
            find_name("vim.log", &["vim.log", "vim.log_1"])?,
            "vim.log_2"
        );
        Ok(())
    }

    #[test]
    fn find_names_test_none_test() -> Result<()> {
        assert_eq!(find_name("vim.log", &[""])?, "vim.log");
        Ok(())
    }

    #[test]
    fn find_names_already_in_dir_test() -> Result<()> {
        assert_eq!(
            find_name("/tmp/vim.log", &["vim.log", "vim.log_1"])?,
            "vim.log_2"
        );
        Ok(())
    }

    #[test]
    fn in_trash_dir_test() {
        assert_eq!(in_trash_dir("/home/brian/.local/share/Trash"), false);
    }

    #[test]
    fn in_trash_dir_files() {
        assert_eq!(
            in_trash_dir("/home/brian/.local/share/Trash/files/a_file"),
            true
        );
    }

    #[test]
    fn ins_trash_dir_info_test() {
        assert_eq!(
            in_trash_dir("/home/brian/.local/share/Trash/info/another_file"),
            true
        );
    }

    #[test]
    fn in_trash_dir3_test() {
        assert_eq!(in_trash_dir("/home/brian/.local/share/Trash/info"), false);
    }

    #[test]
    fn in_trash_dir4_test() {
        assert_eq!(in_trash_dir("/home/brian/.local/share/Trash/files"), false);
    }

    #[ignore]
    #[test]
    fn read_dir_trash_entries_test_none() -> Result<()> {
        const TEMP_FILE_AMOUNT: usize = 20;

        let _: Vec<_> = temp_file_iter(&*TRASH_FILE_DIR, TEMP_FILE_AMOUNT).collect();

        let trash_entries: Vec<_> = read_dir_trash_entries()?.collect();
        assert_eq!(trash_entries.len(), 0);

        Ok(())
    }

    #[ignore]
    #[test]
    fn test_temp_file_iter() {
        let _: Vec<_> = temp_file_iter(&*TRASH_FILE_DIR, 20)
            .map(|temp| temp.keep().expect("Failed to keep tempfile"))
            .collect();
    }

    #[ignore]
    #[test]
    fn read_dir_trash_entries_test_some_test() -> Result<()> {
        const TEMP_FILE_AMOUNT: usize = 20;

        let temp_files: Vec<_> = temp_file_iter(&*TRASH_FILE_DIR, TEMP_FILE_AMOUNT).collect();
        let temp_file_paths: Vec<_> = temp_files.iter().map(|file| file.path()).collect();

        let trash_entries: Vec<_> = read_dir_trash_entries()?.collect();
        let trash_entry_paths: Vec<&Path> = trash_entries
            .iter()
            .map(|tent| tent.file_path())
            .map(|path| path.file_name().expect("Must have file name"))
            .map(|os_s| os_s.as_ref())
            .collect();

        contains_all_elements(trash_entry_paths, temp_file_paths);

        Ok(())
    }

    #[ignore]
    #[test]
    fn trash_entry_create_test() -> Result<()> {
        let mut tempfile = NamedTempFile::new()?;
        tempfile.write_all(b"this is for the trash_entry_create_test")?;
        let existing: Vec<_> = read_dir_trash_entries()?.collect();
        let trash_entry = TrashEntry::create(tempfile.path(), &existing)?;

        Ok(())
    }
}
