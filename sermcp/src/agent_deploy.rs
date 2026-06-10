//! Agent deploy pipeline — the `dutabo agent` fetch + copy + hook engine.
//!
//! Pipeline (spec §10):
//! 1. Sync the CACHE clone (`~/.cache/dutabo/agents/<agent>-<target>
//!    [-<item>]`, slugged with a stable hash tag so distinct names never
//!    collide; clone-or-fetch + one proxy retry; `--offline` skips the
//!    network and requires the cache).
//! 2. Checkout: `leaf.ref` when present (branch/tag/commit — fetched as
//!    `git fetch origin <ref>` then detached); otherwise the remote
//!    default branch (`reset --hard origin/HEAD`). A dirty cache clone is
//!    refused naming the paths unless `--force`.
//! 3. Materialize into the PROJECT: the cache WORKTREE is copied to
//!    `project_dir/<dst>` (`dst == ""` = the project root). The repo's own
//!    `.git` is NEVER copied. mode merge = overwrite/add, never delete;
//!    mode sync = delete files the repo no longer has (forbidden when
//!    dst resolves to the project root). File exec bits are preserved.
//!    The dst path (and every existing intermediate component) must not
//!    be a symlink (spec §12 rule 6).
//! 4. Run the effective `post_hook` (OPTIONAL): a path relative to the
//!    repo root (the cache clone), direct argv invocation — never
//!    `sh -c` (a non-executable script runs via `sh <script>`), env
//!    `DUTABO_AGENT/TARGET/ITEM/PROJECT_DIR/DST/REPO_DIR`, 120 s kill.
//!
//! Zero network in tests — local git repos with `file://` remotes only.
//!
//! Network rule (the global CLAUDE.md proxy convention, encoded as code —
//! never hardcoded IPs): attempt 1 plain. On failure with a network
//! signature in stderr AND a `http_proxy=` line in `~/.bashrc`, retry ONCE
//! with `http_proxy`/`https_proxy` exported. Retry failure reports both
//! stderr tails.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Env override for the clone cache root (tests/CI).
pub const AGENT_SRC_ENV: &str = "DUTABO_AGENT_SRC";
/// Env override for the git binary (tests/CI seam; default `git`).
pub const AGENT_GIT_ENV: &str = "DUTABO_GIT";

/// The post_hook run timeout (kill the child on expiry).
pub const DEPLOY_TIMEOUT: Duration = Duration::from_secs(120);

/// Everything one deploy run needs (built from the catalog's effective
/// config + CLI flags).
#[derive(Debug, Clone)]
pub struct DeployOptions {
    /// The skill source: a remote git URL (cached clone) or a local
    /// directory (used in place, never cloned).
    pub source: crate::skill_catalog::GitSource,
    /// Optional branch/tag/commit to checkout in the cache clone
    /// (spec §10); absent → the remote default branch.
    pub r#ref: Option<String>,
    /// Post-install script path RELATIVE to the repo root (the cache
    /// clone). Optional — skipped when None.
    pub post_hook: Option<String>,
    /// Copy destination INSIDE the project dir; "" = the project root
    /// (spec §9.1 — no more git-default-name derivation).
    pub dst: String,
    /// `"sync"` or `"merge"` (catalog-validated; asserted again here).
    pub mode: String,
    /// The catalog key path components (cache-dir naming + hook env).
    pub agent: String,
    /// The full selector path below the agent (any depth; `targets`
    /// root → … → leaf name).
    pub path: Vec<String>,
    /// Deploy target: the project dir (the copy lands at
    /// `project_dir/<dst>`; the hook's `DUTABO_PROJECT_DIR`).
    pub project_dir: PathBuf,
    /// Clone cache root (`~/.cache/dutabo/agents`).
    pub src_root: PathBuf,
    /// Skip all network; the cache clone must already exist.
    pub offline: bool,
    /// Discard uncommitted changes in the CACHE clone on redeploy (a
    /// dirty cache is refused naming the paths otherwise — the cache is
    /// what the project is copied FROM).
    pub force: bool,
    pub timeout: Duration,
}

/// A successful deploy.
#[derive(Debug, Clone)]
pub struct DeployReport {
    pub cache_dir: PathBuf,
    /// The copy destination: `project_dir/<dst>` (== project dir when
    /// dst is empty).
    pub deployed_dir: PathBuf,
    /// true = the cache clone existed and was fetched; false = fresh
    /// clone (or offline skip).
    pub updated: bool,
    /// The hook's stdout, line-split (tail for the status line).
    pub stdout_tail: Vec<String>,
    /// Non-fatal notes (e.g. a ref checkout that fell back to the local
    /// object after a refused remote fetch).
    pub warnings: Vec<String>,
}

/// The clone cache root: `DUTABO_AGENT_SRC` → `$XDG_CACHE_HOME/dutabo/
/// agents` → `~/.cache/dutabo/agents`. Env-resolved manually — no new
/// crate.
pub fn default_src_root() -> PathBuf {
    if let Ok(env) = std::env::var(AGENT_SRC_ENV)
        && !env.trim().is_empty()
    {
        return PathBuf::from(env);
    }
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(xdg).join("dutabo").join("agents");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".cache")
            .join("dutabo")
            .join("agents");
    }
    PathBuf::from(".cache").join("dutabo").join("agents")
}

/// One path's cache-dir name component: the raw name with every char
/// outside `[A-Za-z0-9._-]` slugged to `_` — names may legally contain
/// spaces/unicode (spec §12), but never path separators or `ParentDir`
/// escapes out of the cache root. When the slug DIFFERS from the raw
/// name a stable 8-hex FNV-1a tag of the RAW name is appended, so two
/// distinct names whose slugs coincide ("a b" vs "a_b" → both "a_b")
/// never share one cache clone (a shared dir would fetch the FIRST
/// clone's origin and deploy the wrong repo's content). The tag is
/// deterministic: a redeploy always reuses the same cache dir.
fn slug(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let s = if s == "." || s == ".." {
        "_".to_string()
    } else {
        s
    };
    if s != name {
        format!("{s}-h{:08x}", fnv1a(name))
    } else {
        s
    }
}

/// FNV-1a (64-bit) — a stable, non-crypto tag for the cache-dir suffix.
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// One path's cache dir: `<src_root>/<agent>-<target>[-<item>…]`, every
/// component slugged (2- and 3-level names are byte-identical to the
/// historical scheme). Paths DEEPER than an agent-target-item carry a
/// stable `-p<fnv1a>` tag of the raw joined path: dash-joined segments
/// could otherwise collide (`["a-b"]` vs `["a", "b"]`) and deploy the
/// wrong repo's content.
pub fn cache_dir_for(src_root: &Path, agent: &str, path: &[String]) -> PathBuf {
    let mut name = slug(agent);
    for segment in path {
        name.push('-');
        name.push_str(&slug(segment));
    }
    if path.len() > 2 {
        let raw = path.join("/");
        name.push_str(&format!("-p{:016x}", fnv1a(&raw)));
    }
    src_root.join(name)
}

/// Pure parser for the global proxy convention: the `http_proxy=` line in
/// `~/.bashrc` (also `export http_proxy=`). The value is reported VERBATIM
/// — no proxy IP/port is ever hardcoded in this module.
pub fn proxy_pair_from_bashrc(bashrc_text: &str) -> Option<(String, String)> {
    for line in bashrc_text.lines() {
        let t = line.trim();
        let Some(value) = t
            .strip_prefix("export http_proxy=")
            .or_else(|| t.strip_prefix("http_proxy="))
        else {
            continue;
        };
        let v = value.trim().trim_matches('"').trim_matches('\'');
        if !v.is_empty() {
            return Some((v.to_string(), v.to_string()));
        }
    }
    None
}

