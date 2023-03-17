use crate::{directory::*, github::DynGH};
use anyhow::{Context, Result};
use config::Config;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use validator::Validate;

/// Legacy configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Cfg {
    sheriff: sheriff::Cfg,
    cncf: cncf::Cfg,
}

impl Cfg {
    /// Get legacy configuration.
    pub(crate) async fn get(cfg: Arc<Config>, gh: DynGH, ref_: Option<&str>) -> Result<Cfg> {
        // Get sheriff configuration
        let path = &cfg
            .get_string("config.legacy.sheriff.permissionsPath")
            .unwrap();
        let content = gh
            .get_file_content(path, ref_)
            .await
            .context("error getting sheriff permissions file")?;
        let sheriff: sheriff::Cfg = serde_yaml::from_str(&content)?;
        sheriff.validate()?;

        // Get CNCF people configuration
        let path = &cfg.get_string("config.legacy.cncf.peoplePath").unwrap();
        let content = gh
            .get_file_content(path, ref_)
            .await
            .context("error getting cncf people file")?;
        let cncf: cncf::Cfg = serde_json::from_str(&content)?;
        cncf.validate()?;

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
                        "https://github.com/cncf/people/raw/main/images/{}",
                        v
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
    use serde::{Deserialize, Serialize};
    use std::collections::HashMap;
    use validator::Validate;

    /// Type alias to represent a team name.
    pub(crate) type TeamName = String;

    /// Type alias to represent a username.
    pub(crate) type UserName = String;

    /// Sheriff configuration.
    /// https://github.com/electron/sheriff#permissions-file
    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, Validate)]
    pub(crate) struct Cfg {
        #[validate]
        pub teams: Vec<Team>,
        #[validate]
        pub repositories: Vec<Repository>,
    }

    /// Team configuration.
    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, Validate)]
    pub(crate) struct Team {
        #[validate(length(min = 1))]
        pub name: String,
        #[validate(length(min = 1))]
        pub maintainers: Vec<UserName>,
        pub members: Vec<UserName>,
    }

    /// Repository configuration.
    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, Validate)]
    pub(crate) struct Repository {
        #[validate(length(min = 1))]
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
    use serde::{Deserialize, Serialize};
    use validator::Validate;

    /// CNCF people configuration.
    /// https://github.com/cncf/people/tree/main#listing-format
    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, Validate)]
    #[serde(transparent)]
    pub(crate) struct Cfg {
        #[validate]
        pub people: Vec<User>,
    }

    /// User profile.
    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, Validate)]
    pub(crate) struct User {
        #[validate(length(min = 1))]
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
