//! Account creation identities and credentials; credentials are never serializable.

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

pub(crate) struct Password(Zeroizing<Vec<u8>>);

impl Password {
    pub(crate) fn from_text(mut text: Zeroizing<String>) -> anyhow::Result<Self> {
        Self::from_bytes(Zeroizing::new(std::mem::take(&mut *text).into_bytes()))
    }

    pub(crate) fn from_bytes(bytes: Zeroizing<Vec<u8>>) -> anyhow::Result<Self> {
        ensure!(
            !bytes.is_empty() && bytes.len() <= 1024 && !bytes.contains(&0),
            "enter a nonempty account password of at most 1024 UTF-8 bytes, without NUL characters"
        );
        std::str::from_utf8(&bytes).context("account password must be UTF-8")?;
        Ok(Self(bytes))
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.0
    }
}

pub(crate) fn validate_name(name: &str) -> anyhow::Result<()> {
    ensure!(
        name.len() <= 64
            && name.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'),
        "start the new account name with an ASCII letter; use at most 64 letters, digits or underscores"
    );
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Receipt {
    pub version: u32,
    pub owner: u32,
    pub name: String,
    pub uid: u32,
    pub uuid: String,
    pub created: bool,
    pub password_set: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ExistingAccount {
    Ready,
    RetireUnusedReservation,
}

impl Receipt {
    pub(crate) fn validate(&self, owner: u32, name: &str) -> anyhow::Result<()> {
        validate_name(&self.name)?;
        ensure!(
            self.version == 1
                && self.owner == owner
                && self.name == name
                && owner >= 501
                && (501..60000).contains(&self.uid)
                && self.uid != owner,
            "saved account creation belongs to another account or user; nothing changed"
        );
        ensure!(
            self.uuid.len() == 36
                && self.uuid.bytes().enumerate().all(|(i, byte)| {
                    if [8, 13, 18, 23].contains(&i) {
                        byte == b'-'
                    } else {
                        byte.is_ascii_hexdigit()
                    }
                }),
            "invalid account creation identity"
        );
        ensure!(
            !self.password_set || self.created,
            "invalid account creation progress"
        );
        Ok(())
    }

    pub(crate) fn verify_record(
        &self,
        uid: u32,
        uuid: &str,
        home: &std::path::Path,
    ) -> anyhow::Result<()> {
        ensure!(
            uid == self.uid
                && uuid.eq_ignore_ascii_case(&self.uuid)
                && home == std::path::Path::new("/Users").join(&self.name),
            "saved creation no longer matches the account; nothing changed"
        );
        Ok(())
    }

    pub(crate) fn verify_missing(&self, home_exists: bool) -> anyhow::Result<()> {
        ensure!(
            !self.created,
            "the created account was removed or is unavailable; select a new account name"
        );
        ensure!(
            !home_exists,
            "account home already exists; retained without modification"
        );
        Ok(())
    }

    pub(crate) fn require_ready(
        &self,
        uid: u32,
        uuid: &str,
        home: &std::path::Path,
    ) -> anyhow::Result<()> {
        self.verify_record(uid, uuid, home)?;
        ensure!(
            self.created && self.password_set,
            "account creation is unfinished; resume Create account… in Glass from the normal user who began it before enrolling this account"
        );
        Ok(())
    }

    pub(crate) fn existing_selection(
        &self,
        owner: u32,
        uid: u32,
        uuid: &str,
        home: &std::path::Path,
    ) -> anyhow::Result<ExistingAccount> {
        self.validate(owner, &self.name)?;
        if !self.created && !self.password_set && !uuid.eq_ignore_ascii_case(&self.uuid) {
            return Ok(ExistingAccount::RetireUnusedReservation);
        }
        self.require_ready(uid, uuid, home)?;
        Ok(ExistingAccount::Ready)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receipt() -> Receipt {
        Receipt {
            version: 1,
            owner: 501,
            name: "glassdev".into(),
            uid: 510,
            uuid: "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".into(),
            created: false,
            password_set: false,
        }
    }

    #[test]
    fn creation_cannot_resume_another_users_or_another_accounts_identity() {
        let record = receipt();
        assert!(record.validate(501, "glassdev").is_ok());
        assert!(record.validate(502, "glassdev").is_err());
        assert!(record.validate(501, "other").is_err());
        for uid in [0, 500, 501, 60000, u32::MAX] {
            assert!(
                Receipt {
                    uid,
                    ..record.clone()
                }
                .validate(501, "glassdev")
                .is_err()
            );
        }
    }

    #[test]
    fn creation_receipts_cannot_contain_credentials_or_unknown_fields() {
        let mut value = serde_json::to_value(receipt()).unwrap();
        value["password"] = "not retained".into();
        assert!(serde_json::from_value::<Receipt>(value).is_err());
    }

    #[test]
    fn malformed_record_ids_and_shell_or_path_names_are_rejected() {
        for uuid in [
            "",
            "../other",
            "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeee!",
            "aaaaaaaa_bbbb_4ccc_8ddd_eeeeeeeeeeee",
        ] {
            assert!(
                Receipt {
                    uuid: uuid.into(),
                    ..receipt()
                }
                .validate(501, "glassdev")
                .is_err()
            );
        }
        for name in [
            "", "..", "-help", "_system", "0user", "a/b", "a b", "a;id", "ümlaut", "a\0b",
        ] {
            assert!(validate_name(name).is_err());
        }
    }

    #[test]
    fn bounded_unicode_passwords_are_supported_without_serialization() {
        let secret = Password::from_text(Zeroizing::new("local 密码 ' $(`)\n".into())).unwrap();
        assert_eq!(secret.bytes(), "local 密码 ' $(`)\n".as_bytes());
        for bytes in [vec![], vec![b'x'; 1025], vec![b'a', 0, b'b'], vec![255]] {
            assert!(Password::from_bytes(Zeroizing::new(bytes)).is_err());
        }
    }

    #[test]
    fn retries_require_the_same_uid_uuid_and_home() {
        let record = receipt();
        assert!(
            record
                .verify_record(
                    510,
                    &record.uuid.to_uppercase(),
                    std::path::Path::new("/Users/glassdev")
                )
                .is_ok()
        );
        assert!(
            record
                .verify_record(511, &record.uuid, std::path::Path::new("/Users/glassdev"))
                .is_err()
        );
        assert!(
            record
                .verify_record(
                    510,
                    "11111111-2222-4333-8444-555555555555",
                    std::path::Path::new("/Users/glassdev")
                )
                .is_err()
        );
        assert!(
            record
                .verify_record(510, &record.uuid, std::path::Path::new("/Users/other"))
                .is_err()
        );
    }

    #[test]
    fn interrupted_creation_never_adopts_an_existing_home_or_recreates_a_completed_account() {
        let record = receipt();
        assert!(record.verify_missing(false).is_ok());
        assert!(record.verify_missing(true).is_err());
        assert!(
            Receipt {
                created: true,
                ..record
            }
            .verify_missing(false)
            .is_err()
        );
    }

    #[test]
    fn password_checkpoint_cannot_precede_record_creation() {
        assert!(
            Receipt {
                password_set: true,
                ..receipt()
            }
            .validate(501, "glassdev")
            .is_err()
        );
        assert!(
            Receipt {
                created: true,
                password_set: true,
                ..receipt()
            }
            .validate(501, "glassdev")
            .is_ok()
        );
    }

    #[test]
    fn existing_account_enrollment_cannot_bypass_an_unfinished_password_checkpoint() {
        let record = receipt();
        let home = std::path::Path::new("/Users/glassdev");
        assert!(
            record
                .require_ready(record.uid, &record.uuid, home)
                .is_err()
        );
        assert!(
            Receipt {
                created: true,
                ..record.clone()
            }
            .require_ready(record.uid, &record.uuid, home)
            .is_err()
        );
        assert!(
            Receipt {
                created: true,
                password_set: true,
                ..record.clone()
            }
            .require_ready(record.uid, &record.uuid, home)
            .is_ok()
        );
    }

    #[test]
    fn only_an_unclaimed_reservation_for_a_different_record_can_be_retired() {
        let record = receipt();
        let home = std::path::Path::new("/custom/existing");
        let unrelated = "11111111-2222-4333-8444-555555555555";
        assert_eq!(
            record
                .existing_selection(501, 512, unrelated, home)
                .unwrap(),
            ExistingAccount::RetireUnusedReservation
        );
        assert!(
            record
                .existing_selection(501, record.uid, &record.uuid, home)
                .is_err()
        );
        assert!(
            Receipt {
                created: true,
                ..record
            }
            .existing_selection(501, 512, unrelated, home)
            .is_err()
        );
    }

    #[test]
    fn a_ready_created_account_cannot_be_claimed_by_another_normal_owner() {
        let record = Receipt {
            created: true,
            password_set: true,
            ..receipt()
        };
        let home = std::path::Path::new("/Users/glassdev");
        assert_eq!(
            record
                .existing_selection(501, record.uid, &record.uuid, home)
                .unwrap(),
            ExistingAccount::Ready
        );
        assert!(
            record
                .existing_selection(502, record.uid, &record.uuid, home)
                .is_err()
        );
        assert!(
            receipt()
                .existing_selection(502, 512, "11111111-2222-4333-8444-555555555555", home)
                .is_err()
        );
    }
}