/// stderr signatures that trigger the ONE proxy retry.
pub fn is_network_failure(stderr: &str) -> bool {
    [
        "Could not resolve host",
        "Failed to connect",
        "unable to access",
        "Connection timed out",
        "Temporary failure in name resolution",
    ]
    .iter()
    .any(|s| stderr.contains(s))
}

/// Read `~/.bashrc` and extract its proxy pair (None when absent).
fn bashrc_proxy() -> Option<(String, String)> {
    let home = std::env::var_os("HOME")?;
    let text = std::fs::read_to_string(Path::new(&home).join(".bashrc")).ok()?;
    proxy_pair_from_bashrc(&text)
}

/// The last non-empty stderr line (the error summary the UI shows).
fn last_stderr_line(stderr: &str) -> String {
    stderr
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .unwrap_or_default()
        .to_string()
}

/// The git binary (`DUTABO_GIT` override; default `git`).
fn git_bin() -> String {
    std::env::var(AGENT_GIT_ENV)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "git".to_string())
}

fn argv(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

struct GitOutcome {
    ok: bool,
    stdout: String,
    stderr: String,
}

fn run_git(args: &[String], proxy: Option<(&str, &str)>) -> GitOutcome {
    let mut cmd = Command::new(git_bin());
    cmd.args(args);
    if let Some((hp, hsp)) = proxy {
        cmd.env("http_proxy", hp).env("https_proxy", hsp);
    }
    match cmd.output() {
        Ok(o) => GitOutcome {
            ok: o.status.success(),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        },
        Err(e) => GitOutcome {
            ok: false,
            stdout: String::new(),
            stderr: e.to_string(),
        },
    }
}

fn cache_arg(cache_dir: &Path) -> String {
    cache_dir.display().to_string()
}

/// Step (1): clone (fresh) or fetch (existing cache clone). One proxy
/// retry per the network convention. `offline` = zero network: the cache
/// clone must exist. Returns (updated, effective proxy) — the proxy (if
/// any) is reused by the checkout steps.
fn sync_sources(
    opts: &DeployOptions,
    url: &str,
    cache_dir: &Path,
) -> Result<(bool, Option<(String, String)>), String> {
    let existed = cache_dir.join(".git").is_dir();
    if opts.offline {
        if !existed {
            return Err(format!(
                "offline: cache clone missing at {}",
                cache_dir.display()
            ));
        }
        return Ok((false, None));
    }
    let (attempt, args): (&str, Vec<String>) = if existed {
        (
            "fetch",
            argv(&["-C", &cache_arg(cache_dir), "fetch", "origin"]),
        )
    } else {
        std::fs::create_dir_all(&opts.src_root)
            .map_err(|e| format!("cannot create cache dir {}: {e}", opts.src_root.display()))?;
        ("clone", argv(&["clone", url, &cache_arg(cache_dir)]))
    };
    let first = run_git(&args, None);
    if first.ok {
        return Ok((existed, None));
    }
    let proxy = if is_network_failure(&first.stderr) {
        bashrc_proxy()
    } else {
        None
    };
    if let Some((hp, hsp)) = &proxy {
        let second = run_git(&args, Some((hp, hsp)));
        if second.ok {
            return Ok((existed, proxy));
        }
        return Err(format!(
            "git {attempt} failed — {} — proxy retry: {}",
            last_stderr_line(&first.stderr),
            last_stderr_line(&second.stderr)
        ));
    }
    Err(format!(
        "git {attempt} failed — {}",
        last_stderr_line(&first.stderr)
    ))
}

/// `git rev-parse --verify <rev>^{commit}` — resolve a branch/tag/sha to
/// the full commit sha it pins (annotated tags peel to their commit).
/// None when the rev does not resolve locally.
fn try_resolve(cache_dir: &Path, rev: &str, proxy: Option<(&str, &str)>) -> Option<String> {
    let out = run_git(
        &argv(&[
            "-C",
            &cache_arg(cache_dir),
            "rev-parse",
            "--verify",
            &format!("{rev}^{{commit}}"),
        ]),
        proxy,
    );
    (out.ok && !out.stdout.trim().is_empty()).then(|| out.stdout.trim().to_string())
}

fn resolve_commit(
    cache_dir: &Path,
    rev: &str,
    proxy: Option<(&str, &str)>,
) -> Result<String, String> {
    try_resolve(cache_dir, rev, proxy).ok_or_else(|| {
        let out = run_git(
            &argv(&["-C", &cache_arg(cache_dir), "rev-parse", "--verify", rev]),
            proxy,
        );
        format!(
            "cannot resolve ref {rev:?} in {} — {}",
            cache_dir.display(),
            last_stderr_line(&out.stderr)
        )
    })
}

/// Resolve a ref with ONLY local objects (offline): the ref itself, the
/// remote-tracking copy, then the tag namespace.
fn resolve_ref_local(
    cache_dir: &Path,
    r: &str,
    proxy: Option<(&str, &str)>,
) -> Result<String, String> {
    for cand in [
        r.to_string(),
        format!("origin/{r}"),
        format!("refs/tags/{r}"),
    ] {
        if let Some(sha) = try_resolve(cache_dir, &cand, proxy) {
            return Ok(sha);
        }
    }
    Err(format!(
        "cannot resolve ref {r:?} in {} (not fetched or unknown)",
        cache_dir.display()
    ))
}

/// Resolve `leaf.ref` to a commit sha. Online: an explicit refspec fetch
/// FIRST (it leaves `refs/remotes/origin/<ref>` behind so an OFFLINE
/// redeploy can resolve the same ref), then a plain `git fetch origin
/// <ref>` (the only way to fetch a commit sha — no remote ref exists to
/// refspec), then the locally cached object with a warning. Offline:
/// local resolution only.
fn resolve_deploy_ref(
    opts: &DeployOptions,
    cache_dir: &Path,
    r: &str,
    proxy: Option<(&str, &str)>,
    warnings: &mut Vec<String>,
) -> Result<String, String> {
    if opts.offline {
        return resolve_ref_local(cache_dir, r, proxy);
    }
    let spec = format!("+{r}:refs/remotes/origin/{r}");
    let fetch = run_git(
        &argv(&["-C", &cache_arg(cache_dir), "fetch", "origin", &spec]),
        proxy,
    );
    if fetch.ok
        && let Some(sha) = try_resolve(cache_dir, &format!("origin/{r}"), proxy)
    {
        return Ok(sha);
    }
    let fetch = run_git(
        &argv(&["-C", &cache_arg(cache_dir), "fetch", "origin", r]),
        proxy,
    );
    if fetch.ok {
        return resolve_commit(cache_dir, "FETCH_HEAD", proxy);
    }
    warnings.push(format!(
        "ref {r:?} could not be fetched — using the locally cached object"
    ));
    resolve_ref_local(cache_dir, r, proxy)
}

/// Step (2): align the cache clone with `leaf.ref` (or the remote default
/// branch). A dirty cache clone is refused naming the dirty paths unless
/// `--force` (which resets hard, discarding them). The `ref` is fetched
/// as `git fetch origin <ref>` — works for branches and tags, and for
/// commit shas over file:// and permissive servers; on a refused fetch the
/// local object is used directly (with a warning). Detached HEAD keeps
/// redeploys deterministic.
fn checkout_sources(
    opts: &DeployOptions,
    cache_dir: &Path,
    proxy: Option<&(String, String)>,
    warnings: &mut Vec<String>,
) -> Result<(), String> {
    let p = proxy.map(|(hp, hsp)| (hp.as_str(), hsp.as_str()));
    let status = run_git(
        &argv(&["-C", &cache_arg(cache_dir), "status", "--porcelain"]),
        p,
    );
    if !status.ok {
        return Err(format!(
            "git status {} failed — {}",
            cache_dir.display(),
            last_stderr_line(&status.stderr)
        ));
    }
    let dirty: Vec<String> = status
        .stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    if !dirty.is_empty() && !opts.force {
        return Err(format!(
            "{} has uncommitted changes ({}) — refusing to overwrite (use --force)",
            cache_dir.display(),
            dirty.join(", ")
        ));
    }
    match &opts.r#ref {
        Some(r) => {
            // Resolve the ref to a COMMIT sha first (branch/tag/sha all
            // resolve; `checkout --detach <branch>` hits git's DWIM
            // branch-creation path, which rejects `--detach`).
            let sha = resolve_deploy_ref(opts, cache_dir, r, p, warnings)?;
            if opts.force {
                let reset = run_git(
                    &argv(&["-C", &cache_arg(cache_dir), "reset", "--hard", &sha]),
                    p,
                );
                if !reset.ok {
                    return Err(format!(
                        "git reset --hard {sha} in {} failed — {}",
                        cache_dir.display(),
                        last_stderr_line(&reset.stderr)
                    ));
                }
            } else {
                let co = run_git(
                    &argv(&["-C", &cache_arg(cache_dir), "checkout", "--detach", &sha]),
                    p,
                );
                if !co.ok {
                    return Err(format!(
                        "git checkout {r:?} in {} failed — {}",
                        cache_dir.display(),
                        last_stderr_line(&co.stderr)
                    ));
                }
            }
        }
        None => {
            // No ref → the remote default branch (already fetched by
            // step 1 / present from the fresh clone).
            let reset = run_git(
                &argv(&[
                    "-C",
                    &cache_arg(cache_dir),
                    "reset",
                    "--hard",
                    "origin/HEAD",
                ]),
                p,
            );
            if !reset.ok {
                return Err(format!(
                    "cannot align {} to the remote default branch — {}",
                    cache_dir.display(),
                    last_stderr_line(&reset.stderr)
                ));
            }
        }
    }
    Ok(())
}

