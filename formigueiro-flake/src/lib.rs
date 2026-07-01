//! # formigueiro-flake — the flake-input Environment
//!
//! [`formigueiro_core`] owns the *algorithm* (shadow = current vs latest,
//! promotion, the swarm). This crate is its **`flake-input` [`UpdateEnv`]** — the
//! concrete side-effect boundary that answers, for a signal naming a flake input:
//!
//! - **current** — the input's locked rev, read from a typed parse of `flake.lock`
//!   ([`FlakeLock`]); pure, no I/O.
//! - **latest** — the input's upstream head, resolved via a [`RefResolver`].
//!
//! [`RefResolver`] is the **[TYPED-SPEC triplet] Environment seam**: the real impl
//! ([`GitLsRemoteResolver`]) runs `git ls-remote` with a **typed argv (no shell)**,
//! while tests substitute a mock — so the swarm's flake behavior is exercised end
//! to end without touching the network. This crate does not re-author the shadow
//! algorithm; it satisfies [`UpdateEnv`], which is exactly the boundary the triplet
//! says to abstract behind a trait.
//!
//! All errors are typed ([`FlakeError`]); nothing panics, unwraps a fallible parse,
//! or emits a `format!()`-composed string (the `Display` impl is the render seam).
//!
//! [TYPED-SPEC triplet]: https://github.com/pleme-io/theory (§ TYPED-SPEC + INTERPRETER TRIPLET)

use std::fmt;
use std::path::PathBuf;
use std::process::Command;

use formigueiro_core::{
    AppliedMutation, ApplyError, ApplyExecutor, ApplyReceipt, BlockReason, SignalSource, UpdateEnv,
    UpdateSignal,
};
use serde::{Deserialize, Serialize};

/// A typed error from parsing or resolving a flake. Its [`fmt::Display`] impl is
/// the only string-render seam (no `format!()` of composed messages).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "flakeError", content = "detail")]
pub enum FlakeError {
    /// `flake.lock` was not valid JSON.
    BadJson,
    /// `flake.lock` lacked the `nodes` / `root` structure this parser needs.
    MalformedLock,
    /// The signal named an input not present in the lock.
    UnknownInput(String),
    /// The input has no resolvable upstream (e.g. a `path:` / `indirect:` input).
    Unresolvable(String),
}

impl fmt::Display for FlakeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadJson => f.write_str("flake.lock is not valid JSON"),
            Self::MalformedLock => f.write_str("flake.lock is missing its nodes/root structure"),
            Self::UnknownInput(name) => {
                f.write_str("input not present in flake.lock: ")?;
                f.write_str(name)
            }
            Self::Unresolvable(name) => {
                f.write_str("input has no resolvable upstream: ")?;
                f.write_str(name)
            }
        }
    }
}

impl std::error::Error for FlakeError {}

impl FlakeError {
    /// A [`BlockReason`] carrying this error's typed message — the bridge into
    /// [`formigueiro_core`]'s shadow outcome.
    #[must_use]
    pub fn block(&self) -> BlockReason {
        BlockReason::Error(self.to_string())
    }
}

/// One flake input's locked identity + enough of its origin to resolve upstream.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LockedInput {
    /// The currently-locked revision (the `locked.rev` in `flake.lock`).
    pub locked_rev: Option<String>,
    /// The clone URL to resolve the upstream head against (built from `original`).
    pub url: Option<String>,
    /// The ref (branch/tag) to resolve; `None` means the remote's default head.
    pub git_ref: Option<String>,
}

/// A typed, minimal parse of `flake.lock` — the root's direct inputs, each mapped
/// to its [`LockedInput`]. Enough to shadow input updates; not a full lock model.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlakeLock {
    inputs: std::collections::BTreeMap<String, LockedInput>,
}

