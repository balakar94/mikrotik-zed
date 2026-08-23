use zed_extension_api::{self as zed, LanguageServerId, Result, Worktree};

mod platform;
mod sha256;
mod verify;

const BINARY_NAME: &str = "rsc-ls";
const GITHUB_REPO: &str = "balakar94/mikrotik-zed";

/// In-memory cache for the resolved binary path within this extension instance.
/// Avoids re-downloading or re-probing on every `language_server_command` call
/// (Zed may call this multiple times per worktree).
struct RscExtension {
    cached_binary: Option<String>,
}

impl zed::Extension for RscExtension {
    fn new() -> Self {
        Self {
            cached_binary: None,
        }
    }

    fn language_server_command(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<zed::Command> {
        // Local file/spawn name for this platform (`rsc-ls.exe` on Windows).
        let (os, arch) = zed::current_platform();
        let binary_name = platform::server_binary_name(os);

        // 1) Fast path: binary in PATH (developer local build or manual install).
        //    The plain name is probed everywhere; Windows manual installs keep
        //    the `.exe` suffix, so probe that there too (no-op elsewhere).
        if let Some(path) = worktree
            .which(BINARY_NAME)
            .or_else(|| worktree.which(binary_name))
        {
            eprintln!("[mikrotik-zed] using {BINARY_NAME} from PATH: {path}");
            self.cached_binary = Some(path.clone());
            return Ok(zed::Command {
                command: path,
                args: vec![],
                env: worktree.shell_env(),
            });
        }

        // 2) Reuse cached binary from previous successful resolution in this session
        if let Some(cached) = &self.cached_binary {
            // `cached` may be an absolute PATH result or the platform-relative
            // spawn name ("rsc-ls" / "rsc-ls.exe") from a prior download.
            // Spawnability is probed directly: Zed's host turns
            // `make_file_executable` into an unconditional no-op on non-unix
            // platforms, so its `Ok` says nothing about presence or exec bits.
            let probe = if cached.as_str() == binary_name {
                platform::is_executable(cached)
            } else {
                // Absolute path from PATH – assume it still exists; worktree.which already failed
                // so this is a stale cache; fall through to download.
                false
            };
            if probe {
                eprintln!("[mikrotik-zed] reusing cached binary: {cached}");
                return Ok(zed::Command {
                    command: cached.clone(),
                    args: vec![],
                    env: worktree.shell_env(),
                });
            }
        }

        // 3) Reuse a previously downloaded binary from the extension work dir.
        // Downloaded binaries are stored (uncompressed) under the platform spawn
        // name — `rsc-ls`, or `rsc-ls.exe` on Windows — and must already carry
        // their exec bits from installation. If something stripped them since,
        // fall through to a fresh download instead of a doomed spawn: its
        // strict post-download gate either repairs the file or fails loudly.
        if platform::is_executable(binary_name) {
            eprintln!("[mikrotik-zed] found cached {binary_name} in extension dir, reusing");
            self.cached_binary = Some(binary_name.to_string());
            return Ok(zed::Command {
                command: binary_name.to_string(),
                args: vec![],
                env: worktree.shell_env(),
            });
        }

        // 4) Auto-download from GitHub Releases
        let triple = match platform::asset_triple(os, arch) {
            Ok(t) => t,
            Err(e) => {
                return Err(format!(
                    "{e} Install {BINARY_NAME} manually: cargo build -p rsc-ls --release and put it in PATH, \
                    or download from https://github.com/{GITHUB_REPO}/releases. \
                    Current platform: os={os:?} arch={arch:?}"
                ));
            }
        };

        let asset_name = format!("{BINARY_NAME}-{triple}");
        eprintln!(
            "[mikrotik-zed] {BINARY_NAME} not in PATH, attempting auto-download for {triple} (asset {asset_name})"
        );

        // Try GitHub API first (latest release), then fallback to versioned URL.
        let version = env!("CARGO_PKG_VERSION");
        let mut download_url: Option<String> = None;

        // Attempt latest_github_release
        zed::set_language_server_installation_status(
            language_server_id,
            &zed::LanguageServerInstallationStatus::CheckingForUpdate,
        );

        let github_opts = zed::GithubReleaseOptions {
            require_assets: true,
            pre_release: false,
        };

        if let Ok(release) = zed::latest_github_release(GITHUB_REPO, github_opts) {
            for asset in &release.assets {
                if asset.name == asset_name {
                    eprintln!(
                        "[mikrotik-zed] found asset in latest release {}: {}",
                        release.version, asset.name
                    );
                    download_url = Some(asset.download_url.clone());
                    break;
                }
            }
            if download_url.is_none() {
                eprintln!(
                    "[mikrotik-zed] asset {asset_name} not in latest release {}, trying tag v{version}",
                    release.version
                );
            }
        } else {
            eprintln!("[mikrotik-zed] latest_github_release failed, trying tag lookup");
        }

        // Fallback: github_release_by_tag_name for current version
        if download_url.is_none() {
            let tag = format!("v{version}");
            if let Ok(release) = zed::github_release_by_tag_name(GITHUB_REPO, &tag) {
                for asset in &release.assets {
                    if asset.name == asset_name {
                        eprintln!("[mikrotik-zed] found asset in tag {tag}: {}", asset.name);
                        download_url = Some(asset.download_url.clone());
                        break;
                    }
                }
            }
        }

        // Final fallback: construct direct download URL (no API)
        let url = download_url.unwrap_or_else(|| {
            format!("https://github.com/{GITHUB_REPO}/releases/download/v{version}/{asset_name}")
        });

        eprintln!("[mikrotik-zed] downloading {BINARY_NAME} from {url}");
        zed::set_language_server_installation_status(
            language_server_id,
            &zed::LanguageServerInstallationStatus::Downloading,
        );

        // Destination uses the platform spawn name: on Windows the bytes must
        // land in a file named `rsc-ls.exe` or the spawn will fail.
        let download_result =
            zed::download_file(&url, binary_name, zed::DownloadedFileType::Uncompressed);

        if let Err(e) = download_result {
            let msg = format!(
                "Failed to download {BINARY_NAME} ({triple}) from {url}: {e}. \
                Manual install: cargo build -p rsc-ls --release and add target/release to PATH, \
                or download {asset_name} from https://github.com/{GITHUB_REPO}/releases \
                and place as {binary_name} in PATH. Original error: {e}"
            );
            eprintln!("[mikrotik-zed] {msg}");
            zed::set_language_server_installation_status(
                language_server_id,
                &zed::LanguageServerInstallationStatus::Failed(msg.clone()),
            );
            return Err(msg);
        }

        // 5) Supply-chain verification: hash the downloaded binary against the
        // release's `.sha256` companion BEFORE it is made executable or run.
        // Fail closed on any verification problem — never fall back to
        // executing an unverified binary.
        let verified_digest = match verify::verify_downloaded_binary(binary_name, &url) {
            Ok(prefix) => prefix,
            Err(failure) => {
                let msg = failure.describe(&url);
                // Best-effort cleanup so neither this session nor a later one
                // can pick up the unverified binary from the work dir.
                if let Err(remove_err) = std::fs::remove_file(binary_name) {
                    eprintln!(
                        "[mikrotik-zed] warning: could not remove unverified {binary_name}: {remove_err}"
                    );
                }
                eprintln!("[mikrotik-zed] {msg}");
                zed::set_language_server_installation_status(
                    language_server_id,
                    &zed::LanguageServerInstallationStatus::Failed(msg.clone()),
                );
                return Err(msg);
            }
        };
        eprintln!("[mikrotik-zed] sha256 verified {verified_digest} ({triple})");

        if let Err(e) = zed::make_file_executable(binary_name) {
            let msg = format!("Downloaded {binary_name} but failed to make executable: {e}");
            eprintln!("[mikrotik-zed] {msg}");
            zed::set_language_server_installation_status(
                language_server_id,
                &zed::LanguageServerInstallationStatus::Failed(msg.clone()),
            );
            return Err(msg);
        }

        zed::set_language_server_installation_status(
            language_server_id,
            &zed::LanguageServerInstallationStatus::None,
        );
        eprintln!(
            "[mikrotik-zed] {BINARY_NAME} downloaded and cached for {triple} -> {binary_name}"
        );

        self.cached_binary = Some(binary_name.to_string());

        Ok(zed::Command {
            command: binary_name.to_string(),
            args: vec![],
            env: worktree.shell_env(),
        })
    }
}

zed::register_extension!(RscExtension);
