// SPDX-FileCopyrightText: 2026 eagle contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Provider tokens in the OS keyring. The service and account names match the
//! layout used before providers stopped touching the keyring, so saved sessions survive.

use eagle_core::{ProviderError, Token};

const SERVICE: &str = "eagle";

pub fn load(provider: &str) -> Result<Option<Token>, ProviderError> {
    map_load(entry(provider)?.get_password())
}

pub fn save(provider: &str, token: &Token) -> Result<(), ProviderError> {
    map_save(entry(provider)?.set_password(token.expose()))
}

pub fn delete(provider: &str) -> Result<(), ProviderError> {
    map_delete(entry(provider)?.delete_credential())
}

fn entry(provider: &str) -> Result<keyring::Entry, ProviderError> {
    keyring::Entry::new(SERVICE, provider).map_err(|_| ProviderError::Keyring)
}

fn map_load(result: keyring::Result<String>) -> Result<Option<Token>, ProviderError> {
    match result {
        Ok(password) => Ok(Some(Token::new(password))),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(_) => Err(ProviderError::Keyring),
    }
}

fn map_save(result: keyring::Result<()>) -> Result<(), ProviderError> {
    result.map_err(|_| ProviderError::Keyring)
}

fn map_delete(result: keyring::Result<()>) -> Result<(), ProviderError> {
    match result {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(_) => Err(ProviderError::Keyring),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_entry_is_not_an_error() {
        assert!(matches!(map_load(Err(keyring::Error::NoEntry)), Ok(None)));
        assert!(map_delete(Err(keyring::Error::NoEntry)).is_ok());
    }

    #[test]
    fn storage_failures_map_to_keyring_error() {
        let failure = || keyring::Error::TooLong("account".into(), 1);
        assert!(matches!(
            map_load(Err(failure())),
            Err(ProviderError::Keyring)
        ));
        assert!(matches!(
            map_save(Err(failure())),
            Err(ProviderError::Keyring)
        ));
        assert!(matches!(
            map_delete(Err(failure())),
            Err(ProviderError::Keyring)
        ));
    }

    #[test]
    fn stored_password_becomes_token() {
        let token = map_load(Ok("secret".into())).unwrap().unwrap();
        assert_eq!(token.expose(), "secret");
    }
}