impl FlakeLock {
    /// Parse `flake.lock` JSON into the typed input map. Resolves each root input
    /// name through the `nodes` graph to its locked node, extracting the locked rev
    /// and (for github/git inputs) a resolvable URL + ref. Inputs that follow
    /// another input (array refs) or lack a locked node are skipped, not errors.
    ///
    /// # Errors
    /// [`FlakeError::BadJson`] on invalid JSON; [`FlakeError::MalformedLock`] when
    /// the `nodes` / root-`inputs` structure is absent.
    pub fn parse(json: &str) -> Result<Self, FlakeError> {
        let value: serde_json::Value =
            serde_json::from_str(json).map_err(|_| FlakeError::BadJson)?;
        let nodes = value
            .get("nodes")
            .and_then(serde_json::Value::as_object)
            .ok_or(FlakeError::MalformedLock)?;
        let root_key = value.get("root").and_then(serde_json::Value::as_str).unwrap_or("root");
        let root_inputs = nodes
            .get(root_key)
            .and_then(|n| n.get("inputs"))
            .and_then(serde_json::Value::as_object)
            .ok_or(FlakeError::MalformedLock)?;

        let mut inputs = std::collections::BTreeMap::new();
        for (name, node_ref) in root_inputs {
            // A direct input maps to a node key (string). A `follows` input maps to
            // an array path — skip it (it has no independent lock to bump).
            let Some(node_key) = node_ref.as_str() else {
                continue;
            };
            let Some(node) = nodes.get(node_key) else {
                continue;
            };
            let locked_rev = node
                .get("locked")
                .and_then(|l| l.get("rev"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            let (url, git_ref) = origin_of(node.get("original").or_else(|| node.get("locked")));
            inputs.insert(
                name.clone(),
                LockedInput {
                    locked_rev,
                    url,
                    git_ref,
                },
            );
        }
        Ok(Self { inputs })
    }

    /// The locked rev of `input`, if tracked.
    #[must_use]
    pub fn locked_rev(&self, input: &str) -> Option<&str> {
        self.inputs.get(input).and_then(|i| i.locked_rev.as_deref())
    }

    /// The full [`LockedInput`] for `input`, if tracked.
    #[must_use]
    pub fn input(&self, input: &str) -> Option<&LockedInput> {
        self.inputs.get(input)
    }

    /// Every tracked input name.
    #[must_use]
    pub fn input_names(&self) -> Vec<&str> {
        self.inputs.keys().map(String::as_str).collect()
    }
}

/// Build a resolvable `(url, git_ref)` from a node's `original`/`locked` object.
/// `github` inputs get an `https://github.com/<owner>/<repo>` URL built by typed
/// concatenation (no `format!()` of the URL). `git` inputs use their `url`.
fn origin_of(origin: Option<&serde_json::Value>) -> (Option<String>, Option<String>) {
    let Some(o) = origin else {
        return (None, None);
    };
    let git_ref = o.get("ref").and_then(serde_json::Value::as_str).map(str::to_owned);
    let kind = o.get("type").and_then(serde_json::Value::as_str).unwrap_or_default();
    match kind {
        "github" => {
            let owner = o.get("owner").and_then(serde_json::Value::as_str);
            let repo = o.get("repo").and_then(serde_json::Value::as_str);
            let url = match (owner, repo) {
                (Some(owner), Some(repo)) => {
                    let mut u = String::from("https://github.com/");
                    u.push_str(owner);
                    u.push('/');
                    u.push_str(repo);
                    Some(u)
                }
                _ => None,
            };
            (url, git_ref)
        }
        "git" => (
            o.get("url").and_then(serde_json::Value::as_str).map(str::to_owned),
            git_ref,
        ),
        _ => (None, git_ref),
    }
}

/// Resolve the current upstream head for a locked input. The **Environment seam**:
/// the real impl runs `git ls-remote` (typed argv, no shell); tests mock it.
pub trait RefResolver {
    /// The upstream head rev for `input`.
    ///
    /// # Errors
    /// A [`BlockReason`] when the input can't be resolved (no URL, unreachable,
    /// no matching ref).
    fn resolve(&self, input: &LockedInput) -> Result<String, BlockReason>;
}

/// Rewrite an `https://github.com/...` URL to carry a token as Basic-auth
/// credentials (`https://x-access-token:<token>@github.com/...`) — GitHub's git
/// endpoint accepts a classic or fine-grained token as the password for **both**
/// public and private repos, so `git ls-remote` authenticates without a credential
/// helper or keychain (a launchd/systemd daemon has neither). `None` for a non-https
/// URL. Typed concatenation, no `format!()`.
fn authed_url(url: &str, token: &str) -> Option<String> {
    url.strip_prefix("https://").map(|rest| {
        let mut authed = String::from("https://x-access-token:");
        authed.push_str(token);
        authed.push('@');
        authed.push_str(rest);
        authed
    })
}

/// The production [`RefResolver`]: `git ls-remote <url> <ref>`, argv-typed, no
/// shell. When constructed [`GitLsRemoteResolver::with_token`], it injects a bearer
/// header for github so **private** repos resolve in a credential-less daemon
/// context (the fix for the private-input blocks a keychain-less launchd agent hit).
/// Parses the leading SHA from the first output line.
#[derive(Clone, Debug, Default)]
pub struct GitLsRemoteResolver {
    token: Option<String>,
}

impl GitLsRemoteResolver {
    /// A resolver for public repos only (no auth).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A resolver that falls back to authenticating github with `token` for repos
    /// that don't resolve anonymously (private). A `None`/empty token is equivalent
    /// to [`GitLsRemoteResolver::new`].
    #[must_use]
    pub fn with_token(token: Option<String>) -> Self {
        Self {
            token: token.filter(|t| !t.is_empty()),
        }
    }

    /// One `git ls-remote <url> <ref>` attempt, **fully non-interactive**: no
    /// credential helper (`-c credential.helper=` → never invokes the macOS keychain
    /// / a GUI password prompt) and no terminal prompt (`GIT_TERMINAL_PROMPT=0`). So
    /// auth comes ONLY from the URL-embedded token; a repo it can't reach fails
    /// silently (returns `None`) instead of blocking on a popup. Critical for a
    /// launchd/systemd daemon — a prompt would hang the agent + spam the user.
    fn try_ls_remote(url: &str, git_ref: &str) -> Option<String> {
        let output = Command::new("git")
            .args(["ls-remote", url, git_ref])
            // Definitive no-prompt in a daemon: point BOTH config paths at /dev/null
            // so git loads NO config at all — the system gitconfig sets
            // credential.helper=osxkeychain (the GUI keychain popup), and neither
            // `-c credential.helper=` nor GIT_CONFIG_NOSYSTEM reliably suppresses it on
            // git's HTTP credential path; replacing the config paths does. Plus no
            // terminal prompt and a null askpass. Auth comes ONLY from the URL token.
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "true")
            .env("SSH_ASKPASS", "true")
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .next()
            .map(str::to_owned)
    }
}

impl RefResolver for GitLsRemoteResolver {
    fn resolve(&self, input: &LockedInput) -> Result<String, BlockReason> {
        let url = input
            .url
            .as_deref()
            .ok_or_else(|| FlakeError::Unresolvable(String::new()).block())?;
        let git_ref = input.git_ref.as_deref().unwrap_or("HEAD");
        // Token-authenticated first (Basic auth works for both public and private);
        // fall back to anonymous when there's no token or the token can't reach it.
        if let Some(token) = &self.token {
            if let Some(authed) = authed_url(url, token) {
                if let Some(sha) = Self::try_ls_remote(&authed, git_ref) {
                    return Ok(sha);
                }
            }
        }
        Self::try_ls_remote(url, git_ref).ok_or(BlockReason::Unreachable)
    }
}

/// The `flake-input` [`UpdateEnv`]: adapts a parsed [`FlakeLock`] (for the current
/// locked rev) plus a [`RefResolver`] (for the upstream head) to the boundary
/// [`formigueiro_core`] observes. A signal's `subject` is the flake input name.
pub struct FlakeEnv<'a, R: RefResolver> {
    lock: &'a FlakeLock,
    resolver: &'a R,
}

