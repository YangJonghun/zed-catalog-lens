use std::fs;

use semver::Version;

use zed_extension_api::{
    self as zed, Architecture, Command, DownloadedFileType, GithubReleaseOptions, LanguageServerId,
    LanguageServerInstallationStatus, Os, Result, Worktree, serde_json,
    settings::{CommandSettings, LspSettings},
};

const BINARY: &str = "catalog-lens";
const REPO: &str = "YangJonghun/zed-catalog-lens";
const INSTALL_HINT: &str = "catalog-lens could not be downloaded from \
https://github.com/YangJonghun/zed-catalog-lens/releases. Putting a catalog-lens binary on PATH, \
or setting lsp.catalog-lens.binary.path, also works.";

fn platform_slug(os: Os, arch: Architecture) -> Result<&'static str> {
    match (os, arch) {
        (Os::Mac, Architecture::Aarch64) => Ok("darwin-arm64"),
        (Os::Mac, Architecture::X8664) => Ok("darwin-amd64"),
        (Os::Linux, Architecture::X8664) => Ok("linux-amd64"),
        (Os::Linux, Architecture::Aarch64) => Ok("linux-arm64"),
        (_, Architecture::X86) => Err("32-bit x86 is not supported".to_string()),
        (Os::Windows, _) => Err("Windows is not supported".to_string()),
    }
}

fn asset_name(os: Os, arch: Architecture) -> Result<String> {
    Ok(format!("{BINARY}-{}.tar.gz", platform_slug(os, arch)?))
}

fn version_dir(release_version: &str) -> String {
    format!("{BINARY}-{release_version}")
}

fn parse_version(version: &str) -> Option<Version> {
    Version::parse(version.strip_prefix('v').unwrap_or(version)).ok()
}

fn newest_version_dir(names: impl IntoIterator<Item = String>) -> Option<String> {
    let prefix = format!("{BINARY}-");
    names
        .into_iter()
        .filter_map(|name| {
            let version = parse_version(name.strip_prefix(&prefix)?)?;
            Some((version, name))
        })
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, name)| name)
}

fn stale_version_dirs(names: impl IntoIterator<Item = String>, keep: &str) -> Vec<String> {
    let prefix = format!("{BINARY}-");
    names
        .into_iter()
        .filter(|name| {
            name != keep
                && name
                    .strip_prefix(&prefix)
                    .is_some_and(|version| parse_version(version).is_some())
        })
        .collect()
}

fn work_dir_entries() -> Vec<String> {
    fs::read_dir(".")
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .collect()
}

fn is_file(path: &str) -> bool {
    fs::metadata(path).is_ok_and(|meta| meta.is_file())
}

fn downloaded_binary() -> Option<String> {
    let path = format!("{}/{BINARY}", newest_version_dir(work_dir_entries())?);
    is_file(&path).then_some(path)
}

struct CatalogLens {
    downloaded: Option<String>,
}

impl CatalogLens {
    fn install_latest(&self, id: &LanguageServerId) -> Result<String> {
        let (os, arch) = zed::current_platform();
        let asset_name = asset_name(os, arch)?;

        zed::set_language_server_installation_status(
            id,
            &LanguageServerInstallationStatus::CheckingForUpdate,
        );
        let release = zed::latest_github_release(
            REPO,
            GithubReleaseOptions {
                require_assets: true,
                pre_release: false,
            },
        )?;
        let asset = release
            .assets
            .iter()
            .find(|asset| asset.name == asset_name)
            .ok_or_else(|| format!("release {} has no asset {asset_name}", release.version))?;

        let dir = version_dir(&release.version);
        let path = format!("{dir}/{BINARY}");
        if !is_file(&path) {
            zed::set_language_server_installation_status(
                id,
                &LanguageServerInstallationStatus::Downloading,
            );
            zed::download_file(&asset.download_url, &dir, DownloadedFileType::GzipTar)?;
            zed::make_file_executable(&path)?;
            for stale in stale_version_dirs(work_dir_entries(), &dir) {
                fs::remove_dir_all(stale).ok();
            }
        }
        Ok(path)
    }

