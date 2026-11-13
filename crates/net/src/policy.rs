// SPDX-License-Identifier: GPL-3.0-or-later
//! Policy for background transfers when platform network information is incomplete.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum NetworkStatus {
    Yes,
    No,
    #[default]
    Unknown,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NetworkPolicy {
    pub online: NetworkStatus,
    pub metered: NetworkStatus,
    pub allow_metered: bool,
}
impl NetworkPolicy {
    pub fn allows_background(self) -> bool {
        self.online != NetworkStatus::No
            && (self.metered != NetworkStatus::Yes || self.allow_metered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn background_is_blocked_only_when_offline_or_known_metered_without_opt_in() {
        assert!(NetworkPolicy::default().allows_background());
        assert!(
            !NetworkPolicy {
                online: NetworkStatus::No,
                ..Default::default()
            }
            .allows_background()
        );
        assert!(
            !NetworkPolicy {
                metered: NetworkStatus::Yes,
                ..Default::default()
            }
            .allows_background()
        );
        assert!(
            NetworkPolicy {
                metered: NetworkStatus::Yes,
                allow_metered: true,
                ..Default::default()
            }
            .allows_background()
        );
        assert!(
            !NetworkPolicy {
                online: NetworkStatus::No,
                metered: NetworkStatus::Yes,
                allow_metered: true
            }
            .allows_background()
        );
    }
}