impl<'a, R: RefResolver> FlakeEnv<'a, R> {
    /// Compose an env from a parsed lock + a resolver.
    #[must_use]
    pub fn new(lock: &'a FlakeLock, resolver: &'a R) -> Self {
        Self { lock, resolver }
    }
}

impl<R: RefResolver> UpdateEnv for FlakeEnv<'_, R> {
    fn current(&self, sig: &UpdateSignal) -> Option<String> {
        self.lock.locked_rev(&sig.subject).map(str::to_owned)
    }
    fn latest(&self, sig: &UpdateSignal) -> Result<String, BlockReason> {
        match self.lock.input(&sig.subject) {
            Some(input) => self.resolver.resolve(input),
            None => Err(FlakeError::UnknownInput(sig.subject.clone()).block()),
        }
    }
}

/// A [`SignalSource`] over a parsed [`FlakeLock`]: emits a `flake-input` signal for
/// every tracked input — "consider every flake input this cycle." Pairs with
/// [`FlakeEnv`] as the ingestion half (source = what; env = current/latest).
pub struct FlakeSignalSource<'a> {
    lock: &'a FlakeLock,
}

impl<'a> FlakeSignalSource<'a> {
    /// A signal source over `lock`'s inputs.
    #[must_use]
    pub fn new(lock: &'a FlakeLock) -> Self {
        Self { lock }
    }
}

