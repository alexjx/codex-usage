//! Persistent source configuration. Configuration never causes an unselected source to be read.
use crate::model::Source;
use anyhow::{anyhow, bail, Context, Result};
use directories::{BaseDirs, ProjectDirs};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    env,
    fs::{self, OpenOptions},
    io::Write,
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    #[serde(default)]
    pub sources: Vec<Source>,
}

/// Sources shown by `sources`; discovery does not persist or enable candidates.
#[derive(Debug)]
pub struct SourceCatalog {
    pub sources: Vec<Source>,
    pub candidates: Vec<Source>,
}
impl SourceCatalog {
    fn new(sources: Vec<Source>, mut discovered: Vec<Source>) -> Result<Self> {
        let mut names: BTreeSet<_> = sources.iter().map(|s| s.name.clone()).collect();
        let mut roots = sources
            .iter()
            .map(|s| path_identity(&s.root))
            .collect::<Result<BTreeSet<_>>>()?;
        let mut candidates = Vec::new();
        discovered.sort_by(|a, b| a.root.cmp(&b.root).then(a.name.cmp(&b.name)));
        for mut candidate in discovered {
            candidate.root = normalize_path(&candidate.root)?;
            if !roots.insert(path_identity(&candidate.root)?) {
                continue;
            }
            // A discovered folder must not shadow a different saved source.
            let base = candidate.name.chars().take(48).collect::<String>();
            candidate.name = base.clone();
            if names.contains(&candidate.name) {
                candidate.name = format!("{base}-discovered");
                let mut suffix = 2;
                while names.contains(&candidate.name) {
                    candidate.name = format!("{base}-discovered-{suffix}");
                    suffix += 1;
                }
            }
            candidate.enabled = false;
            names.insert(candidate.name.clone());
            candidates.push(candidate);
        }
        validate_sources(&candidates)?;
        Ok(Self {
            sources,
            candidates,
        })
    }
}

