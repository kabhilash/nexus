//! Operator-password master key. See DD-007 §4.2.
//!
//! `master_key = scrypt(password, salt, N, r, p) -> 32 bytes`
//!
//! Parameters follow RFC 7914's "interactive login" profile by
//! default. Deployments on slower embedded CPUs may override with
//! [`DerivedKeySource::with_params`].

use scrypt::{Params, scrypt};

use crate::secret::SecretString;

use super::{KeyError, MasterKeySource};

/// Scrypt parameters. Exposed so tests and performance-sensitive
/// integrators can pick tighter bounds than the interactive
/// default.
#[derive(Debug, Clone, Copy)]
pub struct ScryptParams {
    pub log_n: u8,
    pub r: u32,
    pub p: u32,
}

impl Default for ScryptParams {
    /// RFC 7914 interactive login profile: N=2^15, r=8, p=1.
    fn default() -> Self {
        Self {
            log_n: 15,
            r: 8,
            p: 1,
        }
    }
}

impl ScryptParams {
    /// Extra-fast parameters for tests. N=2^10 is ~32x weaker than
    /// the default but 32x faster; fine for correctness tests, not
    /// for production.
    pub const fn testing() -> Self {
        Self {
            log_n: 10,
            r: 8,
            p: 1,
        }
    }
}

/// Derive a master key from an operator-supplied password.
pub struct DerivedKeySource {
    password: SecretString,
    salt: Vec<u8>,
    params: ScryptParams,
}

impl DerivedKeySource {
    pub fn new(password: SecretString, salt: impl Into<Vec<u8>>) -> Self {
        Self {
            password,
            salt: salt.into(),
            params: ScryptParams::default(),
        }
    }

    pub fn with_params(mut self, params: ScryptParams) -> Self {
        self.params = params;
        self
    }
}

impl MasterKeySource for DerivedKeySource {
    fn master_key(&self) -> Result<[u8; 32], KeyError> {
        let params = Params::new(self.params.log_n, self.params.r, self.params.p, 32)
            .map_err(|e| KeyError::Scrypt(e.to_string()))?;
        let mut out = [0u8; 32];
        scrypt(
            self.password.expose_secret().as_bytes(),
            &self.salt,
            &params,
            &mut out,
        )
        .map_err(|e| KeyError::Scrypt(e.to_string()))?;
        Ok(out)
    }

    fn name(&self) -> &'static str {
        "derived"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derivation_is_deterministic_for_same_password_and_salt() {
        let pw = SecretString::from("correct horse battery staple");
        let src = DerivedKeySource::new(pw.clone(), *b"abcdef0123456789")
            .with_params(ScryptParams::testing());
        let a = src.master_key().unwrap();
        let b = src.master_key().unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn different_passwords_produce_different_keys() {
        let a = DerivedKeySource::new(SecretString::from("one"), *b"abcdef0123456789")
            .with_params(ScryptParams::testing())
            .master_key()
            .unwrap();
        let b = DerivedKeySource::new(SecretString::from("two"), *b"abcdef0123456789")
            .with_params(ScryptParams::testing())
            .master_key()
            .unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn different_salts_produce_different_keys() {
        let a = DerivedKeySource::new(SecretString::from("pw"), *b"aaaaaaaaaaaaaaaa")
            .with_params(ScryptParams::testing())
            .master_key()
            .unwrap();
        let b = DerivedKeySource::new(SecretString::from("pw"), *b"bbbbbbbbbbbbbbbb")
            .with_params(ScryptParams::testing())
            .master_key()
            .unwrap();
        assert_ne!(a, b);
    }
}