impl SignalSource for FlakeSignalSource<'_> {
    fn signals(&self) -> Vec<UpdateSignal> {
        self.lock
            .input_names()
            .into_iter()
            .map(|name| UpdateSignal::new("flake-input", name))
            .collect()
    }
}

/// The real `flake-input` [`ApplyExecutor`]: runs `nix flake update <subject>` in a
/// flake directory (**typed argv, no shell**) to bump one input's lock. Reached
/// only for a promoted mutation (an [`AppliedMutation`] can't be built otherwise),
/// and only when explicitly wired — the swarm's default is [`NullExecutor`].
/// Handles only `flake-input`; other kinds are [`ApplyError::Unsupported`].
pub struct NixFlakeExecutor {
    flake_dir: PathBuf,
}

impl NixFlakeExecutor {
    /// An executor that mutates the flake at `flake_dir`.
    pub fn new(flake_dir: impl Into<PathBuf>) -> Self {
        Self {
            flake_dir: flake_dir.into(),
        }
    }
}

impl ApplyExecutor for NixFlakeExecutor {
    fn apply(&self, mutation: &AppliedMutation) -> Result<ApplyReceipt, ApplyError> {
        if mutation.kind() != "flake-input" {
            return Err(ApplyError::Unsupported(mutation.kind().to_owned()));
        }
        let output = Command::new("nix")
            .args(["flake", "update", mutation.subject()])
            .current_dir(&self.flake_dir)
            .output()
            .map_err(|e| ApplyError::Failed(e.to_string()))?;
        if output.status.success() {
            Ok(ApplyReceipt::of(mutation))
        } else {
            Err(ApplyError::Failed(
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use formigueiro_core::{
        Colony, FlakeInputKind, MemPlanStore, ShadowOutcome, SignalSource, Swarm, TickOutcome,
        UpdateKind,
    };
    use outorga::{PromotionMode, PromotionPolicy};

    const LOCK: &str = r#"{
      "nodes": {
        "root": { "inputs": { "blackmatter": "blackmatter", "nixpkgs": "nixpkgs" } },
        "blackmatter": {
          "locked":   { "rev": "aaa111", "type": "github", "owner": "pleme-io", "repo": "blackmatter" },
          "original": { "type": "github", "owner": "pleme-io", "repo": "blackmatter", "ref": "main" }
        },
        "nixpkgs": {
          "locked":   { "rev": "bbb222", "type": "github", "owner": "NixOS", "repo": "nixpkgs" },
          "original": { "type": "github", "owner": "NixOS", "repo": "nixpkgs", "ref": "nixos-unstable" }
        }
      },
      "root": "root",
      "version": 7
    }"#;

    /// A mock resolver: input name (by URL suffix) → upstream head.
    struct MockResolver {
        head: &'static str,
    }
    impl RefResolver for MockResolver {
        fn resolve(&self, _input: &LockedInput) -> Result<String, BlockReason> {
            Ok(self.head.to_owned())
        }
    }

    #[test]
    fn parses_locked_revs_and_github_origins() {
        let lock = FlakeLock::parse(LOCK).unwrap();
        assert_eq!(lock.locked_rev("blackmatter"), Some("aaa111"));
        assert_eq!(lock.locked_rev("nixpkgs"), Some("bbb222"));
        let bm = lock.input("blackmatter").unwrap();
        assert_eq!(bm.url.as_deref(), Some("https://github.com/pleme-io/blackmatter"));
        assert_eq!(bm.git_ref.as_deref(), Some("main"));
        assert_eq!(lock.input_names(), vec!["blackmatter", "nixpkgs"]);
    }

    #[test]
    fn bad_json_and_malformed_lock_are_typed_errors() {
        assert_eq!(FlakeLock::parse("not json"), Err(FlakeError::BadJson));
        assert_eq!(FlakeLock::parse("{}"), Err(FlakeError::MalformedLock));
    }

    #[test]
    fn authed_url_embeds_the_token_as_basic_auth() {
        assert_eq!(
            authed_url("https://github.com/pleme-io/blackmatter", "ghp_abc").as_deref(),
            Some("https://x-access-token:ghp_abc@github.com/pleme-io/blackmatter")
        );
        // non-https (e.g. git@) has no https prefix → no rewrite
        assert_eq!(authed_url("git://x/y", "t"), None);
    }

    #[test]
    fn with_token_keeps_a_real_token_and_drops_an_empty_one() {
        assert!(GitLsRemoteResolver::with_token(Some("t".into())).token.is_some());
        assert!(GitLsRemoteResolver::with_token(Some(String::new())).token.is_none());
        assert!(GitLsRemoteResolver::new().token.is_none());
    }

    #[test]
    fn flake_env_reports_current_from_lock_and_latest_from_resolver() {
        let lock = FlakeLock::parse(LOCK).unwrap();
        let env = FlakeEnv::new(&lock, &MockResolver { head: "ccc333" });
        let sig = UpdateSignal::new("flake-input", "blackmatter");
        assert_eq!(env.current(&sig), Some("aaa111".to_owned()));
        assert_eq!(env.latest(&sig), Ok("ccc333".to_owned()));
    }

    #[test]
    fn unknown_input_is_a_typed_block_not_a_panic() {
        let lock = FlakeLock::parse(LOCK).unwrap();
        let env = FlakeEnv::new(&lock, &MockResolver { head: "x" });
        let sig = UpdateSignal::new("flake-input", "ghost");
        assert_eq!(env.current(&sig), None);
        assert!(matches!(env.latest(&sig), Err(BlockReason::Error(_))));
    }

    #[test]
    fn the_kind_shadows_a_real_flake_bump_through_this_env() {
        // current aaa111 (lock) vs latest ccc333 (resolver) → WouldApply.
        let lock = FlakeLock::parse(LOCK).unwrap();
        let env = FlakeEnv::new(&lock, &MockResolver { head: "ccc333" });
        let sig = UpdateSignal::new("flake-input", "blackmatter");
        assert_eq!(
            FlakeInputKind.shadow(&sig, &env),
            ShadowOutcome::WouldApply {
                from: "aaa111".into(),
                to: "ccc333".into()
            }
        );
        // already-current → UpToDate.
        let same = FlakeEnv::new(&lock, &MockResolver { head: "aaa111" });
        assert_eq!(FlakeInputKind.shadow(&sig, &same), ShadowOutcome::UpToDate);
    }

    #[test]
    fn flake_signal_source_drives_a_full_ingestion_cycle() {
        let lock = FlakeLock::parse(LOCK).unwrap();
        let source = FlakeSignalSource::new(&lock);
        let sigs = source.signals();
        assert_eq!(sigs.len(), 2, "one signal per flake input");
        assert!(sigs.iter().all(|s| s.kind == "flake-input"));
        // drive a frozen swarm straight from the source (ingestion → cycle).
        let env = FlakeEnv::new(&lock, &MockResolver { head: "ccc333" });
        let mut swarm = Swarm::new(
            Colony::new()
                .register(
                    Box::new(FlakeInputKind),
                    PromotionPolicy::new(PromotionMode::Effect),
                )
                .frozen(true),
            MemPlanStore::new(),
        );
        let report = swarm.run_cycle_from(&source, &env, 1);
        assert_eq!(report.total(), 2);
        assert_eq!(report.shadowed, 2, "both inputs bump; frozen → shadowed");
        assert_eq!(report.applied, 0);
    }

    #[test]
    fn a_swarm_shadows_the_flake_input_end_to_end() {
        let lock = FlakeLock::parse(LOCK).unwrap();
        let env = FlakeEnv::new(&lock, &MockResolver { head: "ccc333" });
        let mut swarm = Swarm::new(
            Colony::new()
                .register(
                    Box::new(FlakeInputKind),
                    PromotionPolicy::new(PromotionMode::Effect),
                )
                .frozen(true), // M0: shadow-only
            MemPlanStore::new(),
        );
        let report = swarm.run_cycle(&[UpdateSignal::new("flake-input", "blackmatter")], &env, 1);
        assert_eq!(report.shadowed, 1);
        assert_eq!(report.applied, 0, "frozen swarm never applies");
        // and the pending plan surfaces the concrete bump.
        let plan = swarm.pending_plan(1);
        assert_eq!(plan.pending.len(), 1);
        assert_eq!(plan.pending[0].to, "ccc333");
        assert!(matches!(
            swarm
                .colony()
                .ingest(
                    &UpdateSignal::new("flake-input", "blackmatter"),
                    &env,
                    &lock_obs(),
                    1
                )
                .tick_outcome(),
            Some(TickOutcome::Shadowed { .. })
        ));
    }

    #[test]
    fn nix_executor_rejects_non_flake_kinds_without_running_nix() {
        use formigueiro_core::ColonyOutcome;
        // an Applied outcome for a kind this executor doesn't handle
        let outcome = ColonyOutcome::Ticked {
            kind: "image-tag".into(),
            subject: "x".into(),
            outcome: TickOutcome::Applied {
                from: "a".into(),
                to: "b".into(),
            },
        };
        let mutation = AppliedMutation::from_outcome(&outcome).unwrap();
        let exec = NixFlakeExecutor::new("/nonexistent");
        // returns Unsupported BEFORE ever spawning nix (no write, no subprocess).
        assert_eq!(
            exec.apply(&mutation),
            Err(ApplyError::Unsupported("image-tag".to_owned()))
        );
    }

    // a minimal Observation for the direct ingest assertion above
    fn lock_obs() -> impl outorga::Observation {
        #[derive(Clone, Copy)]
        struct O;
        impl outorga::Observation for O {
            fn ready(&self) -> bool {
                true
            }
            fn stale(&self) -> bool {
                false
            }
            fn conflict(&self) -> bool {
                false
            }
            fn ready_since(&self) -> Option<i64> {
                Some(0)
            }
            fn operator_confirmed(&self) -> bool {
                false
            }
        }
        O
    }
}
