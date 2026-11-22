//! Visible/durable filesystem model on four independent axes.
//!
//! The point of this module is that durability is **not** one property. The accepted
//! contract separates:
//!
//! | axis | becomes durable when | collapsing it loses |
//! |---|---|---|
//! | inode content | `sync_file` / `sync_entry` | "content synced but the name is not" |
//! | name→inode binding | `sync_dir` on the parent | overwrite/rename semantics |
//! | a directory's own binding | `sync_parent_of` / `sync_dir` on *its* parent | a missing parent sync during init |
//! | inode aliasing | `link`/no-overwrite publication | alias visibility and `AlreadyExists` |
//!
//! Two earlier model revisions were wrong in exactly these places, and both mistakes are
//! preserved as regression tests in the `tests` submodule. The corrections are:
//!
//! * names are `(parent, name) -> entry` bindings, so `replace_current` can legally leave the
//!   same name bound to the old inode after a crash;
//! * a directory is itself a binding in its parent, so a crash state is computed by
//!   **reachability from the root**, not by "which files exist".

pub mod fs_model;

#[cfg(test)]
mod fs_model_tests;

use std::collections::{BTreeMap, BTreeSet};

/// Identity of an inode within one model instance. Never reused.
pub type InodeId = u64;

/// What a name binds to.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Entry {
    /// A regular file backed by `InodeId`.
    File(InodeId),
    /// A directory. Its own entries live in the namespace keyed by its path.
    Dir,
}

/// A relative path inside the modelled tree, e.g. `sage-db/manifests/v1-x.sgm`.
///
/// The root is the empty path. Paths are plain byte-stable strings because the model must not
/// depend on the platform's path type; the real seam uses absolute [`std::path::Path`]s and the
/// adapter is a later, single file.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct RelPath(String);

impl RelPath {
    /// The root directory.
    #[must_use]
    pub fn root() -> Self {
        Self(String::new())
    }

    /// Build a path from its components, joined with `/`.
    ///
    /// # Panics
    ///
    /// Panics on a component that is empty or contains `/`, because that would make two
    /// different logical paths alias one string and hide a path-containment defect.
    #[must_use]
    pub fn join(parent: &Self, name: &str) -> Self {
        assert!(!name.is_empty(), "empty path component");
        assert!(!name.contains('/'), "component must not contain '/'");
        if parent.0.is_empty() {
            Self(name.to_owned())
        } else {
            Self(format!("{}/{name}", parent.0))
        }
    }

    /// The parent path; `None` for the root.
    #[must_use]
    pub fn parent(&self) -> Option<Self> {
        if self.0.is_empty() {
            return None;
        }
        match self.0.rsplit_once('/') {
            Some((head, _)) => Some(Self(head.to_owned())),
            None => Some(Self::root()),
        }
    }

    /// The last component; `None` for the root.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        if self.0.is_empty() {
            return None;
        }
        Some(
            self.0
                .rsplit_once('/')
                .map_or(self.0.as_str(), |(_, tail)| tail),
        )
    }

    /// The path as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this is the root.
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }
}

/// One directory's binding table.
///
/// Two tables exist per directory: the visible one the crashed process had populated, and the
/// durable one. Which of them a crash exposes depends on whether that directory was synced.
#[derive(Clone, Default, Debug)]
pub struct DirBindings {
    /// Bindings currently visible to the (live) caller.
    pub visible: BTreeMap<String, Entry>,
    /// Bindings known to be durable.
    pub durable: BTreeMap<String, Entry>,
}

/// The modelled filesystem.
///
/// All state is plain data. No trait, no I/O, no clock: the model's job is to enumerate what a
/// crash may legally leave behind, not to talk to a kernel.
#[derive(Clone, Default, Debug)]
pub struct ModelFs {
    /// Visible bytes per inode.
    inode_visible: BTreeMap<InodeId, Vec<u8>>,
    /// Durable bytes per inode, once synced.
    inode_durable: BTreeMap<InodeId, Vec<u8>>,
    /// Inodes whose visible content differs from durable content.
    inode_dirty: BTreeSet<InodeId>,
    next_inode: InodeId,
    /// Per-directory binding tables, keyed by directory path.
    dirs: BTreeMap<RelPath, DirBindings>,
    /// Directories whose binding table has unflushed changes.
    dir_dirty: BTreeSet<RelPath>,
}

