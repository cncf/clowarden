use crate::{directory::*, github::DynGH, multierror::MultiError};
use anyhow::{Context, Error, Result};
use config::Config;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Legacy configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Cfg {
    sheriff: sheriff::Cfg,
    cncf: cncf::Cfg,
}

impl Cfg {
    /// Get legacy configuration.
    pub(crate) async fn get(
        cfg: Arc<Config>,
        gh: DynGH,
        ref_: Option<&str>,
    ) -> Result<Cfg, MultiError> {
        let mut merr = MultiError::new();

        // Get sheriff configuration
        let path = &cfg
            .get_string("config.legacy.sheriff.permissionsPath")
            .unwrap();
        let content = gh
            .get_file_content(path, ref_)
            .await
            .context("error getting sheriff permissions file")?;
        let sheriff: sheriff::Cfg = serde_yaml::from_str(&content)
            .map_err(Error::new)
            .context("error parsing sheriff permissions file")?;
        if let Err(sheriff_merr) = sheriff.validate() {
            merr.join(sheriff_merr);
        }

        // Get CNCF people configuration
        let path = &cfg.get_string("config.legacy.cncf.peoplePath").unwrap();
        let content = gh
            .get_file_content(path, ref_)
            .await
            .context("error getting cncf people file")?;
        let cncf: cncf::Cfg = serde_json::from_str(&content)
            .map_err(Error::new)
            .context("error parsing cncf people file")?;
        if let Err(cncf_merr) = cncf.validate() {
            merr.join(cncf_merr);
        }

        if merr.has_errors() {
            return Err(merr);
        }
        Ok(Cfg { sheriff, cncf })
    }
}

impl From<Cfg> for Directory {
    /// Create a new directory instance from the legacy configuration.
    fn from(cfg: Cfg) -> Self {
        // Teams
        let teams = cfg
            .sheriff
            .teams
            .into_iter()
            .map(|t| Team {
                name: t.name,
                maintainers: t.maintainers,
                members: t.members,
                ..Default::default()
            })
            .collect();

        // Users
        let users = cfg
            .cncf
            .people
            .into_iter()
            .map(|u| {
                let image_url = match u.image {
                    Some(v) if v.starts_with("https://") => Some(v),
                    Some(v) => Some(format!(
                        "https://github.com/cncf/people/raw/main/images/{v}",
                    )),
                    None => None,
                };
                User {
                    full_name: u.name,
                    email: u.email,
                    image_url,
                    languages: u.languages,
                    bio: u.bio,
                    website: u.website,
                    company: u.company,
                    pronouns: u.pronouns,
                    location: u.location,
                    slack_id: u.slack_id,
                    linkedin_url: u.linkedin,
                    twitter_url: u.twitter,
                    github_url: u.github,
                    wechat_url: u.wechat,
                    youtube_url: u.youtube,
                    ..Default::default()
                }
            })
            .collect();

        Directory { teams, users }
    }
}

pub(crate) mod sheriff {
    use crate::{
        directory::{TeamName, UserName},
        multierror::MultiError,
    };
    use anyhow::{format_err, Result};
    use serde::{Deserialize, Serialize};
    use std::collections::HashMap;

    /// Sheriff configuration.
    /// https://github.com/electron/sheriff#permissions-file
    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    pub(crate) struct Cfg {
        pub teams: Vec<Team>,
        pub repositories: Vec<Repository>,
    }

    impl Cfg {
        /// Validate configuration.
        pub(crate) fn validate(&self) -> Result<(), MultiError> {
            let mut merr = MultiError::new();

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

            if merr.has_errors() {
                return Err(merr);
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

    /// Repository configuration.
    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    pub(crate) struct Repository {
        pub name: String,
        pub external_collaborators: Option<HashMap<UserName, Role>>,
        pub teams: Option<HashMap<TeamName, Role>>,
        pub visibility: Option<RepositoryVisibility>,
    }

    /// Role a user or team may have assigned.
    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "lowercase")]
    pub(crate) enum Role {
        Admin,
        Maintain,
        #[default]
        Read,
        Triage,
        Write,
    }

    /// Repository's visibility.
    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "lowercase")]
    pub(crate) enum RepositoryVisibility {
        #[default]
        Private,
        Public,
    }
}

pub(crate) mod cncf {
    use crate::multierror::MultiError;
    use anyhow::{format_err, Result};
    use serde::{Deserialize, Serialize};

    /// CNCF people configuration.
    /// https://github.com/cncf/people/tree/main#listing-format
    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    #[serde(transparent)]
    pub(crate) struct Cfg {
        pub people: Vec<User>,
    }

    impl Cfg {
        /// Validate configuration.
        pub(crate) fn validate(&self) -> Result<(), MultiError> {
            let mut merr = MultiError::new();

            for (i, user) in self.people.iter().enumerate() {
                // Name must be provided
                if user.name.is_empty() {
                    merr.push(format_err!("user[{}]: name must be provided", i));
                }
            }

            if merr.has_errors() {
                return Err(merr);
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
        pub languages: Vec<String>,
        pub projects: Vec<String>,
        pub category: Vec<String>,
        pub email: Option<String>,
        pub slack_id: Option<String>,
        pub image: Option<String>,
    }
}
