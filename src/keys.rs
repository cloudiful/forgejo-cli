use forgejo_api::{Auth, Forgejo};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::ErrorKind,
    path::Path,
};
use tokio::io::AsyncWriteExt;
use url::Url;

use crate::{ftl_eprintln, ftl_eyre};

#[derive(serde::Serialize, serde::Deserialize, Clone, Default)]
pub struct KeyInfo {
    pub hosts: BTreeMap<String, LoginInfo>,
    #[serde(default)]
    pub aliases: BTreeMap<String, String>,
    #[serde(default)]
    pub default_ssh: BTreeSet<String>,
}

impl KeyInfo {
    // Try the old path for backwards compatibility
    // Give this a release or two for users to migrate and then remove this codepath
    async fn load_fallback(new_path: &Path) -> eyre::Result<Self> {
        let fallback = directories::ProjectDirs::from("", "Cyborus", "forgejo-cli")
            .ok_or_else(|| ftl_eyre!("msg-project-dir-not-found"))?
            .data_dir()
            .join("keys.json");
        let json = tokio::fs::read(fallback).await;
        let this = match json {
            Ok(x) => {
                ftl_eprintln!("msg-old-key-file-read");
                serde_json::from_slice::<Self>(&x)?
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(e) => return Err(e.into()),
        };
        if this.save().await.is_err() {
            ftl_eprintln!(
                "msg-save-migrated-key-file-fail",
                path = new_path.to_string_lossy()
            );
        }
        Ok(this)
    }

    pub async fn load() -> eyre::Result<Self> {
        let path = directories::ProjectDirs::from("", "forgejo-cli", "forgejo-cli")
            .ok_or_else(|| ftl_eyre!("msg-project-dir-not-found"))?
            .data_dir()
            .join("keys.json");
        let json = tokio::fs::read(path.as_path()).await;
        let this = match json {
            Ok(x) => serde_json::from_slice::<Self>(&x)?,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                KeyInfo::load_fallback(path.as_path()).await?
            }
            Err(e) => return Err(e.into()),
        };
        Ok(this)
    }

    pub async fn save(&self) -> eyre::Result<()> {
        let json = serde_json::to_vec_pretty(self)?;
        let dirs = directories::ProjectDirs::from("", "forgejo-cli", "forgejo-cli")
            .ok_or_else(|| ftl_eyre!("msg-project-dir-not-found"))?;
        let path = dirs.data_dir();

        tokio::fs::create_dir_all(path).await?;

        let mut options = std::fs::OpenOptions::new();
        options.create(true).write(true).truncate(true);

        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }

        let mut file = tokio::fs::OpenOptions::from(options)
            .open(path.join("keys.json"))
            .await?;
        file.write_all(&json).await?;

