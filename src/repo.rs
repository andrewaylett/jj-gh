//! Working out which GitHub repository a jj remote points at.

use anyhow::{Result, bail};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRef {
    pub host: String,
    pub owner: String,
    pub name: String,
}

impl RepoRef {
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    /// Parse a git remote URL (https, ssh, scp-style) into a repository.
    pub fn from_url(url: &str) -> Result<Self> {
        let url = url.trim();
        let (host, path) = if let Some(rest) = url.split_once("://").map(|(_, r)| r) {
            let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
            let authority = authority.rsplit('@').next().unwrap_or(authority);
            let host = authority.split(':').next().unwrap_or(authority);
            (host.to_string(), path.to_string())
        } else if let Some((user_host, path)) = url.split_once(':') {
            // scp-style: git@github.com:owner/name.git
            let host = user_host.rsplit('@').next().unwrap_or(user_host);
            (host.to_string(), path.to_string())
        } else {
            bail!("remote URL `{url}` doesn't look like a GitHub repository");
        };
        Self::from_path(&host, &path)
    }

    /// Parse `owner/name` or `host/owner/name`.
    pub fn from_slug(slug: &str) -> Result<Self> {
        let parts: Vec<&str> = slug.trim_matches('/').split('/').collect();
        match parts.as_slice() {
            [owner, name] => Self::from_path("github.com", &format!("{owner}/{name}")),
            [host, owner, name] => Self::from_path(host, &format!("{owner}/{name}")),
            _ => bail!("expected OWNER/NAME or HOST/OWNER/NAME, got `{slug}`"),
        }
    }

    fn from_path(host: &str, path: &str) -> Result<Self> {
        let path = path.trim_matches('/');
        let path = path.strip_suffix(".git").unwrap_or(path);
        let mut it = path.split('/');
        match (it.next(), it.next(), it.next()) {
            (Some(owner), Some(name), None) if !owner.is_empty() && !name.is_empty() => {
                Ok(RepoRef {
                    host: host.to_string(),
                    owner: owner.to_string(),
                    name: name.to_string(),
                })
            }
            _ => bail!("`{host}/{path}` doesn't look like OWNER/NAME"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(host: &str, owner: &str, name: &str) -> RepoRef {
        RepoRef {
            host: host.into(),
            owner: owner.into(),
            name: name.into(),
        }
    }

    #[test]
    fn parses_common_url_forms() {
        let want = r("github.com", "andrewaylett", "jj-gh");
        for url in [
            "https://github.com/andrewaylett/jj-gh",
            "https://github.com/andrewaylett/jj-gh.git",
            "https://user:tok@github.com/andrewaylett/jj-gh.git",
            "git@github.com:andrewaylett/jj-gh.git",
            "ssh://git@github.com/andrewaylett/jj-gh.git",
            "ssh://git@github.com:22/andrewaylett/jj-gh",
        ] {
            assert_eq!(RepoRef::from_url(url).unwrap(), want, "{url}");
        }
    }

    #[test]
    fn parses_enterprise_hosts() {
        assert_eq!(
            RepoRef::from_url("git@ghe.example.com:team/app.git").unwrap(),
            r("ghe.example.com", "team", "app")
        );
    }

    #[test]
    fn rejects_non_repos() {
        assert!(RepoRef::from_url("/some/local/path/to/repo").is_err());
        assert!(RepoRef::from_url("https://github.com/only-owner").is_err());
    }

    #[test]
    fn parses_slugs() {
        assert_eq!(
            RepoRef::from_slug("a/b").unwrap(),
            r("github.com", "a", "b")
        );
        assert_eq!(
            RepoRef::from_slug("ghe.example.com/a/b").unwrap(),
            r("ghe.example.com", "a", "b")
        );
        assert!(RepoRef::from_slug("nope").is_err());
    }
}
