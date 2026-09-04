//! Archive packaging (tar + gzip, pure Rust) and hardened extraction
//! (PRD §9.6): traversal, absolute paths, escaping links, and special
//! members are rejected; a disk-headroom check runs before anything is
//! written. All functions here are blocking — call them via
//! `tokio::task::spawn_blocking`.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use sha2::{Digest, Sha256};
use tar::{Archive, Builder, Entry, EntryType};
use tracing::{debug, warn};

use crate::config::Source;
use crate::error::{ArkError, Result};
use crate::hash::hex;

/// Free space required before extracting, as a multiple of the archive size.
pub const HEADROOM_MULTIPLIER: u64 = 3;

/// What a packing run produced.
#[derive(Debug, Clone)]
pub struct PackReport {
    pub path: PathBuf,
    pub size: u64,
    /// Bare hex SHA-256 of the archive bytes.
    pub sha256: String,
}

/// What an extraction produced.
#[derive(Debug, Clone, Default)]
pub struct UnpackReport {
    pub entries: u64,
    pub bytes: u64,
    /// Entries refused with the reason (`path: reason`).
    pub skipped: Vec<String>,
}

/// File-source ignore rules (KB §2.2), matched on the entry **basename**.
#[derive(Debug, Clone, Default)]
pub struct IgnoreRules {
    pub startswith: Vec<String>,
    /// fnmatch-style patterns (`*`, `?`).
    pub patterns: Vec<String>,
    /// Extensions without the dot, case-insensitive.
    pub extensions: Vec<String>,
}

impl IgnoreRules {
    pub fn from_source(source: &Source) -> Self {
        Self {
            startswith: source.effective_ignore_startswith(),
            patterns: source.effective_ignore(),
            extensions: source.effective_ignore_extensions(),
        }
    }

    /// Whether a top-level entry named `name` is excluded.
    pub fn excludes(&self, name: &str) -> bool {
        if self.startswith.iter().any(|p| name.starts_with(p.as_str())) {
            return true;
        }
        if self.patterns.iter().any(|p| glob_match(p, name)) {
            return true;
        }
        // Text after the last dot, so dotfiles (`.DS_Store`) and bundles
        // (`Photos.photoslibrary`) both match.
        let ext = name.rsplit_once('.').map(|(_, e)| e).unwrap_or_default();
        !ext.is_empty() && self.extensions.iter().any(|e| e.eq_ignore_ascii_case(ext))
    }
}

/// Minimal fnmatch: `*` matches any run, `?` one character, else literal.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    // dp[i][j]: p[..i] matches t[..j]
    let mut prev = vec![false; t.len().saturating_add(1)];
    prev[0] = true;
    for pc in &p {
        let mut cur = vec![false; t.len().saturating_add(1)];
        cur[0] = prev[0] && *pc == '*';
        for j in 1..=t.len() {
            let matched = glob_step(*pc, j, &prev, &cur, &t);
            cur[j] = matched;
        }
        prev = cur;
    }
    prev[t.len()]
}

/// One cell of the fnmatch table: does `pattern[..i]` match `text[..j]`?
fn glob_step(pc: char, j: usize, prev: &[bool], cur: &[bool], t: &[char]) -> bool {
    let jm1 = j.saturating_sub(1);
    match pc {
        '*' => cur[jm1] || prev[j],
        '?' => prev[jm1],
        c => prev[jm1] && t[jm1] == c,
    }
}

/// Pack the contents of `dir` (entries relative to it) into `dest`.
pub fn pack_dir(dir: &Path, dest: &Path) -> Result<PackReport> {
    let mut builder = new_builder(dest)?;
    builder.append_dir_all(".", dir)?;
    finish(builder, dest)
}

/// Pack a file-source tree: top-level entries of `root` filtered by `rules`,
/// subtrees preserved, symlinks kept as symlinks (never followed).
pub fn pack_tree(root: &Path, rules: &IgnoreRules, dest: &Path) -> Result<PackReport> {
    let mut builder = new_builder(dest)?;
    let mut entries: Vec<_> = std::fs::read_dir(root)?.collect::<std::io::Result<_>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if rules.excludes(&name_str) {
            debug!(entry = %name_str, "excluded by ignore rules");
            continue;
        }
        let path = entry.path();
        if path.is_dir() && !path.is_symlink() {
            builder.append_dir_all(&name, &path)?;
        } else {
            builder.append_path_with_name(&path, &name)?;
        }
    }
    finish(builder, dest)
}

