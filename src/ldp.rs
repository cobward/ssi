use std::collections::HashMap as Map;
use std::convert::TryFrom;
use std::str::FromStr;

use async_trait::async_trait;
use chrono::prelude::*;

const EDPK_PREFIX: [u8; 4] = [13, 15, 37, 217];
const EDSIG_PREFIX: [u8; 5] = [9, 245, 205, 134, 18];
const SPSIG_PREFIX: [u8; 5] = [13, 115, 101, 19, 63];
const P2SIG_PREFIX: [u8; 4] = [54, 240, 44, 52];

// use crate::did::{VerificationMethod, VerificationMethodMap};
use crate::caip10::BlockchainAccountId;
use crate::did::{Resource, VerificationMethodMap};
use crate::did_resolve::{dereference, Content, DIDResolver, DereferencingInputMetadata};
#[cfg(feature = "keccak-hash")]
use crate::eip712::TypedData;
use crate::error::Error;
use crate::hash::sha256;
use crate::jsonld::{json_to_dataset, StaticLoader};
use crate::jwk::Base64urlUInt;
use crate::jwk::{Algorithm, OctetParams as JWKOctetParams, Params as JWKParams, JWK};
use crate::jws::Header;
use crate::rdf::DataSet;
use crate::urdna2015;
use serde::{Deserialize, Serialize};
use serde_json::Value;

// TODO: factor out proof types
lazy_static! {
    /// JSON-LD context for Linked Data Proofs based on Tezos addresses
    pub static ref TZ_CONTEXT: Value = {
        let context_str = ssi_contexts::TZ_V2;
        serde_json::from_str(&context_str).unwrap()
    };
    pub static ref TZVM_CONTEXT: Value = {
        let context_str = ssi_contexts::TZVM_V1;
        serde_json::from_str(&context_str).unwrap()
    };
    pub static ref EIP712VM_CONTEXT: Value = {
        let context_str = ssi_contexts::EIP712VM;
        serde_json::from_str(&context_str).unwrap()
    };
    pub static ref SOLVM_CONTEXT: Value = {
        let context_str = ssi_contexts::SOLVM;
        serde_json::from_str(&context_str).unwrap()
    };
}