        Ok(())
    }

    pub fn get_login(&mut self, url: &Url) -> Option<&mut LoginInfo> {
        let host = crate::host_name(url);
        let login_info = self.hosts.get_mut(host)?;
        Some(login_info)
    }

    /// The scoped credential for an authenticated attachment request to `url`.
    ///
    /// The variant is chosen from the stored login and never exposes the token
    /// outside the request header it builds.
    pub fn attachment_auth(&self, url: &Url) -> AttachmentAuth<'_> {
        match self.hosts.get(crate::host_name(url)) {
            Some(LoginInfo::Application { token }) => AttachmentAuth::Token(token),
            Some(LoginInfo::OAuth { token, .. }) => AttachmentAuth::Bearer(token),
            None => AttachmentAuth::None,
        }
    }

    pub async fn get_api(&mut self, url: &Url) -> eyre::Result<Forgejo> {
        match self.get_login(url) {
            Some(login) => {
                let was_refreshed = login.refresh(url).await?;
                let api = login.api_for(url).await?;
                if was_refreshed {
                    self.save().await?;
                }
                Ok(api)
            }
            None => Forgejo::with_user_agent(Auth::None, url.clone(), crate::USER_AGENT)
                .map_err(Into::into),
        }
    }

    pub fn deref_alias(&self, url: url::Url) -> url::Url {
        match self.aliases.get(crate::host_name(&url)) {
            Some(replacement) => {
                let s = format!(
                    "{}{}{}",
                    &url[..url::Position::BeforeHost],
                    replacement,
                    &url[url::Position::AfterPort..]
                );
                url::Url::parse(&s).unwrap()
            }
            None => url,
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
#[serde(tag = "type")]
pub enum LoginInfo {
    Application {
        token: String,
    },
    OAuth {
        token: String,
        refresh_token: String,
        expires_at: time::OffsetDateTime,
    },
}

/// A credential for one authenticated attachment download request.
///
/// Deliberately not `Debug` or `Serialize`: the token is only readable through
/// [`AttachmentAuth::header_value`].
pub enum AttachmentAuth<'a> {
    None,
    Token(&'a str),
    Bearer(&'a str),
}

impl AttachmentAuth<'_> {
    /// The `Authorization` header value, or `None` for anonymous access.
    pub fn header_value(&self) -> Option<String> {
        match self {
            AttachmentAuth::None => None,
            AttachmentAuth::Token(token) => Some(format!("token {token}")),
            AttachmentAuth::Bearer(token) => Some(format!("Bearer {token}")),
        }
    }
}

impl LoginInfo {
    async fn refresh(&mut self, url: &Url) -> eyre::Result<bool> {
        if let LoginInfo::OAuth {
            token,
            refresh_token,
            expires_at,
            ..
        } = self
            && time::OffsetDateTime::now_utc() >= *expires_at
        {
            let api = Forgejo::with_user_agent(Auth::None, url.clone(), crate::USER_AGENT)?;
            let client_id = crate::auth::get_client_info_for(url)
                .await?
                .ok_or_else(|| eyre::eyre!("Can't refresh token: no client info for {url}."))?;
            let response = api
                .oauth_get_access_token(forgejo_api::structs::OAuthTokenRequest::Refresh {
                    refresh_token,
                    client_id: &client_id,
                    client_secret: "",
                })
                .await?;
            *token = response.access_token;
            *refresh_token = response.refresh_token;
            // A minute less, in case any weirdness happens at the exact moment it
            // expires. Better to refresh slightly too soon than slightly too late.
            let expires_in =
                std::time::Duration::from_secs(response.expires_in.saturating_sub(60) as u64);
            *expires_at = time::OffsetDateTime::now_utc() + expires_in;
            return Ok(true);
        }
        Ok(false)
    }

    pub async fn api_for(&self, url: &Url) -> eyre::Result<Forgejo> {
        match self {
            LoginInfo::Application { token, .. } => {
                let api =
                    Forgejo::with_user_agent(Auth::Token(token), url.clone(), crate::USER_AGENT)?;
                Ok(api)
            }
            LoginInfo::OAuth { token, .. } => {
                let api =
                    Forgejo::with_user_agent(Auth::Token(token), url.clone(), crate::USER_AGENT)?;
                Ok(api)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_auth_maps_login_kind_to_header() {
        let url = Url::parse("https://forgejo.example/").unwrap();
        let host = crate::host_name(&url).to_owned();
        let mut keys = KeyInfo::default();

        assert_eq!(keys.attachment_auth(&url).header_value(), None);

        keys.hosts.insert(
            host.clone(),
            LoginInfo::Application {
                token: "app-token".to_owned(),
            },
        );
        assert_eq!(
            keys.attachment_auth(&url).header_value().as_deref(),
            Some("token app-token")
        );

        keys.hosts.insert(
            host,
            LoginInfo::OAuth {
                token: "oauth-token".to_owned(),
                refresh_token: "refresh".to_owned(),
                expires_at: time::OffsetDateTime::now_utc(),
            },
        );
        assert_eq!(
            keys.attachment_auth(&url).header_value().as_deref(),
            Some("Bearer oauth-token")
        );
    }
}