fn new_builder(dest: &Path) -> Result<Builder<GzEncoder<BufWriter<File>>>> {
    let file = BufWriter::new(File::create(dest)?);
    let mut builder = Builder::new(GzEncoder::new(file, Compression::default()));
    builder.follow_symlinks(false);
    Ok(builder)
}

fn finish(builder: Builder<GzEncoder<BufWriter<File>>>, dest: &Path) -> Result<PackReport> {
    let encoder = builder.into_inner()?;
    let mut writer = encoder.finish()?;
    writer.flush()?;
    drop(writer);
    let (size, sha256) = digest_file(dest)?;
    Ok(PackReport {
        path: dest.to_path_buf(),
        size,
        sha256,
    })
}

/// Validate a local path before it is opened: it must name an existing
/// regular file, and the result is canonical (symlinks and `..` resolved).
/// This is the single point at which a caller-supplied path becomes trusted.
pub fn sanitize(path: &Path) -> Result<PathBuf> {
    let canonical = path
        .canonicalize()
        .map_err(|e| ArkError::Validation(format!("`{}`: {e}", path.display())))?;
    if !canonical.is_file() {
        return Err(ArkError::Validation(format!(
            "`{}` is not a regular file",
            path.display()
        )));
    }
    Ok(canonical)
}

/// Size and bare hex SHA-256 of a file, streamed.
pub fn digest_file(path: &Path) -> Result<(u64, String)> {
    let clean = sanitize(path)?;
    let mut reader = BufReader::new(File::open(&clean)?);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    let mut size: u64 = 0;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size = size.saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
    }
    Ok((size, hex(&hasher.finalize())))
}

/// Refuse to extract unless `dir`'s filesystem has `HEADROOM_MULTIPLIER ×
/// archive_size` bytes available.
pub fn ensure_headroom(dir: &Path, archive_size: u64) -> Result<()> {
    let needed = archive_size.saturating_mul(HEADROOM_MULTIPLIER);
    let available = fs4::available_space(dir)?;
    if available < needed {
        return Err(ArkError::Refused(format!(
            "not enough disk space under {}: {available} bytes available, {needed} needed \
             ({HEADROOM_MULTIPLIER}x the {archive_size}-byte archive)",
            dir.display()
        )));
    }
    Ok(())
}