    fn download(&mut self, id: &LanguageServerId) -> Result<String> {
        if let Some(path) = self.downloaded.as_ref().filter(|path| is_file(path)) {
            return Ok(path.clone());
        }
        let resolved = self
            .install_latest(id)
            .or_else(|err| downloaded_binary().ok_or(err));
        match resolved {
            Ok(path) => {
                zed::set_language_server_installation_status(
                    id,
                    &LanguageServerInstallationStatus::None,
                );
                self.downloaded = Some(path.clone());
                Ok(path)
            }
            Err(err) => {
                let message = format!("{err}\n{INSTALL_HINT}");
                zed::set_language_server_installation_status(
                    id,
                    &LanguageServerInstallationStatus::Failed(message.clone()),
                );
                Err(message)
            }
        }
    }

    fn binary_path(&mut self, id: &LanguageServerId, worktree: &Worktree) -> Result<String> {
        if let Some(path) = worktree.which(BINARY) {
            return Ok(path);
        }
        self.download(id)
    }
}

fn configured_command(binary: Option<CommandSettings>) -> Option<Command> {
    let binary = binary?;
    Some(Command {
        command: binary.path?,
        args: binary.arguments.unwrap_or_default(),
        env: binary.env.unwrap_or_default().into_iter().collect(),
    })
}

impl zed::Extension for CatalogLens {
    fn new() -> Self {
        Self { downloaded: None }
    }

