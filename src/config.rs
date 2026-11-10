//! Persistent source configuration. Configuration never causes an unselected source to be read.
use crate::model::{AgentKind, Source};
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

pub const CONFIG_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "config_version")]
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    #[serde(default)]
    pub sources: Vec<Source>,
}
fn config_version() -> u32 {
    CONFIG_VERSION
}
impl Default for Config {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            timezone: None,
            sources: Vec::new(),
        }
    }
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
            .map(|s| Ok((s.agent, path_identity(&s.root)?)))
            .collect::<Result<BTreeSet<_>>>()?;
        let mut candidates = Vec::new();
        discovered.sort_by(|a, b| {
            a.agent
                .cmp(&b.agent)
                .then(a.root.cmp(&b.root))
                .then(a.name.cmp(&b.name))
        });
        for mut candidate in discovered {
            candidate.root = normalize_path(&candidate.root)?;
            if !roots.insert((candidate.agent, path_identity(&candidate.root)?)) {
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
    if let Some(path) = env::var_os("AGENT_USAGE_CONFIG").filter(|value| !value.is_empty()) {
        return normalize_path(Path::new(&path));
    }
    let project = ProjectDirs::from("", "", "agent-usage")
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
        if config.version != CONFIG_VERSION {
            bail!(
                "Configuration {} uses version {}; version {} is required",
                path.display(),
                config.version,
                CONFIG_VERSION
            );
        }
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
                ".agent-usage-{}-{}.tmp",
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

    pub fn set_source(
        &mut self,
        name: &str,
        agent: Option<AgentKind>,
        kind: Option<&str>,
        root: &Path,
    ) -> Result<()> {
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
            if let Some(agent) = agent {
                existing.agent = agent;
            }
            if let Some(kind) = kind {
                existing.kind = kind.to_owned();
            }
        } else {
            let agent = agent
                .ok_or_else(|| anyhow!("New source {name:?} requires --agent codex|claude-code"))?;
            let kind = kind
                .ok_or_else(|| anyhow!("New source {name:?} requires --kind wsl|windows|linux"))?;
            updated.sources.push(Source {
                name: name.to_owned(),
                agent,
                kind: kind.to_owned(),
                root,
                enabled: true,
                pricing_provider: None,
            });
        }
        updated.validate()?;
        *self = updated;
        Ok(())
    }

    pub fn set_enabled(
        &mut self,
        name: &str,
        enabled: bool,
        agent: Option<AgentKind>,
    ) -> Result<()> {
        if let Some(source) = self.sources.iter_mut().find(|source| source.name == name) {
            if agent.is_some_and(|agent| agent != source.agent) {
                bail!(
                    "Source {name:?} is {}, not {}",
                    source.agent,
                    agent.unwrap()
                );
            }
            source.enabled = enabled;
            return Ok(());
        }
        let catalog = self.source_catalog()?;
        self.set_enabled_from_catalog(name, enabled, agent, catalog)
    }

    pub fn source_catalog(&self) -> Result<SourceCatalog> {
        let sources = if self.sources.is_empty() {
            implicit_sources(&[])?
        } else {
            self.sources.clone()
        };
        SourceCatalog::new(sources, candidates())
    }

    fn set_enabled_from_catalog(
        &mut self,
        name: &str,
        enabled: bool,
        agent: Option<AgentKind>,
        catalog: SourceCatalog,
    ) -> Result<()> {
        let selected = catalog
            .sources
            .iter()
            .chain(&catalog.candidates)
            .find(|source| source.name == name)
            .ok_or_else(|| anyhow!("Unknown source {name:?}; run `agent-usage sources` and use a listed name, or configure a path with `sources set`"))?
            .clone();
        if agent.is_some_and(|agent| agent != selected.agent) {
            bail!(
                "Source {name:?} is {}, not {}",
                selected.agent,
                agent.unwrap()
            );
        }
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

    /// Pure selection, run before any transcript is read.
    pub fn select(
        &self,
        names: &[String],
        roots: &[PathBuf],
        agents: &[AgentKind],
    ) -> Result<Vec<Source>> {
        let requested: BTreeSet<AgentKind> = agents.iter().copied().collect();
        if !names.is_empty() && !roots.is_empty() {
            bail!("Use --source or --root, not both");
        }
        let mut selected = if !roots.is_empty() {
            if requested.len() > 1 {
                bail!("--root requires one --agent value (codex or claude-code)");
            }
            let agent = agents.first().copied().ok_or_else(|| {
                anyhow!("--root requires --agent codex|claude-code so the layout is known")
            })?;
            roots
                .iter()
                .enumerate()
                .map(|(index, root)| {
                    Ok(Source {
                        name: format!("root-{}", index + 1),
                        agent,
                        kind: native_kind(),
                        root: normalize_path(root)?,
                        enabled: true,
                        pricing_provider: None,
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
                    .ok_or_else(|| anyhow!("Unknown source {name:?}; run `agent-usage sources`"))?;
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
                bail!("No sources enabled; run `agent-usage sources enable <name>`");
            }
            selected
        } else {
            implicit_sources(&[])?
        };
        if !requested.is_empty() {
            selected.retain(|source| requested.contains(&source.agent));
        }
        if selected.is_empty() {
            bail!(
                "No selected sources match --agent {}; run `agent-usage sources`",
                agents
                    .iter()
                    .map(|agent| agent.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            );
        }
        validate_sources(&selected)?;
        Ok(selected)
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != CONFIG_VERSION {
            bail!(
                "Configuration version {} is unsupported; version {} is required",
                self.version,
                CONFIG_VERSION
            );
        }
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
        if !paths.insert((source.agent, path_identity(&source.root)?)) {
            bail!(
                "Source {:?} points to an already configured {} root: {}",
                source.name,
                source.agent,
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

fn env_root(agent: AgentKind) -> (&'static str, &'static str) {
    match agent {
        AgentKind::Codex => ("CODEX_HOME", ".codex"),
        AgentKind::ClaudeCode => ("CLAUDE_CONFIG_DIR", ".claude"),
    }
}

/// Native implicit roots. An explicit environment root is retained even when it
/// is currently unreadable; a missing conventional default is skipped.
fn implicit_sources(agents: &[AgentKind]) -> Result<Vec<Source>> {
    let requested: BTreeSet<AgentKind> = agents.iter().copied().collect();
    let mut sources = Vec::new();
    for agent in [AgentKind::Codex, AgentKind::ClaudeCode] {
        if !requested.is_empty() && !requested.contains(&agent) {
            continue;
        }
        let (variable, default) = env_root(agent);
        let root = if let Some(root) = env::var_os(variable).filter(|root| !root.is_empty()) {
            normalize_path(Path::new(&root))?
        } else {
            let root = home_dir()?.join(default);
            if !root.is_dir() {
                continue;
            }
            normalize_path(&root)?
        };
        sources.push(Source {
            name: agent.as_str().into(),
            agent,
            kind: native_kind(),
            root,
            enabled: true,
            pricing_provider: None,
        });
    }
    Ok(sources)
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
            let label = profile
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
            for agent in [AgentKind::Codex, AgentKind::ClaudeCode] {
                let (_, default) = env_root(agent);
                let root = profile.path().join(default);
                if !root.is_dir() {
                    continue;
                }
                let Ok(root) = normalize_path(&root) else {
                    continue;
                };
                if !seen.insert((agent, root.clone())) {
                    continue;
                }
                result.push(Source {
                    name: format!("windows-{label}-{agent}"),
                    agent,
                    kind: "windows".into(),
                    root,
                    enabled: false,
                    pricing_provider: None,
                });
            }
        }
    }
    result.sort_by(|a, b| a.root.cmp(&b.root).then(a.agent.cmp(&b.agent)));
    if result.len() == 1 {
        result[0].name = format!("windows-{}", result[0].agent);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn source(name: &str, agent: AgentKind, root: PathBuf) -> Source {
        Source {
            name: name.into(),
            agent,
            kind: "linux".into(),
            root,
            enabled: true,
            pricing_provider: None,
        }
    }

    #[test]
    fn enabling_discovered_windows_preserves_implicit_native_source_and_persists() {
        let dir = tempdir().unwrap();
        let native = source("codex", AgentKind::Codex, dir.path().join("codex"));
        let windows = Source {
            enabled: false,
            ..source(
                "windows-user-codex",
                AgentKind::Codex,
                dir.path().join("windows"),
            )
        };
        let mut config = Config::default();
        let catalog = SourceCatalog::new(vec![native.clone()], vec![windows.clone()]).unwrap();
        assert!(!catalog.candidates[0].enabled);
        assert!(config.sources.is_empty());
        config
            .set_enabled_from_catalog("windows-user-codex", true, None, catalog)
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
        assert_eq!(config.select(&[], &[], &[]).unwrap().len(), 2);
        let path = dir.path().join("config.toml");
        config.save(&path).unwrap();
        let mut saved = Config::load(&path).unwrap();
        assert_eq!(saved.sources, config.sources);
        saved
            .set_enabled("windows-user-codex", false, None)
            .unwrap();
        assert_eq!(saved.select(&[], &[], &[]).unwrap(), vec![native]);
    }

    #[test]
    fn same_root_different_agents_are_distinct_sources() {
        let dir = tempdir().unwrap();
        let mut config = Config::default();
        config
            .set_source(
                "shared-codex",
                Some(AgentKind::Codex),
                Some("linux"),
                dir.path(),
            )
            .unwrap();
        config
            .set_source(
                "shared-claude",
                Some(AgentKind::ClaudeCode),
                Some("linux"),
                dir.path(),
            )
            .unwrap();
        assert_eq!(config.sources.len(), 2);
        let selected = config.select(&[], &[], &[AgentKind::ClaudeCode]).unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].name, "shared-claude");
    }

    #[test]
    fn discovered_names_are_unique_and_do_not_replace_saved_sources() {
        let dir = tempdir().unwrap();
        let existing = source("windows", AgentKind::Codex, dir.path().join("custom"));
        let discovered = source("windows", AgentKind::Codex, dir.path().join("discovered"));
        let mut config = Config {
            sources: vec![existing.clone()],
            ..Default::default()
        };
        let catalog = SourceCatalog::new(config.sources.clone(), vec![discovered.clone()]).unwrap();
        let name = catalog.candidates[0].name.clone();
        assert_ne!(name, existing.name);
        config
            .set_enabled_from_catalog(&name, true, None, catalog)
            .unwrap();
        assert_eq!(config.sources[0], existing);
        assert_eq!(config.sources[1].root, discovered.root);
        assert!(config.sources[1].enabled);
    }

    #[test]
    fn catalog_deduplicates_roots_and_unknown_enable_does_not_mutate_config() {
        let dir = tempdir().unwrap();
        let native = source("codex", AgentKind::Codex, dir.path().to_path_buf());
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
            .set_enabled_from_catalog("missing", true, None, catalog)
            .is_err());
        assert!(config.sources.is_empty());
    }

    #[test]
    fn source_updates_and_selection_are_independent() {
        let dir = tempdir().unwrap();
        let mut config = Config::default();
        config
            .set_source(
                "wsl",
                Some(AgentKind::Codex),
                Some("wsl"),
                &dir.path().join("wsl"),
            )
            .unwrap();
        config
            .set_source(
                "windows",
                Some(AgentKind::Codex),
                Some("windows"),
                &dir.path().join("windows"),
            )
            .unwrap();
        config.set_enabled("windows", false, None).unwrap();
        let original = config.sources[1].clone();
        config
            .set_source("wsl", None, None, &dir.path().join("moved"))
            .unwrap();
        assert_eq!(config.sources[1], original);
        assert_eq!(
            config
                .select(&[], &[], &[])
                .unwrap()
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            vec!["wsl"]
        );
        assert_eq!(
            config.select(&["windows".into()], &[], &[]).unwrap()[0],
            original
        );
        assert_eq!(
            config
                .select(&["wsl".into(), "windows".into()], &[], &[])
                .unwrap()
                .len(),
            2
        );
        assert!(!config.sources[1].enabled);
        config
            .set_source("windows", None, None, &dir.path().join("other-windows"))
            .unwrap();
        assert!(!config.sources[1].enabled);
    }

    #[test]
    fn disabled_unknown_conflicting_and_agent_filtered_selection_fail() {
        let dir = tempdir().unwrap();
        let mut config = Config::default();
        config
            .set_source(
                "wsl",
                Some(AgentKind::Codex),
                Some("wsl"),
                &dir.path().join("wsl"),
            )
            .unwrap();
        config.set_enabled("wsl", false, None).unwrap();
        assert!(config
            .select(&[], &[], &[])
            .unwrap_err()
            .to_string()
            .contains("No sources enabled"));
        assert!(config.select(&["unknown".into()], &[], &[]).is_err());
        assert!(config
            .select(&["wsl".into()], &[dir.path().to_path_buf()], &[])
            .is_err());
        // --root requires one explicit agent.
        assert!(config
            .select(&[], &[dir.path().to_path_buf()], &[])
            .is_err());
        assert!(config
            .select(
                &[],
                &[dir.path().to_path_buf()],
                &[AgentKind::Codex, AgentKind::ClaudeCode]
            )
            .is_err());
        let selected = config
            .select(&[], &[dir.path().to_path_buf()], &[AgentKind::ClaudeCode])
            .unwrap();
        assert_eq!(selected[0].name, "root-1");
        assert_eq!(selected[0].agent, AgentKind::ClaudeCode);
        assert_eq!(
            selected[0].root.canonicalize().unwrap(),
            dir.path().canonicalize().unwrap()
        );
        assert!(config
            .select(&["wsl".into()], &[], &[AgentKind::ClaudeCode])
            .is_err());
    }

    #[test]
    fn validation_rejects_duplicates_without_mutating_the_config() {
        let dir = tempdir().unwrap();
        let mut config = Config::default();
        config
            .set_source("first", Some(AgentKind::Codex), Some("linux"), dir.path())
            .unwrap();
        assert!(config
            .set_source(
                "alias",
                Some(AgentKind::Codex),
                Some("wsl"),
                &dir.path().join(".")
            )
            .is_err());
        assert_eq!(config.sources.len(), 1);
        assert!(config
            .set_source("new", Some(AgentKind::Codex), None, &dir.path().join("new"))
            .is_err());
        assert!(config
            .set_source("new", None, Some("linux"), &dir.path().join("new"))
            .is_err());
        assert!(config
            .set_source(
                "bad name",
                Some(AgentKind::Codex),
                Some("linux"),
                &dir.path().join("new")
            )
            .is_err());
        assert!(config
            .set_source(
                "new",
                Some(AgentKind::Codex),
                Some("other"),
                &dir.path().join("new")
            )
            .is_err());
        assert!(config.set_enabled("missing", true, None).is_err());
        assert!(config
            .set_enabled("first", false, Some(AgentKind::ClaudeCode))
            .is_err());
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
            .set_source(
                "wsl",
                Some(AgentKind::Codex),
                Some("wsl"),
                &dir.path().join("not-mounted"),
            )
            .unwrap();
        config.save(&path).unwrap();
        config.set_enabled("wsl", false, None).unwrap();
        config.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.sources, config.sources);
        assert_eq!(loaded.timezone, config.timezone);
        assert_eq!(loaded.version, CONFIG_VERSION);
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn malformed_config_unknown_timezone_and_legacy_version_are_rejected() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert!(Config::load(&path).unwrap().sources.is_empty());
        fs::write(&path, "invalid = true").unwrap();
        assert!(Config::load(&path).is_err());
        fs::write(&path, "version=2\ntimezone = 'not/a-zone'").unwrap();
        assert!(Config::load(&path).is_err());
        fs::write(&path, "version=1\ntimezone = 'UTC'").unwrap();
        assert!(Config::load(&path)
            .unwrap_err()
            .to_string()
            .contains("version"));
        // A source must declare its agent.
        fs::write(
            &path,
            "version=2\n[[sources]]\nname='local'\nkind='linux'\nroot='x'\n",
        )
        .unwrap();
        assert!(Config::load(&path).is_err());
    }

    #[test]
    fn relative_manual_paths_resolve_against_config_location() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "version=2\n[[sources]]\nname='local'\nagent='codex'\nkind='linux'\nroot='sessions-root'\n",
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
            .set_source(
                "real",
                Some(AgentKind::Codex),
                Some("linux"),
                &dir.path().join("real/missing"),
            )
            .unwrap();
        assert!(config
            .set_source(
                "link",
                Some(AgentKind::Codex),
                Some("linux"),
                &dir.path().join("link/missing")
            )
            .is_err());
    }
}
