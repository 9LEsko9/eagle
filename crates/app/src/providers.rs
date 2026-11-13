// SPDX-FileCopyrightText: 2026 eagle contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Registry of the providers built into eagle. The order is the user's priority.

use crate::tokens::{MemoryTokens, TokenStore};
use eagle_core::{Account, AuthMethod, Capabilities, Provider, ProviderError, Token, TrackId};
use eagle_net::Client;
use std::{collections::HashMap, sync::Arc, sync::RwLock};

/// Providers compiled into eagle. Adding one is a new crate plus one line here.
pub fn builtin(net: Arc<Client>) -> Vec<Arc<dyn Provider>> {
    vec![Arc::new(eagle_yandex::Yandex::new(net))]
}

/// What the UI needs to show one provider: whether to offer sign-in and what works.
#[derive(Clone, Debug)]
pub struct ProviderStatus {
    pub id: String,
    pub name: String,
    pub auth: AuthMethod,
    pub signed_in: bool,
    /// Effective capabilities; all off while signed out.
    pub capabilities: Capabilities,
}

/// Reads the `provider_order` setting into provider ids, first wins. The setting is a JSON map
/// of id to fractional position (so a provider can be put between two others by one write and
/// two devices' edits merge per provider); a plain list of ids is accepted as well.
pub fn parse_provider_order(json: &str) -> Vec<String> {
    if let Ok(positions) = serde_json::from_str::<HashMap<String, f64>>(json) {
        let mut positions: Vec<_> = positions
            .into_iter()
            .filter(|(_, at)| at.is_finite())
            .collect();
        positions.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        return positions.into_iter().map(|(id, _)| id).collect();
    }
    serde_json::from_str(json).unwrap_or_default()
}

/// A position strictly between `before` and `after`; either side may be open.
pub fn position_between(before: Option<f64>, after: Option<f64>) -> f64 {
    match (before, after) {
        (Some(a), Some(b)) => (a + b) / 2.0,
        (Some(a), None) => a + 1.0,
        (None, Some(b)) => b - 1.0,
        (None, None) => 0.0,
    }
}

pub struct Providers {
    /// Priority order: the first entry wins.
    all: Vec<Arc<dyn Provider>>,
    /// Accounts of signed-in providers, by provider id.
    accounts: RwLock<HashMap<String, Account>>,
    tokens: Arc<dyn TokenStore>,
}

