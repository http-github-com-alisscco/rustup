//! A transactional interface to file system operations needed by the
//! installer.
//!
//! Installation or uninstallation of a single component is done
//! within a Transaction, which supports a few simple file system
//! operations. If the Transaction is dropped without committing then
//! it will *attempt* to roll back the transaction.
//!
//! FIXME: This uses ensure_dir_exists in some places but rollback
//! does not remove any dirs created by it.

use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use crate::dist::notifications::*;
use crate::dist::prefix::InstallPrefix;
use crate::dist::temp;
use crate::errors::*;
use crate::utils::utils;

/// A Transaction tracks changes to the file system, allowing them to
/// be rolled back in case of an error. Instead of deleting or
/// overwriting file, the old copies are moved to a temporary
/// folder. If the transaction is rolled back, they will be moved back
/// into place. If the transaction is committed, these files are
/// automatically cleaned up using the temp system.
///
/// All operations that create files will automatically create any
/// intermediate directories in the path to the file if they do not
/// already exist.
///
/// All operations that create files will fail if the destination
/// already exists.
pub struct Transaction<'a> {
    prefix: InstallPrefix,
    changes: Vec<ChangedItem<'a>>,
    temp_cfg: &'a temp::Cfg,
    notify_handler: &'a dyn Fn(Notification<'_>),
    committed: bool,
}

impl<'a> Transaction<'a> {
    pub fn new(
        prefix: InstallPrefix,
        temp_cfg: &'a temp::Cfg,
        notify_handler: &'a dyn Fn(Notification<'_>),
    ) -> Self {
        Transaction {
            prefix,
            changes: Vec::new(),
            temp_cfg,
            notify_handler,
            committed: false,
        }
    }

    /// Commit must be called for all successful transactions. If not
    /// called the transaction will be rolled back on drop.
    pub fn commit(mut self) {
        self.committed = true;
    }

    fn change(&mut self, item: ChangedItem<'a>) {
        self.changes.push(item);
    }

    /// Add a file at a relative path to the install prefix. Returns a
    /// `File` that may be used to subsequently write the
    /// contents.
    pub fn add_file(&mut self, component: &str, relpath: PathBuf) -> Result<File> {
        assert!(relpath.is_relative());
        let (item, file) = ChangedItem::add_file(&self.prefix, component, relpath)?;
        self.change(item);
        Ok(file)
    }

    /// Copy a file to a relative path of the install prefix.
    pub fn copy_file(&mut self, component: &str, relpath: PathBuf, src: &Path) -> Result<()> {
        assert!(relpath.is_relative());
        let item = ChangedItem::copy_file(&self.prefix, component, relpath, src)?;
        self.change(item);
        Ok(())
    }

    /// Recursively copy a directory to a relative path of the install prefix.
    pub fn copy_dir(&mut self, component: &str, relpath: PathBuf, src: &Path) -> Result<()> {
        assert!(relpath.is_relative());
        let item = ChangedItem::copy_dir(&self.prefix, component, relpath, src)?;
        self.change(item);
        Ok(())
    }

    /// Remove a file from a relative path to the install prefix.
    pub fn remove_file(&mut self, component: &str, relpath: PathBuf) -> Result<()> {
        assert!(relpath.is_relative());
        let item = ChangedItem::remove_file(
            &self.prefix,
            component,
            relpath,
            &self.temp_cfg,
            self.notify_handler(),
        )?;
        self.change(item);
        Ok(())
    }

    /// Recursively remove a directory from a relative path of the
    /// install prefix.
    pub fn remove_dir(&mut self, component: &str, relpath: PathBuf) -> Result<()> {
        assert!(relpath.is_relative());
        let item = ChangedItem::remove_dir(
            &self.prefix,
            component,
            relpath,
            &self.temp_cfg,
            self.notify_handler(),
        )?;
        self.change(item);
        Ok(())
    }

    /// Create a new file with string contents at a relative path to
    /// the install prefix.
    pub fn write_file(&mut self, component: &str, relpath: PathBuf, content: String) -> Result<()> {
        assert!(relpath.is_relative());
        let (item, mut file) = ChangedItem::add_file(&self.prefix, component, relpath.clone())?;
        self.change(item);
        utils::write_str(
            "component",
            &mut file,
            &self.prefix.abs_path(&relpath),
            &content,
        )?;
        Ok(())
    }

    /// If the file exists back it up for rollback, otherwise ensure that the path
    /// to it exists so that subsequent calls to `File::create` will succeed.
    ///
    /// This is used for arbitrarily manipulating a file.
    pub fn modify_file(&mut self, relpath: PathBuf) -> Result<()> {
        assert!(relpath.is_relative());
        let item = ChangedItem::modify_file(&self.prefix, relpath, &self.temp_cfg)?;
        self.change(item);
        Ok(())
    }

    /// Move a file to a relative path of the install prefix.
    pub fn move_file(&mut self, component: &str, relpath: PathBuf, src: &Path) -> Result<()> {
        assert!(relpath.is_relative());
        let item =
            ChangedItem::move_file(&self.prefix, component, relpath, src, self.notify_handler())?;
        self.change(item);
        Ok(())
    }

    /// Recursively move a directory to a relative path of the install prefix.
    pub fn move_dir(&mut self, component: &str, relpath: PathBuf, src: &Path) -> Result<()> {
        assert!(relpath.is_relative());
        let item =
            ChangedItem::move_dir(&self.prefix, component, relpath, src, self.notify_handler())?;
        self.change(item);
        Ok(())
    }

    pub fn temp(&self) -> &'a temp::Cfg {
        self.temp_cfg
    }
    pub(crate) fn notify_handler(&self) -> &'a dyn Fn(Notification<'_>) {
        self.notify_handler
    }
}