pub fn config_path(override_path: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = override_path {
        return normalize_path(path);
    }
    if let Some(path) = env::var_os("CODEX_USAGE_CONFIG").filter(|value| !value.is_empty()) {
        return normalize_path(Path::new(&path));
    }
    let project = ProjectDirs::from("", "", "codex-usage")
        .ok_or_else(|| anyhow!("Cannot find your configuration directory; use --config <path>"))?;
    Ok(project.config_dir().join("config.toml"))
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let input = match fs::read_to_string(path) {
            Ok(input) => input,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default())
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Cannot read configuration {}", path.display()))
            }
        };
        let mut config: Self = toml::from_str(&input)
            .with_context(|| format!("Invalid configuration {}", path.display()))?;
        // Paths edited by hand are relative to the config file, not an unrelated shell cwd.
        let base = normalize_path(path)?
            .parent()
            .unwrap_or(Path::new("/"))
            .to_path_buf();
        for source in &mut config.sources {
            let normalized = normalize_path(&source.root)?;
            if source.root.is_relative() && !starts_with_home(&source.root) {
                source.root = normalize_path(&base.join(&source.root))?;
            } else {
                source.root = normalized;
            }
        }
        config.validate()?;
        Ok(config)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        let path = normalize_path(path)?;
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("Configuration path has no parent"))?;
        fs::create_dir_all(parent).with_context(|| {
            format!("Cannot create configuration directory {}", parent.display())
        })?;
        let contents = toml::to_string_pretty(self).context("Cannot encode configuration")?;
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let mut temporary = None;
        for _ in 0..100 {
            let name = format!(
                ".codex-usage-{}-{}.tmp",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            );
            let candidate = parent.join(name);
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&candidate) {
                Ok(file) => {
                    temporary = Some((RemoveOnDrop(candidate), file));
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error).context("Cannot create temporary configuration file")
                }
            }
        }
        let (temporary, mut file) =
            temporary.ok_or_else(|| anyhow!("Cannot reserve a temporary configuration file"))?;
        file.write_all(contents.as_bytes())
            .context("Cannot write configuration")?;
        file.sync_all().context("Cannot flush configuration")?;
        drop(file);
        // Same-directory rename replaces the destination atomically on supported local filesystems.
        fs::rename(&temporary.0, &path)
            .with_context(|| format!("Cannot replace configuration {}", path.display()))?;
        #[cfg(unix)]
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    }

    pub fn set_source(&mut self, name: &str, kind: Option<&str>, root: &Path) -> Result<()> {
        validate_name(name)?;
        if let Some(kind) = kind {
            validate_kind(kind)?;
        }
        let root = normalize_path(root)?;
        let mut updated = self.clone();
        if let Some(existing) = updated
            .sources
            .iter_mut()
            .find(|source| source.name == name)
        {
            existing.root = root;
            if let Some(kind) = kind {
                existing.kind = kind.to_owned();
            }
        } else {
            let kind = kind
                .ok_or_else(|| anyhow!("New source {name:?} requires --kind wsl|windows|linux"))?;
            updated.sources.push(Source {
                name: name.to_owned(),
                kind: kind.to_owned(),
                root,
                enabled: true,
            });
        }
        updated.validate()?;
        *self = updated;
        Ok(())
    }

    pub fn set_enabled(&mut self, name: &str, enabled: bool) -> Result<()> {
        if let Some(source) = self.sources.iter_mut().find(|source| source.name == name) {
            source.enabled = enabled;
            return Ok(());
        }
        let catalog = self.source_catalog()?;
        self.set_enabled_from_catalog(name, enabled, catalog)
    }

    pub fn source_catalog(&self) -> Result<SourceCatalog> {
        let sources = if self.sources.is_empty() {
            self.select(&[], &[])?
        } else {
            self.sources.clone()
        };
        SourceCatalog::new(sources, candidates())
    }

    fn set_enabled_from_catalog(
        &mut self,
        name: &str,
        enabled: bool,
        catalog: SourceCatalog,
    ) -> Result<()> {
        let selected=catalog.sources.iter().chain(&catalog.candidates).find(|source|source.name==name)
            .ok_or_else(||anyhow!("Unknown source {name:?}; run `codex-usage sources` and use a listed name, or configure a path with `sources set`"))?.clone();
        let mut updated = self.clone();
        if updated.sources.is_empty() {
            // Preserve the native implicit source when registering the first candidate.
            updated.sources = catalog.sources;
        }
        if let Some(source) = updated
            .sources
            .iter_mut()
            .find(|source| source.name == name)
        {
            source.enabled = enabled;
        } else {
            updated.sources.push(Source {
                enabled,
                ..selected
            });
        }
        updated.validate()?;
        *self = updated;
        Ok(())
    }

    pub fn select(&self, names: &[String], roots: &[PathBuf]) -> Result<Vec<Source>> {
        if !names.is_empty() && !roots.is_empty() {
            bail!("Use --source or --root, not both");
        }
        let selected = if !roots.is_empty() {
            roots
                .iter()
                .enumerate()
                .map(|(index, root)| {
                    Ok(Source {
                        name: format!("root-{}", index + 1),
                        kind: native_kind(),
                        root: normalize_path(root)?,
                        enabled: true,
                    })
                })
                .collect::<Result<Vec<_>>>()?
        } else if !names.is_empty() {
            let mut selected = Vec::new();
            let mut seen = BTreeSet::new();
            for name in names {
                if !seen.insert(name) {
                    continue;
                }
                let source = self
                    .sources
                    .iter()
                    .find(|source| &source.name == name)
                    .ok_or_else(|| anyhow!("Unknown source {name:?}; run `codex-usage sources`"))?;
                selected.push(source.clone());
            }
            selected
        } else if !self.sources.is_empty() {
            let selected: Vec<_> = self
                .sources
                .iter()
                .filter(|source| source.enabled)
                .cloned()
                .collect();
            if selected.is_empty() {
                bail!("No sources enabled; run `codex-usage sources enable <name>`");
            }
            selected
        } else {
            let kind = native_kind();
            let root = if let Some(root) = env::var_os("CODEX_HOME").filter(|root| !root.is_empty())
            {
                normalize_path(Path::new(&root))?
            } else {
                home_dir()?.join(".codex")
            };
            vec![Source {
                name: kind.clone(),
                kind,
                root,
                enabled: true,
            }]
        };
        validate_sources(&selected)?;
        Ok(selected)
    }

    pub fn validate(&self) -> Result<()> {
        if let Some(timezone) = &self.timezone {
            timezone.parse::<chrono_tz::Tz>().with_context(|| {
                format!("Unknown timezone {timezone:?}; use an IANA name such as Asia/Shanghai")
            })?;
        }
        validate_sources(&self.sources)
    }
}