/// Why an entry is refused.
enum Verdict {
    Extract,
    Skip(&'static str),
}

/// Extract `archive` into `dest` (created if absent). Regular files and
/// directories are extracted; symlinks / hardlinks only when their target
/// stays inside `dest`; everything else is skipped and reported. Existing
/// files are never overwritten.
pub fn unpack(archive: &Path, dest: &Path) -> Result<UnpackReport> {
    std::fs::create_dir_all(dest)?;
    let clean = sanitize(archive)?;
    let mut tar = Archive::new(GzDecoder::new(BufReader::new(File::open(&clean)?)));
    tar.set_overwrite(false);
    tar.set_preserve_permissions(false);
    tar.set_unpack_xattrs(false);
    let mut report = UnpackReport::default();
    for entry in tar.entries()? {
        process_entry(entry?, dest, &mut report)?;
    }
    Ok(report)
}

fn process_entry<R: Read>(
    mut entry: Entry<'_, R>,
    dest: &Path,
    report: &mut UnpackReport,
) -> Result<()> {
    if is_root_marker(&entry.path()?) {
        return Ok(());
    }
    let shown = entry
        .path()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    match admit(&entry)? {
        Verdict::Skip(reason) => {
            warn!(entry = %shown, reason, "refused archive entry");
            report.skipped.push(format!("{shown}: {reason}"));
        }
        Verdict::Extract => extract_entry(&mut entry, dest, &shown, report)?,
    }
    Ok(())
}

/// Extract one admitted entry; an existing file is a skip, never a clobber.
fn extract_entry<R: Read>(
    entry: &mut Entry<'_, R>,
    dest: &Path,
    shown: &str,
    report: &mut UnpackReport,
) -> Result<()> {
    let size = entry.header().size()?;
    match entry.unpack_in(dest) {
        Ok(true) => {
            report.entries = report.entries.saturating_add(1);
            report.bytes = report.bytes.saturating_add(size);
        }
        Ok(false) => report
            .skipped
            .push(format!("{shown}: refused by unpack_in")),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            report
                .skipped
                .push(format!("{shown}: already exists (never overwritten)"));
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

fn admit<R: Read>(entry: &Entry<'_, R>) -> Result<Verdict> {
    let path = entry.path()?;
    if !is_safe_relative(&path) {
        return Ok(Verdict::Skip("path is absolute or escapes with `..`"));
    }
    let kind = entry.header().entry_type();
    Ok(match kind {
        EntryType::Regular | EntryType::Directory | EntryType::Continuous => Verdict::Extract,
        EntryType::Symlink | EntryType::Link => match entry.link_name()? {
            Some(target) if link_stays_inside(&path, &target) => Verdict::Extract,
            _ => Verdict::Skip("link target escapes the extraction root"),
        },
        _ => Verdict::Skip("special member (device, fifo, socket, …)"),
    })
}

/// `.` (or empty): the directory-root entry `append_dir_all` emits.
fn is_root_marker(path: &Path) -> bool {
    path.components().all(|c| c == Component::CurDir)
}

/// Relative, non-empty, no `..` component.
fn is_safe_relative(path: &Path) -> bool {
    let mut saw_normal = false;
    for component in path.components() {
        match component {
            Component::Normal(_) => saw_normal = true,
            Component::CurDir => {}
            _ => return false,
        }
    }
    saw_normal
}

/// Whether `target`, resolved lexically against the link's parent directory,
/// stays inside the extraction root.
fn link_stays_inside(link_path: &Path, target: &Path) -> bool {
    if target.is_absolute()
        || target
            .components()
            .any(|c| matches!(c, Component::Prefix(_)))
    {
        return false;
    }
    let mut depth: i64 = 0;
    for component in link_path.parent().unwrap_or(Path::new("")).components() {
        if matches!(component, Component::Normal(_)) {
            depth = depth.saturating_add(1);
        }
    }
    for component in target.components() {
        match component {
            Component::Normal(_) => depth = depth.saturating_add(1),
            Component::ParentDir => depth = depth.saturating_sub(1),
            _ => {}
        }
        if depth < 0 {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn make_tree(root: &Path) {
        std::fs::create_dir_all(root.join("sub/deeper")).unwrap();
        std::fs::write(root.join("a.txt"), "alpha").unwrap();
        std::fs::write(root.join("sub/b.txt"), "beta").unwrap();
        std::fs::write(root.join("sub/deeper/c.txt"), "gamma").unwrap();
        std::fs::write(root.join(".DS_Store"), "junk").unwrap();
        std::fs::write(root.join("cache.tmp"), "junk").unwrap();
        std::fs::write(root.join("pg_internal"), "junk").unwrap();
        symlink("a.txt", root.join("link-to-a")).unwrap();
    }

    #[test]
    fn glob_matches_like_fnmatch() {
        assert!(glob_match("*.tmp", "cache.tmp"));
        assert!(glob_match("cache.?mp", "cache.tmp"));
        assert!(glob_match("*", "anything"));
        assert!(!glob_match("*.tmp", "cache.tmp.bak"));
        assert!(!glob_match("exact", "exactly"));
        assert!(glob_match("", ""));
    }

    #[test]
    fn ignore_rules_match_on_basename() {
        let rules = IgnoreRules {
            startswith: vec!["pg_".into()],
            patterns: vec!["*.tmp".into()],
            extensions: vec!["DS_Store".into()],
        };
        assert!(rules.excludes("pg_internal"));
        assert!(rules.excludes("cache.tmp"));
        assert!(rules.excludes(".DS_Store"));
        assert!(!rules.excludes("a.txt"));
    }

    #[test]
    fn pack_tree_applies_rules_and_round_trips_with_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("src");
        make_tree(&root);
        let rules = IgnoreRules {
            startswith: vec!["pg_".into()],
            patterns: vec!["*.tmp".into()],
            extensions: vec!["DS_Store".into()],
        };
        let archive = dir.path().join("out.tar.gz");
        let report = pack_tree(&root, &rules, &archive).unwrap();
        assert!(report.size > 0);
        assert_eq!(report.sha256.len(), 64);

        let dest = dir.path().join("restored");
        let unpacked = unpack(&archive, &dest).unwrap();
        assert!(unpacked.skipped.is_empty(), "{:?}", unpacked.skipped);
        assert_eq!(
            std::fs::read_to_string(dest.join("a.txt")).unwrap(),
            "alpha"
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("sub/deeper/c.txt")).unwrap(),
            "gamma"
        );
        assert!(
            dest.join("link-to-a").is_symlink(),
            "symlink preserved, not followed"
        );
        assert!(!dest.join(".DS_Store").exists());
        assert!(!dest.join("cache.tmp").exists());
        assert!(!dest.join("pg_internal").exists());
    }

    #[test]
    fn pack_dir_round_trips_and_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("work");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("manifest.json"), "{}").unwrap();
        let archive = dir.path().join("a.tar.gz");
        pack_dir(&src, &archive).unwrap();
        let dest = dir.path().join("out");
        let r = unpack(&archive, &dest).unwrap();
        assert_eq!(r.entries, 1, "the file; the `.` root marker is ignored");
        assert_eq!(
            std::fs::read_to_string(dest.join("manifest.json")).unwrap(),
            "{}"
        );
        // second unpack into the same dir: existing files are not clobbered
        std::fs::write(dest.join("manifest.json"), "changed").unwrap();
        let again = unpack(&archive, &dest).unwrap();
        assert!(!again.skipped.is_empty());
        assert_eq!(
            std::fs::read_to_string(dest.join("manifest.json")).unwrap(),
            "changed"
        );
    }

    #[test]
    fn unpack_refuses_traversal_absolute_and_escaping_links() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("evil.tar.gz");
        {
            let file = File::create(&archive).unwrap();
            let mut b = Builder::new(GzEncoder::new(file, Compression::default()));
            let mut h = tar::Header::new_gnu();
            h.set_size(4);
            h.set_entry_type(EntryType::Regular);
            h.as_gnu_mut().unwrap().name[..8].copy_from_slice(b"../evil\0");
            h.set_cksum();
            b.append(&h, &b"pwnd"[..]).unwrap();

            let mut h = tar::Header::new_gnu();
            h.set_size(4);
            h.set_entry_type(EntryType::Regular);
            h.as_gnu_mut().unwrap().name[..9].copy_from_slice(b"/abs.txt\0");
            h.set_cksum();
            b.append(&h, &b"pwnd"[..]).unwrap();

            let mut h = tar::Header::new_gnu();
            h.set_entry_type(EntryType::Symlink);
            h.set_size(0);
            b.append_link(&mut h, "escape", "../../outside").unwrap();

            let mut h = tar::Header::new_gnu();
            h.set_entry_type(EntryType::Symlink);
            h.set_size(0);
            b.append_link(&mut h, "abslink", "/etc/passwd").unwrap();

            let mut h = tar::Header::new_gnu();
            h.set_entry_type(EntryType::Fifo);
            h.set_size(0);
            h.set_path("pipe").unwrap();
            h.set_cksum();
            b.append(&h, &b""[..]).unwrap();

            let mut h = tar::Header::new_gnu();
            h.set_entry_type(EntryType::Regular);
            h.set_size(2);
            h.set_path("ok.txt").unwrap();
            h.set_cksum();
            b.append(&h, &b"ok"[..]).unwrap();
            b.into_inner().unwrap().finish().unwrap();
        }
        let dest = dir.path().join("out");
        let r = unpack(&archive, &dest).unwrap();
        assert_eq!(r.entries, 1, "{:?}", r.skipped);
        assert_eq!(r.skipped.len(), 5, "{:?}", r.skipped);
        assert!(dest.join("ok.txt").is_file());
        assert!(!dir.path().join("evil").exists());
        assert!(!dest.join("escape").exists() && !dest.join("abslink").exists());
    }

    #[test]
    fn headroom_check_uses_multiplier() {
        let dir = tempfile::tempdir().unwrap();
        assert!(ensure_headroom(dir.path(), 1).is_ok());
        assert!(ensure_headroom(dir.path(), u64::MAX / 2).is_err());
    }

    #[test]
    fn link_containment_is_lexical() {
        assert!(link_stays_inside(Path::new("a/b/link"), Path::new("../c")));
        assert!(link_stays_inside(Path::new("link"), Path::new("file")));
        assert!(!link_stays_inside(Path::new("link"), Path::new("../out")));
        assert!(!link_stays_inside(
            Path::new("a/link"),
            Path::new("../../out")
        ));
        assert!(!link_stays_inside(Path::new("a/link"), Path::new("/abs")));
    }
}
