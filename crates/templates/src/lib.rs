// Copyright 2021 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(clippy::all)]
#![deny(rustdoc::broken_intra_doc_links)]
#![warn(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::missing_errors_doc)]

//! Templates rendering

use std::{
    collections::HashSet,
    io::Cursor,
    path::{Path, PathBuf},
    string::ToString,
    sync::Arc,
};

use anyhow::{bail, Context as _};
use mas_config::TemplatesConfig;
use serde::Serialize;
use tera::{Context, Error as TeraError, Tera};
use thiserror::Error;
use tokio::{fs::OpenOptions, io::AsyncWriteExt, sync::RwLock, task::JoinError};
use tracing::{debug, info, warn};

#[allow(missing_docs)] // TODO
mod context;
mod functions;

#[macro_use]
mod macros;

pub use self::context::{
    EmptyContext, ErrorContext, FormPostContext, IndexContext, LoginContext, LoginFormField,
    PostAuthContext, ReauthContext, ReauthFormField, TemplateContext, WithCsrf,
    WithOptionalSession, WithSession,
};

/// Wrapper around [`tera::Tera`] helping rendering the various templates
#[derive(Debug, Clone)]
pub struct Templates {
    tera: Arc<RwLock<Tera>>,
    config: TemplatesConfig,
}

/// There was an issue while loading the templates
#[derive(Error, Debug)]
pub enum TemplateLoadingError {
    /// Some templates failed to compile
    #[error("could not load and compile some templates")]
    Compile(#[from] TeraError),

    /// Could not join blocking task
    #[error("error from async runtime")]
    Runtime(#[from] JoinError),

    /// There are essential templates missing
    #[error("missing templates {missing:?}")]
    MissingTemplates {
        /// List of missing templates
        missing: HashSet<String>,
        /// List of templates that were loaded
        loaded: HashSet<String>,
    },
}

impl Templates {
    /// List directories to watch
    pub async fn watch_roots(&self) -> Vec<PathBuf> {
        Self::roots(self.config.path.as_deref(), self.config.builtin)
            .await
            .into_iter()
            .filter_map(Result::ok)
            .collect()
    }

    async fn roots(path: Option<&str>, builtin: bool) -> Vec<Result<PathBuf, std::io::Error>> {
        let mut paths = Vec::new();
        if builtin && cfg!(feature = "dev") {
            paths.push(PathBuf::from(format!(
                "{}/src/res",
                env!("CARGO_MANIFEST_DIR")
            )));
        }

        if let Some(path) = path {
            paths.push(PathBuf::from(path));
        }

        let mut ret = Vec::new();
        for path in paths {
            ret.push(tokio::fs::read_dir(&path).await.map(|_| path));
        }

        ret
    }

    fn load_builtin() -> Result<Tera, TemplateLoadingError> {
        let mut tera = Tera::default();
        info!("Loading builtin templates");

        for (name, source) in EXTRA_TEMPLATES {
            if let Some(source) = source {
                tera.add_raw_template(name, source)?;
            }
        }

        for (name, source) in TEMPLATES {
            if let Some(source) = source {
                tera.add_raw_template(name, source)?;
            }
        }

        Ok(tera)
    }

    /// Load the templates from [the config][`TemplatesConfig`]
    pub async fn load_from_config(config: &TemplatesConfig) -> Result<Self, TemplateLoadingError> {
        let tera = Self::load(config.path.as_deref(), config.builtin).await?;

        Ok(Self {
            tera: Arc::new(RwLock::new(tera)),
            config: config.clone(),
        })
    }

    async fn load(path: Option<&str>, builtin: bool) -> Result<Tera, TemplateLoadingError> {
        let mut teras = Vec::new();

        let roots = Self::roots(path, builtin).await;
        for maybe_root in roots {
            let root = match maybe_root {
                Ok(root) => root,
                Err(err) => {
                    warn!(%err, "Could not open a template folder, skipping it");
                    continue;
                }
            };

            // This uses blocking I/Os, do that in a blocking task
            let tera = tokio::task::spawn_blocking(move || {
                // Using `to_string_lossy` here is probably fine
                let path = format!("{}/**/*.{{html,txt}}", root.to_string_lossy());
                info!(%path, "Loading templates from filesystem");
                Tera::parse(&path)
            })
            .await??;

            teras.push(tera);
        }

        if builtin {
            teras.push(Self::load_builtin()?);
        }

        // Merging all Tera instances into a single one
        let mut tera = teras
            .into_iter()
            .try_fold(Tera::default(), |mut acc, tera| {
                acc.extend(&tera)?;
                Ok::<_, TemplateLoadingError>(acc)
            })?;

        tera.build_inheritance_chains()?;
        tera.check_macro_files()?;

        self::functions::register(&mut tera);

        let loaded: HashSet<_> = tera.get_template_names().collect();
        let needed: HashSet<_> = std::array::IntoIter::new(TEMPLATES)
            .map(|(name, _)| name)
            .collect();
        debug!(?loaded, ?needed, "Templates loaded");
        let missing: HashSet<_> = needed.difference(&loaded).collect();

        if missing.is_empty() {
            Ok(tera)
        } else {
            let missing = missing.into_iter().map(ToString::to_string).collect();
            let loaded = loaded.into_iter().map(ToString::to_string).collect();
            Err(TemplateLoadingError::MissingTemplates { missing, loaded })
        }
    }

    /// Reload the templates on disk
    pub async fn reload(&self) -> anyhow::Result<()> {
        // Prepare the new Tera instance
        let new_tera = Self::load(self.config.path.as_deref(), self.config.builtin).await?;

        // Swap it
        *self.tera.write().await = new_tera;

        Ok(())
    }

    /// Save the builtin templates to a folder
    pub async fn save(path: &Path, overwrite: bool) -> anyhow::Result<()> {
        if cfg!(feature = "dev") {
            bail!("Builtin templates are not included in dev binaries")
        }

        tokio::fs::create_dir_all(&path)
            .await
            .context("could not create destination folder")?;

        let templates = std::array::IntoIter::new(TEMPLATES).chain(EXTRA_TEMPLATES);

        let mut options = OpenOptions::new();
        if overwrite {
            options.create(true).truncate(true).write(true);
        } else {
            // With the `create_new` flag, `open` fails with an `AlreadyExists` error to
            // avoid overwriting
            options.create_new(true).write(true);
        };

        for (name, source) in templates {
            if let Some(source) = source {
                let path = path.join(name);

                let mut file = match options.open(&path).await {
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        // Not overwriting a template is a soft error
                        warn!(?path, "Not overwriting template");
                        continue;
                    }
                    x => x.context(format!("could not open file {:?}", path))?,
                };

                let mut buffer = Cursor::new(source);
                file.write_all_buf(&mut buffer)
                    .await
                    .context(format!("could not write file {:?}", path))?;
                info!(?path, "Wrote template");
            }
        }

        Ok(())
    }
}

/// Failed to render a template
#[derive(Error, Debug)]
pub enum TemplateError {
    /// Failed to prepare the context used by this template
    #[error("could not prepare context for template {template:?}")]
    Context {
        /// The name of the template being rendered
        template: &'static str,

        /// The underlying error
        #[source]
        source: TeraError,
    },