struct RemoveOnDrop(PathBuf);
impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn home_dir() -> Result<PathBuf> {
    BaseDirs::new()
        .map(|base| base.home_dir().to_path_buf())
        .ok_or_else(|| anyhow!("Cannot find your home directory; use an absolute source path"))
}
fn starts_with_home(path: &Path) -> bool {
    path.components()
        .next()
        .is_some_and(|component| component.as_os_str() == "~")
}

pub fn normalize_path(path: &Path) -> Result<PathBuf> {
    if path.as_os_str().is_empty() {
        bail!("Source path cannot be empty");
    }
    #[cfg(not(windows))]
    {
        let text = path.to_string_lossy();
        let bytes = text.as_bytes();
        if text.starts_with("\\\\")
            || (bytes.len() >= 3
                && bytes[0].is_ascii_alphabetic()
                && bytes[1] == b':'
                && (bytes[2] == b'\\' || bytes[2] == b'/'))
        {
            bail!("Use a path accessible from this Linux/WSL host, such as /mnt/c/Users/<user>/.codex, instead of a Windows drive or UNC path");
        }
    }
    let expanded = if starts_with_home(path) {
        let mut components = path.components();
        components.next();
        home_dir()?.join(components.as_path())
    } else {
        path.to_path_buf()
    };
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        env::current_dir()?.join(expanded)
    };
    // Resolve existing symlinks before lexical normalization so link/.. keeps OS semantics.
    if !is_unc(&absolute) {
        if let Ok(canonical) = absolute.canonicalize() {
            return Ok(canonical);
        }
    }
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    Ok(normalized)
}

fn path_identity(path: &Path) -> Result<String> {
    let path = normalize_path(path)?;
    if is_unc(&path) {
        // Configuration validation must not connect to disabled network/WSL sources.
        return Ok(path.to_string_lossy().to_lowercase());
    }
    // Canonicalize the nearest existing parent as well, for duplicate roots not created yet.
    let mut ancestor = path.as_path();
    let mut suffix = Vec::new();
    let identity = loop {
        if let Ok(canonical) = ancestor.canonicalize() {
            let mut result = canonical;
            for component in suffix.iter().rev() {
                result.push(component);
            }
            break result;
        }
        if let Some(name) = ancestor.file_name() {
            suffix.push(name.to_owned());
        }
        let Some(parent) = ancestor.parent() else {
            // Offline drive letters and unmounted roots can still be configured.
            break path.clone();
        };
        ancestor = parent;
    };
    let text = identity.to_string_lossy().into_owned();
    #[cfg(windows)]
    let text = text.to_lowercase();
    Ok(text)
}

fn is_unc(path: &Path) -> bool {
    #[cfg(windows)]
    {
        matches!(path.components().next(), Some(Component::Prefix(prefix)) if matches!(prefix.kind(), std::path::Prefix::UNC(_, _) | std::path::Prefix::VerbatimUNC(_, _)))
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        false
    }
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
        || name == "."
        || name == ".."
    {
        bail!("Source name must contain 1–64 letters, digits, '.', '_' or '-' and cannot be '.' or '..'");
    }
    Ok(())
}
fn validate_kind(kind: &str) -> Result<()> {
    if !["windows", "wsl", "linux"].contains(&kind) {
        bail!("Unknown source kind {kind:?}; choose windows, wsl, or linux");
    }
    Ok(())
}
fn validate_sources(sources: &[Source]) -> Result<()> {
    let mut names = BTreeSet::new();
    let mut paths = BTreeSet::new();
    for source in sources {
        validate_name(&source.name)?;
        validate_kind(&source.kind)?;
        if !names.insert(&source.name) {
            bail!("Duplicate source name {:?}", source.name);
        }
        if !paths.insert(path_identity(&source.root)?) {
            bail!(
                "Source {:?} points to an already configured root: {}",
                source.name,
                source.root.display()
            );
        }
    }
    Ok(())
}

pub fn native_kind() -> String {
    if cfg!(windows) {
        "windows".into()
    } else if env::var_os("WSL_DISTRO_NAME").is_some()
        || fs::read_to_string("/proc/sys/kernel/osrelease")
            .is_ok_and(|text| text.to_ascii_lowercase().contains("microsoft"))
    {
        "wsl".into()
    } else {
        "linux".into()
    }
}

