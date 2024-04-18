use std::hash::{Hash, Hasher};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

// Hashed based digest deriving solution
// There's no well known solution for deriving digest methods for general
// structural data i.e. structs and enums (as far as I know), which means to
// compute digest for a structural data e.g. message type, one has to do either:
//   specify the traversal manually
//   derive `Hash` and make use of it
//   derive `Serialize` and make use of it
//   derive `BorshSerialize`, which is similar to `Serialize` but has been
//   claimed to be specially designed for this use case
// currently the second approach is take. the benefit is `Hash` semantic
// guarantees the desired reproducibility, and the main problem is the lack of
// cross-platform compatibility, which is hardly concerned in this codebase
// since it is written for benchmarks performed on unified systems and machines.
// nevertheless, I manually addressed the endianness problem below

pub trait DigestHasher {
    fn write(&mut self, bytes: &[u8]);
}

impl DigestHasher for Sha256 {
    fn write(&mut self, bytes: &[u8]) {
        self.update(bytes)
    }
}

impl DigestHasher for Vec<u8> {
    fn write(&mut self, bytes: &[u8]) {
        self.extend(bytes.iter().cloned())
    }
}

struct ImplHasher<'a, T>(&'a mut T);

impl<T: DigestHasher> Hasher for ImplHasher<'_, T> {
    fn write(&mut self, bytes: &[u8]) {
        self.0.write(bytes)
    }

    fn write_u16(&mut self, i: u16) {
        self.0.write(&i.to_le_bytes())
    }

    fn write_u32(&mut self, i: u32) {
        self.0.write(&i.to_le_bytes())
    }

    fn write_u64(&mut self, i: u64) {
        self.0.write(&i.to_le_bytes())
    }

    fn write_usize(&mut self, i: usize) {
        self.0.write(&i.to_le_bytes())
    }

    fn write_i16(&mut self, i: i16) {
        self.0.write(&i.to_le_bytes())
    }

    fn write_i32(&mut self, i: i32) {
        self.0.write(&i.to_le_bytes())
    }

    fn write_i64(&mut self, i: i64) {
        self.0.write(&i.to_le_bytes())
    }

    fn write_isize(&mut self, i: isize) {
        self.0.write(&i.to_le_bytes())
    }

    fn finish(&self) -> u64 {
        unimplemented!()
    }
}

pub trait DigestHash: Hash {
    fn hash(&self, state: &mut impl DigestHasher) {
        Hash::hash(self, &mut ImplHasher(state))
    }

    fn sha256(&self) -> H256 {
        let mut state = Sha256::new();
        DigestHash::hash(self, &mut state);
        H256(state.finalize().into())
    }
}
impl<T: Hash> DigestHash for T {}

pub use primitive_types::H256;

#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, derive_more::Deref,
)]
pub struct Verifiable<M, S = Signature> {
    #[deref]
    inner: M,
    signature: S,
}

impl<M, S> Verifiable<M, S> {
    pub fn into_inner(self) -> M {
        self.inner
    }
}

pub mod events {
    #[derive(Debug, Clone)]
    pub struct Signed<M, S = super::Signature>(pub super::Verifiable<M, S>);

    #[derive(Debug, Clone)]
    pub struct Verified<M, S = super::Signature>(pub super::Verifiable<M, S>);
}

// the cryptographic library must support seedable RNG based keypair generation
// to be used in this codebase
// it would be better if the library supports prehashed message as well, but a
// fallback `impl DigestHasher for Vec<u8>` is provided above anyway

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Signature {
    Plain(String), // for testing
    Secp256k1(secp256k1::ecdsa::Signature),
    Schnorrkel(peer::Signature),
}

#[derive(Debug, Clone)]
pub struct Crypto {
    provider: CryptoProvider,
    public_keys: Vec<PublicKey>,
}

#[derive(Debug, Clone)]
enum CryptoProvider {
    Insecure(String), // the "signature"
    Secp256k1(Secp256k1Crypto),
    Schnorrkel(Box<peer::Crypto>),
}

#[derive(Debug, Clone)]
struct Secp256k1Crypto {
    secret_key: secp256k1::SecretKey,
    secp: secp256k1::Secp256k1<secp256k1::All>,
}

#[derive(Debug, Clone)]
enum PublicKey {
    Plain(String),
    Secp256k1(secp256k1::PublicKey),
    Schnorrkel(peer::PublicKey),
}

#[derive(Debug, Clone, Copy)]
pub enum CryptoFlavor {
    Plain,
    Secp256k1,
    Schnorrkel,
}

