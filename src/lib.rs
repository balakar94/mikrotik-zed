use zed_extension_api::{self as zed, LanguageServerId, Result, Worktree};

mod cache;
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

/// Best-effort removal of a cached binary and its integrity marker. Every
/// caller treats individual removal failures as non-fatal warnings: leftover
/// files cost disk space, never correctness, because the reuse gate in step 3
/// re-verifies bytes against the marker before anything is spawned.
fn remove_cached_artifacts(stored_name: &str) {
    if let Err(e) = std::fs::remove_file(stored_name) {
        eprintln!("[mikrotik-zed] warning: could not remove {stored_name}: {e}");
    }
    let marker = cache::marker_path(stored_name);
    if let Err(e) = std::fs::remove_file(&marker) {
        eprintln!("[mikrotik-zed] warning: could not remove {marker}: {e}");
    }
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
        let (os, arch) = zed::current_platform();
        // Compile-time crate version (`env!` expands at build time; runtime
        // environment access stays banned in the WASM component). Keying the
        // stored binary by version is what lets an extension update replace
        // its language server instead of silently reusing the old one.
        let version = env!("CARGO_PKG_VERSION");
        // Unversioned name for probing PATH only (developer installs carry no
        // version suffix).
        let binary_name = platform::server_binary_name(os);
        // Versioned work-dir storage name. Every filesystem operation on the
        // cached download — download target, verification, chmod, marker,
        // cleanup, spawn — goes through this variable, never a bare constant.
        let stored_name = platform::stored_binary_name(os, version);

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

        // 2) Reuse cached binary from previous successful resolution in this session.
        // `cached` is either an absolute PATH result or the versioned work-dir
        // name recorded by a prior download in this session. Only the work-dir
        // flavor is re-probed, and its gate is two-fold: spawnability (an
        // existence check in the shipped WASM build — see platform.rs) AND
        // byte-integrity against the digest marker written right after
        // verification. A file truncated or swapped since then is never
        // respawned; control falls through to a fresh download.
        if let Some(cached) = &self.cached_binary {
            let probe = if cached.as_str() == stored_name.as_str() {
                platform::is_executable(cached) && cache::cached_binary_is_intact(cached)
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
        // Downloads are stored under the versioned name `rsc-ls-<version>`
        // (`rsc-ls-<version>.exe` on Windows) beside an `.verified` digest
        // marker recording the SHA-256 that passed checksum verification.
        // This branch delivers the self-healing it promises: when the gate
        // fails — missing/malformed marker, digest mismatch, oversize — the
        // reason is logged, the stale pair is removed best-effort, and control
        // falls through to a fresh, re-verified download instead of respawning
        // a possibly corrupt file forever.
        if platform::is_executable(&stored_name) {
            match cache::integrity_problem(&stored_name) {
                None => {
                    eprintln!(
                        "[mikrotik-zed] found cached {stored_name} in extension dir, reusing"
                    );
                    self.cached_binary = Some(stored_name.clone());
                    return Ok(zed::Command {
                        command: stored_name.clone(),
                        args: vec![],
                        env: worktree.shell_env(),
                    });
                }
                Some(reason) => {
                    eprintln!(
                        "[mikrotik-zed] cached {stored_name} failed its integrity gate ({reason}); removing it and downloading afresh"
                    );
                    remove_cached_artifacts(&stored_name);
                }
            }
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
                    // Only trust API-supplied URLs inside this repo's own
                    // release namespace; anything else falls back to the
                    // self-constructed URL below.
                    download_url = platform::pinned_release_url(&asset.download_url);
                    break;
                }
            }
            if download_url.is_none() {
                eprintln!(
                    "[mikrotik-zed] usable asset {asset_name} not in latest release {}, trying tag v{version}",
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
                        download_url = platform::pinned_release_url(&asset.download_url);
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

        // Destination uses the versioned platform spawn name: on Windows the
        // bytes must land in a `*.exe` file or the spawn will fail, and the
        // version suffix isolates each extension release's artifact.
        let download_result =
            zed::download_file(&url, &stored_name, zed::DownloadedFileType::Uncompressed);

        if let Err(e) = download_result {
            // The host writes downloads non-atomically: a mid-transfer failure
            // can leave a TRUNCATED file at the final path. Remove it (and any
            // stale marker) so neither this nor a later session mistakes
            // residue for a usable binary.
            remove_cached_artifacts(&stored_name);
            let msg = format!(
                "Failed to download {BINARY_NAME} ({triple}) from {url}: {e}. \
                Manual install: cargo build -p rsc-ls --release and add target/release to PATH, \
                or download {asset_name} from https://github.com/{GITHUB_REPO}/releases \
                and place it in PATH."
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
        let verified_digest = match verify::verify_downloaded_binary(&stored_name, &url) {
            Ok(digest) => digest,
            Err(failure) => {
                let msg = failure.describe(&url);
                // Best-effort cleanup so neither this session nor a later one
                // can pick up the unverified binary from the work dir.
                remove_cached_artifacts(&stored_name);
                eprintln!("[mikrotik-zed] {msg}");
                zed::set_language_server_installation_status(
                    language_server_id,
                    &zed::LanguageServerInstallationStatus::Failed(msg.clone()),
                );
                return Err(msg);
            }
        };
        eprintln!(
            "[mikrotik-zed] sha256 verified {} ({triple})",
            verify::short_digest(&verified_digest)
        );

        if let Err(e) = zed::make_file_executable(&stored_name) {
            let msg = format!("Downloaded {stored_name} but failed to make executable: {e}");
            eprintln!("[mikrotik-zed] {msg}");
            zed::set_language_server_installation_status(
                language_server_id,
                &zed::LanguageServerInstallationStatus::Failed(msg.clone()),
            );
            return Err(msg);
        }

        // Record the integrity marker LAST: its presence certifies that these
        // exact bytes passed checksum verification. Writing it is part of the
        // transaction — if it cannot be persisted we fail closed and remove
        // the binary, because an uncertifiable cache entry would bounce off
        // the step-3 gate forever without ever healing itself.
        if let Err(e) = cache::write_marker(&stored_name, &verified_digest) {
            let msg = format!(
                "Verified {BINARY_NAME} ({triple}) but could not record its integrity marker: {e}. \
                Refusing to keep an uncertifiable cached binary."
            );
            remove_cached_artifacts(&stored_name);
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
            "[mikrotik-zed] {BINARY_NAME} downloaded and cached for {triple} -> {stored_name}"
        );

        self.cached_binary = Some(stored_name.clone());

        Ok(zed::Command {
            command: stored_name,
            args: vec![],
            env: worktree.shell_env(),
        })
    }
}

zed::register_extension!(RscExtension);