impl ModelFs {
    /// A model containing only the root directory, whose own binding is already durable.
    #[must_use]
    pub fn new() -> Self {
        let mut dirs = BTreeMap::new();
        dirs.insert(RelPath::root(), DirBindings::default());
        Self {
            inode_visible: BTreeMap::new(),
            inode_durable: BTreeMap::new(),
            inode_dirty: BTreeSet::new(),
            next_inode: 0,
            dirs,
            dir_dirty: BTreeSet::new(),
        }
    }

    fn alloc_inode(&mut self) -> InodeId {
        self.next_inode += 1;
        self.next_inode
    }

    /// Make a directory visible under `parent`, and register it as its own table.
    ///
    /// The new directory's *own* binding is the `parent` entry; that entry becomes durable only
    /// when the parent is synced. This is axis 3 and the reason a missing parent sync strands
    /// everything underneath.
    ///
    /// # Panics
    ///
    /// Panics if the parent is not a known directory, or if the name already exists: the model
    /// refuses to invent a state the real seam cannot produce.
    pub fn create_dir(&mut self, parent: &RelPath, name: &str) {
        let path = RelPath::join(parent, name);
        assert!(self.dirs.contains_key(parent), "parent is not a directory");
        let entry = self.dirs.get_mut(parent).expect("checked above");
        assert!(
            !entry.visible.contains_key(name),
            "name already exists: refusing to invent an impossible state"
        );
        entry.visible.insert(name.to_owned(), Entry::Dir);
        self.dirs.entry(path).or_default();
        self.dir_dirty.insert(parent.clone());
    }

    /// Create a file that must not already exist (mirrors `create_new`).
    ///
    /// # Panics
    ///
    /// Panics if the directory is unknown or the name already exists.
    pub fn create_new(&mut self, dir: &RelPath, name: &str) -> InodeId {
        assert!(self.dirs.contains_key(dir), "not a directory");
        assert!(
            !self.dirs[dir].visible.contains_key(name),
            "AlreadyExists: create_new must never replace"
        );
        let ino = self.alloc_inode();
        self.inode_visible.insert(ino, Vec::new());
        self.inode_dirty.insert(ino);
        let bindings = self.dirs.get_mut(dir).expect("checked above");
        bindings.visible.insert(name.to_owned(), Entry::File(ino));
        self.dir_dirty.insert(dir.clone());
        ino
    }

    /// Append bytes at the current position (mirrors `write`).
    ///
    /// # Panics
    ///
    /// Panics if the inode is unknown: a caller must not write through a stale handle.
    pub fn write(&mut self, ino: InodeId, bytes: &[u8]) {
        let buf = self
            .inode_visible
            .get_mut(&ino)
            .expect("write to unknown inode");
        buf.extend_from_slice(bytes);
        self.inode_dirty.insert(ino);
    }

    /// Persist an inode's **content** (mirrors `sync_file` / `sync_entry`).
    ///
    /// This says nothing about any name: axis 1 only.
    pub fn sync_file(&mut self, ino: InodeId) {
        let Some(bytes) = self.inode_visible.get(&ino).cloned() else {
            return;
        };
        self.inode_durable.insert(ino, bytes);
        self.inode_dirty.remove(&ino);
    }

    /// Persist a directory's **entries** (mirrors `sync_dir`). Axis 2 only.
    pub fn sync_dir(&mut self, dir: &RelPath) {
        if let Some(bindings) = self.dirs.get_mut(dir) {
            bindings.durable = bindings.visible.clone();
        }
        self.dir_dirty.remove(dir);
    }

    /// Persist a directory's **own binding in its parent** (mirrors `sync_parent_of`).
    ///
    /// For a non-root directory this is `sync_dir(parent)`. It is spelled separately because
    /// the real seam has a separate entry point and the model must be able to represent a
    /// caller that syncs the directory's contents but not its parent.
    pub fn sync_parent_of(&mut self, dir: &RelPath) {
        if let Some(parent) = dir.parent() {
            self.sync_dir(&parent);
        }
    }

    /// Bind a second name to an existing inode (mirrors `publish_no_overwrite` on equal bytes).
    ///
    /// # Panics
    ///
    /// Panics when the destination already exists. POSIX `link(2)` fails with `EEXIST` and never
    /// replaces; a model that overwrites here would hide alias defects.
    pub fn link(&mut self, dir: &RelPath, name: &str, ino: InodeId) {
        assert!(self.dirs.contains_key(dir), "not a directory");
        let bindings = self.dirs.get_mut(dir).expect("checked above");
        assert!(
            !bindings.visible.contains_key(name),
            "AlreadyExists: link must not replace"
        );
        bindings.visible.insert(name.to_owned(), Entry::File(ino));
        self.dir_dirty.insert(dir.clone());
    }