/// If a Transaction is dropped without being committed, the changes
/// are automatically rolled back.
impl<'a> Drop for Transaction<'a> {
    fn drop(&mut self) {
        if !self.committed {
            (self.notify_handler)(Notification::RollingBack);
            for item in self.changes.iter().rev() {
                // ok_ntfy!(self.notify_handler,
                //          Notification::NonFatalError,
                match item.roll_back(&self.prefix, self.notify_handler()) {
                    Ok(()) => {}
                    Err(e) => {
                        (self.notify_handler)(Notification::NonFatalError(&e));
                    }
                }
            }
        }
    }
}

/// This is the set of fundamental operations supported on a
/// Transaction. More complicated operations, such as installing a
/// package, or updating a component, distill down into a series of
/// these primitives.
#[derive(Debug)]
enum ChangedItem<'a> {
    AddedFile(PathBuf),
    AddedDir(PathBuf),
    RemovedFile(PathBuf, temp::File<'a>),
    RemovedDir(PathBuf, temp::Dir<'a>),
    ModifiedFile(PathBuf, Option<temp::File<'a>>),
}

impl<'a> ChangedItem<'a> {
    fn roll_back(
        &self,
        prefix: &InstallPrefix,
        notify: &'a dyn Fn(Notification<'_>),
    ) -> Result<()> {
        use self::ChangedItem::*;
        match self {
            AddedFile(path) => utils::remove_file("component", &prefix.abs_path(path))?,
            AddedDir(path) => utils::remove_dir("component", &prefix.abs_path(path), notify)?,
            RemovedFile(path, tmp) | ModifiedFile(path, Some(tmp)) => {
                utils::rename_file("component", &tmp, &prefix.abs_path(path), notify)?
            }
            RemovedDir(path, tmp) => {
                utils::rename_dir("component", &tmp.join("bk"), &prefix.abs_path(path), notify)?
            }
            ModifiedFile(path, None) => {
                let abs_path = prefix.abs_path(path);
                if utils::is_file(&abs_path) {
                    utils::remove_file("component", &abs_path)?;
                }
            }
        }
        Ok(())
    }
    fn dest_abs_path(prefix: &InstallPrefix, component: &str, relpath: &Path) -> Result<PathBuf> {
        let abs_path = prefix.abs_path(relpath);
        if utils::path_exists(&abs_path) {
            Err(anyhow!(RustupError::ComponentConflict {
                name: component.to_owned(),
                path: relpath.to_path_buf(),
            }))
        } else {
            if let Some(p) = abs_path.parent() {
                utils::ensure_dir_exists("component", p, &|_: Notification<'_>| ())?;
            }
            Ok(abs_path)
        }
    }
    fn add_file(prefix: &InstallPrefix, component: &str, relpath: PathBuf) -> Result<(Self, File)> {
        let abs_path = ChangedItem::dest_abs_path(prefix, component, &relpath)?;
        let file = File::create(&abs_path)
            .with_context(|| format!("error creating file '{}'", abs_path.display()))?;
        Ok((ChangedItem::AddedFile(relpath), file))
    }
    fn copy_file(
        prefix: &InstallPrefix,
        component: &str,
        relpath: PathBuf,
        src: &Path,
    ) -> Result<Self> {
        let abs_path = ChangedItem::dest_abs_path(prefix, component, &relpath)?;
        utils::copy_file(src, &abs_path)?;
        Ok(ChangedItem::AddedFile(relpath))
    }
    fn copy_dir(
        prefix: &InstallPrefix,
        component: &str,
        relpath: PathBuf,
        src: &Path,
    ) -> Result<Self> {
        let abs_path = ChangedItem::dest_abs_path(prefix, component, &relpath)?;
        utils::copy_dir(src, &abs_path, &|_: Notification<'_>| ())?;
        Ok(ChangedItem::AddedDir(relpath))
    }
    fn remove_file(
        prefix: &InstallPrefix,
        component: &str,
        relpath: PathBuf,
        temp_cfg: &'a temp::Cfg,
        notify: &'a dyn Fn(Notification<'_>),
    ) -> Result<Self> {
        let abs_path = prefix.abs_path(&relpath);
        let backup = temp_cfg.new_file()?;
        if !utils::path_exists(&abs_path) {
            Err(RustupError::ComponentMissingFile {
                name: component.to_owned(),
                path: relpath,
            }
            .into())
        } else {
            utils::rename_file("component", &abs_path, &backup, notify)?;
            Ok(ChangedItem::RemovedFile(relpath, backup))
        }
    }
    fn remove_dir(
        prefix: &InstallPrefix,
        component: &str,
        relpath: PathBuf,
        temp_cfg: &'a temp::Cfg,
        notify: &'a dyn Fn(Notification<'_>),
    ) -> Result<Self> {
        let abs_path = prefix.abs_path(&relpath);
        let backup = temp_cfg.new_directory()?;
        if !utils::path_exists(&abs_path) {
            Err(RustupError::ComponentMissingDir {
                name: component.to_owned(),
                path: relpath,
            }
            .into())
        } else {
            utils::rename_dir("component", &abs_path, &backup.join("bk"), notify)?;
            Ok(ChangedItem::RemovedDir(relpath, backup))
        }
    }
    fn modify_file(
        prefix: &InstallPrefix,
        relpath: PathBuf,
        temp_cfg: &'a temp::Cfg,
    ) -> Result<Self> {
        let abs_path = prefix.abs_path(&relpath);

        if utils::is_file(&abs_path) {
            let backup = temp_cfg.new_file()?;
            utils::copy_file(&abs_path, &backup)?;
            Ok(ChangedItem::ModifiedFile(relpath, Some(backup)))
        } else {
            if let Some(p) = abs_path.parent() {
                utils::ensure_dir_exists("component", p, &|_: Notification<'_>| {})?;
            }
            Ok(ChangedItem::ModifiedFile(relpath, None))
        }
    }
    fn move_file(
        prefix: &InstallPrefix,
        component: &str,
        relpath: PathBuf,
        src: &Path,
        notify: &'a dyn Fn(Notification<'_>),
    ) -> Result<Self> {
        let abs_path = ChangedItem::dest_abs_path(prefix, component, &relpath)?;
        utils::rename_file("component", src, &abs_path, notify)?;
        Ok(ChangedItem::AddedFile(relpath))
    }
    fn move_dir(
        prefix: &InstallPrefix,
        component: &str,
        relpath: PathBuf,
        src: &Path,
        notify: &'a dyn Fn(Notification<'_>),
    ) -> Result<Self> {
        let abs_path = ChangedItem::dest_abs_path(prefix, component, &relpath)?;
        utils::rename_dir("component", src, &abs_path, notify)?;
        Ok(ChangedItem::AddedDir(relpath))
    }
}