impl Crypto {
    pub fn new_hardcoded(
        n: usize,
        index: impl Into<usize>,
        flavor: CryptoFlavor,
    ) -> anyhow::Result<Self> {
        let secret_keys = (0..n).map(|id| {
            let mut k = [0; 32];
            let k1 = format!("replica-{id}");
            k[..k1.as_bytes().len()].copy_from_slice(k1.as_bytes());
            k
        });
        let crypto = match flavor {
            CryptoFlavor::Plain => Self {
                public_keys: (0..n)
                    .map(|i| PublicKey::Plain(format!("replica-{i:03}")))
                    .collect(),
                provider: CryptoProvider::Insecure(format!("replica-{:03}", index.into())),
            },
            CryptoFlavor::Secp256k1 => {
                let secret_keys = secret_keys
                    .map(|k| secp256k1::SecretKey::from_slice(&k))
                    .collect::<Result<Vec<_>, _>>()?;
                let secp = secp256k1::Secp256k1::new();
                Self {
                    public_keys: secret_keys
                        .iter()
                        .map(|secret_key| PublicKey::Secp256k1(secret_key.public_key(&secp)))
                        .collect(),
                    provider: CryptoProvider::Secp256k1(Secp256k1Crypto {
                        secret_key: secret_keys[index.into()],
                        secp,
                    }),
                }
            }
            CryptoFlavor::Schnorrkel => {
                let mut secret_keys = secret_keys
                    .map(|k| {
                        Ok(schnorrkel::MiniSecretKey::from_bytes(&k)?
                            .expand_to_keypair(schnorrkel::ExpansionMode::Uniform))
                    })
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(anyhow::Error::msg::<schnorrkel::SignatureError>)?;
                Self {
                    public_keys: secret_keys
                        .iter()
                        .map(|keypair| PublicKey::Schnorrkel(keypair.public))
                        .collect(),
                    provider: CryptoProvider::Schnorrkel(Box::new(peer::Crypto {
                        keypair: secret_keys.remove(index.into()),
                        context: schnorrkel::signing_context(b"default"),
                    })),
                }
            }
        };
        Ok(crypto)
    }

    pub fn sign<M: DigestHash>(&self, message: M) -> Verifiable<M> {
        match &self.provider {
            CryptoProvider::Insecure(signature) => Verifiable {
                inner: message,
                signature: Signature::Plain(signature.clone()),
            },
            CryptoProvider::Secp256k1(crypto) => {
                let digest = secp256k1::Message::from_digest(message.sha256().into());
                Verifiable {
                    inner: message,
                    signature: Signature::Secp256k1(
                        crypto.secp.sign_ecdsa(&digest, &crypto.secret_key),
                    ),
                }
            }
            CryptoProvider::Schnorrkel(crypto) => {
                let signed = crypto.sign(message);
                // this feels monkey patch = =
                Verifiable {
                    inner: signed.inner,
                    signature: Signature::Schnorrkel(signed.signature),
                }
            }
        }
    }

    pub fn verify<M: DigestHash>(
        &self,
        index: impl Into<usize>,
        signed: &Verifiable<M>,
    ) -> anyhow::Result<()> {
        let Some(public_key) = self.public_keys.get(index.into()) else {
            anyhow::bail!("no identifier for index")
        };
        match (&self.provider, public_key, &signed.signature) {
            (
                CryptoProvider::Insecure(_),
                PublicKey::Plain(expected_signature),
                Signature::Plain(signature),
            ) => anyhow::ensure!(signature == expected_signature),

            (
                CryptoProvider::Secp256k1(crypto),
                PublicKey::Secp256k1(public_key),
                Signature::Secp256k1(signature),
            ) => {
                let digest = secp256k1::Message::from_digest(signed.inner.sha256().into());
                crypto.secp.verify_ecdsa(&digest, signature, public_key)?
            }
            // this feels even more monkey patch > <
            (
                CryptoProvider::Schnorrkel(crypto),
                PublicKey::Schnorrkel(public_key),
                Signature::Schnorrkel(signature),
            ) => crypto.verify_internal(public_key, &signed.inner, signature)?,
            _ => anyhow::bail!("unimplemented"),
        }
        Ok(())
    }