    /// Remove a visible name (mirrors `remove`). A missing name is success.
    pub fn remove(&mut self, dir: &RelPath, name: &str) {
        if let Some(bindings) = self.dirs.get_mut(dir) {
            bindings.visible.remove(name);
        }
        self.dir_dirty.insert(dir.clone());
    }

    /// Replace the root pointer, the only overwrite in the seam.
    ///
    /// The destination's old binding is dropped from the visible table only; whether the crash
    /// shows the old or the new target depends on the parent's `sync_dir`.
    ///
    /// # Panics
    ///
    /// Panics if the staged file does not exist.
    pub fn replace_current(&mut self, dir: &RelPath, staged: &str, dst: &str) {
        let bindings = self.dirs.get_mut(dir).expect("not a directory");
        let entry = bindings
            .visible
            .remove(staged)
            .expect("staged entry must exist");
        bindings.visible.insert(dst.to_owned(), entry);
        self.dir_dirty.insert(dir.clone());
    }

    /// Move a name from one directory to another as one visible name replacement.
    ///
    /// One visible name resolution: the destination binding is replaced and the source name is
    /// consumed, in a single borrow of the model.
    ///
    /// Scope is deliberately narrow: this models **visible name resolution only**. The destination
    /// name becomes bound to the source entry and the source is consumed, which is what rename(2)
    /// makes visible. Both directories are marked dirty like any other entry change, so the existing
    /// conservative crash enumeration applies to each side unchanged.
    ///
    /// # Panics
    ///
    /// Panics if the source name is not bound: that is a caller error and the caller resolves it
    /// before calling, exactly as with [`Self::replace_current`].
    pub fn rename_across(&mut self, from: &RelPath, src: &str, to: &RelPath, dst: &str) {
        let entry = self
            .dirs
            .get_mut(from)
            .expect("not a directory")
            .visible
            .remove(src)
            .expect("rename source must exist");
        // rename(2) REPLACES an existing destination; it does not fail with EEXIST. Modelling this as
        // `link` + `remove` was wrong: `link` asserts the destination is absent, so a stored
        // `CURRENT` panicked - the opposite of the overwrite contract.
        self.dirs
            .get_mut(to)
            .expect("not a directory")
            .visible
            .insert(dst.to_owned(), entry);
        self.dir_dirty.insert(from.clone());
        self.dir_dirty.insert(to.clone());
    }

    /// Whether `name` is bound in `dir`'s visible table.
    #[must_use]
    pub fn has_entry(&self, dir: &RelPath, name: &str) -> bool {
        self.dirs
            .get(dir)
            .is_some_and(|b| b.visible.contains_key(name))
    }

    /// The inode a visible name binds to, when it is a file.
    #[must_use]
    pub fn inode_of(&self, dir: &RelPath, name: &str) -> Option<InodeId> {
        match self.dirs.get(dir)?.visible.get(name)? {
            Entry::File(ino) => Some(*ino),
            Entry::Dir => None,
        }
    }

    /// Whether a directory exists.
    #[must_use]
    pub fn has_dir(&self, path: &RelPath) -> bool {
        self.dirs.contains_key(path)
    }

    /// How many names a directory currently binds, without copying them.
    #[must_use]
    pub fn entry_len(&self, dir: &RelPath) -> usize {
        self.dirs.get(dir).map_or(0, |b| b.visible.len())
    }

    /// The name at `index` in a directory's ordered binding table, if any.
    ///
    /// Indexed access keeps a bounded traversal free of any up-front collection: the caller reads
    /// one name, charges for it, and asks for the next. `BTreeMap` iteration is ordered, so the
    /// sequence is deterministic without materialising it.
    #[must_use]
    pub fn entry_name_at(&self, dir: &RelPath, index: usize) -> Option<String> {
        self.dirs.get(dir)?.visible.keys().nth(index).cloned()
    }

