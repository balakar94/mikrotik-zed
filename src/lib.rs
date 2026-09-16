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
        //    PATH wins over auto-download by design so local iterations are
        //    picked up without a release round-trip. The plain name is probed
        //    everywhere; Windows manual installs keep the `.exe` suffix, so
        //    probe that there too (no-op elsewhere).
        //
        //    Supply-chain gate `RSC_LS_ALLOW_PATH` (read via
        //    `worktree.shell_env()`, never `std::env`, so the WASM component
        //    stays sandboxed): default `1` preserves compatibility and keeps
        //    PATH first; `0` skips PATH entirely and forces the verified
        //    work-dir cache / auto-download path below. Set
        //    `RSC_LS_ALLOW_PATH=0` in the shell environment when only
        //    checksum-verified binaries may run.
        //
        //    Optional hash pin for developers: set `RSC_LS_PATH_SHA256` to the
        //    expected 64-character lowercase hex digest of the PATH binary
        //    (compare with `sha256sum "$(which rsc-ls)"`). When present and
        //    well-formed, a PATH hit whose bytes hash differently is refused
        //    and resolution falls through to the verified cache path; a
        //    malformed pin value is treated the same way (fail closed, never
        //    spawned). When PATH is used, only the absolute path plus a short
        //    hash prefix are logged -- never environment values or secrets --
        //    alongside the extension version; run `rsc-ls --version` locally
        //    for the binary version (probing it from the shim would require a
        //    spawn, which is not cheap here).
        let shell_env = worktree.shell_env();
        let allow_path = shell_env
            .iter()
            .find(|(k, _)| k == "RSC_LS_ALLOW_PATH")
            .map(|(_, v)| v.trim() != "0")
            .unwrap_or(true);
        if allow_path {
            if let Some(path) = worktree
                .which(BINARY_NAME)
                .or_else(|| worktree.which(binary_name))
            {
                let pinned = shell_env
                    .iter()
                    .find(|(k, _)| k == "RSC_LS_PATH_SHA256")
                    .map(|(_, v)| v.trim().to_ascii_lowercase());
                let hash_result =
                    crate::sha256::sha256_file_hex(&path, crate::verify::MAX_VERIFIED_BINARY_BYTES);
                // A refusal must fall through to the verified cache below;
                // only an accepted PATH hit returns early.
                let mut usable: Option<String> = None;
                match (&pinned, &hash_result) {
                    (Some(pin), Ok(digest)) => {
                        let well_formed =
                            pin.len() == 64 && pin.bytes().all(|b| b.is_ascii_hexdigit());
                        if !well_formed {
                            eprintln!(
                                "[mikrotik-zed] refusing PATH binary {path}: RSC_LS_PATH_SHA256 value is malformed; falling through to verified cache"
                            );
                        } else if !crate::sha256::digests_match(pin, digest) {
                            eprintln!(
                                "[mikrotik-zed] refusing PATH binary {path}: sha256 {}… does not match pin {}…; falling through to verified cache",
                                crate::verify::short_digest(digest),
                                crate::verify::short_digest(pin)
                            );
                        } else {
                            eprintln!(
                                "[mikrotik-zed] using {BINARY_NAME} from PATH: {path} (sha256 {}…, extension v{version}, pin verified; confirm binary version with `rsc-ls --version`)",
                                crate::verify::short_digest(digest)
                            );
                            usable = Some(path);
                        }
                    }
                    (Some(_), Err(_)) => {
                        eprintln!(
                            "[mikrotik-zed] refusing PATH binary {path}: could not hash it to check RSC_LS_PATH_SHA256; falling through to verified cache"
                        );
                    }
                    (None, Ok(digest)) => {
                        eprintln!(
                            "[mikrotik-zed] using {BINARY_NAME} from PATH: {path} (sha256 {}…, extension v{version}; confirm binary version with `rsc-ls --version`)",
                            crate::verify::short_digest(digest)
                        );
                        eprintln!(
                            "[mikrotik-zed] warning: PATH binary bypasses the checksum/.verified gate used by the auto-download path (see verify.rs/cache.rs); ensure it is trusted (set RSC_LS_ALLOW_PATH=0 to force verified cache, or pin with RSC_LS_PATH_SHA256)"
                        );
                        usable = Some(path);
                    }
                    (None, Err(_)) => {
                        eprintln!(
                            "[mikrotik-zed] using {BINARY_NAME} from PATH: {path} (extension v{version}; binary could not be hashed for logging, confirm with `rsc-ls --version`)"
                        );
                        eprintln!(
                            "[mikrotik-zed] warning: PATH binary bypasses the checksum/.verified gate used by the auto-download path (see verify.rs/cache.rs); ensure it is trusted (set RSC_LS_ALLOW_PATH=0 to force verified cache, or pin with RSC_LS_PATH_SHA256)"
                        );
                        usable = Some(path);
                    }
                }
                if let Some(accepted) = usable {
                    self.cached_binary = Some(accepted.clone());
                    return Ok(zed::Command {
                        command: accepted,
                        args: vec![],
                        env: shell_env,
                    });
                }
            }
        } else {
            eprintln!(
                "[mikrotik-zed] PATH lookup disabled by RSC_LS_ALLOW_PATH=0; using verified cache"
            );
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
                // Refuse a symlinked cache entry outright: the extension never
                // creates one, so it is tampering or leftover state.
                !platform::is_symlink(cached)
                    && platform::is_executable(cached)
                    && cache::cached_binary_is_intact(cached)
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
        // A symlink at the stored path is tampering or leftover state: never
        // hash or spawn it. `remove_file` unlinks the link itself, never its
        // target. Keying the reuse gate on `symlinked` (not on the removal
        // succeeding) means an unremovable link still falls through to a fresh
        // download instead of being reused.
        let symlinked = platform::is_symlink(&stored_name);
        if symlinked {
            eprintln!(
                "[mikrotik-zed] refusing to reuse symlinked cache entry {stored_name}; removing it and downloading afresh"
            );
            remove_cached_artifacts(&stored_name);
        }

        if !symlinked && platform::is_executable(&stored_name) {
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
                    // Only trust API-supplied URLs that are exactly this
                    // repo's download URL for the selected asset; anything
                    // else falls back to the URL constructed below.
                    download_url = platform::pinned_release_url(&asset.download_url, &asset_name);
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
                        // The tag is known on this path, so build the URL
                        // from pinned constants instead of trusting the
                        // API-supplied download_url at all.
                        download_url = Some(platform::pinned_asset_url(&tag, &asset_name));
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

        // Staged download: bytes land in `<stored>.part-<pid>-<counter>`
        // and are verified and chmodded there, then renamed over the stored
        // name. The stored path is never a download target, so a
        // mid-transfer failure cannot leave a truncated file where the reuse
        // gate looks.
        let staging = cache::staging_path(&stored_name);
        // Unlink stale residue first; this also drops a pre-existing symlink
        // at the staging path (`remove_file` never follows links).
        let _ = std::fs::remove_file(&staging);
        let download_result =
            zed::download_file(&url, &staging, zed::DownloadedFileType::Uncompressed);

        if let Err(e) = download_result {
            // The host writes downloads non-atomically: a mid-transfer
            // failure can leave a truncated staging file. Remove it (and any
            // stale stored pair) so neither this nor a later session mistakes
            // residue for a usable binary.
            cache::remove_staging(&staging);
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

        // Never verify through a link: the staging path must be a regular
        // file the download just created.
        if platform::is_symlink(&staging) {
            let msg = format!(
                "Downloaded {BINARY_NAME} ({triple}) but the staging file is a symlink; \
                refusing to verify it."
            );
            cache::remove_staging(&staging);
            remove_cached_artifacts(&stored_name);
            eprintln!("[mikrotik-zed] {msg}");
            zed::set_language_server_installation_status(
                language_server_id,
                &zed::LanguageServerInstallationStatus::Failed(msg.clone()),
            );
            return Err(msg);
        }

        // 5) Supply-chain verification: hash the staged binary against the
        // release's `.sha256` companion BEFORE it is made executable or run.
        // Fail closed on any verification problem — never fall back to
        // executing an unverified binary.
        let verified_digest = match verify::verify_downloaded_binary(&staging, &url) {
            Ok(digest) => digest,
            Err(failure) => {
                let msg = failure.describe(&url);
                // Best-effort cleanup so neither this session nor a later one
                // can pick up the unverified bytes.
                cache::remove_staging(&staging);
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

        if let Err(e) = zed::make_file_executable(&staging) {
            let msg = format!("Downloaded {staging} but failed to make executable: {e}");
            eprintln!("[mikrotik-zed] {msg}");
            cache::remove_staging(&staging);
            remove_cached_artifacts(&stored_name);
            zed::set_language_server_installation_status(
                language_server_id,
                &zed::LanguageServerInstallationStatus::Failed(msg.clone()),
            );
            return Err(msg);
        }

        // Publish atomically: rename the verified staging file over the
        // stored name. Chmod happened on the staging file, so the mode
        // carries over with the rename.
        if let Err(e) = platform::atomic_replace(&staging, &stored_name) {
            let msg = format!("Verified {BINARY_NAME} ({triple}) but failed to install it: {e}");
            eprintln!("[mikrotik-zed] {msg}");
            cache::remove_staging(&staging);
            remove_cached_artifacts(&stored_name);
            zed::set_language_server_installation_status(
                language_server_id,
                &zed::LanguageServerInstallationStatus::Failed(msg.clone()),
            );
            return Err(msg);
        }
        // The rename consumes the staging file; drop any residue best-effort.
        cache::remove_staging(&staging);

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
            cache::remove_staging(&staging);
            remove_cached_artifacts(&stored_name);
            eprintln!("[mikrotik-zed] {msg}");
            zed::set_language_server_installation_status(
                language_server_id,
                &zed::LanguageServerInstallationStatus::Failed(msg.clone()),
            );
            return Err(msg);
        }

        // Re-check the gate immediately before spawn: a swap or truncation
        // between publish and exec must fail closed instead of running
        // unverified bytes. Symlinks are refused outright.
        if platform::is_symlink(&stored_name) {
            let msg = format!(
                "Installed {BINARY_NAME} ({triple}) but the stored file is a symlink; \
                refusing to run it."
            );
            cache::remove_staging(&staging);
            remove_cached_artifacts(&stored_name);
            eprintln!("[mikrotik-zed] {msg}");
            zed::set_language_server_installation_status(
                language_server_id,
                &zed::LanguageServerInstallationStatus::Failed(msg.clone()),
            );
            return Err(msg);
        }
        if !platform::is_executable(&stored_name) {
            let msg = format!(
                "Installed {BINARY_NAME} ({triple}) but {stored_name} is not executable; \
                refusing to run it."
            );
            cache::remove_staging(&staging);
            remove_cached_artifacts(&stored_name);
            eprintln!("[mikrotik-zed] {msg}");
            zed::set_language_server_installation_status(
                language_server_id,
                &zed::LanguageServerInstallationStatus::Failed(msg.clone()),
            );
            return Err(msg);
        }
        if let Some(reason) = cache::integrity_problem(&stored_name) {
            let msg = format!(
                "Installed {BINARY_NAME} ({triple}) but it failed its integrity gate ({reason}); \
                refusing to run it."
            );
            cache::remove_staging(&staging);
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