impl Providers {
    /// Sorts `all` by `order` (provider ids): ids missing from `order` follow in their
    /// original order, ids of unknown providers are ignored, repeated providers are dropped.
    pub fn new(all: Vec<Arc<dyn Provider>>, order: &[String]) -> Self {
        let mut rest = all;
        let mut sorted = Vec::with_capacity(rest.len());
        for id in order {
            if let Some(at) = rest.iter().position(|p| p.info().id == id) {
                sorted.push(rest.remove(at));
            }
        }
        for provider in rest {
            if !sorted.iter().any(|p| p.info().id == provider.info().id) {
                sorted.push(provider);
            }
        }
        Self {
            all: sorted,
            accounts: RwLock::new(HashMap::new()),
            tokens: Arc::new(MemoryTokens::default()),
        }
    }
    /// Replaces the in-memory token store (the default) with a persistent one.
    pub fn with_tokens(mut self, tokens: Arc<dyn TokenStore>) -> Self {
        self.tokens = tokens;
        self
    }
    pub fn tokens(&self) -> &Arc<dyn TokenStore> {
        &self.tokens
    }
    /// Signs in with the saved token, or with whatever session the provider already holds,
    /// and records the account. A failure marks the provider signed out.
    pub fn restore(&self, provider: &dyn Provider) -> Result<Account, ProviderError> {
        let id = provider.info().id;
        let result = match self.tokens.load(id) {
            Ok(Some(token)) => provider.restore(token),
            _ => provider.account(),
        };
        self.set_account(id, result.as_ref().ok().cloned());
        result
    }
    /// Signs in with a token the user typed; it is saved only after the provider accepts it.
    pub fn sign_in(&self, provider: &dyn Provider, token: Token) -> Result<Account, ProviderError> {
        let account = provider.login(token.clone())?;
        if let Err(error) = self.tokens.save(provider.info().id, &token) {
            // The session must not outlive a sign-in that reported a failure.
            provider.logout();
            return Err(error);
        }
        self.set_account(provider.info().id, Some(account.clone()));
        Ok(account)
    }
    /// Forgets the provider's session and its saved token.
    pub fn sign_out(&self, provider: &dyn Provider) -> Result<(), ProviderError> {
        provider.logout();
        self.set_account(provider.info().id, None);
        self.tokens.delete(provider.info().id)
    }
    /// Every provider in priority order with its current state.
    pub fn statuses(&self) -> Vec<ProviderStatus> {
        self.all
            .iter()
            .map(|p| {
                let signed_in = self.is_signed_in(p.as_ref());
                ProviderStatus {
                    id: p.info().id.to_owned(),
                    name: p.info().name.to_owned(),
                    auth: p.info().auth.clone(),
                    signed_in,
                    capabilities: if signed_in {
                        self.effective(p.as_ref())
                    } else {
                        Capabilities::none()
                    },
                }
            })
            .collect()
    }
    pub fn all(&self) -> &[Arc<dyn Provider>] {
        &self.all
    }
    pub fn get(&self, provider_id: &str) -> Option<&Arc<dyn Provider>> {
        self.all.iter().find(|p| p.info().id == provider_id)
    }
    pub fn for_track(&self, id: &TrackId) -> Option<&Arc<dyn Provider>> {
        self.get(id.provider())
    }
    /// Records the result of a sign-in check; `None` marks the provider signed out.
    pub fn set_account(&self, provider_id: &str, account: Option<Account>) {
        let mut accounts = self.accounts.write().unwrap_or_else(|p| p.into_inner());
        match account {
            Some(account) => accounts.insert(provider_id.to_owned(), account),
            None => accounts.remove(provider_id),
        };
    }
    pub fn account(&self, provider_id: &str) -> Option<Account> {
        let accounts = self.accounts.read().unwrap_or_else(|p| p.into_inner());
        accounts.get(provider_id).cloned()
    }
    /// A provider that needs no sign-in always counts as signed in.
    pub fn is_signed_in(&self, provider: &dyn Provider) -> bool {
        matches!(provider.info().auth, AuthMethod::None)
            || self.account(provider.info().id).is_some()
    }
    /// What the provider can do now: its own capabilities narrowed by the account's.
    pub fn effective(&self, provider: &dyn Provider) -> Capabilities {
        let info = provider.info();
        match self.account(info.id) {
            Some(account) => info.capabilities.intersect(&account.capabilities),
            None if matches!(info.auth, AuthMethod::None) => info.capabilities,
            None => Capabilities::none(),
        }
    }
    pub fn signed_in(&self) -> impl Iterator<Item = &Arc<dyn Provider>> + '_ {
        self.all.iter().filter(|p| self.is_signed_in(p.as_ref()))
    }
    /// Signed-in providers whose effective capabilities satisfy `f`, by priority.
    pub fn with_capability<'a>(
        &'a self,
        f: impl Fn(&Capabilities) -> bool + 'a,
    ) -> impl Iterator<Item = &'a Arc<dyn Provider>> + 'a {
        self.all
            .iter()
            .filter(move |p| self.is_signed_in(p.as_ref()) && f(&self.effective(p.as_ref())))
    }
    pub fn first_with(&self, f: impl Fn(&Capabilities) -> bool) -> Option<&Arc<dyn Provider>> {
        self.all
            .iter()
            .find(|p| self.is_signed_in(p.as_ref()) && f(&self.effective(p.as_ref())))
    }
    /// The provider whose account owns this installation's rights (offline, downloads, play
    /// reports, radio): the first one that signs in with a token.
    pub fn account_provider(&self) -> Option<&Arc<dyn Provider>> {
        self.all
            .iter()
            .find(|p| matches!(p.info().auth, AuthMethod::Token { .. }))
    }
}

/// Settings key holding the provider's account uid.
pub(crate) fn account_key(provider_id: &str) -> String {
    format!("account:{provider_id}")
}

/// Settings key holding the provider's liked albums (JSON).
pub(crate) fn liked_albums_key(provider_id: &str) -> String {
    format!("liked_albums:{provider_id}")
}