    /// The visible entry names of a directory, in deterministic order.
    ///
    /// Returned as an owned vector because the model is not the bounded implementation; the
    /// *seam* charges per entry and the adapter does that, but the model's own bookkeeping is
    /// plain data and must not pretend to be an allocation-bounded traversal.
    #[must_use]
    pub fn entry_names(&self, dir: &RelPath) -> Vec<String> {
        self.dirs
            .get(dir)
            .map(|b| b.visible.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// The visible bytes of an inode.
    #[must_use]
    pub fn visible_bytes(&self, ino: InodeId) -> Option<&[u8]> {
        self.inode_visible.get(&ino).map(Vec::as_slice)
    }

    /// The durable bytes of an inode, if its content was ever synced.
    #[must_use]
    pub fn durable_bytes(&self, ino: InodeId) -> Option<&[u8]> {
        self.inode_durable.get(&ino).map(Vec::as_slice)
    }

    /// Whether any visible inode content or directory binding is still unflushed.
    #[must_use]
    pub fn has_unflushed(&self) -> bool {
        !self.inode_dirty.is_empty() || !self.dir_dirty.is_empty()
    }

    /// Enumerate the post-crash states this model permits.
    ///
    /// Each state keeps, independently:
    ///
    /// * for a dirty inode, either its durable content or its visible content;
    /// * for a dirty directory, each pending binding independently present or absent, with a
    ///   re-binding (`replace_current` over an existing name) able to yield either target;
    /// * then everything is filtered by **reachability from the root**, so a directory whose own
    ///   binding did not survive takes its whole subtree with it.
    ///
    /// The enumeration is a finite sample, not a proof about every real device. Callers must
    /// report how many states were sampled; see [`CrashStates::sampled`].
    #[must_use]
    pub fn crash_states(&self) -> CrashStates {
        let inode_options: Vec<(InodeId, Vec<Vec<u8>>)> = self
            .inode_visible
            .iter()
            .map(|(&ino, visible)| {
                let durable = self.inode_durable.get(&ino);
                let options: Vec<Vec<u8>> = if self.inode_dirty.contains(&ino) {
                    match durable {
                        Some(d) if d == visible => vec![visible.clone()],
                        Some(d) => vec![d.clone(), visible.clone()],
                        None => vec![Vec::new(), visible.clone()],
                    }
                } else {
                    vec![visible.clone()]
                };
                (ino, options)
            })
            .collect();

        let dir_options: Vec<(RelPath, Vec<BTreeMap<String, Entry>>)> = self
            .dirs
            .iter()
            .map(|(path, bindings)| {
                if !self.dir_dirty.contains(path) {
                    return (path.clone(), vec![bindings.visible.clone()]);
                }
                let mut options = vec![BTreeMap::new()];
                // Every name that is either visible now or was durable before must be considered:
                // a pending *removal* only exists in the durable table, so iterating the visible
                // table alone would make every removal unconditional (a real defect this test
                // caught: an unsynced `remove` could never be un-done by a crash).
                let names: BTreeSet<&String> = bindings
                    .visible
                    .keys()
                    .chain(bindings.durable.keys())
                    .collect();
                for name in names {
                    let visible_entry = bindings.visible.get(name).copied();
                    let durable_entry = bindings.durable.get(name).copied();
                    let name_options: Vec<Option<Entry>> = match (visible_entry, durable_entry) {
                        // Present in both, unchanged: the name is durable as it stands.
                        (Some(v), Some(d)) if v == d => vec![Some(v)],
                        // Present in both, different: a pending rebind may show either target.
                        (Some(v), Some(d)) => vec![Some(d), Some(v)],
                        // Newly visible: the directory entry may not have reached the disk.
                        (Some(v), None) => vec![Some(v), None],
                        // Present durably, removed in the visible view: removal may not have
                        // taken effect, so the old binding can still be there after a crash.
                        (None, Some(d)) => vec![Some(d), None],
                        // Unreachable: the name came from one of the two tables.
                        (None, None) => continue,
                    };
                    let mut next = Vec::with_capacity(options.len() * name_options.len());
                    for base in &options {
                        for choice in &name_options {
                            let mut candidate = base.clone();
                            if let Some(e) = choice {
                                candidate.insert(name.clone(), *e);
                            }
                            next.push(candidate);
                        }
                    }
                    options = next;
                }
                (path.clone(), options)
            })
            .collect();

        let mut out = Vec::new();
        let mut combos: Vec<Vec<(RelPath, BTreeMap<String, Entry>)>> = vec![Vec::new()];
        for (path, options) in &dir_options {
            let mut next = Vec::with_capacity(combos.len() * options.len());
            for base in &combos {
                for option in options {
                    let mut candidate = base.clone();
                    candidate.push((path.clone(), option.clone()));
                    next.push(candidate);
                }
            }
            combos = next;
        }
        for dirs in combos {
            let mut tables: BTreeMap<RelPath, BTreeMap<String, Entry>> = BTreeMap::new();
            for (path, table) in dirs {
                tables.insert(path, table);
            }
            let mut inode_combo: Vec<BTreeMap<InodeId, Vec<u8>>> = vec![BTreeMap::new()];
            for (ino, options) in &inode_options {
                let mut next = Vec::with_capacity(inode_combo.len() * options.len());
                for base in &inode_combo {
                    for option in options {
                        let mut candidate = base.clone();
                        candidate.insert(*ino, option.clone());
                        next.push(candidate);
                    }
                }
                inode_combo = next;
            }
            for bytes in inode_combo {
                out.push(DurableState::from_tables(&tables, &bytes));
            }
        }
        let sampled = out.len();
        CrashStates {
            states: out,
            sampled,
        }
    }
}

/// A post-crash durable state: what is actually reachable after power loss.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct DurableState {
    /// Reachable files, by path, with their durable bytes.
    pub files: BTreeMap<RelPath, Vec<u8>>,
    /// Reachable directories, by path. The root is always present when it was durable.
    pub dirs: BTreeSet<RelPath>,
}

impl DurableState {
    fn from_tables(
        tables: &BTreeMap<RelPath, BTreeMap<String, Entry>>,
        bytes: &BTreeMap<InodeId, Vec<u8>>,
    ) -> Self {
        let mut files = BTreeMap::new();
        let mut dirs = BTreeSet::new();
        // The root exists only if it was ever made durable; a fresh model's root is.
        dirs.insert(RelPath::root());
        let mut frontier: Vec<RelPath> = dirs.iter().cloned().collect();
        while let Some(path) = frontier.pop() {
            let Some(table) = tables.get(&path) else {
                continue;
            };
            for (name, entry) in table {
                let child = RelPath::join(&path, name);
                match entry {
                    Entry::Dir => {
                        if dirs.insert(child.clone()) {
                            frontier.push(child);
                        }
                    }
                    Entry::File(ino) => {
                        if let Some(content) = bytes.get(ino) {
                            files.insert(child, content.clone());
                        }
                    }
                }
            }
        }
        Self { files, dirs }
    }

