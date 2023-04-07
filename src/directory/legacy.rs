use crate::{github::DynGH, multierror::MultiError};
use anyhow::Result;
use config::Config;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Legacy configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Cfg {
    pub sheriff: sheriff::Cfg,
    pub cncf: Option<cncf::Cfg>,
}

impl Cfg {
    /// Get legacy configuration.
    pub(crate) async fn get(cfg: Arc<Config>, gh: DynGH, ref_: Option<&str>) -> Result<Cfg> {
        let mut merr = MultiError::new(Some("invalid directory configuration".to_string()));

        // Get sheriff configuration
        let sheriff = match sheriff::Cfg::get(cfg.clone(), gh.clone(), ref_).await {
            Ok(cfg) => Some(cfg),
            Err(err) => {
                merr.push(err);
                None
            }
        };

        // Get CNCF people configuration
        let cncf = match cncf::Cfg::get(cfg, gh, ref_).await {
            Ok(cfg) => Some(cfg),
            Err(err) => {
                merr.push(err);
                None
            }
        };

        if merr.contains_errors() {
            return Err(merr.into());
        }
        Ok(Cfg {
            sheriff: sheriff.expect("sheriff config must be present"),
            cncf,
        })
    }
}

pub(crate) mod sheriff {
    use crate::{directory::UserName, github::DynGH, multierror::MultiError};
    use anyhow::{format_err, Context, Error, Result};
    use config::Config;
    use serde::{Deserialize, Serialize};
    use std::sync::Arc;

    /// Sheriff configuration.
    /// https://github.com/electron/sheriff#permissions-file
    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    pub(crate) struct Cfg {
        pub teams: Vec<Team>,
    }

    impl Cfg {
        /// Get sheriff configuration.
        pub(crate) async fn get(cfg: Arc<Config>, gh: DynGH, ref_: Option<&str>) -> Result<Self> {
            let path = &cfg.get_string("config.legacy.sheriff.permissionsPath").unwrap();
            let content = gh
                .get_file_content(path, ref_)
                .await
                .context("error getting sheriff permissions file")?;
            let cfg: Cfg = serde_yaml::from_str(&content)
                .map_err(Error::new)
                .context("error parsing sheriff permissions file")?;
            cfg.validate()?;
            Ok(cfg)
        }

        /// Validate configuration.
        fn validate(&self) -> Result<()> {
            let mut merr = MultiError::new(None);

            let mut teams_seen = vec![];
            for (i, team) in self.teams.iter().enumerate() {
                // Define id to be used in subsequent error messages. When
                // available, it'll be the team name. Otherwise we'll use its
                // index on the list.
                let id = if team.name.is_empty() {
                    format!("{}", i)
                } else {
                    team.name.clone()
                };

                // Name must be provided
                if team.name.is_empty() {
                    merr.push(format_err!("team[{id}: name must be provided"));
                }

                // No duplicate config per team
                if !team.name.is_empty() {
                    if teams_seen.contains(&&team.name) {
                        merr.push(format_err!(
                            "team[{id}]: duplicate config for team {}",
                            &team.name
                        ));
                        continue;
                    }
                    teams_seen.push(&team.name);
                }

                // At least one maintainer required
                if team.maintainers.is_empty() {
                    merr.push(format_err!("team[{id}]: must have at least one maintainer"));
                }

                // Users should be either a maintainer or a member, but not both
                for maintainer in &team.maintainers {
                    if team.members.contains(maintainer) {
                        merr.push(format_err!(
                            "team[{id}]: {maintainer} must be either a maintainer or a member, but not both"
                        ));
                    }
                }
            }

            if merr.contains_errors() {
                return Err(merr.into());
            }
            Ok(())
        }
    }

    /// Team configuration.
    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    pub(crate) struct Team {
        pub name: String,
        pub maintainers: Vec<UserName>,
        pub members: Vec<UserName>,
    }
}

pub(crate) mod cncf {
    use crate::{github::DynGH, multierror::MultiError};
    use anyhow::{format_err, Context, Error, Result};
    use config::Config;
    use serde::{Deserialize, Serialize};
    use std::sync::Arc;

    /// CNCF people configuration.
    /// https://github.com/cncf/people/tree/main#listing-format
    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    #[serde(transparent)]
    pub(crate) struct Cfg {
        pub people: Vec<User>,
    }

    impl Cfg {
        /// Get CNCF people configuration.
        pub(crate) async fn get(cfg: Arc<Config>, gh: DynGH, ref_: Option<&str>) -> Result<Self> {
            let path = &cfg.get_string("config.legacy.cncf.peoplePath").unwrap();
            let content = gh
                .get_file_content(path, ref_)
                .await
                .context("error getting cncf people file")?;
            let cfg: Cfg = serde_json::from_str(&content)
                .map_err(Error::new)
                .context("error parsing cncf people file")?;
            cfg.validate()?;
            Ok(cfg)
        }

        /// Validate configuration.
        fn validate(&self) -> Result<()> {
            let mut merr = MultiError::new(None);

            for (i, user) in self.people.iter().enumerate() {
                // Name must be provided
                if user.name.is_empty() {
                    merr.push(format_err!("user[{}]: name must be provided", i));
                }
            }

            if merr.contains_errors() {
                return Err(merr.into());
            }
            Ok(())
        }
    }

    /// User profile.
    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    pub(crate) struct User {
        pub name: String,
        pub bio: Option<String>,
        pub company: Option<String>,
        pub pronouns: Option<String>,
        pub location: Option<String>,
        pub linkedin: Option<String>,
        pub twitter: Option<String>,
        pub github: Option<String>,
        pub wechat: Option<String>,
        pub website: Option<String>,
        pub youtube: Option<String>,
        pub languages: Option<Vec<String>>,
        pub projects: Option<Vec<String>>,
        pub category: Option<Vec<String>>,
        pub email: Option<String>,
        pub slack_id: Option<String>,
        pub image: Option<String>,
    }
}