// Get current time to millisecond precision if possible
pub fn now_ms() -> DateTime<Utc> {
    let datetime = Utc::now();
    let ms = datetime.timestamp_subsec_millis();
    let ns = ms * 1_000_000;
    datetime.with_nanosecond(ns).unwrap_or(datetime)
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait LinkedDataDocument {
    fn get_contexts(&self) -> Result<Option<String>, Error>;
    async fn to_dataset_for_signing(
        &self,
        parent: Option<&(dyn LinkedDataDocument + Sync)>,
    ) -> Result<DataSet, Error>;
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait ProofSuite {
    async fn sign<T, P>(
        &self,
        document: &(dyn LinkedDataDocument + Sync),
        options: &LinkedDataProofOptions<T, P>,
        key: &JWK,
    ) -> Result<Proof<T, P>, Error>
    where
        T: Serialize + Send + Sync + Clone,
        P: Serialize + Send + Sync + Clone;

    async fn prepare<T, P>(
        &self,
        document: &(dyn LinkedDataDocument + Sync),
        options: &LinkedDataProofOptions<T, P>,
        public_key: &JWK,
    ) -> Result<ProofPreparation<T, P>, Error>
    where
        T: Serialize + Send + Sync + Clone,
        P: Serialize + Send + Sync + Clone;

    async fn complete<T, P>(
        &self,
        preparation: ProofPreparation<T, P>,
        signature: &str,
    ) -> Result<Proof<T, P>, Error>
    where
        T: Serialize + Send + Sync + Clone,
        P: Serialize + Send + Sync + Clone;

    async fn verify<T, P>(
        &self,
        proof: &Proof<T, P>,
        document: &(dyn LinkedDataDocument + Sync),
        resolver: &dyn DIDResolver,
    ) -> Result<(), Error>
    where
        Self: Sized,
        T: Serialize + Send + Sync + Clone,
        P: Serialize + Send + Sync + Clone,
    {
        verify(proof, document, resolver).await
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct Proof<T = Map<String, Value>, P = ProofPurpose> {
    #[serde(rename = "@context")]
    // TODO: use consistent types for context
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub context: Value,
    #[serde(rename = "type")]
    pub type_: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proof_purpose: Option<P>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proof_value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub challenge: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub creator: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    // Note: ld-proofs specifies verificationMethod as a "set of parameters",
    // but all examples use a single string.
    pub verification_method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created: Option<DateTime<Utc>>, // ISO 8601
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nonce: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jws: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_key_jwk: Option<JWK>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(flatten)]
    pub property_set: Option<T>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(try_from = "String")]
// #[serde(untagged)]
#[serde(rename_all = "camelCase")]
pub enum ProofPurpose {
    AssertionMethod,
    Authentication,
    KeyAgreement,
    ContractAgreement,
    CapabilityDelegation,
    CapabilityInvocation,
}

impl Default for ProofPurpose {
    fn default() -> Self {
        Self::AssertionMethod
    }
}

macro_rules! assert_local {
    ($cond:expr) => {
        if !$cond {
            return false;
        }
    };
}

impl<T, P> Proof<T, P>
where
    T: Serialize + Send + Sync + Default + Clone,
    P: Serialize + Send + Sync + Default + PartialEq + Clone,
{
    pub fn new(type_: &str) -> Self {
        Self {
            type_: type_.to_string(),
            ..Default::default()
        }
    }

    pub fn matches(
        &self,
        options: &LinkedDataProofOptions<T, P>,
        allowed_vms: &Vec<String>,
    ) -> bool {
        if let Some(ref verification_method) = options.verification_method {
            assert_local!(self.verification_method.as_ref() == Some(verification_method));
        }
        if let Some(vm) = self.verification_method.as_ref() {
            assert_local!(allowed_vms.contains(vm));
        }
        if let Some(created) = self.created {
            assert_local!(options.created.unwrap_or_else(now_ms) >= created);
        } else {
            return false;
        }
        if let Some(ref challenge) = options.challenge {
            assert_local!(self.challenge.as_ref() == Some(challenge));
        }
        if let Some(ref domain) = options.domain {
            assert_local!(self.domain.as_ref() == Some(domain));
        }
        if let Some(ref proof_purpose) = options.proof_purpose {
            assert_local!(self.proof_purpose.as_ref() == Some(proof_purpose));
        }
        true
    }

    pub async fn verify(
        &self,
        document: &(dyn LinkedDataDocument + Sync),
        resolver: &dyn DIDResolver,
    ) -> VerificationResult {
        LinkedDataProofs::verify(self, document, resolver)
            .await
            .into()
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(try_from = "String")]
#[serde(rename_all = "camelCase")]
pub enum Check {
    Proof,
}

impl FromStr for Check {
    type Err = Error;
    fn from_str(purpose: &str) -> Result<Self, Self::Err> {
        match purpose {
            "proof" => Ok(Self::Proof),
            _ => Err(Error::UnsupportedCheck),
        }
    }
}

impl TryFrom<String> for Check {
    type Error = Error;
    fn try_from(purpose: String) -> Result<Self, Self::Error> {
        Self::from_str(&purpose)
    }
}

impl From<Check> for String {
    fn from(purpose: Check) -> String {
        match purpose {
            Check::Proof => "proof".to_string(),
        }
    }
}

// https://w3c-ccg.github.io/vc-http-api/#/Verifier/verifyCredential
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
/// Object summarizing a verification
/// Reference: vc-http-api
pub struct VerificationResult {
    /// The checks performed
    pub checks: Vec<Check>,
    /// Warnings
    pub warnings: Vec<String>,
    /// Errors
    pub errors: Vec<String>,
}

impl VerificationResult {
    pub fn new() -> Self {
        Self {
            checks: vec![],
            warnings: vec![],
            errors: vec![],
        }
    }

    pub fn error(err: &str) -> Self {
        Self {
            checks: vec![],
            warnings: vec![],
            errors: vec![err.to_string()],
        }
    }

    pub fn append(&mut self, other: &mut Self) {
        self.checks.append(&mut other.checks);
        self.warnings.append(&mut other.warnings);
        self.errors.append(&mut other.errors);
    }
}

impl From<Result<(), Error>> for VerificationResult {
    fn from(res: Result<(), Error>) -> Self {
        Self {
            checks: vec![],
            warnings: vec![],
            errors: match res {
                Ok(_) => vec![],
                Err(error) => vec![error.to_string()],
            },
        }
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl<T, P> LinkedDataDocument for Proof<T, P>
where
    T: Serialize + Send + Sync + Clone,
    P: Serialize + Send + Sync + Clone,
{
    fn get_contexts(&self) -> Result<Option<String>, Error> {
        Ok(None)
    }

    async fn to_dataset_for_signing(
        &self,
        parent: Option<&(dyn LinkedDataDocument + Sync)>,
    ) -> Result<DataSet, Error> {
        let mut copy = self.clone();
        copy.jws = None;
        copy.proof_value = None;
        let json = serde_json::to_string(&copy)?;
        let more_contexts = match parent {
            Some(parent) => parent.get_contexts()?,
            None => None,
        };
        let mut loader = StaticLoader;
        json_to_dataset(&json, more_contexts.as_ref(), false, None, &mut loader).await
    }
}

impl FromStr for ProofPurpose {
    type Err = Error;
    fn from_str(purpose: &str) -> Result<Self, Self::Err> {
        match purpose {
            "authentication" => Ok(Self::Authentication),
            "assertionMethod" => Ok(Self::AssertionMethod),
            "keyAgreement" => Ok(Self::KeyAgreement),
            "contractAgreement" => Ok(Self::ContractAgreement),
            "capabilityDelegation" => Ok(Self::CapabilityDelegation),
            "capabilityInvocation" => Ok(Self::CapabilityInvocation),
            _ => Err(Error::UnsupportedProofPurpose),
        }
    }
}

impl TryFrom<String> for ProofPurpose {
    type Error = Error;
    fn try_from(purpose: String) -> Result<Self, Self::Error> {
        Self::from_str(&purpose)
    }
}

impl From<ProofPurpose> for String {
    fn from(purpose: ProofPurpose) -> String {
        match purpose {
            ProofPurpose::Authentication => "authentication".to_string(),
            ProofPurpose::AssertionMethod => "assertionMethod".to_string(),
            ProofPurpose::KeyAgreement => "keyAgreement".to_string(),
            ProofPurpose::ContractAgreement => "contractAgreement".to_string(),
            ProofPurpose::CapabilityDelegation => "capabilityDelegation".to_string(),
            ProofPurpose::CapabilityInvocation => "capabilityInvocation".to_string(),
        }
    }
}

// https://w3c-ccg.github.io/vc-http-api/#/Verifier/verifyCredential
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
/// Options for specifying how the LinkedDataProof is created.
/// Reference: vc-http-api
pub struct LinkedDataProofOptions<T = Map<String, Value>, P = ProofPurpose> {
    #[serde(skip_serializing_if = "Option::is_none")]
    /// The URI of the verificationMethod used for the proof. If omitted a default
    /// assertionMethod will be used.
    pub verification_method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// The purpose of the proof. If omitted "assertionMethod" will be used.
    pub proof_purpose: Option<P>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// The date of the proof. If omitted system time will be used.
    pub created: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// The challenge of the proof.
    pub challenge: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// The domain of the proof.
    pub domain: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Checks to perform
    pub checks: Option<Vec<Check>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Arbitrary extra values to pass in to the proof
    pub property_set: Option<T>,
}

impl<T, P> Default for LinkedDataProofOptions<T, P>
where
    P: Default,
{
    fn default() -> Self {
        Self {
            verification_method: None,
            proof_purpose: Some(P::default()),
            created: Some(now_ms()),
            challenge: None,
            domain: None,
            property_set: None,
            checks: Some(vec![Check::Proof]),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ProofPreparation<T = Map<String, Value>, P = ProofPurpose> {
    pub proof: Proof<T, P>,
    pub jws_header: Option<Header>,
    pub signing_input: SigningInput,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(untagged)]
#[non_exhaustive]
pub enum SigningInput {
    Bytes(Base64urlUInt),
    #[cfg(feature = "keccak-hash")]
    TypedData(TypedData),
    Micheline {
        micheline: String,
    },
}

impl<T, P> ProofPreparation<T, P>
where
    T: Serialize + Send + Sync + Clone,
    P: Serialize + Send + Sync + Clone,
{
    pub async fn complete(self, signature: &str) -> Result<Proof<T, P>, Error> {
        match self.proof.type_.as_str() {
            "RsaSignature2018" => RsaSignature2018.complete(self, signature).await,
            "Ed25519Signature2018" => Ed25519Signature2018.complete(self, signature).await,
            "Ed25519BLAKE2BDigestSize20Base58CheckEncodedSignature2021" => {
                Ed25519BLAKE2BDigestSize20Base58CheckEncodedSignature2021
                    .complete(self, signature)
                    .await
            }
            "P256BLAKE2BDigestSize20Base58CheckEncodedSignature2021" => {
                P256BLAKE2BDigestSize20Base58CheckEncodedSignature2021
                    .complete(self, signature)
                    .await
            }
            "EcdsaSecp256k1Signature2019" => {
                EcdsaSecp256k1Signature2019.complete(self, signature).await
            }
            "EcdsaSecp256k1RecoverySignature2020" => {
                EcdsaSecp256k1RecoverySignature2020
                    .complete(self, signature)
                    .await
            }
            #[cfg(feature = "keccak-hash")]
            "Eip712Signature2021" => Eip712Signature2021.complete(self, signature).await,
            "TezosSignature2021" => TezosSignature2021.complete(self, signature).await,
            "SolanaSignature2021" => SolanaSignature2021.complete(self, signature).await,
            "JsonWebSignature2020" => JsonWebSignature2020.complete(self, signature).await,
            _ => Err(Error::ProofTypeNotImplemented),
        }
    }
}

pub struct LinkedDataProofs;
impl LinkedDataProofs {
    // https://w3c-ccg.github.io/ld-proofs/#proof-algorithm
    pub async fn sign<T, P>(
        document: &(dyn LinkedDataDocument + Sync),
        options: &LinkedDataProofOptions<T, P>,
        key: &JWK,
    ) -> Result<Proof<T, P>, Error>
    where
        T: Serialize + Send + Sync + Clone,
        P: Serialize + Send + Sync + Clone,
    {
        // TODO: select proof type by resolving DID instead of matching on the key.
        match key {
            JWK {
                params: JWKParams::RSA(_),
                public_key_use: _,
                key_operations: _,
                algorithm: _,
                key_id: _,
                x509_url: _,
                x509_certificate_chain: _,
                x509_thumbprint_sha1: _,
                x509_thumbprint_sha256: _,
            } => return RsaSignature2018.sign(document, options, &key).await,
            JWK {
                params:
                    JWKParams::OKP(JWKOctetParams {
                        curve,
                        public_key: _,
                        private_key: _,
                    }),
                public_key_use: _,
                key_operations: _,
                algorithm: _,
                key_id: _,
                x509_url: _,
                x509_certificate_chain: _,
                x509_thumbprint_sha1: _,
                x509_thumbprint_sha256: _,
            } => match &curve[..] {
                "Ed25519" => {
                    if let Some(ref vm) = options.verification_method {
                        if vm.ends_with("#TezosMethod2021") {
                            return TezosSignature2021.sign(document, options, &key).await;
                        }
                        if vm.starts_with("did:tz:") || vm.starts_with("did:pkh:tz:") {
                            return Ed25519BLAKE2BDigestSize20Base58CheckEncodedSignature2021
                                .sign(document, options, &key)
                                .await;
                        }
                        if vm.ends_with("#SolanaMethod2021") {
                            return SolanaSignature2021.sign(document, options, &key).await;
                        }
                    }
                    return Ed25519Signature2018.sign(document, options, &key).await;
                }
                _ => {
                    return Err(Error::ProofTypeNotImplemented);
                }
            },
            JWK {
                params: JWKParams::EC(ec_params),
                public_key_use: _,
                key_operations: _,
                algorithm,
                key_id: _,
                x509_url: _,
                x509_certificate_chain: _,
                x509_thumbprint_sha1: _,
                x509_thumbprint_sha256: _,
            } => {
                let curve = ec_params.curve.as_ref().ok_or(Error::MissingCurve)?;
                match &curve[..] {
                    "secp256k1" => {
                        if algorithm.as_ref() == Some(&Algorithm::ES256KR) {
                            if let Some(ref vm) = options.verification_method {
                                if vm.ends_with("#Eip712Method2021") {
                                    #[cfg(feature = "keccak-hash")]
                                    return Eip712Signature2021.sign(document, options, &key).await;
                                    #[cfg(not(feature = "keccak-hash"))]
                                    return Err(Error::ProofTypeNotImplemented);
                                }
                            }
                            return EcdsaSecp256k1RecoverySignature2020
                                .sign(document, options, &key)
                                .await;
                        } else {
                            if let Some(ref vm) = options.verification_method {
                                if vm.ends_with("#TezosMethod2021") {
                                    return TezosSignature2021.sign(document, options, &key).await;
                                }
                            }
                            return EcdsaSecp256k1Signature2019
                                .sign(document, options, &key)
                                .await;
                        }
                    }
                    "P-256" => {
                        if let Some(ref vm) = options.verification_method {
                            if vm.ends_with("#TezosMethod2021") {
                                return TezosSignature2021.sign(document, options, &key).await;
                            }
                            if vm.starts_with("did:tz:") || vm.starts_with("did:pkh:") {
                                return P256BLAKE2BDigestSize20Base58CheckEncodedSignature2021
                                    .sign(document, options, &key)
                                    .await;
                            }
                        }
                        return EcdsaSecp256r1Signature2019
                            .sign(document, options, &key)
                            .await;
                    }
                    _ => {
                        return Err(Error::CurveNotImplemented(curve.to_string()));
                    }
                }
            }
            _ => {}
        };
        Err(Error::ProofTypeNotImplemented)
    }

    /// Prepare to create a linked data proof. Given a linked data document, proof options, and JWS
    /// algorithm, calculate the signing input bytes. Returns a [`ProofPreparation`] - the data for the caller to sign, along with data to reconstruct the proof.
    pub async fn prepare<T, P>(
        document: &(dyn LinkedDataDocument + Sync),
        options: &LinkedDataProofOptions<T, P>,
        public_key: &JWK,
    ) -> Result<ProofPreparation<T, P>, Error>
    where
        T: Serialize + Send + Sync + Clone,
        P: Serialize + Send + Sync + Clone,
    {
        match public_key.get_algorithm().ok_or(Error::MissingAlgorithm)? {
            Algorithm::RS256 => {
                return RsaSignature2018
                    .prepare(document, options, public_key)
                    .await
            }
            Algorithm::EdDSA => {
                if let Some(ref vm) = options.verification_method {
                    if vm.ends_with("#SolanaMethod2021") {
                        return SolanaSignature2021
                            .prepare(document, options, public_key)
                            .await;
                    }
                }
                return Ed25519Signature2018
                    .prepare(document, options, public_key)
                    .await;
            }
            Algorithm::EdBlake2b => {
                if let Some(ref vm) = options.verification_method {
                    if vm.ends_with("#TezosMethod2021") {
                        return TezosSignature2021
                            .prepare(document, options, public_key)
                            .await;
                    }
                    if vm.starts_with("did:tz:") || vm.starts_with("did:pkh:tz:") {
                        return Ed25519BLAKE2BDigestSize20Base58CheckEncodedSignature2021
                            .prepare(document, options, public_key)
                            .await;
                    }
                }
            }
            Algorithm::ES256 => {
                return EcdsaSecp256r1Signature2019
                    .prepare(document, options, public_key)
                    .await;
            }
            Algorithm::ESBlake2b => {
                if let Some(ref vm) = options.verification_method {
                    if vm.ends_with("#TezosMethod2021") {
                        return TezosSignature2021
                            .prepare(document, options, public_key)
                            .await;
                    }
                    if vm.starts_with("did:tz:") || vm.starts_with("did:pkh:tz:") {
                        return P256BLAKE2BDigestSize20Base58CheckEncodedSignature2021
                            .prepare(document, options, public_key)
                            .await;
                    }
                }
            }
            Algorithm::ES256K => {
                return EcdsaSecp256k1Signature2019
                    .prepare(document, options, public_key)
                    .await;
            }
            Algorithm::ESBlake2bK => {
                if let Some(ref vm) = options.verification_method {
                    if vm.ends_with("#TezosMethod2021") {
                        return TezosSignature2021
                            .prepare(document, options, public_key)
                            .await;
                    }
                    // TODO: resolve the VM to get type, rather than comparing the string directly
                    if (vm.starts_with("did:tz:") || vm.starts_with("did:pkh:tz:"))
                        && vm.ends_with("#TezosMethod2021")
                    {
                        return TezosSignature2021
                            .prepare(document, options, public_key)
                            .await;
                    }
                }
            }
            Algorithm::ES256KR => {
                if let Some(ref vm) = options.verification_method {
                    if vm.ends_with("#Eip712Method2021") {
                        #[cfg(feature = "keccak-hash")]
                        return Eip712Signature2021
                            .prepare(document, options, public_key)
                            .await;
                        #[cfg(not(feature = "keccak-hash"))]
                        return Err(Error::ProofTypeNotImplemented);
                    }
                }
                return EcdsaSecp256k1RecoverySignature2020
                    .prepare(document, options, public_key)
                    .await;
            }
            _ => {}
        };
        Err(Error::ProofTypeNotImplemented)
    }

    // https://w3c-ccg.github.io/ld-proofs/#proof-verification-algorithm
    pub async fn verify<T, P>(
        proof: &Proof<T, P>,
        document: &(dyn LinkedDataDocument + Sync),
        resolver: &dyn DIDResolver,
    ) -> Result<(), Error>
    where
        T: Serialize + Send + Sync + Clone,
        P: Serialize + Send + Sync + Clone,
    {
        match proof.type_.as_str() {
            "RsaSignature2018" => RsaSignature2018.verify(proof, document, resolver).await,
            "Ed25519Signature2018" => Ed25519Signature2018.verify(proof, document, resolver).await,
            "Ed25519BLAKE2BDigestSize20Base58CheckEncodedSignature2021" => {
                Ed25519BLAKE2BDigestSize20Base58CheckEncodedSignature2021
                    .verify(proof, document, resolver)
                    .await
            }
            "P256BLAKE2BDigestSize20Base58CheckEncodedSignature2021" => {
                P256BLAKE2BDigestSize20Base58CheckEncodedSignature2021
                    .verify(proof, document, resolver)
                    .await
            }
            "EcdsaSecp256k1Signature2019" => {
                EcdsaSecp256k1Signature2019
                    .verify(proof, document, resolver)
                    .await
            }
            "EcdsaSecp256k1RecoverySignature2020" => {
                EcdsaSecp256k1RecoverySignature2020
                    .verify(proof, document, resolver)
                    .await
            }
            #[cfg(feature = "keccak-hash")]
            "Eip712Signature2021" => Eip712Signature2021.verify(proof, document, resolver).await,
            "TezosSignature2021" => TezosSignature2021.verify(proof, document, resolver).await,
            "SolanaSignature2021" => SolanaSignature2021.verify(proof, document, resolver).await,
            "JsonWebSignature2020" => JsonWebSignature2020.verify(proof, document, resolver).await,
            "EcdsaSecp256r1Signature2019" => {
                EcdsaSecp256r1Signature2019
                    .verify(proof, document, resolver)
                    .await
            }
            _ => Err(Error::ProofTypeNotImplemented),
        }
    }
}

/// Resolve a verificationMethod to a key
pub async fn resolve_key(
    verification_method: &str,
    resolver: &dyn DIDResolver,
) -> Result<JWK, Error> {
    let vm = resolve_vm(verification_method, resolver).await?;
    if let Some(pk_jwk) = vm.public_key_jwk {
        if vm.public_key_base58.is_some() {
            // https://w3c.github.io/did-core/#verification-material
            // "expressing key material in a verification method using both publicKeyJwk and
            // publicKeyBase58 at the same time is prohibited."
            return Err(Error::MultipleKeyMaterial);
        }
        return Ok(pk_jwk);
    }
    if let Some(pk_bs58) = vm.public_key_base58 {
        return jwk_from_public_key_base58(&pk_bs58, &vm.type_);
    }
    Err(Error::MissingKey)
}

fn jwk_from_public_key_base58(pk_bs58: &str, vm_type: &str) -> Result<JWK, Error> {
    let pk_bytes = bs58::decode(&pk_bs58).into_vec()?;
    let params = match vm_type {
        "Ed25519VerificationKey2018" => JWKParams::OKP(JWKOctetParams {
            curve: "Ed25519".to_string(),
            public_key: Base64urlUInt(pk_bytes),
            private_key: None,
        }),
        _ => return Err(Error::UnsupportedKeyType),
    };
    let jwk = JWK {
        params,
        public_key_use: None,
        key_operations: None,
        algorithm: None,
        key_id: None,
        x509_url: None,
        x509_certificate_chain: None,
        x509_thumbprint_sha1: None,
        x509_thumbprint_sha256: None,
    };
    Ok(jwk)
}

/// Resolve a verificationMethod
pub async fn resolve_vm(
    verification_method: &str,
    resolver: &dyn DIDResolver,
) -> Result<VerificationMethodMap, Error> {
    let (res_meta, object, _meta) = dereference(
        resolver,
        &verification_method,
        &DereferencingInputMetadata::default(),
    )
    .await;
    if let Some(error) = res_meta.error {
        return Err(Error::DIDURLDereference(error));
    }
    let vm = match object {
        Content::Object(Resource::VerificationMethod(vm)) => vm,
        Content::Null => return Err(Error::ResourceNotFound(verification_method.to_string())),
        _ => return Err(Error::ExpectedObject),
    };
    Ok(vm)
}

async fn to_jws_payload<T, P>(
    document: &(dyn LinkedDataDocument + Sync),
    proof: &Proof<T, P>,
) -> Result<Vec<u8>, Error>
where
    T: Serialize + Send + Sync + Clone,
    P: Serialize + Send + Sync + Clone,
{
    let doc_dataset = document.to_dataset_for_signing(None).await?;
    let doc_dataset_normalized = urdna2015::normalize(&doc_dataset)?;
    let doc_normalized = doc_dataset_normalized.to_nquads()?;
    let sigopts_dataset = proof.to_dataset_for_signing(Some(document)).await?;
    let sigopts_dataset_normalized = urdna2015::normalize(&sigopts_dataset)?;
    let sigopts_normalized = sigopts_dataset_normalized.to_nquads()?;
    let sigopts_digest = sha256(sigopts_normalized.as_bytes())?;
    let doc_digest = sha256(doc_normalized.as_bytes())?;
    let data = [
        sigopts_digest.as_ref().to_vec(),
        doc_digest.as_ref().to_vec(),
    ]
    .concat();
    Ok(data)
}

async fn sign<T, P>(
    document: &(dyn LinkedDataDocument + Sync),
    options: &LinkedDataProofOptions<T, P>,
    key: &JWK,
    type_: &str,
    algorithm: Algorithm,
) -> Result<Proof<T, P>, Error>
where
    T: Serialize + Send + Sync + Clone + Default,
    P: Serialize + Send + Sync + Clone + Default + PartialEq,
{
    if let Some(key_algorithm) = key.algorithm {
        if key_algorithm != algorithm {
            return Err(Error::AlgorithmMismatch);
        }
    }
    let proof = Proof {
        proof_purpose: options.proof_purpose.clone(),
        verification_method: options.verification_method.clone(),
        created: Some(options.created.unwrap_or_else(now_ms)),
        domain: options.domain.clone(),
        challenge: options.challenge.clone(),
        property_set: options.property_set.clone(),
        ..Proof::new(type_)
    };
    sign_proof(document, proof, key, algorithm).await
}

async fn sign_proof<T, P>(
    document: &(dyn LinkedDataDocument + Sync),
    mut proof: Proof<T, P>,
    key: &JWK,
    algorithm: Algorithm,
) -> Result<Proof<T, P>, Error>
where
    T: Serialize + Send + Sync + Clone,
    P: Serialize + Send + Sync + Clone,
{
    let message = to_jws_payload(document, &proof).await?;
    let jws = crate::jws::detached_sign_unencoded_payload(algorithm, &message, &key)?;
    proof.jws = Some(jws);
    Ok(proof)
}

async fn prepare<T, P>(
    document: &(dyn LinkedDataDocument + Sync),
    options: &LinkedDataProofOptions<T, P>,
    public_key: &JWK,
    type_: &str,
    algorithm: Algorithm,
) -> Result<ProofPreparation<T, P>, Error>
where
    T: Clone + Default + Send + Serialize + Sync,
    P: Clone + Default + Send + PartialEq + Serialize + Sync,
{
    if let Some(key_algorithm) = public_key.algorithm {
        if key_algorithm != algorithm {
            return Err(Error::AlgorithmMismatch);
        }
    }
    let proof = Proof::<T, P> {
        proof_purpose: options.proof_purpose.clone(),
        verification_method: options.verification_method.clone(),
        created: Some(options.created.unwrap_or_else(now_ms)),
        domain: options.domain.clone(),
        challenge: options.challenge.clone(),
        ..Proof::new(type_)
    };
    prepare_proof(document, proof, algorithm).await
}

async fn prepare_proof<T, P>(
    document: &(dyn LinkedDataDocument + Sync),
    proof: Proof<T, P>,
    algorithm: Algorithm,
) -> Result<ProofPreparation<T, P>, Error>
where
    T: Clone + Default + Send + Serialize + Sync,
    P: Clone + Default + Send + PartialEq + Serialize + Sync,
{
    let message = to_jws_payload(document, &proof).await?;
    let (jws_header, signing_input) =
        crate::jws::prepare_detached_unencoded_payload(algorithm, &message)?;
    Ok(ProofPreparation {
        proof,
        jws_header: Some(jws_header),
        signing_input: SigningInput::Bytes(Base64urlUInt(signing_input)),
    })
}

async fn complete<T, P>(
    preparation: ProofPreparation<T, P>,
    signature: &str,
) -> Result<Proof<T, P>, Error>
where
    T: Clone + Default + Send + Serialize + Sync,
    P: Clone + Default + Send + Serialize + Sync,
{
    complete_proof(preparation, signature).await
}

async fn complete_proof<T, P>(
    preparation: ProofPreparation<T, P>,
    signature: &str,
) -> Result<Proof<T, P>, Error>
where
    T: Clone + Default + Send + Serialize + Sync,
    P: Clone + Default + Send + Serialize + Sync,
{
    let mut proof = preparation.proof;
    let jws_header = preparation.jws_header.ok_or(Error::MissingJWSHeader)?;
    let jws = crate::jws::complete_sign_unencoded_payload(jws_header, signature)?;
    proof.jws = Some(jws);
    Ok(proof)
}

async fn verify<T, P>(
    proof: &Proof<T, P>,
    document: &(dyn LinkedDataDocument + Sync),
    resolver: &dyn DIDResolver,
) -> Result<(), Error>
where
    T: Serialize + Send + Sync + Clone,
    P: Serialize + Send + Sync + Clone,
{
    let jws = proof.jws.as_ref().ok_or(Error::MissingProofSignature)?;
    let verification_method = proof
        .verification_method
        .as_ref()
        .ok_or(Error::MissingVerificationMethod)?;
    let key = resolve_key(&verification_method, resolver).await?;
    let message = to_jws_payload(document, proof).await?;
    crate::jws::detached_verify(&jws, &message, &key)?;
    Ok(())
}

pub struct RsaSignature2018;
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl ProofSuite for RsaSignature2018 {
    async fn sign<T, P>(
        &self,
        document: &(dyn LinkedDataDocument + Sync),
        options: &LinkedDataProofOptions<T, P>,
        key: &JWK,
    ) -> Result<Proof<T, P>, Error>
    where
        T: Serialize + Send + Sync + Clone,
        P: Serialize + Send + Sync + Clone,
    {
        sign(document, options, key, "RsaSignature2018", Algorithm::RS256).await
    }
    async fn prepare<T, P>(
        &self,
        document: &(dyn LinkedDataDocument + Sync),
        options: &LinkedDataProofOptions<T, P>,
        public_key: &JWK,
    ) -> Result<ProofPreparation<T, P>, Error>
    where
        T: Serialize + Send + Sync + Clone,
        P: Serialize + Send + Sync + Clone,
    {
        prepare(
            document,
            options,
            public_key,
            "RsaSignature2018",
            Algorithm::RS256,
        )
        .await
    }
    async fn complete<T, P>(
        &self,
        preparation: ProofPreparation<T, P>,
        signature: &str,
    ) -> Result<Proof<T, P>, Error>
    where
        T: Serialize + Send + Sync + Clone,
        P: Serialize + Send + Sync + Clone,
    {
        complete(preparation, signature).await
    }
}

pub struct Ed25519Signature2018;
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl ProofSuite for Ed25519Signature2018 {
    async fn sign<T, P>(
        &self,
        document: &(dyn LinkedDataDocument + Sync),
        options: &LinkedDataProofOptions<T, P>,
        key: &JWK,
    ) -> Result<Proof<T, P>, Error>
    where
        T: Serialize + Send + Sync + Clone,
        P: Serialize + Send + Sync + Clone,
    {
        sign(
            document,
            options,
            key,
            "Ed25519Signature2018",
            Algorithm::EdDSA,
        )
        .await
    }
    async fn prepare<T, P>(
        &self,
        document: &(dyn LinkedDataDocument + Sync),
        options: &LinkedDataProofOptions<T, P>,
        public_key: &JWK,
    ) -> Result<ProofPreparation<T, P>, Error>
    where
        T: Serialize + Send + Sync + Clone,
        P: Serialize + Send + Sync + Clone,
    {
        prepare(
            document,
            options,
            public_key,
            "Ed25519Signature2018",
            Algorithm::EdDSA,
        )
        .await
    }
    async fn complete<T, P>(
        &self,
        preparation: ProofPreparation<T, P>,
        signature: &str,
    ) -> Result<Proof<T, P>, Error>
    where
        T: Serialize + Send + Sync + Clone,
        P: Serialize + Send + Sync + Clone,
    {
        complete(preparation, signature).await
    }
}

pub struct EcdsaSecp256k1Signature2019;
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl ProofSuite for EcdsaSecp256k1Signature2019 {
    async fn sign<T, P>(
        &self,
        document: &(dyn LinkedDataDocument + Sync),
        options: &LinkedDataProofOptions<T, P>,
        key: &JWK,
    ) -> Result<Proof<T, P>, Error>
    where
        T: Serialize + Send + Sync + Clone,
        P: Serialize + Send + Sync + Clone,
    {
        sign(
            document,
            options,
            key,
            "EcdsaSecp256k1Signature2019",
            Algorithm::ES256K,
        )
        .await
    }
    async fn prepare<T, P>(
        &self,
        document: &(dyn LinkedDataDocument + Sync),
        options: &LinkedDataProofOptions<T, P>,
        public_key: &JWK,
    ) -> Result<ProofPreparation<T, P>, Error>
    where
        T: Serialize + Send + Sync + Clone,
        P: Serialize + Send + Sync + Clone,
    {
        prepare(
            document,
            options,
            public_key,
            "EcdsaSecp256k1Signature2019",
            Algorithm::ES256K,
        )
        .await
    }
    async fn complete<T, P>(
        &self,
        preparation: ProofPreparation<T, P>,
        signature: &str,
    ) -> Result<Proof<T, P>, Error>
    where
        T: Serialize + Send + Sync + Clone,
        P: Serialize + Send + Sync + Clone,
    {
        complete(preparation, signature).await
    }
}

pub struct EcdsaSecp256k1RecoverySignature2020;
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl ProofSuite for EcdsaSecp256k1RecoverySignature2020 {
    async fn sign<T, P>(
        &self,
        document: &(dyn LinkedDataDocument + Sync),
        options: &LinkedDataProofOptions<T, P>,
        key: &JWK,
    ) -> Result<Proof<T, P>, Error>
    where
        T: Serialize + Send + Sync + Clone,
        P: Serialize + Send + Sync + Clone,
    {
        if let Some(key_algorithm) = key.algorithm {
            if key_algorithm != Algorithm::ES256KR {
                return Err(Error::AlgorithmMismatch);
            }
        }
        let proof = Proof {
            context: serde_json::json!([
                crate::jsonld::DIF_ESRS2020_CONTEXT,
                crate::jsonld::ESRS2020_EXTRA_CONTEXT,
            ]),
            proof_purpose: options.proof_purpose.clone(),
            verification_method: options.verification_method.clone(),
            created: Some(options.created.unwrap_or_else(now_ms)),
            domain: options.domain.clone(),
            challenge: options.challenge.clone(),
            ..Proof::new("EcdsaSecp256k1RecoverySignature2020")
        };
        sign_proof(document, proof, key, Algorithm::ES256KR).await
    }

    async fn prepare<T, P>(
        &self,
        document: &(dyn LinkedDataDocument + Sync),
        options: &LinkedDataProofOptions<T, P>,
        _public_key: &JWK,
    ) -> Result<ProofPreparation<T, P>, Error>
    where
        T: Serialize + Send + Sync + Clone,
        P: Serialize + Send + Sync + Clone,
    {
        let proof = Proof {
            context: serde_json::json!([
                crate::jsonld::DIF_ESRS2020_CONTEXT,
                crate::jsonld::ESRS2020_EXTRA_CONTEXT,
            ]),
            proof_purpose: options.proof_purpose.clone(),
            verification_method: options.verification_method.clone(),
            created: Some(options.created.unwrap_or_else(now_ms)),
            domain: options.domain.clone(),
            challenge: options.challenge.clone(),
            ..Proof::new("EcdsaSecp256k1RecoverySignature2020")
        };
        prepare_proof(document, proof, Algorithm::ES256KR).await
    }

    async fn complete<T, P>(
        &self,
        preparation: ProofPreparation<T, P>,
        signature: &str,
    ) -> Result<Proof<T, P>, Error>
    where
        T: Serialize + Send + Sync + Clone,
        P: Serialize + Send + Sync + Clone,
    {
        complete(preparation, signature).await
    }

    async fn verify<T, P>(
        &self,
        proof: &Proof<T, P>,
        document: &(dyn LinkedDataDocument + Sync),
        resolver: &dyn DIDResolver,
    ) -> Result<(), Error>
    where
        T: Serialize + Send + Sync + Clone,
        P: Serialize + Send + Sync + Clone,
    {
        let jws = proof.jws.as_ref().ok_or(Error::MissingProofSignature)?;
        let verification_method = proof
            .verification_method
            .as_ref()
            .ok_or(Error::MissingVerificationMethod)?;
        let vm = resolve_vm(&verification_method, resolver).await?;
        if vm.type_ != "EcdsaSecp256k1RecoveryMethod2020" {
            return Err(Error::VerificationMethodMismatch);
        }
        let message = to_jws_payload(document, proof).await?;
        let (_header, jwk) = crate::jws::detached_recover(&jws, &message)?;
        let account_id_str = vm.blockchain_account_id.ok_or(Error::MissingAccountId)?;
        let account_id = BlockchainAccountId::from_str(&account_id_str)?;
        account_id.verify(&jwk)?;
        Ok(())
    }
}

/// Proof type used with [did:tz](https://github.com/spruceid/did-tezos/) `tz1` addresses.
pub struct Ed25519BLAKE2BDigestSize20Base58CheckEncodedSignature2021;
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl ProofSuite for Ed25519BLAKE2BDigestSize20Base58CheckEncodedSignature2021 {
    async fn sign<T, P>(
        &self,
        document: &(dyn LinkedDataDocument + Sync),
        options: &LinkedDataProofOptions<T, P>,
        key: &JWK,
    ) -> Result<Proof<T, P>, Error>
    where
        T: Serialize + Send + Sync + Clone,
        P: Serialize + Send + Sync + Clone,
    {
        use std::collections::HashMap;
        if let Some(key_algorithm) = key.algorithm {
            if key_algorithm != Algorithm::EdBlake2b {
                return Err(Error::AlgorithmMismatch);
            }
        }
        let mut property_set = HashMap::new();
        let jwk_value = serde_json::to_value(key.to_public())?;
        // This proof type must contain the public key, because the DID is based on the hash of the
        // public key, and the public key is not otherwise recoverable.
        property_set.insert("publicKeyJwk".to_string(), jwk_value);
        // It needs custom JSON_LD context too.
        let proof = Proof {
            context: TZ_CONTEXT.clone(),
            proof_purpose: options.proof_purpose.clone(),
            verification_method: options.verification_method.clone(),
            created: Some(options.created.unwrap_or_else(now_ms)),
            domain: options.domain.clone(),
            challenge: options.challenge.clone(),
            property_set: Some(property_set),
            ..Proof::new("Ed25519BLAKE2BDigestSize20Base58CheckEncodedSignature2021")
        };
        sign_proof(document, proof, key, Algorithm::EdBlake2b).await
    }

    async fn prepare<T, P>(
        &self,
        document: &(dyn LinkedDataDocument + Sync),
        options: &LinkedDataProofOptions<T, P>,
        public_key: &JWK,
    ) -> Result<ProofPreparation<T, P>, Error>
    where
        T: Serialize + Send + Sync + Clone,
        P: Serialize + Send + Sync + Clone,
    {
        use std::collections::HashMap;
        let mut property_set = HashMap::new();
        let jwk_value = serde_json::to_value(public_key.to_public())?;
        // This proof type must contain the public key, because the DID is based on the hash of the
        // public key, and the public key is not otherwise recoverable.
        property_set.insert("publicKeyJwk".to_string(), jwk_value);
        // It needs custom JSON_LD context too.
        let proof = Proof {
            context: TZ_CONTEXT.clone(),
            proof_purpose: options.proof_purpose.clone(),
            verification_method: options.verification_method.clone(),
            created: Some(options.created.unwrap_or_else(now_ms)),
            domain: options.domain.clone(),
            challenge: options.challenge.clone(),
            ..Proof::new("Ed25519BLAKE2BDigestSize20Base58CheckEncodedSignature2021")
        };
        prepare_proof(document, proof, Algorithm::EdBlake2b).await
    }

    async fn complete<T, P>(
        &self,
        preparation: ProofPreparation<T, P>,
        signature: &str,
    ) -> Result<Proof<T, P>, Error>
    where
        T: Serialize + Send + Sync + Clone,
        P: Serialize + Send + Sync + Clone,
    {
        complete(preparation, signature).await
    }

    async fn verify<T, P>(
        &self,
        proof: &Proof<T, P>,
        document: &(dyn LinkedDataDocument + Sync),
        resolver: &dyn DIDResolver,
    ) -> Result<(), Error>
    where
        T: Serialize + Send + Sync + Clone,
        P: Serialize + Send + Sync + Clone,
    {
        let jws = proof.jws.as_ref().ok_or(Error::MissingProofSignature)?;
        let jwk = proof.public_key_jwk.as_ref().ok_or(Error::MissingKey)?;
        // Ensure the verificationMethod corresponds to the hashed public key.
        let verification_method = proof
            .verification_method
            .as_ref()
            .ok_or(Error::MissingVerificationMethod)?;
        let vm = resolve_vm(&verification_method, resolver).await?;
        let account_id: BlockchainAccountId = vm
            .blockchain_account_id
            .ok_or(Error::MissingAccountId)?
            .parse()?;
        account_id.verify(&jwk)?;
        let message = to_jws_payload(document, proof).await?;
        crate::jws::detached_verify(&jws, &message, &jwk)?;
        Ok(())
    }
}

/// Proof type used with [did:tz](https://github.com/spruceid/did-tezos/) `tz3` addresses.
pub struct P256BLAKE2BDigestSize20Base58CheckEncodedSignature2021;
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl ProofSuite for P256BLAKE2BDigestSize20Base58CheckEncodedSignature2021 {
    async fn sign<T, P>(
        &self,
        document: &(dyn LinkedDataDocument + Sync),
        options: &LinkedDataProofOptions<T, P>,
        key: &JWK,
    ) -> Result<Proof<T, P>, Error>
    where
        T: Serialize + Send + Sync + Clone,
        P: Serialize + Send + Sync + Clone,
    {
        use std::collections::HashMap;
        if let Some(key_algorithm) = key.algorithm {
            if key_algorithm != Algorithm::ESBlake2b {
                return Err(Error::AlgorithmMismatch);
            }
        }
        let mut property_set = HashMap::new();
        let jwk_value = serde_json::to_value(key.to_public())?;
        // This proof type must contain the public key, because the DID is based on the hash of the
        // public key, and the public key is not otherwise recoverable.
        property_set.insert("publicKeyJwk".to_string(), jwk_value);
        // It needs custom JSON_LD context too.
        let proof = Proof {
            context: TZ_CONTEXT.clone(),
            proof_purpose: options.proof_purpose.clone(),
            verification_method: options.verification_method.clone(),
            created: Some(options.created.unwrap_or_else(now_ms)),
            domain: options.domain.clone(),
            challenge: options.challenge.clone(),
            property_set: Some(property_set),
            ..Proof::new("P256BLAKE2BDigestSize20Base58CheckEncodedSignature2021")
        };
        sign_proof(document, proof, key, Algorithm::ESBlake2b).await
    }

    async fn prepare<T, P>(
        &self,
        document: &(dyn LinkedDataDocument + Sync),
        options: &LinkedDataProofOptions<T, P>,
        public_key: &JWK,
    ) -> Result<ProofPreparation<T, P>, Error>
    where
        T: Serialize + Send + Sync + Clone,
        P: Serialize + Send + Sync + Clone,
    {
        use std::collections::HashMap;
        let mut property_set = HashMap::new();
        let jwk_value = serde_json::to_value(public_key.to_public())?;
        // This proof type must contain the public key, because the DID is based on the hash of the
        // public key, and the public key is not otherwise recoverable.
        property_set.insert("publicKeyJwk".to_string(), jwk_value);
        // It needs custom JSON_LD context too.
        let proof = Proof {
            context: TZ_CONTEXT.clone(),
            proof_purpose: options.proof_purpose.clone(),
            verification_method: options.verification_method.clone(),
            created: Some(options.created.unwrap_or_else(now_ms)),
            domain: options.domain.clone(),
            challenge: options.challenge.clone(),
            ..Proof::new("P256BLAKE2BDigestSize20Base58CheckEncodedSignature2021")
        };
        prepare_proof(document, proof, Algorithm::ESBlake2b).await
    }

    async fn complete<T, P>(
        &self,
        preparation: ProofPreparation<T, P>,
        signature: &str,
    ) -> Result<Proof<T, P>, Error>
    where
        T: Serialize + Send + Sync + Clone,
        P: Serialize + Send + Sync + Clone,
    {
        complete(preparation, signature).await
    }

    async fn verify<T, P>(
        &self,
        proof: &Proof<T, P>,
        document: &(dyn LinkedDataDocument + Sync),
        resolver: &dyn DIDResolver,
    ) -> Result<(), Error>
    where
        T: Serialize + Send + Sync + Clone,
        P: Serialize + Send + Sync + Clone,
    {
        let jws = proof.jws.as_ref().ok_or(Error::MissingProofSignature)?;
        let jwk = proof.public_key_jwk.as_ref().ok_or(Error::MissingKey)?;
        // Ensure the verificationMethod corresponds to the hashed public key.
        let verification_method = proof
            .verification_method
            .as_ref()
            .ok_or(Error::MissingVerificationMethod)?;
        let vm = resolve_vm(&verification_method, resolver).await?;
        let account_id: BlockchainAccountId = vm
            .blockchain_account_id
            .ok_or(Error::MissingAccountId)?
            .parse()?;
        account_id.verify(&jwk)?;
        let message = to_jws_payload(document, proof).await?;
        crate::jws::detached_verify(&jws, &message, &jwk)?;
        Ok(())
    }
}

#[cfg(feature = "keccak-hash")]
pub struct Eip712Signature2021;
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg(feature = "keccak-hash")]
impl ProofSuite for Eip712Signature2021 {
    async fn sign<T, P>(
        &self,
        document: &(dyn LinkedDataDocument + Sync),
        options: &LinkedDataProofOptions<T, P>,
        key: &JWK,
    ) -> Result<Proof<T, P>, Error>
    where
        T: Serialize + Send + Sync + Clone,
        P: Serialize + Send + Sync + Clone,
    {
        use k256::ecdsa::signature::Signer;
        let mut proof = Proof {
            context: serde_json::json!([EIP712VM_CONTEXT.clone()]),
            proof_purpose: options.proof_purpose.clone(),
            verification_method: options.verification_method.clone(),
            created: Some(options.created.unwrap_or_else(now_ms)),
            domain: options.domain.clone(),
            challenge: options.challenge.clone(),
            ..Proof::new("Eip712Signature2021")
        };
        let typed_data = TypedData::from_document_and_options(document, &proof).await?;
        let bytes = typed_data.bytes()?;
        let ec_params = match &key.params {
            JWKParams::EC(ec) => ec,
            _ => return Err(Error::KeyTypeNotImplemented),
        };
        let secret_key = k256::SecretKey::try_from(ec_params)?;
        let signing_key = k256::ecdsa::SigningKey::from(secret_key);
        let sig: k256::ecdsa::recoverable::Signature = signing_key.try_sign(&bytes)?;
        let sig_bytes = &mut sig.as_ref().to_vec();
        // Recovery ID starts at 27 instead of 0.
        sig_bytes[64] = sig_bytes[64] + 27;
        let sig_hex = crate::keccak_hash::bytes_to_lowerhex(sig_bytes);
        proof.proof_value = Some(sig_hex);
        Ok(proof)
    }

    async fn prepare<T, P>(
        &self,
        document: &(dyn LinkedDataDocument + Sync),
        options: &LinkedDataProofOptions<T, P>,
        _public_key: &JWK,
    ) -> Result<ProofPreparation<T, P>, Error>
    where
        T: Serialize + Send + Sync + Clone,
        P: Serialize + Send + Sync + Clone,
    {
        let proof = Proof {
            context: serde_json::json!([EIP712VM_CONTEXT.clone()]),
            proof_purpose: options.proof_purpose.clone(),
            verification_method: options.verification_method.clone(),
            created: Some(options.created.unwrap_or_else(now_ms)),
            domain: options.domain.clone(),
            challenge: options.challenge.clone(),
            ..Proof::new("Eip712Signature2021")
        };
        let typed_data = TypedData::from_document_and_options(document, &proof).await?;
        Ok(ProofPreparation {
            proof,
            jws_header: None,
            signing_input: SigningInput::TypedData(typed_data),
        })
    }

    async fn complete<T, P>(
        &self,
        preparation: ProofPreparation<T, P>,
        signature: &str,
    ) -> Result<Proof<T, P>, Error>
    where
        T: Serialize + Send + Sync + Clone,
        P: Serialize + Send + Sync + Clone,
    {
        let mut proof = preparation.proof;
        proof.proof_value = Some(signature.to_string());
        Ok(proof)
    }

    async fn verify<T, P>(
        &self,
        proof: &Proof<T, P>,
        document: &(dyn LinkedDataDocument + Sync),
        resolver: &dyn DIDResolver,
    ) -> Result<(), Error>
    where
        T: Serialize + Send + Sync + Clone,
        P: Serialize + Send + Sync + Clone,
    {
        let sig_hex = proof
            .proof_value
            .as_ref()
            .ok_or(Error::MissingProofSignature)?;
        let verification_method = proof
            .verification_method
            .as_ref()
            .ok_or(Error::MissingVerificationMethod)?;
        let vm = resolve_vm(&verification_method, resolver).await?;
        if vm.type_ != "Eip712Method2021" {
            return Err(Error::VerificationMethodMismatch);
        }
        let typed_data = TypedData::from_document_and_options(document, &proof).await?;
        let bytes = typed_data.bytes()?;
        if !sig_hex.starts_with("0x") {
            return Err(Error::HexString);
        }
        let dec_sig = hex::decode(&sig_hex[2..])?;
        let sig = k256::ecdsa::Signature::try_from(&dec_sig[..64])?;
        let rec_id = k256::ecdsa::recoverable::Id::try_from(dec_sig[64] - 27)?;
        let sig = k256::ecdsa::recoverable::Signature::new(&sig, rec_id)?;
        // TODO this step needs keccak-hash, may need better features management
        let recovered_key = sig.recover_verify_key(&bytes)?;
        use crate::jwk::ECParams;
        let jwk = JWK {
            params: JWKParams::EC(ECParams::try_from(&k256::PublicKey::from_sec1_bytes(
                &recovered_key.to_bytes(),
            )?)?),
            public_key_use: None,
            key_operations: None,
            algorithm: None,
            key_id: None,
            x509_url: None,
            x509_certificate_chain: None,
            x509_thumbprint_sha1: None,
            x509_thumbprint_sha256: None,
        };
        let account_id_str = vm.blockchain_account_id.ok_or(Error::MissingAccountId)?;
        let account_id = BlockchainAccountId::from_str(&account_id_str)?;
        account_id.verify(&jwk)?;
        Ok(())
    }
}

async fn micheline_from_document_and_options<T, P>(
    document: &(dyn LinkedDataDocument + Sync),
    proof: &Proof<T, P>,
) -> Result<Vec<u8>, Error>
where
    T: Serialize + Send + Sync + Clone,
    P: Serialize + Send + Sync + Clone,
{
    let doc_dataset = document.to_dataset_for_signing(None).await?;
    let doc_dataset_normalized = urdna2015::normalize(&doc_dataset)?;
    let doc_normalized = doc_dataset_normalized.to_nquads()?;
    let sigopts_dataset = proof.to_dataset_for_signing(Some(document)).await?;
    let sigopts_dataset_normalized = urdna2015::normalize(&sigopts_dataset)?;
    let sigopts_normalized = sigopts_dataset_normalized.to_nquads()?;
    let msg = ["", &sigopts_normalized, &doc_normalized].join("\n");
    let data = crate::tzkey::encode_tezos_signed_message(&msg)?;
    Ok(data)
}

pub struct TezosSignature2021;
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl ProofSuite for TezosSignature2021 {
    async fn sign<T, P>(
        &self,
        document: &(dyn LinkedDataDocument + Sync),
        options: &LinkedDataProofOptions<T, P>,
        key: &JWK,
    ) -> Result<Proof<T, P>, Error>
    where
        T: Serialize + Send + Sync + Clone,
        P: Serialize + Send + Sync + Clone,
    {
        let algorithm = key.get_algorithm().ok_or(Error::MissingAlgorithm)?;
        let mut proof = Proof {
            context: TZVM_CONTEXT.clone(),
            proof_purpose: options.proof_purpose.clone(),
            verification_method: options.verification_method.clone(),
            created: Some(options.created.unwrap_or_else(now_ms)),
            domain: options.domain.clone(),
            challenge: options.challenge.clone(),
            ..Proof::new("TezosSignature2021")
        };
        let micheline = micheline_from_document_and_options(document, &proof).await?;
        let sig = crate::jws::sign_bytes(algorithm, &micheline, key)?;
        let mut sig_prefixed = Vec::new();
        let prefix: &[u8] = match algorithm {
            Algorithm::EdBlake2b => &EDSIG_PREFIX,
            Algorithm::ESBlake2bK => &SPSIG_PREFIX,
            Algorithm::ESBlake2b => &P2SIG_PREFIX,
            _ => return Err(Error::UnsupportedAlgorithm),
        };
        sig_prefixed.extend_from_slice(&prefix);
        sig_prefixed.extend_from_slice(&sig);
        let sig_bs58 = bs58::encode(sig_prefixed).with_check().into_string();
        proof.proof_value = Some(sig_bs58);
        Ok(proof)
    }

    async fn prepare<T, P>(
        &self,
        document: &(dyn LinkedDataDocument + Sync),
        options: &LinkedDataProofOptions<T, P>,
        public_key: &JWK,
    ) -> Result<ProofPreparation<T, P>, Error>
    where
        T: Serialize + Send + Sync + Clone,
        P: Serialize + Send + Sync + Clone,
    {
        let mut property_set = std::collections::HashMap::new();
        // TODO: dereference VM URL to check if VM already contains public key.
        let jwk_value = serde_json::to_value(public_key.to_public())?;
        property_set.insert("publicKeyJwk".to_string(), jwk_value);
        let proof = Proof {
            context: TZVM_CONTEXT.clone(),
            proof_purpose: options.proof_purpose.clone(),
            verification_method: options.verification_method.clone(),
            created: Some(options.created.unwrap_or_else(now_ms)),
            domain: options.domain.clone(),
            challenge: options.challenge.clone(),
            property_set: Some(property_set),
            ..Proof::new("TezosSignature2021")
        };
        let micheline = micheline_from_document_and_options(document, &proof).await?;
        let micheline_string = hex::encode(micheline);
        Ok(ProofPreparation {
            proof,
            jws_header: None,
            signing_input: SigningInput::Micheline {
                micheline: micheline_string,
            },
        })
    }

    async fn complete<T, P>(
        &self,
        preparation: ProofPreparation<T, P>,
        signature: &str,
    ) -> Result<Proof<T, P>, Error>
    where
        T: Serialize + Send + Sync + Clone,
        P: Serialize + Send + Sync + Clone,
    {
        let mut proof = preparation.proof;
        proof.proof_value = Some(signature.to_string());
        Ok(proof)
    }

    async fn verify<T, P>(
        &self,
        proof: &Proof<T, P>,
        document: &(dyn LinkedDataDocument + Sync),
        resolver: &dyn DIDResolver,
    ) -> Result<(), Error>
    where
        T: Serialize + Send + Sync + Clone,
        P: Serialize + Send + Sync + Clone,
    {
        let sig_bs58 = proof
            .proof_value
            .as_ref()
            .ok_or(Error::MissingProofSignature)?;
        let verification_method = proof
            .verification_method
            .as_ref()
            .ok_or(Error::MissingVerificationMethod)?;
        let proof_jwk_opt: Option<&JWK> = proof.public_key_jwk.as_ref();

        let (algorithm, sig) = crate::tzkey::decode_tzsig(sig_bs58)?;
        let vm = resolve_vm(&verification_method, resolver).await?;
        if vm.type_ != "TezosMethod2021" {
            return Err(Error::VerificationMethodMismatch);
        }

        let micheline = micheline_from_document_and_options(document, &proof).await?;
        let account_id_opt: Option<BlockchainAccountId> = match vm.blockchain_account_id {
            Some(account_id_string) => Some(account_id_string.parse()?),
            None => None,
        };

        // VM must have either publicKeyJwk or blockchainAccountId.
        if let Some(vm_jwk) = vm.public_key_jwk {
            // If VM has publicKey, use that to veify the signature.
            crate::jws::verify_bytes(algorithm, &micheline, &vm_jwk, &sig)?;
            // Note: VM blockchainAccountId is ignored in this case.
        } else {
            if let Some(account_id) = account_id_opt {
                // VM does not have publicKeyJwk: proof must have public key
                if let Some(proof_jwk) = proof_jwk_opt {
                    // Proof has public key: verify it with blockchainAccountId,
                    account_id.verify(&proof_jwk)?;
                    // and verify the signature.
                    crate::jws::verify_bytes(algorithm, &micheline, &proof_jwk, &sig)?;
                } else {
                    return Err(Error::MissingKey);
                }
            } else {
                return Err(Error::MissingKey);
            }
        };
        Ok(())
    }
}

pub struct SolanaSignature2021;
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl ProofSuite for SolanaSignature2021 {
    async fn sign<T, P>(
        &self,
        document: &(dyn LinkedDataDocument + Sync),
        options: &LinkedDataProofOptions<T, P>,
        key: &JWK,
    ) -> Result<Proof<T, P>, Error>
    where
        T: Serialize + Send + Sync + Clone,
        P: Serialize + Send + Sync + Clone,
    {
        let mut proof = Proof {
            context: serde_json::json!([SOLVM_CONTEXT.clone()]),
            proof_purpose: options.proof_purpose.clone(),
            verification_method: options.verification_method.clone(),
            created: Some(options.created.unwrap_or_else(now_ms)),
            domain: options.domain.clone(),
            challenge: options.challenge.clone(),
            ..Proof::new("SolanaSignature2021")
        };
        let message = to_jws_payload(document, &proof).await?;
        let tx = crate::soltx::LocalSolanaTransaction::with_message(&message);
        let bytes = tx.to_bytes();
        let sig = crate::jws::sign_bytes(Algorithm::EdDSA, &bytes, key)?;
        let sig_b58 = bs58::encode(&sig).into_string();
        proof.proof_value = Some(sig_b58);
        Ok(proof)
    }

    async fn prepare<T, P>(
        &self,
        document: &(dyn LinkedDataDocument + Sync),
        options: &LinkedDataProofOptions<T, P>,
        _public_key: &JWK,
    ) -> Result<ProofPreparation<T, P>, Error>
    where
        T: Serialize + Send + Sync + Clone,
        P: Serialize + Send + Sync + Clone,
    {
        let proof = Proof {
            context: serde_json::json!([SOLVM_CONTEXT.clone()]),
            proof_purpose: options.proof_purpose.clone(),
            verification_method: options.verification_method.clone(),
            created: Some(options.created.unwrap_or_else(now_ms)),
            domain: options.domain.clone(),
            challenge: options.challenge.clone(),
            ..Proof::new("SolanaSignature2021")
        };
        let message = to_jws_payload(document, &proof).await?;
        let tx = crate::soltx::LocalSolanaTransaction::with_message(&message);
        let bytes = tx.to_bytes();
        Ok(ProofPreparation {
            proof,
            jws_header: None,
            signing_input: SigningInput::Bytes(Base64urlUInt(bytes)),
        })
    }

    async fn complete<T, P>(
        &self,
        preparation: ProofPreparation<T, P>,
        signature: &str,
    ) -> Result<Proof<T, P>, Error>
    where
        T: Serialize + Send + Sync + Clone,
        P: Serialize + Send + Sync + Clone,
    {
        let mut proof = preparation.proof;
        proof.proof_value = Some(signature.to_string());
        Ok(proof)
    }

    async fn verify<T, P>(
        &self,
        proof: &Proof<T, P>,
        document: &(dyn LinkedDataDocument + Sync),
        resolver: &dyn DIDResolver,
    ) -> Result<(), Error>
    where
        T: Serialize + Send + Sync + Clone,
        P: Serialize + Send + Sync + Clone,
    {
        let sig_b58 = proof
            .proof_value
            .as_ref()
            .ok_or(Error::MissingProofSignature)?;
        let verification_method = proof
            .verification_method
            .as_ref()
            .ok_or(Error::MissingVerificationMethod)?;
        let vm = resolve_vm(&verification_method, resolver).await?;
        if vm.type_ != "SolanaMethod2021" {
            return Err(Error::VerificationMethodMismatch);
        }
        let key = vm.public_key_jwk.ok_or(Error::MissingKey)?;
        let message = to_jws_payload(document, &proof).await?;
        let tx = crate::soltx::LocalSolanaTransaction::with_message(&message);
        let bytes = tx.to_bytes();
        let sig = bs58::decode(&sig_b58).into_vec()?;
        crate::jws::verify_bytes(Algorithm::EdDSA, &bytes, &key, &sig)?;
        Ok(())
    }
}

pub struct EcdsaSecp256r1Signature2019;
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl ProofSuite for EcdsaSecp256r1Signature2019 {
    async fn sign<T, P>(
        &self,
        document: &(dyn LinkedDataDocument + Sync),
        options: &LinkedDataProofOptions<T, P>,
        key: &JWK,
    ) -> Result<Proof<T, P>, Error>
    where
        T: Serialize + Send + Sync + Clone,
        P: Serialize + Send + Sync + Clone,
    {
        sign(
            document,
            options,
            key,
            "EcdsaSecp256r1Signature2019",
            Algorithm::ES256,
        )
        .await
    }
    async fn prepare<T, P>(
        &self,
        document: &(dyn LinkedDataDocument + Sync),
        options: &LinkedDataProofOptions<T, P>,
        public_key: &JWK,
    ) -> Result<ProofPreparation<T, P>, Error>
    where
        T: Serialize + Send + Sync + Clone,
        P: Serialize + Send + Sync + Clone,
    {
        prepare(
            document,
            options,
            public_key,
            "EcdsaSecp256r1Signature2019",
            Algorithm::ES256,
        )
        .await
    }
    async fn complete<T, P>(
        &self,
        preparation: ProofPreparation<T, P>,
        signature: &str,
    ) -> Result<Proof<T, P>, Error>
    where
        T: Serialize + Send + Sync + Clone,
        P: Serialize + Send + Sync + Clone,
    {
        complete(preparation, signature).await
    }
}

/// <https://w3c-ccg.github.io/lds-jws2020/>
pub struct JsonWebSignature2020;
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl ProofSuite for JsonWebSignature2020 {
    async fn sign<T, P>(
        &self,
        document: &(dyn LinkedDataDocument + Sync),
        options: &LinkedDataProofOptions<T, P>,
        key: &JWK,
    ) -> Result<Proof<T, P>, Error>
    where
        T: Serialize + Send + Sync + Clone,
        P: Serialize + Send + Sync + Clone,
    {
        let algorithm = key.get_algorithm().ok_or(Error::MissingAlgorithm)?;
        if algorithm != Algorithm::ES256 {
            // TODO: support JsonWebSignature2020 more generally
            return Err(Error::UnsupportedAlgorithm)?;
        }
        let proof = Proof {
            context: serde_json::json!([crate::jsonld::LDS_JWS2020_V1_CONTEXT.clone()]),
            proof_purpose: options.proof_purpose.clone(),
            verification_method: options.verification_method.clone(),
            created: Some(options.created.unwrap_or_else(now_ms)),
            domain: options.domain.clone(),
            challenge: options.challenge.clone(),
            ..Proof::new("JsonWebSignature2020")
        };
        sign_proof(document, proof, key, algorithm).await
    }
    async fn prepare<T, P>(
        &self,
        document: &(dyn LinkedDataDocument + Sync),
        options: &LinkedDataProofOptions<T, P>,
        public_key: &JWK,
    ) -> Result<ProofPreparation<T, P>, Error>
    where
        T: Serialize + Send + Sync + Clone,
        P: Serialize + Send + Sync + Clone,
    {
        let algorithm = public_key.get_algorithm().ok_or(Error::MissingAlgorithm)?;
        if algorithm != Algorithm::ES256 {
            return Err(Error::UnsupportedAlgorithm)?;
        }
        let proof = Proof {
            context: serde_json::json!([crate::jsonld::LDS_JWS2020_V1_CONTEXT.clone()]),
            proof_purpose: options.proof_purpose.clone(),
            verification_method: options.verification_method.clone(),
            created: Some(options.created.unwrap_or_else(now_ms)),
            domain: options.domain.clone(),
            challenge: options.challenge.clone(),
            ..Proof::new("JsonWebSignature2020")
        };
        prepare_proof(document, proof, algorithm).await
    }
    async fn complete<T, P>(
        &self,
        preparation: ProofPreparation<T, P>,
        signature: &str,
    ) -> Result<Proof<T, P>, Error>
    where
        T: Serialize + Send + Sync + Clone,
        P: Serialize + Send + Sync + Clone,
    {
        complete(preparation, signature).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jsonld::CREDENTIALS_V1_CONTEXT;

    struct ExampleDocument;

    #[async_trait]
    impl LinkedDataDocument for ExampleDocument {
        fn get_contexts(&self) -> Result<Option<String>, Error> {
            Ok(Some(serde_json::to_string(&*CREDENTIALS_V1_CONTEXT)?))
        }
        async fn to_dataset_for_signing(
            &self,
            _parent: Option<&(dyn LinkedDataDocument + Sync)>,
        ) -> Result<DataSet, Error> {
            use crate::rdf;
            let mut dataset = DataSet::default();
            let statement = rdf::Statement {
                subject: rdf::Subject::BlankNodeLabel(rdf::BlankNodeLabel("_:c14n0".to_string())),
                predicate: rdf::Predicate::IRIRef(rdf::IRIRef(
                    "http://www.w3.org/1999/02/22-rdf-syntax-ns#type".to_string(),
                )),
                object: rdf::Object::IRIRef(rdf::IRIRef(
                    "http://example.org/vocab#Foo".to_string(),
                )),
                graph_label: None,
            };
            dataset.add_statement(statement);
            Ok(dataset)
        }
    }

    #[cfg(feature = "secp256k1")]
    #[async_std::test]
    async fn eip712vm() {
        let mut key = JWK::generate_secp256k1().unwrap();
        key.algorithm = Some(Algorithm::ES256KR);
        let vm = format!("{}#Eip712Method2021", "did:example:foo");
        let issue_options = LinkedDataProofOptions {
            verification_method: Some(vm),
            ..Default::default()
        };
        let doc = ExampleDocument;
        let _proof = LinkedDataProofs::sign(&doc, &issue_options, &key)
            .await
            .unwrap();
    }

    #[async_std::test]
    async fn tezos_vm_tz1() {
        let mut key = JWK::generate_ed25519().unwrap();
        key.algorithm = Some(Algorithm::EdBlake2b);
        let vm = format!("{}#TezosMethod2021", "did:example:foo");
        let issue_options = LinkedDataProofOptions {
            verification_method: Some(vm),
            ..Default::default()
        };
        let doc = ExampleDocument;
        let proof = LinkedDataProofs::sign(&doc, &issue_options, &key)
            .await
            .unwrap();
        println!("{}", serde_json::to_string(&proof).unwrap());
        // TODO: verify
    }

    #[async_std::test]
    #[cfg(feature = "secp256k1")]
    async fn tezos_vm_tz2() {
        let mut key = JWK::generate_secp256k1().unwrap();
        key.algorithm = Some(Algorithm::ESBlake2bK);
        let vm = format!("{}#TezosMethod2021", "did:example:foo");
        let issue_options = LinkedDataProofOptions {
            verification_method: Some(vm),
            ..Default::default()
        };
        let doc = ExampleDocument;
        let proof = LinkedDataProofs::sign(&doc, &issue_options, &key)
            .await
            .unwrap();
        println!("{}", serde_json::to_string(&proof).unwrap());
    }

    /*
    #[async_std::test]
    async fn solvm() {
        let mut key = JWK::generate_secp256k1().unwrap();
        key.algorithm = Some(Algorithm::ES256KR);
        let vm = format!("{}#SolanaMethod2021", "did:example:foo");
        let issue_options = LinkedDataProofOptions {
            verification_method: Some(vm),
            ..Default::default()
        };
        let doc = ExampleDocument;
        let _proof = LinkedDataProofs::sign(&doc, &issue_options, &key)
            .await
            .unwrap();
    }
    */
}