/// Discover accessible Windows candidates on WSL, without selecting or saving them.
/// Windows UNC roots are explicit: probing a stopped distribution can wake it up.
pub fn candidates() -> Vec<Source> {
    if native_kind() != "wsl" {
        return Vec::new();
    }
    let mut mounts = BTreeSet::new();
    if let Ok(contents) = fs::read_to_string("/proc/mounts") {
        for line in contents.lines() {
            let columns: Vec<_> = line.split_whitespace().collect();
            if columns.len() >= 3 && ["9p", "drvfs"].contains(&columns[2]) {
                mounts.insert(PathBuf::from(columns[1].replace("\\040", " ")));
            }
        }
    }
    // Include conventional mounted drives when mount metadata is unavailable.
    if let Ok(entries) = fs::read_dir("/mnt") {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                mounts.insert(entry.path());
            }
        }
    }
    let mut result = Vec::new();
    let mut seen = BTreeSet::new();
    for mount in mounts {
        let Ok(profiles) = fs::read_dir(mount.join("Users")) else {
            continue;
        };
        for profile in profiles.flatten() {
            let root = profile.path().join(".codex");
            if !root.is_dir() {
                continue;
            }
            let Ok(root) = normalize_path(&root) else {
                continue;
            };
            if !seen.insert(root.clone()) {
                continue;
            }
            let name = profile
                .file_name()
                .to_string_lossy()
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                        c
                    } else {
                        '-'
                    }
                })
                .collect::<String>();
            result.push(Source {
                name: format!("windows-{name}"),
                kind: "windows".into(),
                root,
                enabled: false,
            });
        }
    }
    result.sort_by(|a, b| a.root.cmp(&b.root));
    if result.len() == 1 {
        result[0].name = "windows".into();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn enabling_discovered_windows_preserves_implicit_native_source_and_persists() {
        let dir = tempdir().unwrap();
        let native = Source {
            name: "wsl".into(),
            kind: "wsl".into(),
            root: dir.path().join("wsl"),
            enabled: true,
        };
        let windows = Source {
            name: "windows".into(),
            kind: "windows".into(),
            root: dir.path().join("windows"),
            enabled: false,
        };
        let mut config = Config::default();
        let catalog = SourceCatalog::new(vec![native.clone()], vec![windows.clone()]).unwrap();
        assert!(!catalog.candidates[0].enabled);
        assert!(config.sources.is_empty());
        config
            .set_enabled_from_catalog("windows", true, catalog)
            .unwrap();
        assert_eq!(config.sources.len(), 2);
        assert_eq!(config.sources[0], native);
        assert_eq!(
            config.sources[1],
            Source {
                enabled: true,
                ..windows
            }
        );
        assert_eq!(config.select(&[], &[]).unwrap().len(), 2);
        let path = dir.path().join("config.toml");
        config.save(&path).unwrap();
        let mut saved = Config::load(&path).unwrap();
        assert_eq!(saved.sources, config.sources);
        saved.set_enabled("windows", false).unwrap();
        assert_eq!(saved.select(&[], &[]).unwrap(), vec![native]);
    }

    #[test]
    fn discovered_names_are_unique_and_do_not_replace_saved_sources() {
        let dir = tempdir().unwrap();
        let existing = Source {
            name: "windows".into(),
            kind: "windows".into(),
            root: dir.path().join("custom"),
            enabled: false,
        };
        let discovered = Source {
            name: "windows".into(),
            kind: "windows".into(),
            root: dir.path().join("discovered"),
            enabled: false,
        };
        let mut config = Config {
            sources: vec![existing.clone()],
            ..Default::default()
        };
        let catalog = SourceCatalog::new(config.sources.clone(), vec![discovered.clone()]).unwrap();
        let name = catalog.candidates[0].name.clone();
        assert_ne!(name, existing.name);
        config
            .set_enabled_from_catalog(&name, true, catalog)
            .unwrap();
        assert_eq!(config.sources[0], existing);
        assert_eq!(config.sources[1].root, discovered.root);
        assert!(config.sources[1].enabled);
    }

    #[test]
    fn catalog_deduplicates_roots_and_unknown_enable_does_not_mutate_config() {
        let dir = tempdir().unwrap();
        let native = Source {
            name: "wsl".into(),
            kind: "wsl".into(),
            root: dir.path().to_path_buf(),
            enabled: true,
        };
        let catalog = SourceCatalog::new(
            vec![native.clone()],
            vec![Source {
                name: "windows".into(),
                root: dir.path().join("."),
                ..native.clone()
            }],
        )
        .unwrap();
        assert!(catalog.candidates.is_empty());
        let mut config = Config::default();
        assert!(config
            .set_enabled_from_catalog("missing", true, catalog)
            .is_err());
        assert!(config.sources.is_empty());
    }

    #[test]
    fn source_updates_and_selection_are_independent() {
        let dir = tempdir().unwrap();
        let mut config = Config::default();
        config
            .set_source("wsl", Some("wsl"), &dir.path().join("wsl"))
            .unwrap();
        config
            .set_source("windows", Some("windows"), &dir.path().join("windows"))
            .unwrap();
        config.set_enabled("windows", false).unwrap();
        let original = config.sources[1].clone();
        config
            .set_source("wsl", None, &dir.path().join("moved"))
            .unwrap();
        assert_eq!(config.sources[1], original);
        assert_eq!(
            config
                .select(&[], &[])
                .unwrap()
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            vec!["wsl"]
        );
        assert_eq!(
            config.select(&["windows".into()], &[]).unwrap()[0],
            original
        );
        assert_eq!(
            config
                .select(&["wsl".into(), "windows".into()], &[])
                .unwrap()
                .len(),
            2
        );
        assert!(!config.sources[1].enabled);
        config
            .set_source("windows", None, &dir.path().join("other-windows"))
            .unwrap();
        assert!(!config.sources[1].enabled);
    }

    #[test]
    fn disabled_unknown_and_conflicting_selection_fail() {
        let dir = tempdir().unwrap();
        let mut config = Config::default();
        config
            .set_source("wsl", Some("wsl"), &dir.path().join("wsl"))
            .unwrap();
        config.set_enabled("wsl", false).unwrap();
        assert!(config
            .select(&[], &[])
            .unwrap_err()
            .to_string()
            .contains("No sources enabled"));
        assert!(config.select(&["unknown".into()], &[]).is_err());
        assert!(config
            .select(&["wsl".into()], &[dir.path().to_path_buf()])
            .is_err());
        let selected = config.select(&[], &[dir.path().to_path_buf()]).unwrap();
        assert_eq!(selected[0].name, "root-1");
        assert_eq!(
            selected[0].root.canonicalize().unwrap(),
            dir.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn validation_rejects_duplicates_without_mutating_the_config() {
        let dir = tempdir().unwrap();
        let mut config = Config::default();
        config
            .set_source("first", Some("linux"), dir.path())
            .unwrap();
        assert!(config
            .set_source("alias", Some("wsl"), &dir.path().join("."))
            .is_err());
        assert_eq!(config.sources.len(), 1);
        assert!(config
            .set_source("new", None, &dir.path().join("new"))
            .is_err());
        assert!(config
            .set_source("bad name", Some("linux"), &dir.path().join("new"))
            .is_err());
        assert!(config
            .set_source("new", Some("other"), &dir.path().join("new"))
            .is_err());
        assert!(config.set_enabled("missing", true).is_err());
    }

    #[test]
    fn atomic_save_can_replace_and_round_trip_missing_roots() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested/config.toml");
        let mut config = Config {
            timezone: Some("Asia/Shanghai".into()),
            ..Config::default()
        };
        config
            .set_source("wsl", Some("wsl"), &dir.path().join("not-mounted"))
            .unwrap();
        config.save(&path).unwrap();
        config.set_enabled("wsl", false).unwrap();
        config.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.sources, config.sources);
        assert_eq!(loaded.timezone, config.timezone);
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn malformed_config_and_unknown_timezone_are_errors() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert!(Config::load(&path).unwrap().sources.is_empty());
        fs::write(&path, "invalid = true").unwrap();
        assert!(Config::load(&path).is_err());
        fs::write(&path, "timezone = 'not/a-zone'").unwrap();
        assert!(Config::load(&path).is_err());
    }

    #[test]
    fn relative_manual_paths_resolve_against_config_location() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "[[sources]]\nname='local'\nkind='linux'\nroot='sessions-root'\n",
        )
        .unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.sources[0].root, dir.path().join("sessions-root"));
        assert!(config.sources[0].enabled);
    }

    #[cfg(unix)]
    #[test]
    fn missing_roots_under_symlink_parents_are_duplicate_aliases() {
        use std::os::unix::fs::symlink;
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("real")).unwrap();
        symlink(dir.path().join("real"), dir.path().join("link")).unwrap();
        let mut config = Config::default();
        config
            .set_source("real", Some("linux"), &dir.path().join("real/missing"))
            .unwrap();
        assert!(config
            .set_source("link", Some("linux"), &dir.path().join("link/missing"))
            .is_err());
    }
}