/// Spec §12 rule 6: `project/<dst>` and every EXISTING intermediate
/// component must not be a symlink (sync deletions and merge writes both
/// land inside that tree). Missing components stop the walk (nothing
/// deeper can exist yet).
fn guard_dst_path(project_dir: &Path, dst: &str) -> Result<(), String> {
    if dst.is_empty() {
        return Ok(());
    }
    let mut cur = project_dir.to_path_buf();
    for comp in Path::new(dst).components() {
        let Component::Normal(c) = comp else {
            return Err(format!(
                "dst {dst:?} contains an unsupported path component"
            ));
        };
        cur.push(c);
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(format!(
                    "refusing to deploy through symlink {}",
                    cur.display()
                ));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Err(e) => return Err(format!("cannot inspect {}: {e}", cur.display())),
        }
    }
    Ok(())
}

/// Recursively walk a worktree: every relative path (files AND dirs) with
/// its `symlink_metadata` file type (symlinks are entries, never followed).
/// `skip_root_git` drops the top-level `.git` — the repo's own metadata
/// never enters the project (spec §10).
fn walk_worktree(
    dir: &Path,
    rel: &Path,
    skip_root_git: bool,
    out: &mut Vec<(PathBuf, std::fs::FileType)>,
) -> Result<(), String> {
    let entries =
        std::fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
    let mut items: Vec<(PathBuf, std::fs::FileType, PathBuf)> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
        let name = entry.file_name();
        if skip_root_git && rel.as_os_str().is_empty() && name == ".git" {
            continue;
        }
        let abs = entry.path();
        let ft = std::fs::symlink_metadata(&abs)
            .map_err(|e| format!("cannot stat {}: {e}", abs.display()))?
            .file_type();
        items.push((rel.join(&name), ft, abs));
    }
    for (relp, ft, abs) in items {
        out.push((relp.clone(), ft));
        if ft.is_dir() {
            walk_worktree(&abs, &relp, false, out)?;
        }
    }
    Ok(())
}

fn ensure_parent(dst: &Path) -> Result<(), String> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    Ok(())
}

/// Remove whatever sits at `dst` (symlink/file/dir) — used when the src
/// entry needs a different kind at that path.
fn remove_existing(dst: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(dst) {
        Ok(m) if m.file_type().is_dir() && !m.file_type().is_symlink() => {
            std::fs::remove_dir_all(dst)
                .map_err(|e| format!("cannot remove {}: {e}", dst.display()))?;
        }
        Ok(_) => {
            std::fs::remove_file(dst)
                .map_err(|e| format!("cannot remove {}: {e}", dst.display()))?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("cannot inspect {}: {e}", dst.display())),
    }
    Ok(())
}

/// Copy ONE worktree entry into the project (dirs, files, symlinks —
/// `fs::copy` preserves the file permission/exec bits).
fn copy_entry(src: &Path, dst: &Path, ft: std::fs::FileType) -> Result<(), String> {
    if ft.is_symlink() {
        let target = std::fs::read_link(src)
            .map_err(|e| format!("cannot read link {}: {e}", src.display()))?;
        remove_existing(dst)?;
        ensure_parent(dst)?;
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&target, dst).map_err(|e| {
                format!("cannot link {} → {}: {e}", dst.display(), target.display())
            })?;
        }
        #[cfg(not(unix))]
        {
            let _ = dst;
            return Err("symlinks in agent repos require a unix platform".into());
        }
        return Ok(());
    }
    if ft.is_dir() {
        match std::fs::symlink_metadata(dst) {
            Ok(m) if m.file_type().is_dir() && !m.file_type().is_symlink() => {}
            Ok(_) => remove_existing(dst)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("cannot inspect {}: {e}", dst.display())),
        }
        std::fs::create_dir_all(dst)
            .map_err(|e| format!("cannot create {}: {e}", dst.display()))?;
        return Ok(());
    }
    // Regular file: a dir/symlink in the way is replaced.
    match std::fs::symlink_metadata(dst) {
        Ok(m) if m.file_type().is_dir() && !m.file_type().is_symlink() => {
            remove_existing(dst)?;
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("cannot inspect {}: {e}", dst.display())),
    }
    ensure_parent(dst)?;
    std::fs::copy(src, dst)
        .map_err(|e| format!("cannot copy {} → {}: {e}", src.display(), dst.display()))?;
    Ok(())
}

/// Step (3a): mode merge — overwrite/add the repo's files; NEVER delete
/// anything that exists only in the project.
fn materialize_merge(cache_dir: &Path, deployed_dir: &Path) -> Result<(), String> {
    let mut entries: Vec<(PathBuf, std::fs::FileType)> = Vec::new();
    walk_worktree(cache_dir, Path::new(""), true, &mut entries)?;
    for (rel, ft) in entries {
        copy_entry(&cache_dir.join(&rel), &deployed_dir.join(&rel), ft)?;
    }
    Ok(())
}

