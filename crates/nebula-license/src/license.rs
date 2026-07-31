//! Licence files and validation.

use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::fingerprint::Fingerprint;
use crate::{LicenseError, Result};

/// What a licence entitles the holder to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Plan {
    /// The $10/month subscription: the editor, local intelligence, the SDK and
    /// marketplace, and settings sync. Cloud model use is the holder's own
    /// direct API cost and is not metered here.
    Individual,
    /// A seat in a team subscription.
    Team,
    /// A perpetual licence, for offline or air-gapped deployments.
    Perpetual,
}

impl Plan {
    /// The name shown in the UI.
    pub const fn display_name(&self) -> &'static str {
        match self {
            Plan::Individual => "Individual",
            Plan::Team => "Team",
            Plan::Perpetual => "Perpetual",
        }
    }

    /// Whether this plan ever expires.
    pub const fn expires(&self) -> bool {
        !matches!(self, Plan::Perpetual)
    }
}

/// The signed body of a licence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LicenseClaims {
    /// Licence identifier, for support and revocation.
    pub id: String,
    /// Who it was issued to.
    pub licensee: String,
    /// What it entitles them to.
    pub plan: Plan,
    /// When it was issued, RFC 3339.
    pub issued_at: String,
    /// When it expires, RFC 3339. Absent for a perpetual licence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// The machine it was issued for. Absent for a floating licence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<Fingerprint>,
    /// Days of grace after expiry before the editor stops.
    #[serde(default = "default_grace_days")]
    pub grace_days: u32,
}

fn default_grace_days() -> u32 {
    // Long enough to cover a failed card renewal and a weekend, because that is
    // what this is nearly always for.
    14
}

/// A licence file: claims plus the issuer's signature over them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct License {
    /// The claims.
    pub claims: LicenseClaims,
    /// Ed25519 signature over the canonical claims, base64-encoded.
    pub signature: String,
}

/// The state a licence is in right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LicenseStatus {
    /// Valid and current.
    Valid {
        /// The plan.
        plan: Plan,
        /// Days until it expires, if it does.
        days_remaining: Option<i64>,
    },
    /// Expired but inside its grace period.
    ///
    /// The editor keeps working and says so, because the usual cause is a
    /// payment that failed rather than a licence that was never bought.
    InGrace {
        /// The plan.
        plan: Plan,
        /// Days of grace left.
        days_remaining: i64,
    },
    /// Expired, grace exhausted.
    Expired {
        /// When it expired.
        expired_on: String,
    },
    /// Issued for a different machine.
    WrongMachine,
    /// The signature does not verify.
    Invalid,
}

impl LicenseStatus {
    /// Whether the editor's licensed features are available.
    pub fn is_usable(&self) -> bool {
        matches!(self, LicenseStatus::Valid { .. } | LicenseStatus::InGrace { .. })
    }

    /// A message for the status bar, or `None` when nothing needs saying.
    pub fn message(&self) -> Option<String> {
        match self {
            LicenseStatus::Valid { days_remaining: Some(days), .. } if *days <= 7 => {
                Some(format!("Your subscription renews in {days} day(s)."))
            }
            LicenseStatus::Valid { .. } => None,
            LicenseStatus::InGrace { days_remaining, .. } => Some(format!(
                "Your subscription could not be renewed. Nebula keeps working for {days_remaining} more day(s)."
            )),
            LicenseStatus::Expired { expired_on } => {
                Some(format!("Your subscription expired on {expired_on}."))
            }
            LicenseStatus::WrongMachine => {
                Some("This licence was issued for a different machine.".to_string())
            }
            LicenseStatus::Invalid => Some("This licence file is not valid.".to_string()),
        }
    }
}

impl LicenseClaims {
    /// The canonical bytes a signature is made over.
    ///
    /// Field order is fixed and every field is length-prefixed, so two different
    /// sets of claims cannot produce the same signing input. Serialising the
    /// struct with serde would work too, but would tie the signature to serde's
    /// output format — a field reordering would silently invalidate every issued
    /// licence.
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut field = |value: &[u8]| {
            out.extend_from_slice(&(value.len() as u64).to_le_bytes());
            out.extend_from_slice(value);
        };

