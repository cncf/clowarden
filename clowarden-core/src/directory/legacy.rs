//! This module defines the types used to represent the legacy configuration
//! format (Sheriff's and CNCF's users). The directory module relies on this
//! module to create new directory instances from the legacy configuration.

use std::sync::LazyLock;

use anyhow::Result;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::{
    cfg::Legacy,
    github::{DynGH, Source},
    multierror::MultiError,
};

pub(crate) static VALID_TEAM_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z0-9\-]+$").expect("expr in VALID_TEAM_NAME to be valid"));

/// Legacy configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Cfg {
    pub sheriff: sheriff::Cfg,
    pub cncf: Option<cncf::Cfg>,
}

impl Cfg {
    /// Get legacy configuration.
    pub(crate) async fn get(gh: DynGH, legacy: &Legacy, src: &Source) -> Result<Cfg> {
        let mut merr = MultiError::new(Some("invalid directory configuration".to_string()));

        // Get sheriff configuration
        let sheriff = match sheriff::Cfg::get(gh.clone(), src, &legacy.sheriff_permissions_path).await {
            Ok(cfg) => Some(cfg),
            Err(err) => {
                merr.push(err);
                None
            }
        };

        // Get CNCF people configuration
        let cncf = match cncf::Cfg::get(gh, src, legacy.cncf_people_path.as_deref()).await {
            Ok(cfg) => cfg,
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

pub mod sheriff {
    use super::VALID_TEAM_NAME;
    use crate::{
        directory::{TeamName, UserName},
        github::{DynGH, Source},
        multierror::MultiError,
    };
    use anyhow::{format_err, Context, Error, Result};
    use serde::{Deserialize, Serialize};

    /// Sheriff configuration.
    /// https://github.com/electron/sheriff#permissions-file
    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    pub(crate) struct Cfg {
        pub teams: Vec<Team>,
    }

    impl Cfg {
        /// Get sheriff configuration.
        pub(crate) async fn get(gh: DynGH, src: &Source, path: &str) -> Result<Self> {
            // Fetch configuration file and parse it
            let content = gh.get_file_content(src, path).await.context("error getting permissions file")?;
            let mut cfg: Cfg = serde_yaml::from_str(&content)
                .map_err(Error::new)
                .context("error parsing permissions file")?;

            // Process and validate configuration
            cfg.process_composite_teams();
            cfg.remove_duplicates();
            cfg.validate()?;

            Ok(cfg)
        }

        /// Extend team's maintainers and members with the maintainers and
        /// members of the teams listed in the formation field.
        fn process_composite_teams(&mut self) {
            let teams_copy = self.teams.clone();

            for team in &mut self.teams {
                if let Some(formation) = &team.formation {
                    for team_name in formation {
                        if let Some(source_team) = teams_copy.iter().find(|t| &t.name == team_name) {
                            // Maintainers
                            if let Some(maintainers) = team.maintainers.as_mut() {
                                maintainers
                                    .extend_from_slice(source_team.maintainers.as_ref().unwrap_or(&vec![]));
                            } else {
                                team.maintainers.clone_from(&source_team.maintainers);
                            }

                            // Members
                            if let Some(members) = team.members.as_mut() {
                                members.extend_from_slice(source_team.members.as_ref().unwrap_or(&vec![]));
                            } else {
                                team.members.clone_from(&source_team.members);
                            }
                        }
                    }
                }
            }
        }

        /// Remove duplicates in teams' maintainers and members.
        fn remove_duplicates(&mut self) {
            for team in &mut self.teams {
                // Maintainers
                if let Some(maintainers) = team.maintainers.as_mut() {
                    maintainers.sort();
                    maintainers.dedup();
                }

                // Members
                if let Some(members) = team.members.as_mut() {
                    members.sort();
                    members.dedup();
                }
            }
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
                    format!("{i}")
                } else {
                    team.name.clone()
                };

                // Name must be provided
                if team.name.is_empty() {
                    merr.push(format_err!("team[{id}: name must be provided"));
                }

                // Name must be valid
                if !VALID_TEAM_NAME.is_match(&team.name) {
                    merr.push(format_err!(
                        "team[{id}]: name must be lowercase alphanumeric with dashes (team slug)"
                    ));
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
                if team.maintainers.as_ref().unwrap_or(&vec![]).is_empty() {
                    merr.push(format_err!("team[{id}]: must have at least one maintainer"));
                }

                // Users should be either a maintainer or a member, but not both
                for maintainer in team.maintainers.as_ref().unwrap_or(&vec![]) {
                    if team.members.as_ref().unwrap_or(&vec![]).contains(maintainer) {
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
    pub struct Team {
        pub name: String,

        #[serde(skip_serializing_if = "Option::is_none")]
        pub maintainers: Option<Vec<UserName>>,

        #[serde(skip_serializing_if = "Option::is_none")]
        pub members: Option<Vec<UserName>>,

        #[serde(skip_serializing_if = "Option::is_none")]
        pub formation: Option<Vec<TeamName>>,
    }

    impl From<crate::directory::Team> for Team {
        fn from(team: crate::directory::Team) -> Self {
            Team {
                name: team.name,
                maintainers: Some(team.maintainers),
                members: Some(team.members),
                ..Default::default()
            }
        }
    }
}

pub(crate) mod cncf {
    use crate::{
        github::{DynGH, Source},
        multierror::MultiError,
    };
    use anyhow::{format_err, Context, Error, Result};
    use serde::{Deserialize, Serialize};

    /// CNCF people configuration.
    /// https://github.com/cncf/people/tree/main#listing-format
    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    #[serde(transparent)]
    pub(crate) struct Cfg {
        pub people: Vec<User>,
    }

    impl Cfg {
        /// Get CNCF people configuration.
        pub(crate) async fn get(gh: DynGH, src: &Source, path: Option<&str>) -> Result<Option<Self>> {
            match path {
                Some(path) => {
                    let content =
                        gh.get_file_content(src, path).await.context("error getting cncf people file")?;
                    let cfg: Cfg = serde_json::from_str(&content)
                        .map_err(Error::new)
                        .context("error parsing cncf people file")?;
                    cfg.validate()?;
                    Ok(Some(cfg))
                }
                None => Ok(None),
            }
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
