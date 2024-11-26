//! This module defines the types used to represent the legacy configuration
//! format (Sheriff's). The state module relies on this module to create new
//! state instances from the legacy configuration.

pub(crate) mod sheriff {
    use anyhow::{format_err, Context, Error, Result};
    use serde::{Deserialize, Serialize};

    use crate::{
        directory::legacy::VALID_TEAM_NAME,
        github::{DynGH, Source},
        multierror::MultiError,
        services::github::state::Repository,
    };

    /// Sheriff configuration.
    /// https://github.com/electron/sheriff#permissions-file
    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    pub(crate) struct Cfg {
        pub repositories: Vec<Repository>,
    }

    impl Cfg {
        /// Get sheriff configuration.
        pub(crate) async fn get(gh: DynGH, src: &Source, path: &str) -> Result<Self> {
            let content =
                gh.get_file_content(src, path).await.context("error getting sheriff permissions file")?;
            let cfg: Cfg = serde_yaml::from_str(&content)
                .map_err(Error::new)
                .context("error parsing permissions file")?;
            cfg.validate()?;
            Ok(cfg)
        }

        /// Validate configuration.
        fn validate(&self) -> Result<()> {
            let mut merr = MultiError::new(Some("invalid github service configuration".to_string()));

            let mut repos_seen = vec![];
            for (i, repo) in self.repositories.iter().enumerate() {
                // Define id to be used in subsequent error messages. When
                // available, it'll be the repo name. Otherwise we'll use its
                // index on the list.
                let id = if repo.name.is_empty() {
                    format!("{i}")
                } else {
                    repo.name.clone()
                };

                // Name must be provided
                if repo.name.is_empty() {
                    merr.push(format_err!("repo[{id}]: name must be provided"));
                }

                // No duplicate config per repo
                if !repo.name.is_empty() {
                    if repos_seen.contains(&&repo.name) {
                        merr.push(format_err!(
                            "repo[{id}]: duplicate config for repo {}",
                            &repo.name
                        ));
                        continue;
                    }
                    repos_seen.push(&repo.name);
                }

                // Teams names must be valid
                if let Some(teams) = &repo.teams {
                    for team_name in teams.keys() {
                        if !VALID_TEAM_NAME.is_match(team_name) {
                            merr.push(format_err!(
                                "repo[{id}]: team[{team_name}] name must be lowercase alphanumeric with dashes (team slug)",
                            ));
                        }
                    }
                }
            }

            if merr.contains_errors() {
                return Err(merr.into());
            }
            Ok(())
        }
    }
}