    /// Read a reachable file's bytes.
    #[must_use]
    pub fn read(&self, path: &RelPath) -> Option<&[u8]> {
        self.files.get(path).map(Vec::as_slice)
    }

    /// Whether a directory survived.
    #[must_use]
    pub fn has_dir(&self, path: &RelPath) -> bool {
        self.dirs.contains(path)
    }

    /// Every reachable file path, for set comparisons in tests.
    #[must_use]
    pub fn file_paths(&self) -> BTreeSet<&RelPath> {
        self.files.keys().collect()
    }
}

/// Why a sampled durable state could not be turned back into a model.
///
/// Restoration constructs the visible AND durable namespace from the sample and nothing else. Each refusal names a
/// structural contradiction in the sample rather than repairing it: a harness that normalized an invalid state would
/// report a recovery the real system could never perform.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RestoreError {
    /// The sample has no root directory, so nothing is reachable and no namespace can be built.
    MissingRoot,
    /// A file path has no surviving parent directory in the sample.
    ///
    /// This is a genuine structural defect: `DurableState` is produced by reachability from the root, so a file whose
    /// parent is absent cannot have come from that sampling and must not be silently re-parented or dropped.
    ParentMissing {
        /// The orphaned file path.
        path: RelPath,
    },
    /// The same path appears as both a directory and a file.
    FileDirCollision {
        /// The colliding path.
        path: RelPath,
    },
    /// A directory's own parent is missing from the sample (other than for the root).
    DirParentMissing {
        /// The orphaned directory path.
        path: RelPath,
    },
    /// The caller's explicit root path is not absolute, so as a virtual root context it is ambiguous.
    RootNotAbsolute,
}