    /// Failed to render the template
    #[error("could not render template {template:?}")]
    Render {
        /// The name of the template being rendered
        template: &'static str,

        /// The underlying error
        #[source]
        source: TeraError,
    },
}

impl warp::reject::Reject for TemplateError {}

register_templates! {
    extra = { "base.html" };

    /// Render the login page
    pub fn render_login(WithCsrf<LoginContext>) { "login.html" }

    /// Render the registration page
    pub fn render_register(WithCsrf<EmptyContext>) { "register.html" }

    /// Render the home page
    pub fn render_index(WithCsrf<WithOptionalSession<IndexContext>>) { "index.html" }

    /// Render the re-authentication form
    pub fn render_reauth(WithCsrf<WithSession<ReauthContext>>) { "reauth.html" }

    /// Render the form used by the form_post response mode
    pub fn render_form_post<T: Serialize>(FormPostContext<T>) { "form_post.html" }

    /// Render the HTML error page
    pub fn render_error(ErrorContext) { "error.html" }
}

impl Templates {
    /// Render all templates with the generated samples to check if they render
    /// properly
    pub async fn check_render(&self) -> anyhow::Result<()> {
        check::render_login(self).await?;
        check::render_register(self).await?;
        check::render_index(self).await?;
        check::render_reauth(self).await?;
        check::render_form_post::<EmptyContext>(self).await?;
        check::render_error(self).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn check_builtin_templates() {
        let config = TemplatesConfig {
            path: None,
            builtin: true,
        };

        let templates = Templates::load_from_config(&config).await.unwrap();
        templates.check_render().await.unwrap();
    }
}