/// Settings key holding the provider's liked artists (JSON).
pub(crate) fn liked_artists_key(provider_id: &str) -> String {
    format!("liked_artists:{provider_id}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use eagle_provider_sdk::testkit::FakeProvider;

    fn fakes() -> Vec<Arc<dyn Provider>> {
        vec![
            Arc::new(FakeProvider::new("fakea")),
            Arc::new(FakeProvider::new("fakeb")),
            Arc::new(FakeProvider::new("fakec")),
        ]
    }
    fn ids(providers: &Providers) -> Vec<&'static str> {
        providers.all().iter().map(|p| p.info().id).collect()
    }
    fn account(capabilities: Capabilities) -> Account {
        Account {
            id: "1".into(),
            subscription: true,
            capabilities,
        }
    }

    #[test]
    fn order_setting_is_a_map_of_fractional_positions_or_a_plain_list() {
        let order = parse_provider_order(r#"{"fakea":2.0,"fakeb":1.5,"fakec":-1.0}"#);
        assert_eq!(order, ["fakec", "fakeb", "fakea"]);
        assert_eq!(
            parse_provider_order(r#"["fakeb","fakea"]"#),
            ["fakeb", "fakea"]
        );
        assert!(parse_provider_order("not json").is_empty());
        let between = position_between(Some(1.5), Some(2.0));
        assert!(1.5 < between && between < 2.0);
        assert!(position_between(None, Some(0.0)) < 0.0);
        assert!(position_between(Some(3.0), None) > 3.0);
    }
    #[test]
    fn order_follows_setting_and_appends_new_providers() {
        let order = ["fakeb".to_owned()];
        assert_eq!(
            ids(&Providers::new(fakes(), &order)),
            ["fakeb", "fakea", "fakec"]
        );
    }
    #[test]
    fn unknown_and_repeated_ids_in_order_are_ignored() {
        let order = ["gone".to_owned(), "fakec".to_owned(), "fakec".to_owned()];
        assert_eq!(
            ids(&Providers::new(fakes(), &order)),
            ["fakec", "fakea", "fakeb"]
        );
    }
    #[test]
    fn duplicate_providers_keep_the_first() {
        let mut all = fakes();
        all.push(Arc::new(FakeProvider::new("fakea")));
        assert_eq!(Providers::new(all, &[]).all().len(), 3);
    }
    #[test]
    fn signed_out_providers_are_skipped() {
        let providers = Providers::new(fakes(), &[]);
        assert_eq!(providers.signed_in().count(), 0);
        providers.set_account("fakeb", Some(account(Capabilities::default())));
        let signed: Vec<_> = providers.signed_in().map(|p| p.info().id).collect();
        assert_eq!(signed, ["fakeb"]);
        providers.set_account("fakeb", None);
        assert_eq!(providers.signed_in().count(), 0);
    }
    #[test]
    fn capability_filter_uses_account_narrowing_and_priority() {
        let providers = Providers::new(fakes(), &["fakec".to_owned()]);
        let mut full = Capabilities::default();
        full.radio = true;
        providers.set_account("fakea", Some(account(Capabilities::default())));
        providers.set_account("fakeb", Some(account(full)));
        providers.set_account("fakec", Some(account(full)));
        let radio: Vec<_> = providers
            .with_capability(|c| c.radio)
            .map(|p| p.info().id)
            .collect();
        assert_eq!(radio, ["fakec", "fakeb"]);
        assert_eq!(
            providers.first_with(|c| c.radio).map(|p| p.info().id),
            Some("fakec")
        );
        assert!(providers.first_with(|c| c.lyrics).is_none());
    }
    #[test]
    fn lookup_by_track_prefix() {
        let providers = Providers::new(fakes(), &[]);
        let id: TrackId = "fakeb:7".parse().unwrap();
        assert_eq!(providers.for_track(&id).map(|p| p.info().id), Some("fakeb"));
        assert!(providers.for_track(&"nope:1".parse().unwrap()).is_none());
    }

    struct FailingSave;
    impl TokenStore for FailingSave {
        fn load(&self, _: &str) -> Result<Option<Token>, ProviderError> {
            Ok(None)
        }
        fn save(&self, _: &str, _: &Token) -> Result<(), ProviderError> {
            Err(ProviderError::Unavailable)
        }
        fn delete(&self, _: &str) -> Result<(), ProviderError> {
            Ok(())
        }
    }

    #[test]
    fn sign_in_that_cannot_save_the_token_leaves_no_session() {
        let fake = Arc::new(FakeProvider::new("fakea").signed_out());
        let providers = Providers::new(vec![fake.clone() as Arc<dyn Provider>], &[])
            .with_tokens(Arc::new(FailingSave));
        assert!(
            providers
                .sign_in(fake.as_ref(), Token::new("t".into()))
                .is_err()
        );
        assert!(fake.account().is_err());
        assert!(!providers.is_signed_in(fake.as_ref()));
    }
}