/// Step (3b): mode sync — the dst ends up identical to the repo's
/// deployed content: stale project files (and dirs) are deleted
/// deepest-first, then the merge copy adds/overwrites the current
/// content. Sync into the project root is refused (spec §9.2).
fn materialize_sync(
    opts: &DeployOptions,
    cache_dir: &Path,
    deployed_dir: &Path,
) -> Result<(), String> {
    if opts.dst.is_empty() || opts.dst == "." {
        return Err(format!(
            "sync mode with dst {:?} — refusing to sync the project root",
            opts.dst
        ));
    }
    let mut repo_entries: Vec<(PathBuf, std::fs::FileType)> = Vec::new();
    walk_worktree(cache_dir, Path::new(""), true, &mut repo_entries)?;
    let repo_paths: BTreeSet<PathBuf> = repo_entries.iter().map(|(r, _)| r.clone()).collect();
    let mut dst_entries: Vec<(PathBuf, std::fs::FileType)> = Vec::new();
    if deployed_dir.exists() {
        walk_worktree(deployed_dir, Path::new(""), false, &mut dst_entries)?;
    }
    let mut stale: Vec<PathBuf> = dst_entries
        .iter()
        .filter(|(r, _)| !repo_paths.contains(r))
        .map(|(r, _)| r.clone())
        .collect();
    // Deepest first: children before their parents.
    stale.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    for rel in stale {
        let p = deployed_dir.join(&rel);
        let ft = std::fs::symlink_metadata(&p)
            .map_err(|e| format!("cannot stat {}: {e}", p.display()))?
            .file_type();
        if ft.is_dir() && !ft.is_symlink() {
            std::fs::remove_dir_all(&p)
                .map_err(|e| format!("cannot remove {}: {e}", p.display()))?;
        } else {
            std::fs::remove_file(&p).map_err(|e| format!("cannot remove {}: {e}", p.display()))?;
        }
    }
    for (rel, ft) in repo_entries {
        copy_entry(&cache_dir.join(&rel), &deployed_dir.join(&rel), ft)?;
    }
    Ok(())
}

/// Step (4): run the effective post_hook (OPTIONAL — skipped when None).
/// The hook is a COMMAND LINE: its first token is a script path relative
/// to the repo root (the cache clone; its cwd), the rest are arguments.
/// Direct argv invocation — never `sh -c` (a non-executable script runs
/// via `sh <script> [args…]`). Env: DUTABO_AGENT/TARGET/ITEM/PATH/
/// PROJECT_DIR/DST/REPO_DIR (TARGET = the first path segment, ITEM the
/// second, PATH the full `/`-joined path). Timeout kills the child.
fn run_script(opts: &DeployOptions, repo_root: &Path) -> Result<Vec<String>, String> {
    let Some(hook) = &opts.post_hook else {
        return Ok(Vec::new());
    };
    let parts = crate::skill_catalog::split_command_line(hook)
        .ok_or_else(|| format!("post_hook {hook:?} has an unterminated quote"))?;
    let script = repo_root.join(&parts[0]);
    if !script.is_file() {
        return Err(format!(
            "{} does not exist in {}",
            parts[0],
            repo_root.display()
        ));
    }
    let hook_label = hook.to_string();
    let executable = {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(&script)
                .map(|m| m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        }
        #[cfg(not(unix))]
        {
            false
        }
    };
    let mut cmd = if executable {
        let mut c = Command::new(&script);
        c.args(&parts[1..]);
        c
    } else {
        let mut c = Command::new("sh");
        c.arg(&script);
        c.args(&parts[1..]);
        c
    };
    cmd.current_dir(repo_root)
        .env("DUTABO_AGENT", &opts.agent)
        .env(
            "DUTABO_TARGET",
            opts.path.first().map(String::as_str).unwrap_or(""),
        )
        .env(
            "DUTABO_ITEM",
            opts.path.get(1).map(String::as_str).unwrap_or(""),
        )
        .env("DUTABO_PATH", opts.path.join("/"))
        .env("DUTABO_PROJECT_DIR", &opts.project_dir)
        .env("DUTABO_DST", &opts.dst)
        .env("DUTABO_REPO_DIR", repo_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cannot run {hook_label}: {e}"))?;
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let deadline = Instant::now() + opts.timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut out = String::new();
                let mut err = String::new();
                if let Some(mut p) = stdout_pipe.take() {
                    let _ = p.read_to_string(&mut out);
                }
                if let Some(mut p) = stderr_pipe.take() {
                    let _ = p.read_to_string(&mut err);
                }
                if !status.success() {
                    return Err(format!(
                        "{hook_label} exit {} — {}",
                        status.code().unwrap_or(-1),
                        last_stderr_line(&err)
                    ));
                }
                return Ok(out
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect());
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("timed out after {} s", opts.timeout.as_secs()));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("deploy process error: {e}")),
        }
    }
}

/// The deploy pipeline (spec §10): cache sync → checkout → symlink guard
/// → merge/sync copy → optional post_hook. Sync — call from a worker
/// thread.
pub fn deploy(opts: &DeployOptions) -> Result<DeployReport, String> {
    // Re-assert the mode contract (the catalog validates it; DeployOptions
    // can be built directly — an unknown mode must never silently deploy
    // as merge).
    if opts.mode != "sync" && opts.mode != "merge" {
        return Err(format!(
            "invalid deploy mode {:?} (expected \"sync\" or \"merge\")",
            opts.mode
        ));
    }
    // Sync with an empty dst would be a no-op copy (hook-only skips the
    // copy stage) — refuse it so the intent is never silently dropped.
    if opts.mode == "sync" && opts.dst.is_empty() {
        return Err(
            "deploy.mode \"sync\" with an empty dst — hook-only deploys must use \"merge\"".into(),
        );
    }
    let (repo_root, updated, cache_dir, warnings) = match &opts.source {
        crate::skill_catalog::GitSource::Local(path) => {
            if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
                return Err(format!(
                    "local source symlink not supported: {}",
                    path.display()
                ));
            }
            if !path.is_dir() {
                return Err(format!(
                    "local source directory not found: {}",
                    path.display()
                ));
            }
            (path.clone(), false, None, Vec::new())
        }
        crate::skill_catalog::GitSource::Remote(url) => {
            let cache_dir = cache_dir_for(&opts.src_root, &opts.agent, &opts.path);
            let (updated, proxy) = sync_sources(opts, url, &cache_dir)?;
            let mut warnings: Vec<String> = Vec::new();
            checkout_sources(opts, &cache_dir, proxy.as_ref(), &mut warnings)?;
            (cache_dir.clone(), updated, Some(cache_dir), warnings)
        }
    };
    let deployed_dir = opts.project_dir.join(&opts.dst);
    guard_dst_path(&opts.project_dir, &opts.dst)?;
    std::fs::create_dir_all(&opts.project_dir).map_err(|e| {
        format!(
            "cannot create project dir {}: {e}",
            opts.project_dir.display()
        )
    })?;
    // An empty dst is HOOK-ONLY: nothing is copied into the project —
    // the post hook (e.g. the skills repo's install.sh) owns the entire
    // layout. This is the shared-monorepo deploy model where copying the
    // whole source tree into the project root would be wrong.
    if !opts.dst.is_empty() {
        match opts.mode.as_str() {
            "sync" => materialize_sync(opts, &repo_root, &deployed_dir)?,
            "merge" => materialize_merge(&repo_root, &deployed_dir)?,
            _ => unreachable!("mode validated above"),
        }
    }
    let stdout_tail = run_script(opts, &repo_root)?;
    Ok(DeployReport {
        cache_dir: cache_dir.unwrap_or_else(|| repo_root.clone()),
        deployed_dir,
        updated,
        stdout_tail,
        warnings,
    })
}