    fn language_server_command(
        &mut self,
        id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<Command> {
        let binary = LspSettings::for_worktree(id.as_ref(), worktree)
            .ok()
            .and_then(|settings| settings.binary);
        if let Some(command) = configured_command(binary) {
            return Ok(command);
        }
        Ok(Command {
            command: self.binary_path(id, worktree)?,
            args: Vec::new(),
            env: Vec::new(),
        })
    }

    fn language_server_workspace_configuration(
        &mut self,
        id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<Option<serde_json::Value>> {
        Ok(LspSettings::for_worktree(id.as_ref(), worktree)?.settings)
    }
}

zed::register_extension!(CatalogLens);

#[cfg(test)]
mod tests {
    use super::*;

    const RELEASE_WORKFLOW: &str = include_str!("../.github/workflows/release-please.yml");
    const EXTENSION_TOML: &str = include_str!("../extension.toml");
    const CARGO_TOML: &str = include_str!("../Cargo.toml");

    const SUPPORTED: [(Os, Architecture, &str); 4] = [
        (Os::Mac, Architecture::Aarch64, "darwin-arm64"),
        (Os::Mac, Architecture::X8664, "darwin-amd64"),
        (Os::Linux, Architecture::X8664, "linux-amd64"),
        (Os::Linux, Architecture::Aarch64, "linux-arm64"),
    ];

    fn assigned(text: &str, prefix: &str) -> String {
        let value = text
            .lines()
            .find_map(|line| line.strip_prefix(prefix))
            .unwrap_or_else(|| panic!("`{prefix}` not found"));
        value.trim_matches('"').to_string()
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn asset_name_maps_supported_platforms() {
        for (os, arch, slug) in SUPPORTED {
            assert_eq!(
                asset_name(os, arch),
                Ok(format!("catalog-lens-{slug}.tar.gz"))
            );
        }
    }

    #[test]
    fn asset_name_rejects_32_bit_x86_and_windows() {
        assert!(asset_name(Os::Linux, Architecture::X86).is_err());
        assert!(asset_name(Os::Mac, Architecture::X86).is_err());
        assert!(asset_name(Os::Windows, Architecture::X8664).is_err());
        assert!(asset_name(Os::Windows, Architecture::Aarch64).is_err());
    }

    #[test]
    fn release_workflow_covers_every_downloaded_asset() {
        let field = |name: &str| -> Vec<&str> {
            RELEASE_WORKFLOW
                .lines()
                .filter_map(|line| line.trim().strip_prefix(name))
                .collect()
        };
        let [archive] = field("ARCHIVE: ")[..] else {
            panic!("release workflow needs exactly one ARCHIVE");
        };
        let slugs = field("- slug: ");
        assert_eq!(slugs.len(), SUPPORTED.len(), "release workflow matrix");
        for (os, arch, slug) in SUPPORTED {
            assert!(slugs.contains(&slug), "release workflow lacks {slug}");
            assert_eq!(
                archive.replace("${{ matrix.slug }}", slug),
                asset_name(os, arch).unwrap()
            );
        }
        assert!(
            RELEASE_WORKFLOW.contains(&format!("release\" {BINARY}\n")),
            "the archive holds the {BINARY} binary at its root"
        );
    }

    #[test]
    fn newest_version_dir_compares_numerically() {
        let dirs = names(&[
            "catalog-lens-v0.9.0",
            "catalog-lens-v0.10.0",
            "catalog-lens-v0.2.5",
        ]);
        assert_eq!(
            newest_version_dir(dirs),
            Some("catalog-lens-v0.10.0".to_string())
        );
    }

    #[test]
    fn newest_version_dir_ignores_unrelated_entries() {
        let entries = names(&[
            "catalog-lens",
            "catalog-lens-darwin-arm64.tar.gz",
            "catalog-lens-latest",
            "other-0.9.0",
            "catalog-lens-0.1.0",
        ]);
        assert_eq!(
            newest_version_dir(entries),
            Some("catalog-lens-0.1.0".to_string())
        );
        assert_eq!(newest_version_dir(names(&["other-1.0.0"])), None);
        assert_eq!(newest_version_dir(Vec::new()), None);
    }

    #[test]
    fn version_dir_round_trips_through_newest_version_dir() {
        let dir = version_dir("v1.2.3");
        assert_eq!(newest_version_dir([dir.clone()]), Some(dir));
    }

    #[test]
    fn stale_version_dirs_lists_every_other_version_dir() {
        let entries = names(&[
            "catalog-lens-0.1.0",
            "catalog-lens-0.2.0",
            "catalog-lens-0.3.0",
            "catalog-lens",
            "catalog-lens-latest",
            "other-0.1.0",
        ]);
        assert_eq!(
            stale_version_dirs(entries, "catalog-lens-0.3.0"),
            names(&["catalog-lens-0.1.0", "catalog-lens-0.2.0"])
        );
    }

    #[test]
    fn stale_version_dirs_keeps_the_only_dir() {
        let only = names(&["catalog-lens-0.1.0", "catalog-lens"]);
        assert!(stale_version_dirs(only, "catalog-lens-0.1.0").is_empty());
    }

    #[test]
    fn configured_binary_path_overrides_the_server_lookup() {
        let configured = configured_command(Some(CommandSettings {
            path: Some("/opt/catalog-lens".into()),
            arguments: Some(vec!["--verbose".into()]),
            env: Some([("CATALOG_LENS_LOG".into(), "debug".into())].into()),
        }))
        .unwrap();
        assert_eq!(configured.command, "/opt/catalog-lens");
        assert_eq!(configured.args, vec!["--verbose".to_string()]);
        assert_eq!(
            configured.env,
            vec![("CATALOG_LENS_LOG".to_string(), "debug".to_string())]
        );
        let without_path = CommandSettings {
            path: None,
            arguments: Some(vec!["--verbose".into()]),
            env: None,
        };
        assert!(configured_command(Some(without_path)).is_none());
        assert!(configured_command(None).is_none());
    }

    #[test]
    fn install_hint_points_at_releases_and_path() {
        assert!(INSTALL_HINT.contains("https://github.com/YangJonghun/zed-catalog-lens/releases"));
        assert!(INSTALL_HINT.contains("PATH"));
        assert!(INSTALL_HINT.contains("lsp.catalog-lens.binary.path"));
    }

    #[test]
    fn repository_urls_agree_across_manifests() {
        let url = format!("https://github.com/{REPO}");
        assert_eq!(assigned(EXTENSION_TOML, "repository = "), url);
        assert_eq!(assigned(CARGO_TOML, "repository = "), url);
    }

    #[test]
    fn extension_manifest_matches_crate() {
        assert_eq!(
            assigned(EXTENSION_TOML, "version = "),
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(assigned(EXTENSION_TOML, "id = "), "catalog-lens-lsp");
        assert!(EXTENSION_TOML.contains("[language_servers.catalog-lens]"));
    }
}
