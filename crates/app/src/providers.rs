// SPDX-FileCopyrightText: 2026 eagle contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Registry of the providers built into eagle. The order is the user's priority.

use eagle_core::{Account, AuthMethod, Capabilities, Provider, TrackId};
use eagle_net::Client;
use std::{collections::HashMap, sync::Arc, sync::RwLock};

/// Providers compiled into eagle. Adding one is a new crate plus one line here.
pub fn builtin(net: Arc<Client>) -> Vec<Arc<dyn Provider>> {
    vec![Arc::new(eagle_yandex::Yandex::new(net))]
}

pub struct Providers {
    /// Priority order: the first entry wins.
    all: Vec<Arc<dyn Provider>>,
    /// Accounts of signed-in providers, by provider id.
    accounts: RwLock<HashMap<String, Account>>,
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
        }
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
}