#[cfg(test)]
mod tests {

    use super::*;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    /// Serialize env access with the other lib tests that mutate env vars.
    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn git(repo: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} in {} failed: {}",
            repo.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A temp git repo with the given files committed (the local "remote").
    /// `exec` entries get +x (scripts). Real git, zero network.
    fn make_agent_repo(root: &Path, files: &[(&str, &str)], exec: &[&str]) -> PathBuf {
        let repo = root.join("agent-repo");
        std::fs::create_dir_all(&repo).unwrap();
        for (rel, content) in files {
            let p = repo.join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&p, content).unwrap();
        }
        use std::os::unix::fs::PermissionsExt;
        for rel in exec {
            let p = repo.join(rel);
            let mode = std::fs::metadata(&p).unwrap().permissions().mode();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode | 0o755)).unwrap();
        }
        // `-b main` pins the branch name — immune to the host's
        // init.defaultBranch config (parallel tests share no git config).
        git(&repo, &["init", "-b", "main"]);
        git(
            &repo,
            &["-c", "user.name=t", "-c", "user.email=t@t", "add", "."],
        );
        git(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-m",
                "init",
            ],
        );
        repo
    }

    fn opts(repo: &Path, project: &Path, src_root: &Path, script: Option<&str>) -> DeployOptions {
        DeployOptions {
            source: crate::skill_catalog::GitSource::Remote(format!("file://{}", repo.display())),
            r#ref: None,
            post_hook: script.map(String::from),
            dst: "dst".into(),
            mode: "merge".into(),
            agent: "a1".into(),
            path: vec!["t1".into()],
            project_dir: project.to_path_buf(),
            src_root: src_root.to_path_buf(),
            offline: false,
            force: false,
            timeout: Duration::from_secs(60),
        }
    }

    /// The hook that records the SPEC §11 env contract into the project
    /// (argv count too — argv-only invocation carries no positional args).
    const ENV_HOOK: &str = "#!/bin/sh\n\
printf '%s\\n' \"$DUTABO_AGENT|$DUTABO_TARGET|$DUTABO_ITEM|$DUTABO_PROJECT_DIR|$DUTABO_DST|$DUTABO_REPO_DIR\" > \"$DUTABO_PROJECT_DIR/env-marker\"\n\
printf '%s\\n' \"$PWD\" > \"$DUTABO_PROJECT_DIR/cwd-marker\"\n\
printf '%s\\n' \"$#\" > \"$DUTABO_PROJECT_DIR/argv-count\"\n";

    /// The production FNV-1a tag, copied verbatim (the naming contract is
    /// a cache-reuse guarantee: a redeploy MUST resolve the same dir).
    fn fnv1a_tag(s: &str) -> String {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in s.bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        format!("{h:08x}")
    }

    /// d1: the cache dir = `<src_root>/<agent>-<target>[-<item>]` (spec
    /// §10 — under `agents/` now); names are slugged for the cache path
    /// with a stable hash tag whenever the slug differs from the raw
    /// name (collision-free — see d18); `DUTABO_AGENT_SRC` overrides the
    /// root.
    #[test]
    fn cache_dir_composition() {
        let _guard = lock_env();
        // Safe names (no chars outside [A-Za-z0-9._-]) stay EXACTLY the
        // same — no tag.
        assert_eq!(
            cache_dir_for(Path::new("/cache"), "a1", &["t1".to_string()]),
            PathBuf::from("/cache/a1-t1")
        );
        assert_eq!(
            cache_dir_for(Path::new("/cache"), "a1", &["t1".into(), "i1".into()]),
            PathBuf::from("/cache/a1-t1-i1")
        );
        // Legal-but-odd names (interior space, unicode, `..`, `/`) stay
        // INSIDE the cache root via the slug, tagged per raw name.
        assert_eq!(
            cache_dir_for(Path::new("/cache"), "my agent", &["My Target".into()]),
            PathBuf::from(format!(
                "/cache/my_agent-h{}-My_Target-h{}",
                fnv1a_tag("my agent"),
                fnv1a_tag("My Target")
            )),
        );
        let weird = cache_dir_for(Path::new("/cache"), "..", &["x/y".into()]);
        assert_eq!(
            weird,
            PathBuf::from(format!(
                "/cache/_-h{}-x_y-h{}",
                fnv1a_tag(".."),
                fnv1a_tag("x/y")
            )),
        );
        assert!(
            weird.components().all(|c| c != Component::ParentDir),
            "no name can escape the cache root: {weird:?}"
        );
        assert_eq!(
            cache_dir_for(Path::new("/cache"), ".", &["t".into()]),
            PathBuf::from(format!("/cache/_-h{}-t", fnv1a_tag("."))),
        );
        // Paths deeper than target+item carry a path-hash tag so
        // dash-joined segments can never collide across shapes.
        let deep = cache_dir_for(
            Path::new("/cache"),
            "a",
            &["b".into(), "c".into(), "d".into()],
        );
        let flat = cache_dir_for(Path::new("/cache"), "a", &["b-c".into(), "d".into()]);
        assert_ne!(deep, flat, "distinct paths never share a cache clone");
        unsafe {
            std::env::set_var("DUTABO_AGENT_SRC", "/tmp/agent-src");
        }
        assert_eq!(default_src_root(), PathBuf::from("/tmp/agent-src"));
        unsafe {
            std::env::remove_var("DUTABO_AGENT_SRC");
            std::env::set_var("XDG_CACHE_HOME", "/xdg");
        }
        assert_eq!(
            default_src_root(),
            PathBuf::from("/xdg/dutabo/agents"),
            "cache root is <XDG_CACHE_HOME>/dutabo/agents (spec §10)"
        );
        unsafe {
            std::env::remove_var("XDG_CACHE_HOME");
        }
    }

    /// d2: `proxy_pair_from_bashrc` parses the `http_proxy=` line (the
    /// global CLAUDE.md convention) and returns None when absent. An
    /// arbitrary made-up value is returned VERBATIM — the production path
    /// never hardcodes any proxy IP/port.
    #[test]
    fn test_proxy_pair_from_bashrc() {
        let bashrc = "# comment\nexport http_proxy=http://10.1.2.3:7890\nalias ll='ls -l'\n";
        let pair = proxy_pair_from_bashrc(bashrc).expect("parses the proxy line");
        assert_eq!(
            pair,
            (
                "http://10.1.2.3:7890".to_string(),
                "http://10.1.2.3:7890".to_string()
            )
        );
        let weird = proxy_pair_from_bashrc("export http_proxy=http://weird-proxy.example:9999\n");
        assert_eq!(
            weird,
            Some((
                "http://weird-proxy.example:9999".to_string(),
                "http://weird-proxy.example:9999".to_string()
            ))
        );
        assert!(proxy_pair_from_bashrc("no proxy here\n").is_none());
        assert!(proxy_pair_from_bashrc("").is_none());
    }

    /// d3: END-TO-END (2-level path): the project dst gets a plain FILE
    /// COPY — files land with exec bits preserved, the repo's `.git` is
    /// NOT copied, the hook ran with cwd = the CACHE clone (repo root)
    /// and the spec §11 env vars (argv-only invocation: no positional
    /// args).
    #[test]
    fn deploy_copies_worktree_no_git_and_runs_hook_with_spec_env() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = make_agent_repo(
            tmp.path(),
            &[
                ("install.sh", ENV_HOOK),
                ("files/a.txt", "A\n"),
                ("plain.txt", "P\n"),
            ],
            &["install.sh"],
        );
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let src_root = tmp.path().join("cache");
        let report =
            deploy(&opts(&repo, &project, &src_root, Some("install.sh"))).expect("deploy ok");
        assert!(!report.updated, "fresh cache clone");
        let dst = project.join("dst");
        assert_eq!(report.deployed_dir, dst);
        // Plain file copy: content landed, no .git anywhere in the dst.
        assert_eq!(
            std::fs::read_to_string(dst.join("files/a.txt")).unwrap(),
            "A\n"
        );
        assert_eq!(
            std::fs::read_to_string(dst.join("plain.txt")).unwrap(),
            "P\n"
        );
        assert!(
            !dst.join(".git").exists(),
            "the repo's .git is NEVER copied (spec §10)"
        );
        // Exec bits preserved.
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dst.join("install.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert!(
            mode & 0o111 != 0,
            "install.sh stayed executable in the project"
        );
        // The hook ran from the cache clone with the spec env contract.
        let cwd = std::fs::read_to_string(project.join("cwd-marker")).unwrap();
        assert_eq!(
            cwd.trim_end(),
            report.cache_dir.display().to_string(),
            "hook cwd = the repo root (cache clone)"
        );
        let env = std::fs::read_to_string(project.join("env-marker")).unwrap();
        assert_eq!(
            env.trim_end(),
            format!(
                "a1|t1||{}|dst|{}",
                project.display(),
                report.cache_dir.display()
            ),
            "DUTABO_AGENT/TARGET/ITEM/PROJECT_DIR/DST/REPO_DIR (2-level: ITEM=\"\")"
        );
        assert_eq!(
            std::fs::read_to_string(project.join("argv-count"))
                .unwrap()
                .trim_end(),
            "0",
            "argv-only invocation: no positional args"
        );
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        // The cache clone exists with its .git intact.
        assert!(report.cache_dir.join(".git").is_dir());
    }

    /// d4: `leaf.ref` is CHECKED OUT — branch, tag, and commit-sha refs
    /// all pin the copy; absent ref follows the remote default branch.
    #[test]
    fn ref_checkout_pins_branch_tag_commit_and_default() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = make_agent_repo(
            tmp.path(),
            &[("install.sh", "#!/bin/sh\ntrue\n"), ("who.txt", "main\n")],
            &["install.sh"],
        );
        let main_sha = String::from_utf8(
            std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&repo)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        // A dev branch + a tag with different content.
        git(&repo, &["checkout", "-b", "dev"]);
        std::fs::write(repo.join("who.txt"), "dev\n").unwrap();
        git(
            &repo,
            &["-c", "user.name=t", "-c", "user.email=t@t", "add", "."],
        );
        git(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-m",
                "dev",
            ],
        );
        git(&repo, &["tag", "v1"]);
        git(&repo, &["checkout", "main"]);

        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let src_root = tmp.path().join("cache");
        let who = |o: &DeployOptions| {
            deploy(o).expect("deploy ok");
            std::fs::read_to_string(o.project_dir.join("dst").join("who.txt")).unwrap()
        };
        // No ref → the remote default branch (main).
        let mut o = opts(&repo, &project, &src_root, Some("install.sh"));
        assert_eq!(who(&o), "main\n");
        // Branch ref.
        o.r#ref = Some("dev".into());
        assert_eq!(who(&o), "dev\n");
        // Tag ref.
        o.r#ref = Some("v1".into());
        assert_eq!(who(&o), "dev\n", "v1 tags the dev commit");
        // Commit-sha ref.
        o.r#ref = Some(main_sha.clone());
        assert_eq!(who(&o), "main\n");
        // Back to default: a NEW main commit must land after redeploy.
        std::fs::write(repo.join("who.txt"), "main2\n").unwrap();
        git(
            &repo,
            &["-c", "user.name=t", "-c", "user.email=t@t", "add", "."],
        );
        git(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-m",
                "main2",
            ],
        );
        o.r#ref = None;
        assert_eq!(
            who(&o),
            "main2\n",
            "no ref follows the updated default branch"
        );
    }

    /// d6: mode semantics — merge keeps extras and stale files; sync
    /// deletes stale project files (and dirs) so dst mirrors the repo.
    #[test]
    fn merge_keeps_extras_sync_deletes_stale() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = make_agent_repo(
            tmp.path(),
            &[
                ("install.sh", "#!/bin/sh\ntrue\n"),
                ("a.txt", "A1\n"),
                ("sub/keep.txt", "K\n"),
            ],
            &["install.sh"],
        );
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let src_root = tmp.path().join("cache");
        // v1 deploy (merge).
        let mut o = opts(&repo, &project, &src_root, Some("install.sh"));
        deploy(&o).expect("v1 deploy");
        // User extras appear in dst.
        std::fs::write(project.join("dst/extra.txt"), "E\n").unwrap();
        // Upstream v2: a.txt removed, b.txt added, keep.txt updated.
        std::fs::remove_file(repo.join("a.txt")).unwrap();
        std::fs::write(repo.join("b.txt"), "B\n").unwrap();
        std::fs::write(repo.join("sub/keep.txt"), "K2\n").unwrap();
        git(
            &repo,
            &["-c", "user.name=t", "-c", "user.email=t@t", "add", "."],
        );
        git(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-m",
                "v2",
            ],
        );
        // MERGE redeploy: stale a.txt and extra.txt both SURVIVE.
        deploy(&o).expect("merge redeploy");
        assert!(project.join("dst/a.txt").exists(), "merge never deletes");
        assert!(project.join("dst/extra.txt").exists(), "merge keeps extras");
        assert_eq!(
            std::fs::read_to_string(project.join("dst/b.txt")).unwrap(),
            "B\n"
        );
        assert_eq!(
            std::fs::read_to_string(project.join("dst/sub/keep.txt")).unwrap(),
            "K2\n",
            "merge overwrites"
        );
        // SYNC redeploy: a.txt and extra.txt are DELETED; the rest lands.
        o.mode = "sync".into();
        deploy(&o).expect("sync redeploy");
        assert!(
            !project.join("dst/a.txt").exists(),
            "sync deletes stale files"
        );
        assert!(
            !project.join("dst/extra.txt").exists(),
            "sync deletes extras"
        );
        assert_eq!(
            std::fs::read_to_string(project.join("dst/b.txt")).unwrap(),
            "B\n"
        );
        assert_eq!(
            std::fs::read_to_string(project.join("dst/sub/keep.txt")).unwrap(),
            "K2\n"
        );
        // A stale EMPTY dir is removed too.
        std::fs::create_dir_all(project.join("dst/junk-dir")).unwrap();
        deploy(&o).expect("sync removes stale dirs");
        assert!(
            !project.join("dst/junk-dir").exists(),
            "sync deletes stale dirs"
        );
    }

    /// d7: sync with dst == "" is REFUSED at the deploy level too (the
    /// catalog validates it; DeployOptions can be built directly).
    #[test]
    fn sync_into_project_root_refused() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = make_agent_repo(
            tmp.path(),
            &[("install.sh", "#!/bin/sh\ntrue\n"), ("a.txt", "A\n")],
            &["install.sh"],
        );
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("precious.txt"), "P\n").unwrap();
        let mut o = opts(
            &repo,
            &project,
            &tmp.path().join("cache"),
            Some("install.sh"),
        );
        o.dst = String::new();
        o.mode = "sync".into();
        let err = deploy(&o).unwrap_err();
        assert!(err.contains("sync"), "{err}");
        assert!(err.contains("hook-only"), "{err}");
        assert_eq!(
            std::fs::read_to_string(project.join("precious.txt")).unwrap(),
            "P\n",
            "nothing was destroyed"
        );
    }

    /// d8: post_hook missing from the repo → an error naming it; an
    /// ABSENT post_hook (None) is skipped silently (spec v1: optional).
    #[test]
    fn post_hook_missing_and_optional() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = make_agent_repo(
            tmp.path(),
            &[("install.sh", "#!/bin/sh\ntrue\n"), ("a.txt", "A\n")],
            &["install.sh"],
        );
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let err = deploy(&opts(
            &repo,
            &project,
            &tmp.path().join("cache"),
            Some("nope.sh"),
        ))
        .unwrap_err();
        assert!(err.contains("nope.sh"), "{err}");
        // None → skipped: deploy succeeds with no hook output.
        let report = deploy(&opts(&repo, &project, &tmp.path().join("c2"), None))
            .expect("no hook deploys fine");
        assert!(report.stdout_tail.is_empty());
        assert!(project.join("dst/a.txt").exists(), "the copy still landed");
    }

    /// d9: a NON-executable hook runs via `sh <script>` (argv, never
    /// `sh -c`) — the marker proves it executed.
    #[test]
    fn non_executable_hook_runs_via_sh_argv() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = make_agent_repo(
            tmp.path(),
            &[(
                "install.sh",
                "#!/bin/sh\nprintf ran > \"$DUTABO_PROJECT_DIR/non-exec-ran\"\n",
            )],
            &[], // no +x
        );
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        deploy(&opts(
            &repo,
            &project,
            &tmp.path().join("cache"),
            Some("install.sh"),
        ))
        .expect("deploy ok");
        assert_eq!(
            std::fs::read_to_string(project.join("non-exec-ran")).unwrap(),
            "ran"
        );
    }

    /// d10: a script exiting non-zero reports `exit N` with the last
    /// stderr line.
    #[test]
    fn nonzero_exit_reported() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = make_agent_repo(
            tmp.path(),
            &[("install.sh", "#!/bin/sh\necho boom >&2\nexit 3\n")],
            &["install.sh"],
        );
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let err = deploy(&opts(
            &repo,
            &project,
            &tmp.path().join("cache"),
            Some("install.sh"),
        ))
        .unwrap_err();
        assert!(err.contains("exit 3"), "{err}");
        assert!(err.contains("boom"), "stderr tail surfaces: {err}");
    }

    /// d11: a clone failure (nonexistent remote path) reports the git
    /// stderr tail — real git, zero network.
    /// Hook-only deploy (dst ""): the hook runs with the full env
    /// contract but NOT ONE file of the repo lands in the project — the
    /// post hook owns the layout (the shared-skills-repo model).
    #[test]
    fn empty_dst_is_hook_only_no_project_copy() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = make_agent_repo(
            tmp.path(),
            &[
                ("install.sh", ENV_HOOK),
                ("skills/rockchip/rk-build/SKILL.md", "fixture\n"),
            ],
            &["install.sh"],
        );
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let mut options = opts(
            &repo,
            &project,
            &tmp.path().join("cache"),
            Some("install.sh"),
        );
        options.dst = String::new();
        let report = deploy(&options).expect("hook-only deploy ok");
        assert_eq!(report.deployed_dir, project);
        assert!(
            !project.join("skills").exists(),
            "the repo tree is NEVER copied when dst is empty"
        );
        assert!(
            std::fs::read_to_string(project.join("cwd-marker")).is_ok(),
            "the post hook still ran from the cache clone"
        );
        let env = std::fs::read_to_string(project.join("env-marker")).unwrap();
        assert!(env.contains("|"), "spec §11 env contract: {env}");
    }

    #[test]
    fn clone_failure_reported() {
        let tmp = tempfile::TempDir::new().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let mut o = opts(
            &tmp.path().join("does-not-exist"),
            &project,
            &tmp.path().join("cache"),
            Some("install.sh"),
        );
        o.source = crate::skill_catalog::GitSource::Remote(
            tmp.path().join("does-not-exist").display().to_string(),
        );
        let err = deploy(&o).unwrap_err();
        assert!(err.contains("git clone failed"), "{err}");
        assert!(err.contains("does-not-exist"), "{err}");
    }

    /// d12: `--offline` skips the network — a missing cache clone → Err;
    /// an existing cache clone deploys with zero network (the source repo
    /// is DELETED before the offline redeploy), including a ref checkout
    /// against the locally cached objects.
    #[test]
    fn offline_requires_cache_and_checkouts_locally() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = make_agent_repo(
            tmp.path(),
            &[
                (
                    "install.sh",
                    "#!/bin/sh\necho x >> \"$DUTABO_PROJECT_DIR/count.txt\"\n",
                ),
                ("who.txt", "main\n"),
            ],
            &["install.sh"],
        );
        git(&repo, &["checkout", "-b", "dev"]);
        std::fs::write(repo.join("who.txt"), "dev\n").unwrap();
        git(
            &repo,
            &["-c", "user.name=t", "-c", "user.email=t@t", "add", "."],
        );
        git(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-m",
                "dev",
            ],
        );
        git(&repo, &["checkout", "main"]);

        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let src_root = tmp.path().join("cache");
        let mut o = opts(&repo, &project, &src_root, Some("install.sh"));
        // Offline with NO cache → Err.
        o.offline = true;
        let err = deploy(&o).unwrap_err();
        assert!(err.contains("offline") || err.contains("cache"), "{err}");
        // Online deploy first → cache exists.
        o.offline = false;
        o.r#ref = Some("dev".into());
        deploy(&o).expect("online deploy ok");
        assert_eq!(
            std::fs::read_to_string(project.join("dst/who.txt")).unwrap(),
            "dev\n"
        );
        // Delete the source repo → any network attempt would fail.
        std::fs::remove_dir_all(&repo).unwrap();
        // Offline redeploy from the cache → Ok, ref checked out locally,
        // script ran again.
        o.offline = true;
        let report = deploy(&o).expect("offline deploy from cache");
        assert!(!report.updated, "offline never fetches");
        assert_eq!(
            std::fs::read_to_string(project.join("dst/who.txt")).unwrap(),
            "dev\n",
            "ref checkout works offline against cached objects"
        );
        let count = std::fs::read_to_string(project.join("count.txt")).unwrap();
        assert_eq!(count.lines().count(), 2, "the script ran again: {count}");
    }

    /// d13: symlink safety (spec §12 rule 6) — deploying THROUGH a
    /// symlinked dst (or intermediate component) is refused; symlinks
    /// INSIDE the repo are copied AS symlinks (never followed).
    #[test]
    fn symlink_dst_refused_repo_symlinks_copied() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = make_agent_repo(
            tmp.path(),
            &[("install.sh", "#!/bin/sh\ntrue\n"), ("target.txt", "T\n")],
            &["install.sh"],
        );
        // A symlink inside the repo.
        symlink("target.txt", repo.join("link.txt")).unwrap();
        git(
            &repo,
            &["-c", "user.name=t", "-c", "user.email=t@t", "add", "."],
        );
        git(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-m",
                "link",
            ],
        );

        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let src_root = tmp.path().join("cache");
        let o = opts(&repo, &project, &src_root, Some("install.sh"));
        // dst itself is a symlink → refused.
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, project.join("dst")).unwrap();
        let err = deploy(&o).unwrap_err();
        assert!(err.contains("symlink"), "{err}");
        assert_eq!(
            std::fs::read_dir(&outside).unwrap().count(),
            0,
            "nothing written through the symlink"
        );
        std::fs::remove_file(project.join("dst")).unwrap();
        // An INTERMEDIATE component is a symlink → refused.
        let mut nested = o.clone();
        nested.dst = "outer/dst".into();
        symlink(&outside, project.join("outer")).unwrap();
        let err = deploy(&nested).unwrap_err();
        assert!(err.contains("symlink"), "{err}");
        std::fs::remove_file(project.join("outer")).unwrap();
        // A clean deploy copies the repo's OWN symlink as a symlink.
        let report = deploy(&o).expect("clean deploy ok");
        assert_eq!(
            std::fs::read_link(report.deployed_dir.join("link.txt")).unwrap(),
            PathBuf::from("target.txt"),
            "repo symlinks are copied as symlinks"
        );
        assert_eq!(
            std::fs::read_to_string(report.deployed_dir.join("target.txt")).unwrap(),
            "T\n"
        );
    }

    /// d14: a DIRTY cache clone (local edits in the repo the project is
    /// copied FROM) is refused naming the paths; `--force` discards them.
    #[test]
    fn dirty_cache_refused_without_force() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = make_agent_repo(
            tmp.path(),
            &[("install.sh", "#!/bin/sh\ntrue\n"), ("a.txt", "A\n")],
            &["install.sh"],
        );
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let src_root = tmp.path().join("cache");
        let o = opts(&repo, &project, &src_root, Some("install.sh"));
        deploy(&o).expect("first deploy ok");
        // Local edit INSIDE the cache clone.
        let cache_a = src_root.join("a1-t1").join("a.txt");
        std::fs::write(&cache_a, "LOCAL EDIT\n").unwrap();
        // Refuse without --force: the dirty path is named, nothing copied.
        let err = deploy(&o).unwrap_err();
        assert!(err.contains("a.txt"), "the dirty path is named: {err}");
        assert!(err.contains("--force"), "the override is named: {err}");
        assert_eq!(
            std::fs::read_to_string(project.join("dst/a.txt")).unwrap(),
            "A\n",
            "the previous deploy is untouched"
        );
        // --force proceeds: the local edit is DISCARDED (reset --hard)
        // and the upstream content deploys; the cache is clean again.
        let mut forced = o;
        forced.force = true;
        deploy(&forced).expect("force redeploy ok");
        assert_eq!(
            std::fs::read_to_string(project.join("dst/a.txt")).unwrap(),
            "A\n",
            "force discards the local cache edit and deploys upstream"
        );
        assert_eq!(
            std::fs::read_to_string(&cache_a).unwrap(),
            "A\n",
            "the cache clone was reset"
        );
    }

    /// d15: redeploy updates content in place (overwrite), preserving
    /// exec bits of the repo copy — no `.git` machinery anywhere.
    #[test]
    fn redeploy_overwrites_and_preserves_modes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = make_agent_repo(
            tmp.path(),
            &[("install.sh", "#!/bin/sh\ntrue\n"), ("a.txt", "v1\n")],
            &["install.sh"],
        );
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let src_root = tmp.path().join("cache");
        let o = opts(&repo, &project, &src_root, Some("install.sh"));
        let first = deploy(&o).expect("first deploy");
        // chmod the PROJECT copy to non-executable; upstream moves on.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            first.deployed_dir.join("install.sh"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        std::fs::write(repo.join("a.txt"), "v2\n").unwrap();
        git(
            &repo,
            &["-c", "user.name=t", "-c", "user.email=t@t", "add", "."],
        );
        git(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-m",
                "v2",
            ],
        );
        let second = deploy(&o).expect("second deploy");
        assert!(second.updated, "the cache clone was fetched");
        assert_eq!(
            std::fs::read_to_string(project.join("dst/a.txt")).unwrap(),
            "v2\n",
            "overwritten in place"
        );
        let mode = std::fs::metadata(project.join("dst/install.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert!(
            mode & 0o111 != 0,
            "exec bits restored from the repo copy on redeploy"
        );
    }

    /// d17: an unknown mode is REFUSED at the deploy level (the catalog
    /// validates it, but DeployOptions can be built directly — an unknown
    /// mode must never silently deploy as merge).
    #[test]
    fn unknown_mode_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = make_agent_repo(
            tmp.path(),
            &[("install.sh", "#!/bin/sh\ntrue\n"), ("a.txt", "A\n")],
            &["install.sh"],
        );
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let mut o = opts(
            &repo,
            &project,
            &tmp.path().join("cache"),
            Some("install.sh"),
        );
        o.mode = "rsync".into();
        let err = deploy(&o).unwrap_err();
        assert!(err.contains("mode") && err.contains("rsync"), "{err}");
        assert!(
            !project.join("dst").exists(),
            "nothing deployed on an unknown mode"
        );
    }

    /// d18: cache-dir names are COLLISION-FREE — two distinct names whose
    /// slugs coincide ("a b" vs "a_b") must never share one cache clone
    /// (a shared dir would fetch the FIRST clone's origin and copy the
    /// wrong repo's content).
    #[test]
    fn cache_dir_slug_never_collides() {
        assert_ne!(
            cache_dir_for(Path::new("/cache"), "a", &["b c".into()]),
            cache_dir_for(Path::new("/cache"), "a", &["b_c".into()])
        );
        assert_ne!(
            cache_dir_for(Path::new("/cache"), "my agent", &["t".into()]),
            cache_dir_for(Path::new("/cache"), "my_agent", &["t".into()])
        );
        assert_ne!(
            cache_dir_for(Path::new("/cache"), "x", &["y".into(), "i j".into()]),
            cache_dir_for(Path::new("/cache"), "x", &["y".into(), "i_j".into()])
        );
    }

    /// d16: a 3-LEVEL path sets DUTABO_TARGET=group, DUTABO_ITEM=item
    /// (spec §11) and names the cache dir `<agent>-<target>-<item>`.
    #[test]
    fn three_level_path_env_and_cache_naming() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = make_agent_repo(
            tmp.path(),
            &[("install.sh", ENV_HOOK), ("a.txt", "A\n")],
            &["install.sh"],
        );
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let src_root = tmp.path().join("cache");
        let mut o = opts(&repo, &project, &src_root, Some("install.sh"));
        o.agent = "claude-code".into();
        o.path = vec!["Rockchip".into(), "AOSP".into()];
        let report = deploy(&o).expect("3-level deploy ok");
        assert_eq!(
            report.cache_dir,
            src_root.join("claude-code-Rockchip-AOSP"),
            "3-level cache dir"
        );
        let env = std::fs::read_to_string(project.join("env-marker")).unwrap();
        assert_eq!(
            env.trim_end(),
            format!(
                "claude-code|Rockchip|AOSP|{}|dst|{}",
                project.display(),
                report.cache_dir.display()
            ),
            "DUTABO_TARGET=group, DUTABO_ITEM=item"
        );
    }
}
