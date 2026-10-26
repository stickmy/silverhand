//! The two mode axes an operational face reports.
//!
//! They answer different questions and are configured independently — the boot
//! options carry one as `mode` and the other inside `serverConfig` — so they are two
//! types rather than one. Both spell one of their values `"embedded"`, which is
//! exactly why folding them would be dangerous: a face that read the deployment axis
//! and rendered it as the server axis would agree with the product on every embedded
//! deployment and only diverge on the configurations nobody runs in a test.

/// How this server is being run relative to the desktop app it serves.
///
/// Pinned to `ServerModeSchema` (`packages/shared/src/config.ts`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServerMode {
    Embedded,
    Remote,
}

impl ServerMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Embedded => "embedded",
            Self::Remote => "remote",
        }
    }
}

/// Which composition installed this backend, and therefore which capabilities it is
/// expected to have. A standalone backend is control-plane only: it has no
/// co-located daemon host, so the machine it runs on is not a machine any agent runs
/// on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeploymentMode {
    Embedded,
    Standalone,
}

impl DeploymentMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Embedded => "embedded",
            Self::Standalone => "standalone",
        }
    }
}

/// The launcher's explicit choice of how this backend is packaged.
///
/// One backend product, two profiles; each LOCKS both mode axes together, so the
/// mixed combinations — an embedded server that is standalone, a remote server that is
/// deployment-embedded — are unconstructable rather than merely unconfigured. A
/// launcher chooses a profile, never the two axes independently: the Desktop app runs
/// [`Self::Embedded`], the standalone server runs [`Self::RemoteOnly`], and nothing
/// infers the choice from the listen address, the daemon spec, or any other runtime
/// signal.
///
/// There is deliberately no `Default`. A process with no profile has not been told
/// what it is, and must fail to start rather than guess — guessing is exactly how a
/// control-plane server would come to probe its own machine for runtimes it cannot
/// run, or an embedded backend would gate readiness on a transport it does not use.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeploymentProfile {
    /// The Desktop launcher's profile. Locks `ServerMode::Embedded` +
    /// `DeploymentMode::Embedded`. The profile is a packaging INTENT, not a claim about
    /// what is wired today: it is the profile under which a co-located daemon host, an
    /// embedded transport, and local runtime detection are REQUIRED to be constructed.
    /// Local runtime detection is wired (this build mounts the detecting runtimes
    /// router); the co-located host and embedded transport are named migration
    /// residuals this tag commits the profile to, not capabilities it asserts exist.
    Embedded,
    /// The standalone server launcher's profile. Locks `ServerMode::Remote` +
    /// `DeploymentMode::Standalone`: control-plane only, with no machine of its own for
    /// an agent to run on, so it constructs no runtime detector and `/api/runtimes`
    /// refuses.
    RemoteOnly,
}

impl DeploymentProfile {
    /// The server axis this profile locks in.
    pub const fn server_mode(self) -> ServerMode {
        match self {
            Self::Embedded => ServerMode::Embedded,
            Self::RemoteOnly => ServerMode::Remote,
        }
    }

    /// The deployment axis this profile locks in.
    pub const fn deployment_mode(self) -> DeploymentMode {
        match self {
            Self::Embedded => DeploymentMode::Embedded,
            Self::RemoteOnly => DeploymentMode::Standalone,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both axes' wire spellings, pinned to their TypeScript authorities.
    #[test]
    fn each_axis_spells_its_own_values() {
        assert_eq!(
            [ServerMode::Embedded.as_str(), ServerMode::Remote.as_str()],
            ["embedded", "remote"]
        );
        assert_eq!(
            [
                DeploymentMode::Embedded.as_str(),
                DeploymentMode::Standalone.as_str()
            ],
            ["embedded", "standalone"]
        );
    }

    /// The two profiles are the ONLY legal `(server, deployment)` pairings: an embedded
    /// backend is deployment-embedded, a remote-only backend is standalone. No profile
    /// projects a mixed pair — which is the whole reason the launcher chooses a profile
    /// and not two free axes.
    #[test]
    fn each_profile_locks_both_axes_together() {
        assert_eq!(
            (
                DeploymentProfile::Embedded.server_mode(),
                DeploymentProfile::Embedded.deployment_mode()
            ),
            (ServerMode::Embedded, DeploymentMode::Embedded)
        );
        assert_eq!(
            (
                DeploymentProfile::RemoteOnly.server_mode(),
                DeploymentProfile::RemoteOnly.deployment_mode()
            ),
            (ServerMode::Remote, DeploymentMode::Standalone)
        );
    }
}
