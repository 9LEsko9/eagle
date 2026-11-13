// SPDX-FileCopyrightText: 2026 eagle contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Where provider tokens are kept between runs. The app only sees this trait, so tests
//! never touch the user's keychain.

use eagle_core::{ProviderError, Token};
use std::{collections::HashMap, sync::Mutex, sync::PoisonError};

pub trait TokenStore: Send + Sync {
    fn load(&self, provider: &str) -> Result<Option<Token>, ProviderError>;
    fn save(&self, provider: &str, token: &Token) -> Result<(), ProviderError>;
    fn delete(&self, provider: &str) -> Result<(), ProviderError>;
}

/// The OS keyring.
pub struct Keyring;

impl TokenStore for Keyring {
    fn load(&self, provider: &str) -> Result<Option<Token>, ProviderError> {
        eagle_platform::tokens::load(provider)
    }
    fn save(&self, provider: &str, token: &Token) -> Result<(), ProviderError> {
        eagle_platform::tokens::save(provider, token)
    }
    fn delete(&self, provider: &str) -> Result<(), ProviderError> {
        eagle_platform::tokens::delete(provider)
    }
}

/// Tokens held in memory only: isolated sessions and tests.
#[derive(Default)]
pub struct MemoryTokens(Mutex<HashMap<String, String>>);

impl MemoryTokens {
    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<String, String>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl TokenStore for MemoryTokens {
    fn load(&self, provider: &str) -> Result<Option<Token>, ProviderError> {
        Ok(self.map().get(provider).map(|t| Token::new(t.clone())))
    }
    fn save(&self, provider: &str, token: &Token) -> Result<(), ProviderError> {
        self.map()
            .insert(provider.to_owned(), token.expose().to_owned());
        Ok(())
    }
    fn delete(&self, provider: &str) -> Result<(), ProviderError> {
        self.map().remove(provider);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_round_trips_and_forgets() {
        let tokens = MemoryTokens::default();
        assert!(tokens.load("fakea").unwrap().is_none());
        tokens.save("fakea", &Token::new("t".into())).unwrap();
        assert_eq!(tokens.load("fakea").unwrap().unwrap().expose(), "t");
        assert!(tokens.load("fakeb").unwrap().is_none());
        tokens.delete("fakea").unwrap();
        assert!(tokens.load("fakea").unwrap().is_none());
    }
}
