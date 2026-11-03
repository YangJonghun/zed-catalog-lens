use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use saphyr::{MarkedYaml, Scalar, ScalarStyle, YamlData, YamlLoader};
use saphyr_parser::{BufferedInput, Marker, Parser};
use tower_lsp_server::ls_types::{Location, Range, Uri};

use crate::package_json::{self, Duplicates, LineIndex, Node, Reference};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Manager {
    Pnpm,
    Yarn,
    Bun,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultSource {
    Catalog,
    CatalogsDefault,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BothDefaults {
    Allowed,
    RejectsFile,
    RejectsSharedPackages,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    Recursive,
    Protocol(&'static str),
}

impl Rejection {
    fn matches(self, version: &str) -> bool {
        match self {
            Rejection::Recursive => version.starts_with(package_json::CATALOG_PREFIX),
            Rejection::Protocol(protocol) => version.split(':').next() == Some(protocol),
        }
    }

    pub fn protocol(self) -> &'static str {
        match self {
            Rejection::Recursive => "catalog",
            Rejection::Protocol(protocol) => protocol,
        }
    }
}

struct Rules {
    display_name: &'static str,
    package_manager: &'static str,
    lockfiles: &'static [&'static str],
    file_name: &'static str,
    trims_name: bool,
    bare: &'static [DefaultSource],
    named_default: Option<&'static [DefaultSource]>,
    both_defaults: BothDefaults,
    rejects: &'static [Rejection],
}

const PNPM: Rules = Rules {
    display_name: "pnpm",
    package_manager: "pnpm",
    lockfiles: &["pnpm-lock.yaml"],
    file_name: "pnpm-workspace.yaml",
    trims_name: true,
    bare: &[DefaultSource::Catalog, DefaultSource::CatalogsDefault],
    named_default: Some(&[DefaultSource::Catalog, DefaultSource::CatalogsDefault]),
    both_defaults: BothDefaults::RejectsFile,
    rejects: &[
        Rejection::Recursive,
        Rejection::Protocol("link"),
        Rejection::Protocol("file"),
    ],
};

const YARN: Rules = Rules {
    display_name: "Yarn",
    package_manager: "yarn",
    lockfiles: &["yarn.lock"],
    file_name: ".yarnrc.yml",
    trims_name: false,
    bare: &[DefaultSource::Catalog],
    named_default: None,
    both_defaults: BothDefaults::Allowed,
    rejects: &[],
};

const BUN: Rules = Rules {
    display_name: "Bun",
    package_manager: "bun",
    lockfiles: &["bun.lock", "bun.lockb"],
    file_name: "package.json",
    trims_name: true,
    bare: &[DefaultSource::Catalog, DefaultSource::CatalogsDefault],
    named_default: Some(&[DefaultSource::CatalogsDefault, DefaultSource::Catalog]),
    both_defaults: BothDefaults::RejectsSharedPackages,
    rejects: &[],
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lookup<'a> {
    Default(&'static [DefaultSource]),
    Named(&'a str),
}

impl Lookup<'_> {
    pub fn name(self) -> CatalogName {
        match self {
            Lookup::Default(_) => CatalogName::Default,
            Lookup::Named(name) => CatalogName::Named(name.to_owned()),
        }
    }
}

impl Manager {
    pub const DISCOVERY_ORDER: [Manager; 3] = [Manager::Pnpm, Manager::Yarn, Manager::Bun];

    fn rules(self) -> &'static Rules {
        match self {
            Manager::Pnpm => &PNPM,
            Manager::Yarn => &YARN,
            Manager::Bun => &BUN,
        }
    }

    pub fn name(self) -> &'static str {
        self.rules().display_name
    }

    pub fn file_name(self) -> &'static str {
        self.rules().file_name
    }

    pub fn lookup(self, raw: &str) -> Lookup<'_> {
        let rules = self.rules();
        let name = if rules.trims_name { raw.trim() } else { raw };
        match (name, rules.named_default) {
            ("", _) => Lookup::Default(rules.bare),
            ("default", Some(sources)) => Lookup::Default(sources),
            _ => Lookup::Named(name),
        }
    }

    fn definition_in(self, dir: &Path, buffers: Buffers) -> Option<PathBuf> {
        let path = dir.join(self.file_name());
        let present = match self {
            Manager::Pnpm => path.is_file(),
            Manager::Yarn => path.is_file() && dir.join("package.json").is_file(),
            Manager::Bun => read_text(&path, buffers).is_some_and(|(_, text)| {
                let root = package_json::parse(&text, Duplicates::All);
                bun_catalog_root(&root).is_some()
            }),
        };
        present.then_some(path)
    }

    fn parse_catalogs(self, text: &str) -> Result<Catalogs, String> {
        match self {
            Manager::Pnpm => parse_yaml_catalogs(text, YamlSchema::Core),
            Manager::Yarn => parse_yaml_catalogs(text, YamlSchema::Failsafe)
                .map(Catalogs::without_empty_versions),
            Manager::Bun => parse_bun_catalogs(text),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CatalogName {
    Default,
    Named(String),
}

impl fmt::Display for CatalogName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CatalogName::Default => f.write_str("default"),
            CatalogName::Named(name) => f.write_str(name),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub version: String,
    pub range: Range,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalog {
    pub entries: BTreeMap<String, Entry>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalogs {
    pub catalog: Option<Catalog>,
    pub named: BTreeMap<String, Catalog>,
}

impl Catalogs {
    fn without_empty_versions(mut self) -> Self {
        for catalog in self.catalog.iter_mut().chain(self.named.values_mut()) {
            catalog.entries.retain(|_, entry| !entry.version.is_empty());
        }
        self
    }

    fn source(&self, source: DefaultSource) -> Option<&Catalog> {
        match source {
            DefaultSource::Catalog => self.catalog.as_ref(),
            DefaultSource::CatalogsDefault => self.named.get("default"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Buffer { version: i32 },
    Disk { mtime: SystemTime, len: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogState {
    Ok(Catalogs),
    Stale(Catalogs),
    Unavailable,
}

#[derive(Debug, Clone)]
pub struct Workspace {
    pub manager: Manager,
    pub uri: Uri,
    pub source: Source,
    pub state: CatalogState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub version: String,
    pub location: Location,
    pub catalog: CatalogName,
    pub file: &'static str,
    pub ambiguous: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    Resolved(Resolved),
    MissingEntry {
        catalog: CatalogName,
        file: &'static str,
    },
    MissingCatalog {
        name: String,
        file: &'static str,
    },
    InvalidEntry {
        catalog: CatalogName,
        file: &'static str,
        rejection: Rejection,
    },
    NoCatalogs,
}

impl Workspace {
    pub fn file_name(&self) -> &'static str {
        self.manager.file_name()
    }

    pub fn catalogs(&self) -> Option<&Catalogs> {
        match &self.state {
            CatalogState::Ok(catalogs) | CatalogState::Stale(catalogs) => Some(catalogs),
            CatalogState::Unavailable => None,
        }
    }

    pub fn resolve(&self, reference: &Reference) -> Resolution {
        let Some(catalogs) = self.catalogs() else {
            return Resolution::NoCatalogs;
        };
        let file = self.file_name();
        let lookup = self.manager.lookup(&reference.raw_catalog);
        let Some(entry) = self.find(catalogs, lookup, &reference.package) else {
            return match lookup {
                Lookup::Named(name) if !catalogs.named.contains_key(name) => {
                    Resolution::MissingCatalog {
                        name: name.to_owned(),
                        file,
                    }
                }
                _ => Resolution::MissingEntry {
                    catalog: lookup.name(),
                    file,
                },
            };
        };
        let rules = self.manager.rules();
        if let Some(&rejection) = rules.rejects.iter().find(|r| r.matches(&entry.version)) {
            return Resolution::InvalidEntry {
                catalog: lookup.name(),
                file,
                rejection,
            };
        }
        Resolution::Resolved(Resolved {
            version: entry.version.clone(),
            location: Location::new(self.uri.clone(), entry.range),
            ambiguous: matches!(lookup, Lookup::Default(_))
                && self.declared_on_both_default_sides(catalogs, &reference.package),
            catalog: lookup.name(),
            file,
        })
    }

    fn find<'a>(&self, catalogs: &'a Catalogs, lookup: Lookup, package: &str) -> Option<&'a Entry> {
        match lookup {
            Lookup::Default(sources) => sources
                .iter()
                .find_map(|&source| catalogs.source(source)?.entries.get(package)),
            Lookup::Named(name) => catalogs.named.get(name)?.entries.get(package),
        }
    }

    fn declared_on_both_default_sides(&self, catalogs: &Catalogs, package: &str) -> bool {
        self.manager.rules().both_defaults == BothDefaults::RejectsSharedPackages
            && [DefaultSource::Catalog, DefaultSource::CatalogsDefault]
                .into_iter()
                .all(|source| {
                    catalogs
                        .source(source)
                        .is_some_and(|catalog| catalog.entries.contains_key(package))
                })
    }

    pub fn rejects_both_defaults(&self) -> bool {
        self.manager.rules().both_defaults == BothDefaults::RejectsFile
            && self.catalogs().is_some_and(|catalogs| {
                catalogs.catalog.is_some() && catalogs.named.contains_key("default")
            })
    }

    pub fn catalog_names(&self) -> Vec<CatalogName> {
        let Some(catalogs) = self.catalogs() else {
            return Vec::new();
        };
        let bare = self.manager.lookup("");
        let mut names = Vec::new();
        if self.has_entries(catalogs, bare) {
            names.push(CatalogName::Default);
        }
        names.extend(
            catalogs
                .named
                .keys()
                .map(|name| self.manager.lookup(name))
                .filter(|lookup| {
                    matches!(lookup, Lookup::Named(_)) && self.has_entries(catalogs, *lookup)
                })
                .map(Lookup::name),
        );
        names
    }

    fn has_entries(&self, catalogs: &Catalogs, lookup: Lookup) -> bool {
        match lookup {
            Lookup::Default(sources) => sources.iter().any(|&source| {
                catalogs
                    .source(source)
                    .is_some_and(|catalog| !catalog.entries.is_empty())
            }),
            Lookup::Named(name) => catalogs
                .named
                .get(name)
                .is_some_and(|catalog| !catalog.entries.is_empty()),
        }
    }

    pub fn entry(&self, name: &CatalogName, package: &str) -> Option<&Entry> {
        let lookup = match name {
            CatalogName::Default => self.manager.lookup(""),
            CatalogName::Named(name) => Lookup::Named(name),
        };
        self.find(self.catalogs()?, lookup, package)
    }

    fn parse(
        manager: Manager,
        path: PathBuf,
        source: Source,
        text: &str,
        previous: Option<&Workspace>,
    ) -> Option<Workspace> {
        let uri = Uri::from_file_path(&path)?;
        let state = match manager.parse_catalogs(text) {
            Ok(catalogs) => CatalogState::Ok(catalogs),
            Err(error) => {
                tracing::debug!(?path, error, "workspace file does not parse");
                match previous.and_then(Workspace::catalogs) {
                    Some(last_good) => CatalogState::Stale(last_good.clone()),
                    None => CatalogState::Unavailable,
                }
            }
        };
        Some(Workspace {
            manager,
            uri,
            source,
            state,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Buffer {
    pub version: i32,
    pub text: Arc<str>,
}

pub type Buffers<'a> = &'a dyn Fn(&Path) -> Option<Buffer>;

fn current(path: &Path, buffers: Buffers) -> Option<(Source, Option<Arc<str>>)> {
    if let Some(buffer) = buffers(path) {
        let source = Source::Buffer {
            version: buffer.version,
        };
        return Some((source, Some(buffer.text)));
    }
    let metadata = std::fs::metadata(path).ok()?;
    let source = Source::Disk {
        mtime: metadata.modified().ok()?,
        len: metadata.len(),
    };
    Some((source, None))
}

fn text(path: &Path, at_hand: Option<Arc<str>>) -> Option<Arc<str>> {
    at_hand.or_else(|| Some(std::fs::read_to_string(path).ok()?.into()))
}

fn read_text(path: &Path, buffers: Buffers) -> Option<(Source, Arc<str>)> {
    let (source, at_hand) = current(path, buffers)?;
    Some((source, text(path, at_hand)?))
}

pub fn discover(package_json: &Path, buffers: Buffers) -> Option<(Manager, PathBuf)> {
    let mut dir = package_json.parent()?;
    loop {
        let preferred = named_manager(dir, buffers);
        let others = Manager::DISCOVERY_ORDER
            .into_iter()
            .filter(|&manager| Some(manager) != preferred);
        for manager in preferred.into_iter().chain(others) {
            if let Some(path) = manager.definition_in(dir, buffers) {
                return Some((manager, path));
            }
        }
        dir = dir.parent()?;
    }
}

fn named_manager(dir: &Path, buffers: Buffers) -> Option<Manager> {
    let declared = read_text(&dir.join("package.json"), buffers).and_then(|(_, text)| {
        let root = package_json::parse(&text, Duplicates::Last);
        let Some(Node::Str(spec)) = root.field("packageManager") else {
            return None;
        };
        let name = spec.value.split('@').next()?;
        Manager::DISCOVERY_ORDER
            .into_iter()
            .find(|manager| manager.rules().package_manager == name)
    });
    declared.or_else(|| {
        Manager::DISCOVERY_ORDER.into_iter().find(|manager| {
            manager
                .rules()
                .lockfiles
                .iter()
                .any(|lockfile| dir.join(lockfile).is_file())
        })
    })
}

#[derive(Default)]
pub struct WorkspaceCache {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    discovery: Option<HashMap<PathBuf, Option<(Manager, PathBuf)>>>,
    loaded: HashMap<PathBuf, Arc<Workspace>>,
}

impl WorkspaceCache {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                discovery: Some(HashMap::new()),
                loaded: HashMap::new(),
            }),
        }
    }

    pub fn disable_discovery_cache(&self) {
        self.lock().discovery = None;
    }

    pub fn forget_discovery(&self, dir: &Path) {
        if let Some(discovery) = &mut self.lock().discovery {
            discovery.retain(|package_json, _| !package_json.starts_with(dir));
        }
    }

    pub fn is_definition(&self, path: &Path) -> bool {
        self.lock().loaded.contains_key(path)
    }

    pub fn workspace_for(&self, package_json: &Path, buffers: Buffers) -> Option<Arc<Workspace>> {
        let mut inner = self.lock();
        let cached = inner
            .discovery
            .as_ref()
            .and_then(|discovery| discovery.get(package_json).cloned());
        let found = match cached {
            Some(found) => found,
            None => {
                let found = discover(package_json, buffers);
                if let Some(discovery) = &mut inner.discovery {
                    discovery.insert(package_json.to_owned(), found.clone());
                }
                found
            }
        };
        let (manager, path) = found?;
        let latest = current(&path, buffers);
        let previous = inner.loaded.get(&path).cloned();
        if let (Some(previous), Some((source, _))) = (&previous, &latest)
            && previous.source == *source
        {
            return Some(previous.clone());
        }
        let Some((source, text)) =
            latest.and_then(|(source, at_hand)| Some((source, text(&path, at_hand)?)))
        else {
            inner.loaded.remove(&path);
            if let Some(discovery) = &mut inner.discovery {
                discovery.remove(package_json);
            }
            return None;
        };
        let workspace = Arc::new(Workspace::parse(
            manager,
            path.clone(),
            source,
            &text,
            previous.as_deref(),
        )?);
        inner.loaded.insert(path, workspace.clone());
        Some(workspace)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn parse_bun_catalogs(text: &str) -> Result<Catalogs, String> {
    let root = package_json::parse(text, Duplicates::All);
    let Node::Object(_) = root else {
        return Err("package.json is not an object".to_owned());
    };
    let index = LineIndex::new(text);
    let mut catalogs = Catalogs::default();
    let Some(holder) = bun_catalog_root(&root) else {
        return Ok(catalogs);
    };
    if let Some(catalog) = holder.field("catalog") {
        bun_append(catalogs.catalog.get_or_insert_default(), catalog, &index);
    }
    for (name, entries) in holder
        .field("catalogs")
        .map(Node::fields)
        .unwrap_or_default()
    {
        // bun files a catalog with an empty name under the default one.
        let catalog = if name.value.is_empty() {
            catalogs.catalog.get_or_insert_default()
        } else {
            catalogs.named.entry(name.value.clone()).or_default()
        };
        bun_append(catalog, entries, &index);
    }
    Ok(catalogs)
}

fn bun_append(catalog: &mut Catalog, entries: &Node, index: &LineIndex) {
    for (package, value) in entries.fields() {
        // bun reports a repeated package and keeps the first one.
        if let Node::Str(version) = value {
            catalog
                .entries
                .entry(package.value.clone())
                .or_insert_with(|| bun_entry(version, index));
        }
    }
}

fn bun_entry(version: &package_json::Str, index: &LineIndex) -> Entry {
    Entry {
        version: version.value.clone(),
        range: index.range(version.start, version.end),
    }
}

/// bun reads `catalog`/`catalogs` from `workspaces` first and falls back to
/// the root only when a `workspaces` field exists without them.
fn bun_catalog_root(root: &Node) -> Option<&Node> {
    let workspaces = root.field("workspaces")?;
    [workspaces, root]
        .into_iter()
        .find(|node| node.field("catalog").is_some() || node.field("catalogs").is_some())
}

type Yaml<'a> = MarkedYaml<'a>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum YamlSchema {
    /// pnpm: `18`, `1.0`, `true` are numbers and booleans, which pnpm rejects
    /// as versions, and a duplicate key is an error.
    Core,
    /// yarn reads `.yarnrc.yml` with js-yaml's FAILSAFE_SCHEMA, where they stay
    /// strings, and its `json` option, where the last duplicate key wins.
    Failsafe,
}

struct YamlSource<'a> {
    text: &'a str,
    index: LineIndex<'a>,
    skipped: usize,
    schema: YamlSchema,
}

impl YamlSource<'_> {
    fn offset(&self, marker: &Marker) -> Option<usize> {
        let line = marker.line().checked_sub(1)?;
        let skipped = if line == 0 { self.skipped } else { 0 };
        self.index.offset_at_char(line, marker.col() + skipped)
    }
}

fn parse_yaml_catalogs(text: &str, schema: YamlSchema) -> Result<Catalogs, String> {
    let body = text
        .strip_prefix(package_json::BYTE_ORDER_MARK)
        .unwrap_or(text);
    let mut parser = Parser::new(BufferedInput::new(body.chars()));
    let mut loader: YamlLoader<Yaml> = YamlLoader::default();
    loader.early_parse(false);
    loader.allow_duplicate_keys(schema == YamlSchema::Failsafe);
    parser
        .load(&mut loader, true)
        .map_err(|error| error.to_string())?;
    if let Some(error) = loader.error() {
        return Err(error.to_string());
    }
    let docs = loader.into_documents();
    let source = YamlSource {
        text,
        index: LineIndex::new(text),
        skipped: usize::from(body.len() < text.len()),
        schema,
    };
    let mut catalogs = Catalogs::default();
    let Some(root) = docs.first() else {
        return Ok(catalogs);
    };
    let Some(root) = root.data.as_mapping() else {
        return Err("document is not a mapping".to_owned());
    };
    for (key, value) in root {
        match key_text(key).as_deref() {
            Some("catalog") => catalogs.catalog = yaml_catalog(value, &source),
            Some("catalogs") => {
                let Some(named) = value.data.as_mapping() else {
                    continue;
                };
                for (name, entries) in named {
                    let Some(name) = key_text(name) else { continue };
                    if let Some(catalog) = yaml_catalog(entries, &source) {
                        catalogs.named.insert(name, catalog);
                    }
                }
            }
            _ => {}
        }
    }
    Ok(catalogs)
}

fn yaml_catalog(node: &Yaml, source: &YamlSource) -> Option<Catalog> {
    let entries = yaml_entries(node, source)?;
    Some(Catalog {
        entries: entries
            .into_iter()
            .filter_map(|(package, entry)| Some((package, entry?)))
            .collect(),
    })
}

/// Explicit keys win over `<<` merges, and earlier merge sources over later ones.
fn yaml_entries(node: &Yaml, source: &YamlSource) -> Option<BTreeMap<String, Option<Entry>>> {
    let mapping = node.data.as_mapping()?;
    let mut entries = BTreeMap::new();
    let mut merged = Vec::new();
    for (key, value) in mapping {
        let Some(package) = key_text(key) else {
            continue;
        };
        if package == "<<" {
            match &value.data {
                YamlData::Sequence(items) => merged.extend(items),
                _ => merged.push(value),
            }
            continue;
        }
        entries.insert(package, yaml_entry(value, source));
    }
    for node in merged {
        for (package, entry) in yaml_entries(node, source).unwrap_or_default() {
            entries.entry(package).or_insert(entry);
        }
    }
    Some(entries)
}

fn yaml_entry(value: &Yaml, source: &YamlSource) -> Option<Entry> {
    let YamlData::Representation(text, style, tag) = &value.data else {
        return None;
    };
    let version = match Scalar::parse_from_cow_and_metadata(text.clone(), *style, tag.as_ref())? {
        Scalar::Null => return None,
        Scalar::String(version) => version.into_owned(),
        _ if source.schema == YamlSchema::Failsafe => text.to_string(),
        _ => return None,
    };
    let block = matches!(style, ScalarStyle::Literal | ScalarStyle::Folded);
    let version = if block {
        version.trim_end_matches('\n').to_owned()
    } else {
        version
    };
    let range = source
        .offset(&value.span.start)
        .map_or_else(Range::default, |span_start| {
            let text = source.text;
            let span_end = text[span_start..]
                .char_indices()
                .nth(value.span.len())
                .map_or(text.len(), |(i, _)| span_start + i);
            let start = content_start(text, span_start, block).min(span_end);
            let end = match text[start..].chars().next() {
                // A quoted scalar's span runs on over trailing blanks and a comment.
                Some(quote @ ('"' | '\'')) => quoted_end(text, start, quote),
                // A block scalar's span runs on over trailing blank lines.
                _ => start + text[start..span_end].trim_end().len(),
            };
            source.index.range(start, end)
        });
    Some(Entry { version, range })
}

fn key_text(key: &Yaml) -> Option<String> {
    match &key.data {
        YamlData::Representation(text, _, _) => Some(text.to_string()),
        YamlData::Value(saphyr::Scalar::String(text)) => Some(text.to_string()),
        YamlData::Sequence(items) if items.len() == 1 => key_text(&items[0]),
        _ => None,
    }
}

/// saphyr starts a span at the node's anchor or tag, and a block scalar's
/// content begins after its header line.
fn content_start(text: &str, start: usize, block: bool) -> usize {
    let mut at = start;
    while text[at..].starts_with(['&', '!']) {
        at += text[at..]
            .find(char::is_whitespace)
            .unwrap_or(text.len() - at);
        at = skip_separation(text, at);
    }
    if !block {
        return at;
    }
    // Without properties the span already starts at the content, which may
    // itself begin with `&` or `!`.
    if at == start || !text[at..].starts_with(['|', '>']) {
        return start;
    }
    let header_end = text[at..].find('\n').map_or(text.len(), |i| at + i);
    text.len() - text[header_end..].trim_start().len()
}

fn skip_separation(text: &str, mut at: usize) -> usize {
    loop {
        at = text.len() - text[at..].trim_start().len();
        if !text[at..].starts_with('#') {
            return at;
        }
        at += text[at..].find('\n').unwrap_or(text.len() - at);
    }
}

fn quoted_end(text: &str, start: usize, quote: char) -> usize {
    let mut chars = text[start + 1..].char_indices().peekable();
    let mut escaped = false;
    while let Some((i, ch)) = chars.next() {
        if quote == '"' && escaped {
            escaped = false;
        } else if quote == '"' && ch == '\\' {
            escaped = true;
        } else if ch == quote {
            if quote == '\'' && chars.next_if(|&(_, next)| next == '\'').is_some() {
                continue;
            }
            return start + 1 + i + 1;
        }
    }
    text.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp_server::ls_types::Position;

    fn reference(package: &str, raw_catalog: &str) -> Reference {
        Reference {
            key: package.to_owned(),
            package: package.to_owned(),
            raw_catalog: raw_catalog.to_owned(),
            range: Range::default(),
            entry: Range::default(),
        }
    }

    fn workspace(manager: Manager, text: &str) -> Workspace {
        Workspace::parse(
            manager,
            PathBuf::from("/ws").join(manager.file_name()),
            Source::Buffer { version: 1 },
            text,
            None,
        )
        .unwrap()
    }

    fn entry_range(text: &str, needle: &str) -> Range {
        let index = LineIndex::new(text);
        let start = text.find(needle).unwrap();
        index.range(start, start + needle.len())
    }

    #[test]
    fn named_catalog_lookups_report_missing_entries_and_catalogs() {
        let text = "catalog:\n  react: ^18.3.1\ncatalogs:\n  react19:\n    react: ^19.0.0\n";
        let ws = workspace(Manager::Pnpm, text);
        match ws.resolve(&reference("react", "react19")) {
            Resolution::Resolved(r) => {
                assert!(!r.ambiguous);
                assert_eq!(r.catalog, CatalogName::Named("react19".into()));
                assert_eq!(r.location.range, entry_range(text, "^19.0.0"));
                assert_eq!(r.location.uri.path().as_str(), "/ws/pnpm-workspace.yaml");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            ws.resolve(&reference("vue", "")),
            Resolution::MissingEntry {
                catalog: CatalogName::Default,
                file: "pnpm-workspace.yaml"
            }
        );
        assert_eq!(
            ws.resolve(&reference("react", "react17")),
            Resolution::MissingCatalog {
                name: "react17".into(),
                file: "pnpm-workspace.yaml"
            }
        );
        assert_eq!(
            ws.catalog_names(),
            vec![CatalogName::Default, CatalogName::Named("react19".into())]
        );
    }

    #[test]
    fn catalog_names_follow_each_managers_default() {
        let text = "catalog:\n  react: ^18.3.1\ncatalogs:\n  default:\n    react: ^17.0.2\n";
        assert_eq!(
            workspace(Manager::Pnpm, text).catalog_names(),
            vec![CatalogName::Default]
        );
        assert_eq!(
            workspace(Manager::Yarn, text).catalog_names(),
            vec![CatalogName::Default, CatalogName::Named("default".into())]
        );
    }

    #[test]
    fn bun_trims_catalog_names_and_reads_workspaces_catalog() {
        let text = r#"{
  "workspaces": {
    "catalog": { "react": "^19.0.0" },
    "catalogs": { "legacy": { "react": "^18.3.1" } }
  }
}"#;
        let ws = workspace(Manager::Bun, text);
        match ws.resolve(&reference("react", " ")) {
            Resolution::Resolved(r) => {
                assert_eq!(r.version, "^19.0.0");
                assert_eq!(r.catalog, CatalogName::Default);
                assert_eq!(r.location.range, entry_range(text, "\"^19.0.0\""));
            }
            other => panic!("{other:?}"),
        }
        match ws.resolve(&reference("react", " legacy ")) {
            Resolution::Resolved(r) => assert_eq!(r.version, "^18.3.1"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn bun_root_mid_edit_keeps_its_catalog() {
        let text = "{\n  \"workspaces\": [],\n  \"catalog\": {\n    \"react\": \"^19.0.0\",\n    \"vue\": \"^3.\n";
        let ws = workspace(Manager::Bun, text);
        match ws.resolve(&reference("react", "")) {
            Resolution::Resolved(r) => assert_eq!(r.version, "^19.0.0"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn yarn_flow_sequence_keys_and_quoted_values() {
        let text = "catalog:\n  [\"@types/react\"]: \"^18.3.12\"\n  lodash: '^4.17.21' # pinned\n";
        let ws = workspace(Manager::Yarn, text);
        match ws.resolve(&reference("@types/react", "")) {
            Resolution::Resolved(r) => {
                assert_eq!(r.version, "^18.3.12");
                assert_eq!(r.location.range, entry_range(text, "\"^18.3.12\""));
            }
            other => panic!("{other:?}"),
        }
        match ws.resolve(&reference("lodash", "")) {
            Resolution::Resolved(r) => {
                assert_eq!(r.version, "^4.17.21");
                assert_eq!(r.location.range, entry_range(text, "'^4.17.21'"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn yaml_plain_scalar_stops_at_comment_and_flow_delimiters() {
        let text = "catalog:\n  react: ^18.3.1\t# tab comment\ncatalogs:\n  flow: { vue: ^3.4.0, pinia: ^2.1.0 }\n";
        let ws = workspace(Manager::Pnpm, text);
        let catalogs = ws.catalogs().unwrap();
        let react = &catalogs.catalog.as_ref().unwrap().entries["react"];
        assert_eq!(react.version, "^18.3.1");
        assert_eq!(react.range, entry_range(text, "^18.3.1"));
        let flow = &catalogs.named["flow"].entries;
        assert_eq!(flow["vue"].range, entry_range(text, "^3.4.0"));
        assert_eq!(flow["pinia"].range, entry_range(text, "^2.1.0"));
    }

    #[test]
    fn yarn_numbers_keep_their_source_text() {
        let text = "catalog:\n  a: 19\n  b: 1.10\n";
        let ws = workspace(Manager::Yarn, text);
        let entries = &ws.catalogs().unwrap().catalog.as_ref().unwrap().entries;
        assert_eq!(entries["a"].version, "19");
        assert_eq!(entries["b"].version, "1.10");
    }

    #[test]
    fn yaml_aliases_point_at_their_use_and_merge_keys_at_the_anchor() {
        let text = "base: &base\n  react: ^18.3.1\nshared: &v ^1.0.0\ncatalog:\n  <<: *base\n  lodash: *v\n";
        let ws = workspace(Manager::Pnpm, text);
        let entries = &ws.catalogs().unwrap().catalog.as_ref().unwrap().entries;
        assert_eq!(entries["react"].version, "^18.3.1");
        assert_eq!(entries["react"].range, entry_range(text, "^18.3.1"));
        assert_eq!(entries["lodash"].version, "^1.0.0");
        assert_eq!(entries["lodash"].range, entry_range(text, "*v"));
    }

    #[test]
    fn yaml_multiline_scalars_and_escapes() {
        let text = "catalog:\n  a: >-\n    ^1.0.0\n  b: \"^2.0.0\\t\"\n  c: 'it''s'\n";
        let ws = workspace(Manager::Pnpm, text);
        let entries = &ws.catalogs().unwrap().catalog.as_ref().unwrap().entries;
        assert_eq!(entries["a"].version, "^1.0.0");
        assert_eq!(entries["a"].range, entry_range(text, "^1.0.0"));
        assert_eq!(entries["b"].version, "^2.0.0\t");
        assert_eq!(entries["b"].range, entry_range(text, "\"^2.0.0\\t\""));
        assert_eq!(entries["c"].version, "it's");
        assert_eq!(entries["c"].range, entry_range(text, "'it''s'"));
    }

    #[test]
    fn yaml_positions_are_utf16_after_multibyte_text() {
        let text = "# 설명 🔭\ncatalog:\n  react: ^18.3.1 # 한글\n";
        let ws = workspace(Manager::Pnpm, text);
        let react = &ws.catalogs().unwrap().catalog.as_ref().unwrap().entries["react"];
        assert_eq!(react.range, entry_range(text, "^18.3.1"));
        assert_eq!(react.range.start, Position::new(2, 9));
    }

    #[test]
    fn yaml_definitions_count_lone_carriage_returns_as_line_breaks() {
        let text = "catalog:\r  vue: ^3.4.0\r\n  react: ^18.2.0\r";
        let ws = workspace(Manager::Pnpm, text);
        let entries = &ws.catalogs().unwrap().catalog.as_ref().unwrap().entries;
        assert_eq!(
            entries["vue"].range,
            Range::new(Position::new(1, 7), Position::new(1, 13))
        );
        assert_eq!(
            entries["react"].range,
            Range::new(Position::new(2, 9), Position::new(2, 16))
        );
    }

    #[test]
    fn broken_yaml_keeps_last_good_or_reports_unavailable() {
        let good = workspace(Manager::Pnpm, "catalog:\n  react: ^18.3.1\n");
        let broken = "catalog:\n  react: ^18.3.1\n  - bad\n";
        let stale = Workspace::parse(
            Manager::Pnpm,
            PathBuf::from("/ws/pnpm-workspace.yaml"),
            Source::Buffer { version: 2 },
            broken,
            Some(&good),
        )
        .unwrap();
        assert!(matches!(stale.state, CatalogState::Stale(_)));
        match stale.resolve(&reference("react", "")) {
            Resolution::Resolved(r) => assert_eq!(r.version, "^18.3.1"),
            other => panic!("{other:?}"),
        }
        let fresh = workspace(Manager::Pnpm, broken);
        assert_eq!(fresh.state, CatalogState::Unavailable);
        assert_eq!(
            fresh.resolve(&reference("react", "")),
            Resolution::NoCatalogs
        );
    }

    #[test]
    fn discovery_walks_up_and_prefers_pnpm_then_yarn_then_bun() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let app = root.join("packages").join("app");
        std::fs::create_dir_all(&app).unwrap();
        let no_buffers = |_: &Path| None;
        let package_json = app.join("package.json");
        std::fs::write(&package_json, "{}").unwrap();
        assert_eq!(discover(&package_json, &no_buffers), None);

        std::fs::write(
            root.join("package.json"),
            r#"{ "workspaces": { "catalog": {} } }"#,
        )
        .unwrap();
        assert_eq!(
            discover(&package_json, &no_buffers),
            Some((Manager::Bun, root.join("package.json")))
        );
        std::fs::write(root.join(".yarnrc.yml"), "catalog: {}\n").unwrap();
        assert_eq!(
            discover(&package_json, &no_buffers),
            Some((Manager::Yarn, root.join(".yarnrc.yml")))
        );
        std::fs::write(root.join("pnpm-workspace.yaml"), "catalog: {}\n").unwrap();
        assert_eq!(
            discover(&package_json, &no_buffers),
            Some((Manager::Pnpm, root.join("pnpm-workspace.yaml")))
        );
    }

    #[test]
    fn discovery_reads_package_manager_from_an_open_buffer() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let app = root.join("app");
        std::fs::create_dir_all(&app).unwrap();
        let bun_root = r#"{ "workspaces": { "catalog": { "react": "^19.0.0" } } }"#;
        std::fs::write(root.join("package.json"), bun_root).unwrap();
        std::fs::write(root.join("pnpm-workspace.yaml"), "catalog: {}\n").unwrap();
        let package_json = app.join("package.json");
        std::fs::write(&package_json, "{}").unwrap();
        let no_buffers = |_: &Path| None;
        assert_eq!(
            discover(&package_json, &no_buffers),
            Some((Manager::Pnpm, root.join("pnpm-workspace.yaml")))
        );
        let edited = |path: &Path| {
            (path == root.join("package.json")).then(|| Buffer {
                version: 2,
                text: r#"{ "packageManager": "bun@1.3.0", "workspaces": { "catalog": { "react": "^19.0.0" } } }"#.into(),
            })
        };
        assert_eq!(
            discover(&package_json, &edited),
            Some((Manager::Bun, root.join("package.json")))
        );
    }

    #[test]
    fn yarn_requires_a_sibling_package_json() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let app = root.join("app");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(root.join(".yarnrc.yml"), "catalog: {}\n").unwrap();
        let package_json = app.join("package.json");
        std::fs::write(&package_json, "{}").unwrap();
        let no_buffers = |_: &Path| None;
        assert_eq!(discover(&package_json, &no_buffers), None);
        std::fs::write(root.join("package.json"), "{}").unwrap();
        assert_eq!(
            discover(&package_json, &no_buffers),
            Some((Manager::Yarn, root.join(".yarnrc.yml")))
        );
    }

    #[test]
    fn cache_prefers_buffer_and_rereads_disk_only_when_changed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::create_dir_all(&root).unwrap();
        let definition = root.join("pnpm-workspace.yaml");
        std::fs::write(&definition, "catalog:\n  react: ^18.0.0\n").unwrap();
        let package_json = root.join("package.json");
        std::fs::write(&package_json, "{}").unwrap();
        let cache = WorkspaceCache::new();
        let no_buffers = |_: &Path| None;

        let first = cache.workspace_for(&package_json, &no_buffers).unwrap();
        let again = cache.workspace_for(&package_json, &no_buffers).unwrap();
        assert!(Arc::ptr_eq(&first, &again));

        let buffered = |path: &Path| {
            (path == definition).then(|| Buffer {
                version: 7,
                text: "catalog:\n  react: ^18.9.9\n".into(),
            })
        };
        let from_buffer = cache.workspace_for(&package_json, &buffered).unwrap();
        assert_eq!(from_buffer.source, Source::Buffer { version: 7 });
        match from_buffer.resolve(&reference("react", "")) {
            Resolution::Resolved(r) => assert_eq!(r.version, "^18.9.9"),
            other => panic!("{other:?}"),
        }

        std::fs::write(&definition, "catalog:\n  react: ^18.1.0\n").unwrap();
        let reread = cache.workspace_for(&package_json, &no_buffers).unwrap();
        match reread.resolve(&reference("react", "")) {
            Resolution::Resolved(r) => assert_eq!(r.version, "^18.1.0"),
            other => panic!("{other:?}"),
        }

        std::fs::remove_file(&definition).unwrap();
        assert!(cache.workspace_for(&package_json, &no_buffers).is_none());
    }

    #[test]
    fn cache_forgets_discovery_so_a_new_definition_file_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::create_dir_all(&root).unwrap();
        let package_json = root.join("package.json");
        std::fs::write(&package_json, "{}").unwrap();
        let cache = WorkspaceCache::new();
        let no_buffers = |_: &Path| None;
        assert!(cache.workspace_for(&package_json, &no_buffers).is_none());

        std::fs::write(root.join("pnpm-workspace.yaml"), "catalog: {}\n").unwrap();
        assert!(cache.workspace_for(&package_json, &no_buffers).is_none());
        cache.forget_discovery(&root);
        assert!(cache.workspace_for(&package_json, &no_buffers).is_some());

        let uncached = WorkspaceCache::new();
        uncached.disable_discovery_cache();
        std::fs::remove_file(root.join("pnpm-workspace.yaml")).unwrap();
        assert!(uncached.workspace_for(&package_json, &no_buffers).is_none());
        std::fs::write(root.join("pnpm-workspace.yaml"), "catalog: {}\n").unwrap();
        assert!(uncached.workspace_for(&package_json, &no_buffers).is_some());
    }
}
