//! Complete shared-library observations, with an exclusive expiry boundary.
use pitcrew_intake::AuthorizedViewer;
use pitcrew_intake::pipeline::{AccessGrant, AccessProvider};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessSnapshot {
    account_id: String,
    library_ids: Option<Vec<String>>,
    observed_at: u64,
    expires_at: u64,
}

impl AccessProvider for AccessSnapshot {
    fn access(&self, account_id: &str, now: u64) -> Option<AccessGrant> {
        let viewer = self.authorize(now).filter(|v| v.account_id == account_id)?;
        Some(AccessGrant {
            account_id: viewer.account_id,
            library_ids: viewer.library_ids,
            observed_at: self.observed_at,
            expires_at: self.expires_at,
        })
    }
}

impl AccessSnapshot {
    /// Trusted auth adapters only: never construct from a viewer-submitted body.
    pub fn from_grants(
        account_id: String,
        library_ids: Vec<String>,
        observed_at: u64,
        expires_at: u64,
    ) -> Self {
        Self {
            account_id,
            library_ids: Some(library_ids),
            observed_at,
            expires_at,
        }
    }

    pub fn authorize(&self, now: u64) -> Option<AuthorizedViewer> {
        let libraries = self.library_ids.as_ref()?;
        if self.account_id.is_empty()
            || libraries.iter().any(String::is_empty)
            || now < self.observed_at
            || now >= self.expires_at
        {
            return None;
        }
        Some(AuthorizedViewer {
            account_id: self.account_id.clone(),
            library_ids: libraries.clone(),
        })
    }

    /// Parse the owner-authenticated Plex shared_servers XML response. Unknown
    /// accounts get an empty grant; truncated or malformed data grants nothing.
    /// Section keys are mapped explicitly to this installation's library IDs.
    pub fn from_shared_servers_xml(
        xml: &[u8],
        account_id: &str,
        section_map: &[(String, String)],
        observed_at: u64,
        expires_at: u64,
    ) -> Result<Self, crate::Error> {
        if xml.len() > 1024 * 1024 {
            return Err(crate::Error::Bound);
        }
        let mut mapped = BTreeSet::new();
        if section_map.iter().any(|(key, _)| !mapped.insert(key)) {
            return Err(crate::Error::Ambiguous);
        }
        let text = std::str::from_utf8(xml).map_err(|_| crate::Error::Malformed)?;
        let doc = roxmltree::Document::parse(text).map_err(|_| crate::Error::Malformed)?;
        let root = doc.root_element();
        if !root.has_tag_name("MediaContainer") {
            return Err(crate::Error::Malformed);
        }
        let servers: Vec<_> = root.children().filter(|n| n.is_element()).collect();
        let count: usize = root
            .attribute("size")
            .ok_or(crate::Error::Malformed)?
            .parse()
            .map_err(|_| crate::Error::Malformed)?;
        if count != servers.len() {
            return Err(crate::Error::Malformed);
        }
        let mut grants = Vec::new();
        let mut seen = false;
        for server in servers {
            if !server.has_tag_name("SharedServer") {
                return Err(crate::Error::Malformed);
            }
            if server.attribute("userID") != Some(account_id) {
                continue;
            }
            if seen {
                return Err(crate::Error::Ambiguous);
            }
            seen = true;
            let mut sections = BTreeSet::new();
            for section in server.children().filter(|n| n.is_element()) {
                if !section.has_tag_name("Section") {
                    return Err(crate::Error::Malformed);
                }
                let key = section.attribute("key").ok_or(crate::Error::Malformed)?;
                if !sections.insert(key) {
                    return Err(crate::Error::Ambiguous);
                }
                let shared = section.attribute("shared").ok_or(crate::Error::Malformed)?;
                if shared == "0" {
                    continue;
                }
                if shared != "1" {
                    return Err(crate::Error::Malformed);
                }
                grants.extend(
                    section_map
                        .iter()
                        .filter(|(id, _)| id == key)
                        .map(|(_, library)| library.clone()),
                );
            }
        }
        grants.sort();
        grants.dedup();
        Ok(Self::from_grants(
            account_id.into(),
            grants,
            observed_at,
            expires_at,
        ))
    }
}