impl std::fmt::Display for RestoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingRoot => write!(f, "durable state has no root directory"),
            Self::ParentMissing { path } => {
                write!(f, "file {} has no surviving parent", path.as_str())
            }
            Self::FileDirCollision { path } => {
                write!(f, "{} is both a file and a directory", path.as_str())
            }
            Self::DirParentMissing { path } => {
                write!(f, "directory {} has no surviving parent", path.as_str())
            }
            Self::RootNotAbsolute => write!(f, "restore root path is not absolute"),
        }
    }
}

impl std::error::Error for RestoreError {}

impl ModelFs {
    /// Rebuild a model whose visible AND durable state are EXACTLY `state`.
    ///
    /// Two properties matter and are both structural, not cosmetic:
    ///
    /// * nothing is dirty, so `has_unflushed()` is false and no crash enumeration can invent a difference the sample
    ///   did not contain;
    /// * the inode table, directory tables and every binding are constructed from the sample, so there are no old
    ///   handles, cursors, faults or locks carried over from the live run this state was sampled from.
    ///
    /// The sample is validated before anything is built: a missing root, an orphaned file or directory, or a
    /// file/directory collision is refused with [`RestoreError`] rather than repaired. A partial family set is legal —
    /// a crash may leave only some families on disk — so completeness is deliberately NOT required.
    ///
    /// # Errors
    ///
    /// [`RestoreError`] as described above.
    pub fn from_durable_state(state: DurableState) -> Result<Self, RestoreError> {
        if !state.dirs.contains(&RelPath::root()) {
            return Err(RestoreError::MissingRoot);
        }
        // Structural validation first: every non-root path needs a surviving parent directory.
        for dir in &state.dirs {
            if dir.is_root() {
                continue;
            }
            let parent = dir
                .parent()
                .ok_or(RestoreError::DirParentMissing { path: dir.clone() })?;
            if !state.dirs.contains(&parent) {
                return Err(RestoreError::DirParentMissing { path: dir.clone() });
            }
            if state.files.contains_key(dir) {
                return Err(RestoreError::FileDirCollision { path: dir.clone() });
            }
        }
        for path in state.files.keys() {
            let parent = path
                .parent()
                .ok_or(RestoreError::ParentMissing { path: path.clone() })?;
            if !state.dirs.contains(&parent) {
                return Err(RestoreError::ParentMissing { path: path.clone() });
            }
        }

        // Construct from the sample only. Every table starts empty; nothing is inherited.
        let mut model = Self {
            inode_visible: BTreeMap::new(),
            inode_durable: BTreeMap::new(),
            inode_dirty: BTreeSet::new(),
            next_inode: 0,
            dirs: BTreeMap::new(),
            dir_dirty: BTreeSet::new(),
        };
        for dir in &state.dirs {
            model.dirs.insert(dir.clone(), DirBindings::default());
        }
        // Files: one inode each, visible bytes == durable bytes, bound in its parent's table.
        for (path, bytes) in &state.files {
            let ino = model.alloc_inode();
            model.inode_visible.insert(ino, bytes.clone());
            model.inode_durable.insert(ino, bytes.clone());
            let parent = path.parent().expect("validated above");
            let name = path.name().expect("non-root file path has a name");
            model
                .dirs
                .get_mut(&parent)
                .expect("validated above")
                .visible
                .insert(name.to_owned(), Entry::File(ino));
        }
        // Directories: each is a binding in its parent, plus its own table.
        for dir in &state.dirs {
            if dir.is_root() {
                continue;
            }
            let parent = dir.parent().expect("validated above");
            let name = dir.name().expect("non-root path has a name");
            model
                .dirs
                .get_mut(&parent)
                .expect("validated above")
                .visible
                .insert(name.to_owned(), Entry::Dir);
        }
        // Visible == durable for every directory, so the restored state is already "on disk".
        for bindings in model.dirs.values_mut() {
            bindings.durable = bindings.visible.clone();
        }
        Ok(model)
    }
}

/// The sampled set of legal post-crash states.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CrashStates {
    /// The states themselves.
    pub states: Vec<DurableState>,
    /// How many states were enumerated. Reported so no caller can present a sample as a proof.
    pub sampled: usize,
}

impl CrashStates {
    /// Number of distinct states.
    #[must_use]
    pub fn len(&self) -> usize {
        self.states.len()
    }

    /// Whether the sample is empty (only possible for a degenerate model).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.states.is_empty()
    }

    /// Iterate the states.
    pub fn iter(&self) -> impl Iterator<Item = &DurableState> {
        self.states.iter()
    }
}

#[cfg(test)]
mod tests;