        field(b"nebula-license-v1");
        field(self.id.as_bytes());
        field(self.licensee.as_bytes());
        field(self.plan.display_name().as_bytes());
        field(self.issued_at.as_bytes());
        field(self.expires_at.as_deref().unwrap_or("").as_bytes());
        field(&self.grace_days.to_le_bytes());

        match &self.fingerprint {
            None => field(b""),
            Some(fingerprint) => {
                // Sorted, so a fingerprint's signal order cannot change the
                // signature.
                let mut signals: Vec<String> = fingerprint
                    .signals
                    .iter()
                    .map(|s| format!("{}={}", s.kind, s.digest))
                    .collect();
                signals.sort();
                field(signals.join(";").as_bytes());
            }
        }
        out
    }
}

/// Verifies licences against an issuer's public key.
#[derive(Debug, Clone)]
pub struct Validator {
    issuer: VerifyingKey,
}

impl Validator {
    /// A validator for the issuer key baked into this build.
    ///
    /// The key is public by definition: it is in every copy of the binary. What
    /// it proves is that a licence came from the holder of the corresponding
    /// private key, which lives on the issuing server and nowhere else.
    pub fn production() -> Result<Self> {
        Self::with_issuer(PRODUCTION_ISSUER_KEY)
    }

    /// A validator for an explicit issuer key.
    pub fn with_issuer(base64_key: &str) -> Result<Self> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(base64_key.trim())
            .map_err(|e| LicenseError::Malformed(format!("issuer key is not base64: {e}")))?;
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| LicenseError::Malformed("issuer key must be 32 bytes".to_string()))?;
        let issuer = VerifyingKey::from_bytes(&bytes)
            .map_err(|e| LicenseError::Malformed(e.to_string()))?;
        Ok(Self { issuer })
    }

    /// Validate a licence for this machine, at this moment.
    pub fn validate(&self, license: &License, machine: &Fingerprint) -> LicenseStatus {
        self.validate_at(license, machine, OffsetDateTime::now_utc())
    }

    /// Validate as of an explicit time.
    ///
    /// Exposed so expiry and grace can be tested without waiting or moving the
    /// system clock.
    pub fn validate_at(
        &self,
        license: &License,
        machine: &Fingerprint,
        now: OffsetDateTime,
    ) -> LicenseStatus {
        // 1. Signature. Everything else is a claim until this passes.
        let Ok(signature_bytes) =
            base64::engine::general_purpose::STANDARD.decode(&license.signature)
        else {
            return LicenseStatus::Invalid;
        };
        let Ok(signature_bytes) = <[u8; 64]>::try_from(signature_bytes) else {
            return LicenseStatus::Invalid;
        };
        let signature = ed25519_dalek::Signature::from_bytes(&signature_bytes);
        if self.issuer.verify(&license.claims.signing_bytes(), &signature).is_err() {
            return LicenseStatus::Invalid;
        }

        // 2. Machine binding.
        if let Some(issued_for) = &license.claims.fingerprint
            && !issued_for.matches(machine)
        {
            return LicenseStatus::WrongMachine;
        }

        // 3. Expiry.
        let Some(expires_at) = &license.claims.expires_at else {
            return LicenseStatus::Valid { plan: license.claims.plan, days_remaining: None };
        };
        let Ok(expiry) =
            OffsetDateTime::parse(expires_at, &time::format_description::well_known::Rfc3339)
        else {
            return LicenseStatus::Invalid;
        };

        if now < expiry {
            let days = (expiry - now).whole_days();
            return LicenseStatus::Valid {
                plan: license.claims.plan,
                days_remaining: Some(days),
            };
        }

        let grace_end = expiry + time::Duration::days(license.claims.grace_days as i64);
        if now < grace_end {
            return LicenseStatus::InGrace {
                plan: license.claims.plan,
                days_remaining: (grace_end - now).whole_days().max(0),
            };
        }

        LicenseStatus::Expired { expired_on: expires_at.clone() }
    }

    /// Load a licence from disk and validate it.
    pub fn validate_file(
        &self,
        path: impl AsRef<std::path::Path>,
        machine: &Fingerprint,
    ) -> Result<LicenseStatus> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(LicenseError::Missing);
            }
            Err(e) => return Err(LicenseError::Io(e)),
        };
        let license: License = serde_json::from_str(&text)
            .map_err(|e| LicenseError::Malformed(e.to_string()))?;
        Ok(self.validate(&license, machine))
    }
}