    pub fn verify_batched<I: Clone + Into<usize>, M: DigestHash>(
        &self,
        indexes: &[I],
        signed: &[Verifiable<M>],
    ) -> anyhow::Result<()> {
        let CryptoProvider::Schnorrkel(crypto) = &self.provider else {
            anyhow::bail!("unimplemented") // TODO fallback to verify one by one?
        };
        let mut transcripts = Vec::new();
        let mut signatures = Vec::new();
        let mut public_keys = Vec::new();
        for (index, verifiable) in indexes.iter().zip(signed) {
            let (
                PublicKey::Schnorrkel(public_key),
                Signature::Schnorrkel(peer::Signature(signature)),
            ) = (
                &self.public_keys[index.clone().into()],
                &verifiable.signature,
            )
            else {
                anyhow::bail!("unimplemented")
            };
            let mut state = Sha256::new();
            DigestHash::hash(&verifiable.inner, &mut state);
            transcripts.push(crypto.context.hash256(state));
            signatures.push(*signature);
            public_keys.push(*public_key)
        }
        schnorrkel::verify_batch(transcripts, &signatures, &public_keys, true)
            .map_err(anyhow::Error::msg)
    }
}

pub mod peer {
    use std::{fmt::Debug, hash::Hash};

    use rand::{CryptoRng, RngCore};
    use schnorrkel::{context::SigningContext, Keypair};
    use serde::{Deserialize, Serialize};
    use sha2::{Digest, Sha256};

    use super::DigestHash;

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Signature(pub schnorrkel::Signature);

    impl Ord for Signature {
        fn cmp(&self, other: &Self) -> std::cmp::Ordering {
            self.0.to_bytes().cmp(&other.0.to_bytes())
        }
    }

    impl PartialOrd for Signature {
        fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
            Some(self.cmp(other))
        }
    }

    impl Hash for Signature {
        fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
            Hash::hash(&self.0.to_bytes(), state)
        }
    }

    pub type Verifiable<M> = super::Verifiable<M, Signature>;

    pub type PublicKey = schnorrkel::PublicKey;

    pub mod events {
        #[derive(Debug, Clone)]
        pub struct Signed<M>(pub super::Verifiable<M>);

        #[derive(Debug, Clone)]
        pub struct Verified<M>(pub super::Verifiable<M>);
    }

    #[derive(Clone)]
    pub struct Crypto {
        pub keypair: Keypair,
        pub context: SigningContext,
    }

    impl Debug for Crypto {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Crypto")
                .field("keypair", &self.keypair)
                .finish_non_exhaustive()
        }
    }

    impl Crypto {
        pub fn new_random(rng: &mut (impl CryptoRng + RngCore)) -> Self {
            Self {
                keypair: Keypair::generate_with(rng),
                context: SigningContext::new(b"default"),
            }
        }

        pub fn public_key(&self) -> PublicKey {
            self.keypair.public
        }

        pub fn sign<M: DigestHash>(&self, message: M) -> Verifiable<M> {
            let mut state = Sha256::new();
            DigestHash::hash(&message, &mut state);
            let signature = self.keypair.sign(self.context.hash256(state));
            Verifiable {
                inner: message,
                signature: Signature(signature),
            }
        }

        pub fn verify<M: DigestHash>(
            &self,
            public_key: &PublicKey,
            signed: &Verifiable<M>,
        ) -> anyhow::Result<()> {
            self.verify_internal(public_key, &signed.inner, &signed.signature)
        }

        pub fn verify_internal<M: DigestHash>(
            &self,
            public_key: &PublicKey,
            message: &M,
            Signature(signature): &Signature,
        ) -> anyhow::Result<()> {
            let mut state = Sha256::new();
            DigestHash::hash(message, &mut state);
            public_key
                .verify(self.context.hash256(state), signature)
                .map_err(anyhow::Error::msg)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn struct_digest() {
        #[derive(Hash)]
        struct Foo {
            a: u32,
            bs: Vec<u8>,
        }
        let foo = Foo {
            a: 42,
            bs: b"hello".to_vec(),
        };
        assert_ne!(foo.sha256(), Default::default());
    }

    #[test]
    fn verify_batched() -> anyhow::Result<()> {
        let message = "hello";
        let crypto = (0..4usize)
            .map(|i| Crypto::new_hardcoded(4, i, CryptoFlavor::Schnorrkel))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let verifiable = crypto
            .iter()
            .map(|crypto| crypto.sign(message))
            .collect::<Vec<_>>();
        crypto[0].verify_batched(&[0usize, 1, 2, 3], &verifiable)
    }
}

// cSpell:words hasher Borsh endianness seedable keypair prehashed secp256k1
// cSpell:words schnorrkel secp
