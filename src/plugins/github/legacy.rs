pub(crate) mod sheriff {
    use crate::{github::DynGH, multierror::MultiError, plugins::github::cfg::Repository};
    use anyhow::{format_err, Context, Error, Result};
    use config::Config;
    use serde::{Deserialize, Serialize};
    use std::sync::Arc;

    /// Sheriff configuration.
    /// https://github.com/electron/sheriff#permissions-file
    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    pub(crate) struct Cfg {
        pub repositories: Vec<Repository>,
    }

    impl Cfg {
        /// Get sheriff configuration.
        pub(crate) async fn get(cfg: Arc<Config>, gh: DynGH, ref_: Option<&str>) -> Result<Self> {
            let path = &cfg
                .get_string("config.legacy.sheriff.permissionsPath")
                .unwrap();
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
            let mut merr = MultiError::new(Some("invalid github plugin configuration".to_string()));

            for (i, repo) in self.repositories.iter().enumerate() {
                // Define id to be used in subsequent error messages. When
                // available, it'll be the repo name. Otherwise we'll use its
                // index on the list.
                let id = if repo.name.is_empty() {
                    format!("{}", i)
                } else {
                    repo.name.clone()
                };

                // Name must be provided
                if repo.name.is_empty() {
                    merr.push(format_err!("repo[{id}]: name must be provided"));
                }
            }

            if merr.contains_errors() {
                return Err(merr.into());
            }
            Ok(())
        }
    }
}