/// The issuer key for release builds.
///
/// A licence signed by anything else does not validate. This constant is
/// replaced at release time by the build pipeline; the value here is a
/// development key whose private half is in the repository's test fixtures, so
/// a development build cannot validate a production licence and vice versa.
pub const PRODUCTION_ISSUER_KEY: &str = "3b6a27bcceb6a42d62a3a8d02a6f0d73653215771de243a63ac048a18b59da29";

/// Issues licences. Used by the licensing service, and by tests.
///
/// The private key this holds must never ship in a client build.
pub struct Issuer {
    signing: SigningKey,
}

impl Issuer {
    /// Generate a new issuer key.
    pub fn generate() -> Self {
        use rand::TryRngCore;

        let mut bytes = [0u8; 32];
        rand::rngs::OsRng
            .try_fill_bytes(&mut bytes)
            .expect("the OS random source must be available to generate an issuer key");
        Self { signing: SigningKey::from_bytes(&bytes) }
    }

    /// The public key clients verify against.
    pub fn public_key_base64(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(self.signing.verifying_key().to_bytes())
    }

    /// Sign a set of claims.
    pub fn issue(&self, claims: LicenseClaims) -> License {
        let signature = self.signing.sign(&claims.signing_bytes());
        License {
            claims,
            signature: base64::engine::general_purpose::STANDARD.encode(signature.to_bytes()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fingerprint::HardwareId;
    use time::Duration;

    fn machine(name: &str) -> Fingerprint {
        let mut signals = vec![
            HardwareId::new("hostname", name.as_bytes()),
            HardwareId::new("cpu", format!("{name}-cpu").as_bytes()),
            HardwareId::new("machine-id", format!("{name}-id").as_bytes()),
        ];
        signals.sort_by(|a, b| a.kind.cmp(&b.kind));
        Fingerprint { signals }
    }

    fn claims(expires_in_days: Option<i64>, fingerprint: Option<Fingerprint>) -> LicenseClaims {
        let now = OffsetDateTime::now_utc();
        let format = &time::format_description::well_known::Rfc3339;
        LicenseClaims {
            id: "lic_test_001".to_string(),
            licensee: "developer@example.com".to_string(),
            plan: Plan::Individual,
            issued_at: now.format(format).unwrap(),
            expires_at: expires_in_days
                .map(|days| (now + Duration::days(days)).format(format).unwrap()),
            fingerprint,
            grace_days: 14,
        }
    }

    fn setup() -> (Issuer, Validator) {
        let issuer = Issuer::generate();
        let validator = Validator::with_issuer(&issuer.public_key_base64()).unwrap();
        (issuer, validator)
    }

    #[test]
    fn a_current_licence_is_valid() {
        let (issuer, validator) = setup();
        let machine = machine("laptop");
        let license = issuer.issue(claims(Some(30), Some(machine.clone())));

        match validator.validate(&license, &machine) {
            LicenseStatus::Valid { plan, days_remaining } => {
                assert_eq!(plan, Plan::Individual);
                assert!(days_remaining.unwrap() >= 29);
            }
            other => panic!("expected Valid, got {other:?}"),
        }
    }

    #[test]
    fn a_licence_signed_by_another_key_is_invalid() {
        // The property the whole scheme rests on: anyone can write a licence
        // file, but only the issuer can sign one.
        let (_issuer, validator) = setup();
        let forger = Issuer::generate();
        let machine = machine("laptop");

        let forged = forger.issue(claims(Some(3650), Some(machine.clone())));
        assert_eq!(validator.validate(&forged, &machine), LicenseStatus::Invalid);
    }

    #[test]
    fn editing_the_claims_invalidates_the_signature() {
        let (issuer, validator) = setup();
        let machine = machine("laptop");
        let mut license = issuer.issue(claims(Some(30), Some(machine.clone())));

        // Extend the expiry by hand, as someone editing the file would.
        license.claims.expires_at = Some(
            (OffsetDateTime::now_utc() + Duration::days(3650))
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap(),
        );
        assert_eq!(validator.validate(&license, &machine), LicenseStatus::Invalid);

        // As would upgrading the plan.
        let mut license = issuer.issue(claims(Some(30), Some(machine.clone())));
        license.claims.plan = Plan::Perpetual;
        assert_eq!(validator.validate(&license, &machine), LicenseStatus::Invalid);
    }

    #[test]
    fn a_licence_for_another_machine_is_refused() {
        let (issuer, validator) = setup();
        let license = issuer.issue(claims(Some(30), Some(machine("their-laptop"))));

        assert_eq!(
            validator.validate(&license, &machine("my-laptop")),
            LicenseStatus::WrongMachine
        );
    }

    #[test]
    fn a_floating_licence_works_on_any_machine() {
        let (issuer, validator) = setup();
        let license = issuer.issue(claims(Some(30), None));

        assert!(validator.validate(&license, &machine("a")).is_usable());
        assert!(validator.validate(&license, &machine("b")).is_usable());
    }

    #[test]
    fn an_expired_licence_enters_grace_rather_than_locking_the_editor() {
        // The common cause is a card that failed, not fraud.
        let (issuer, validator) = setup();
        let machine = machine("laptop");
        let license = issuer.issue(claims(Some(-1), Some(machine.clone())));

        match validator.validate(&license, &machine) {
            LicenseStatus::InGrace { days_remaining, .. } => {
                assert!(days_remaining >= 12, "grace was {days_remaining} days");
            }
            other => panic!("expected InGrace, got {other:?}"),
        }
        assert!(
            validator.validate(&license, &machine).is_usable(),
            "the editor must keep working during grace"
        );
    }

    #[test]
    fn the_grace_message_explains_what_happened() {
        let (issuer, validator) = setup();
        let machine = machine("laptop");
        let license = issuer.issue(claims(Some(-1), Some(machine.clone())));

        let message = validator.validate(&license, &machine).message().unwrap();
        assert!(message.contains("could not be renewed"), "{message}");
        assert!(message.contains("keeps working"), "{message}");
    }

    #[test]
    fn a_licence_past_its_grace_period_expires() {
        let (issuer, validator) = setup();
        let machine = machine("laptop");
        let license = issuer.issue(claims(Some(-30), Some(machine.clone())));

        match validator.validate(&license, &machine) {
            LicenseStatus::Expired { .. } => {}
            other => panic!("expected Expired, got {other:?}"),
        }
        assert!(!validator.validate(&license, &machine).is_usable());
    }

    #[test]
    fn expiry_is_evaluated_against_a_supplied_clock() {
        let (issuer, validator) = setup();
        let machine = machine("laptop");
        let license = issuer.issue(claims(Some(10), Some(machine.clone())));
        let now = OffsetDateTime::now_utc();

        assert!(validator.validate_at(&license, &machine, now).is_usable());
        assert!(
            validator.validate_at(&license, &machine, now + Duration::days(15)).is_usable(),
            "day 15 is inside the 14-day grace after a day-10 expiry"
        );
        assert!(
            !validator.validate_at(&license, &machine, now + Duration::days(40)).is_usable(),
            "day 40 is past expiry plus grace"
        );
    }

    #[test]
    fn a_perpetual_licence_never_expires() {
        let (issuer, validator) = setup();
        let machine = machine("laptop");
        let mut c = claims(None, Some(machine.clone()));
        c.plan = Plan::Perpetual;
        let license = issuer.issue(c);

        let far_future = OffsetDateTime::now_utc() + Duration::days(365 * 50);
        match validator.validate_at(&license, &machine, far_future) {
            LicenseStatus::Valid { plan, days_remaining } => {
                assert_eq!(plan, Plan::Perpetual);
                assert_eq!(days_remaining, None);
            }
            other => panic!("expected Valid, got {other:?}"),
        }
    }

    #[test]
    fn a_licence_survives_a_hardware_change() {
        // Same reasoning as the fingerprint tests: a paid-for licence must not
        // break because the user upgraded their machine.
        let (issuer, validator) = setup();

        let mut original = machine("laptop");
        let license = issuer.issue(claims(Some(30), Some(original.clone())));

        // Replace one signal, as a disk swap or rename would.
        original.signals[0] = HardwareId::new(&original.signals[0].kind, b"changed");
        assert!(
            validator.validate(&license, &original).is_usable(),
            "one changed signal must not invalidate the licence"
        );
    }

    #[test]
    fn a_renewal_reminder_appears_only_near_expiry() {
        let (issuer, validator) = setup();
        let machine = machine("laptop");

        let far = issuer.issue(claims(Some(300), Some(machine.clone())));
        assert_eq!(validator.validate(&far, &machine).message(), None, "no nagging");

        let near = issuer.issue(claims(Some(3), Some(machine.clone())));
        let message = validator.validate(&near, &machine).message().unwrap();
        assert!(message.contains("renews in"), "{message}");
    }

    #[test]
    fn a_malformed_signature_is_invalid_rather_than_a_panic() {
        let (issuer, validator) = setup();
        let machine = machine("laptop");
        let mut license = issuer.issue(claims(Some(30), Some(machine.clone())));

        for bad in ["", "not base64!!!", "dG9vc2hvcnQ="] {
            license.signature = bad.to_string();
            assert_eq!(validator.validate(&license, &machine), LicenseStatus::Invalid);
        }
    }

    #[test]
    fn a_malformed_expiry_is_invalid() {
        let (issuer, validator) = setup();
        let machine = machine("laptop");
        let mut c = claims(Some(30), Some(machine.clone()));
        c.expires_at = Some("not a date".to_string());
        let license = issuer.issue(c);

        assert_eq!(validator.validate(&license, &machine), LicenseStatus::Invalid);
    }

    #[test]
    fn licences_round_trip_through_a_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("nebula.lic");
        let (issuer, validator) = setup();
        let machine = machine("laptop");

        let license = issuer.issue(claims(Some(30), Some(machine.clone())));
        std::fs::write(&path, serde_json::to_string_pretty(&license).unwrap()).unwrap();

        assert!(validator.validate_file(&path, &machine).unwrap().is_usable());
    }

    #[test]
    fn a_missing_licence_file_is_reported_as_missing() {
        let (_issuer, validator) = setup();
        let err = validator
            .validate_file("/definitely/not/a/licence.lic", &machine("laptop"))
            .unwrap_err();
        assert!(matches!(err, LicenseError::Missing));
    }

    #[test]
    fn the_signing_input_is_unambiguous() {
        // Without length prefixes, moving a character between adjacent fields
        // would produce the same signing input.
        let mut first = claims(None, None);
        first.id = "ab".to_string();
        first.licensee = "c".to_string();

        let mut second = claims(None, None);
        second.id = "a".to_string();
        second.licensee = "bc".to_string();
        second.issued_at = first.issued_at.clone();

        assert_ne!(first.signing_bytes(), second.signing_bytes());
    }

    #[test]
    fn fingerprint_signal_order_does_not_affect_the_signature() {
        let mut c = claims(Some(30), Some(machine("laptop")));
        let ordered = c.signing_bytes();

        if let Some(fingerprint) = &mut c.fingerprint {
            fingerprint.signals.reverse();
        }
        assert_eq!(ordered, c.signing_bytes());
    }
}
